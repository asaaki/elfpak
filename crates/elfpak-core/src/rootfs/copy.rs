//! Materialization of a [`BundlePlan`] into a directory tree.
//!
//! Nothing is written outside the output root, the source filesystem is only
//! ever read, and no file appears that the plan did not list.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result, io};
use crate::plan::{BundlePlan, PlannedFile, PlannedFileKind};

/// Fixed mtime for every regular file, so repeated runs are byte-identical.
/// Overridable through `SOURCE_DATE_EPOCH`.
fn source_date_epoch() -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_secs(source_date_epoch_secs())
}

pub(crate) fn source_date_epoch_secs() -> u64 {
    std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

pub struct RootFsBuilder {
    output: PathBuf,
    clean: bool,
}

impl RootFsBuilder {
    pub fn new(output: impl Into<PathBuf>) -> RootFsBuilder {
        RootFsBuilder {
            output: output.into(),
            clean: false,
        }
    }

    /// Remove an existing output directory before writing.
    pub fn clean(mut self, clean: bool) -> RootFsBuilder {
        self.clean = clean;
        self
    }

    pub fn apply(&self, plan: &BundlePlan) -> Result<RootFsReport> {
        if self.clean && self.output.exists() {
            std::fs::remove_dir_all(&self.output).map_err(|e| io(&self.output, e))?;
        }
        std::fs::create_dir_all(&self.output).map_err(|e| io(&self.output, e))?;
        let output = self
            .output
            .canonicalize()
            .map_err(|e| io(&self.output, e))?;

        let mut report = RootFsReport::default();
        // Entries are sorted by destination, so parents always precede children.
        for file in &plan.files {
            let target = self.target_path(&output, file)?;
            match file.kind {
                PlannedFileKind::Directory => {
                    if !target.exists() {
                        std::fs::create_dir_all(&target).map_err(|e| io(&target, e))?;
                    }
                    set_mode(&target, file.mode)?;
                    report.directories += 1;
                }
                PlannedFileKind::Symlink => {
                    let link_target = file
                        .link_target
                        .clone()
                        .unwrap_or_else(|| PathBuf::from("/"));
                    if std::fs::symlink_metadata(&target).is_ok() {
                        std::fs::remove_file(&target).map_err(|e| io(&target, e))?;
                    }
                    std::os::unix::fs::symlink(&link_target, &target)
                        .map_err(|e| io(&target, e))?;
                    report.symlinks += 1;
                }
                _ => {
                    let bytes = match (&file.content, &file.source) {
                        (Some(content), _) => content.clone(),
                        (None, Some(source)) => std::fs::read(source).map_err(|e| io(source, e))?,
                        (None, None) => Vec::new(),
                    };
                    std::fs::write(&target, &bytes).map_err(|e| io(&target, e))?;
                    set_mode(&target, file.mode)?;
                    set_mtime(&target)?;
                    report.files += 1;
                    report.bytes += bytes.len() as u64;
                }
            }
        }

        Ok(report)
    }

    /// Resolve a plan destination inside the output root, refusing anything that
    /// would write through a symlink or outside the root.
    fn target_path(&self, output: &Path, file: &PlannedFile) -> Result<PathBuf> {
        let target = crate::paths::join_under(output, &file.destination);
        if !target.starts_with(output) {
            return Err(Error::PathEscape {
                path: file.destination.clone(),
                kind: "output",
            });
        }
        if let Some(parent) = target.parent() {
            if has_symlinked_ancestor(output, parent) {
                return Err(Error::PathEscape {
                    path: file.destination.clone(),
                    kind: "output",
                });
            }
            std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
        Ok(target)
    }
}

/// Reject any existing symlink between the output root and `path`. Checking
/// only the immediate parent lets `create_dir_all` follow a pre-existing
/// symlink higher in the path.
fn has_symlinked_ancestor(output: &Path, path: &Path) -> bool {
    let mut current = path;
    while current != output {
        match std::fs::symlink_metadata(current) {
            Ok(metadata) if metadata.is_symlink() => return true,
            Ok(_) | Err(_) => {}
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
    }
    false
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RootFsReport {
    pub files: usize,
    pub directories: usize,
    pub symlinks: usize,
    pub bytes: u64,
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|e| io(path, e))
}

fn set_mtime(path: &Path) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| io(path, e))?;
    let time = source_date_epoch();
    let times = std::fs::FileTimes::new()
        .set_accessed(time)
        .set_modified(time);
    // Not every filesystem supports this; reproducibility is best-effort.
    let _ = file.set_times(times);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::has_symlinked_ancestor;

    #[test]
    fn detects_a_symlink_above_a_missing_parent() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&output).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, output.join("nested")).unwrap();

        assert!(has_symlinked_ancestor(
            &output,
            &output.join("nested/deeper")
        ));
    }
}
