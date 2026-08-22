//! Materialization of a [`BundlePlan`] into a directory tree.
//!
//! Nothing is written outside the output root, the source filesystem is only
//! ever read, and no file appears that the plan did not list.

use crate::{
    error::{Error, Result, io},
    hash::{HashingReader, ensure_matches_plan},
    plan::{BundlePlan, PlannedFile, PlannedFileKind},
};
use std::path::{Path, PathBuf};

/// Fixed mtime for every entry, so repeated runs are byte-identical.
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

#[derive(Debug)]
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

    /// Materialize a plan: create, link and copy, in plan order.
    pub fn apply(&self, plan: &BundlePlan) -> Result<RootFsReport> {
        if self.clean && self.output.exists() {
            guard_clean(&self.output)?;
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
            file.assert_well_formed();
            let target = self.target_path(&output, file)?;
            assert!(target.starts_with(&output));

            match file.kind {
                PlannedFileKind::Directory => {
                    write_directory(&target, file.mode)?;
                    report.directories += 1;
                }
                PlannedFileKind::Symlink => {
                    write_symlink(&target, file.link_target.as_deref())?;
                    report.symlinks += 1;
                }
                _ => {
                    // Removing first is what keeps the write inside the output
                    // root: writing onto a pre-existing symlink would follow it.
                    remove_existing(&target)?;
                    report.bytes += write_file(&target, file)?;
                    set_mode(&target, file.mode)?;
                    pin_times(&target);
                    report.files += 1;
                }
            }
        }

        // Directory timestamps are pinned last: writing children updates them.
        // Deepest first, so a parent is not touched again after it is pinned.
        for file in plan
            .files
            .iter()
            .rev()
            .filter(|f| f.kind == PlannedFileKind::Directory)
        {
            pin_times(&crate::paths::join_under(&output, &file.destination));
        }

        let entries = report.files + report.directories + report.symlinks;
        assert_eq!(entries as usize, plan.files.len());
        Ok(report)
    }

    /// Resolve a plan destination inside the output root, refusing anything that
    /// would write through a symlink or outside the root.
    fn target_path(&self, output: &Path, file: &PlannedFile) -> Result<PathBuf> {
        assert!(file.destination.is_absolute());

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

/// Create a directory, replacing anything else that occupies the path —
/// including a symlink, which a later write would silently follow.
fn write_directory(target: &Path, mode: u32) -> Result<()> {
    let existing = std::fs::symlink_metadata(target).ok();
    if !existing.is_some_and(|metadata| metadata.is_dir()) {
        remove_existing(target)?;
        std::fs::create_dir_all(target).map_err(|e| io(target, e))?;
    }
    set_mode(target, mode)
}

/// Recreate a symlink verbatim. Plan validation guarantees a target is present.
fn write_symlink(target: &Path, link_target: Option<&Path>) -> Result<()> {
    let link_target = link_target.expect("validated symlinks have a target");
    remove_existing(target)?;
    std::os::unix::fs::symlink(link_target, target).map_err(|e| io(target, e))
}

/// Write the contents of a planned entry, returning the number of bytes.
///
/// Source-backed files are copied rather than read into memory, so an
/// `--include` of an arbitrarily large file costs no more than a buffer.
fn write_file(target: &Path, file: &PlannedFile) -> Result<u64> {
    match (&file.content, &file.source) {
        (Some(content), None) => {
            assert_eq!(content.len() as u64, file.size);
            std::fs::write(target, content).map_err(|e| io(target, e))?;
            Ok(content.len() as u64)
        }
        (None, Some(source)) => {
            let input = std::fs::File::open(source).map_err(|e| io(source, e))?;
            let mut input = HashingReader::new(std::io::BufReader::new(input));
            let mut output = std::fs::File::create(target).map_err(|e| io(target, e))?;
            let copy_result = std::io::copy(&mut input, &mut output).map_err(|e| io(source, e));
            drop(output);
            let (digest, size) = input.finish();

            if let Err(error) = copy_result {
                let _ = remove_existing(target);
                return Err(error);
            }
            let expected = file
                .sha256
                .as_ref()
                .expect("validated regular files have a digest");
            if let Err(error) = ensure_matches_plan(source, expected, file.size, digest, size) {
                let _ = remove_existing(target);
                return Err(error);
            }
            Ok(size)
        }
        _ => unreachable!("validated regular files have exactly one content source"),
    }
}

/// Refuse to delete something that is obviously not a previous bundle: `--clean`
/// is meant to replace an output directory, not to wipe a filesystem.
fn guard_clean(output: &Path) -> Result<()> {
    let resolved = output
        .canonicalize()
        .unwrap_or_else(|_| output.to_path_buf());
    if resolved.parent().is_none() {
        return Err(Error::Config {
            message: format!("refusing to --clean `{}`", resolved.display()),
        });
    }
    Ok(())
}

/// Remove whatever currently occupies `path`, without following symlinks.
fn remove_existing(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        // `is_dir` is false for a symlink to a directory, so the link itself is
        // unlinked and its target is left alone.
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path).map_err(|e| io(path, e)),
        Ok(_) => std::fs::remove_file(path).map_err(|e| io(path, e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io(path, e)),
    }
}

/// Reject any existing symlink between the output root and `path`. Checking
/// only the immediate parent lets `create_dir_all` follow a pre-existing
/// symlink higher in the path.
fn has_symlinked_ancestor(output: &Path, path: &Path) -> bool {
    // Every step drops one component, so the walk is bounded by the depth of
    // the path it starts from.
    let depth = path.components().count();
    let mut steps = 0usize;
    let mut current = path;
    while current != output {
        steps += 1;
        assert!(steps <= depth);

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

/// What was written, counted as it was written.
#[derive(Debug, Default, Clone, Copy)]
pub struct RootFsReport {
    pub files: u32,
    pub directories: u32,
    pub symlinks: u32,
    pub bytes: u64,
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    assert!(mode <= 0o7777);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|e| io(path, e))
}

/// Pin access and modification times. Not every filesystem supports this, and
/// symlink timestamps cannot be set through `std` at all, so this is
/// best-effort: the tar backend is the byte-reproducible artifact.
fn pin_times(path: &Path) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let time = source_date_epoch();
    let _ = file.set_times(
        std::fs::FileTimes::new()
            .set_accessed(time)
            .set_modified(time),
    );
}

#[cfg(test)]
mod tests {
    use super::{guard_clean, has_symlinked_ancestor, remove_existing};
    use std::path::Path;

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

    #[test]
    fn removing_a_symlink_leaves_its_target_alone() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let link = temp.path().join("link");
        std::fs::write(&target, b"keep me").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        remove_existing(&link).unwrap();
        assert!(!link.exists());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep me");

        // Removing something that is not there is not an error.
        remove_existing(&link).unwrap();
    }

    #[test]
    fn clean_refuses_to_delete_a_filesystem_root() {
        let err = guard_clean(Path::new("/")).unwrap_err();
        assert_eq!(err.code(), "E4001");
        let temp = tempfile::tempdir().unwrap();
        guard_clean(&temp.path().join("rootfs")).unwrap();
    }
}
