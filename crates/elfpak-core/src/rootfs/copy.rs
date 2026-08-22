//! Materialization of a [`BundlePlan`] into a directory tree.
//!
//! Nothing is written outside the output root and the source filesystem is only
//! ever read. A clean output contains only planned entries; without `clean`,
//! pre-existing unplanned entries are deliberately retained.

use crate::{
    error::{Error, Result, io},
    hash::{HashingReader, ensure_matches_plan},
    plan::{BundlePlan, PlannedFile, PlannedFileKind},
};
use std::path::{Path, PathBuf};

/// Explicit reproducible-build timestamp, when configured by the caller.
fn source_date_epoch() -> Result<Option<std::time::SystemTime>> {
    let Some(seconds) = configured_source_date_epoch_secs()? else {
        return Ok(None);
    };
    std::time::UNIX_EPOCH
        .checked_add(std::time::Duration::from_secs(seconds))
        .map(Some)
        .ok_or_else(|| Error::Config {
            message: "SOURCE_DATE_EPOCH is outside the supported system-time range".to_string(),
        })
}

pub(crate) fn source_date_epoch_secs() -> Result<u64> {
    Ok(configured_source_date_epoch_secs()?.unwrap_or(0))
}

fn configured_source_date_epoch_secs() -> Result<Option<u64>> {
    match std::env::var("SOURCE_DATE_EPOCH") {
        Ok(value) => value
            .trim()
            .parse::<u64>()
            .map(Some)
            .map_err(|_| Error::Config {
                message: format!(
                    "invalid SOURCE_DATE_EPOCH `{value}` (expected an unsigned integer)"
                ),
            }),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(Error::Config {
            message: "SOURCE_DATE_EPOCH is not valid Unicode".to_string(),
        }),
    }
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

    /// Exclude an existing output directory from the staged replacement.
    pub fn clean(mut self, clean: bool) -> RootFsBuilder {
        self.clean = clean;
        self
    }

    /// Materialize a plan into a sibling staging directory, then publish it.
    /// A failed build leaves an existing output untouched and exposes no new
    /// output when the destination did not exist.
    pub fn apply(&self, plan: &BundlePlan) -> Result<RootFsReport> {
        // Capture one time for all planned entries. Reproducible-build callers
        // can override the ordinary materialization time through SOURCE_DATE_EPOCH.
        let timestamp = source_date_epoch()?.unwrap_or_else(std::time::SystemTime::now);
        guard_output(&self.output)?;
        let parent = output_parent(&self.output);
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;

        let stage = tempfile::Builder::new()
            .prefix(".elfpak-rootfs-")
            .tempdir_in(parent)
            .map_err(|e| io(parent, e))?;

        if path_exists(&self.output) {
            ensure_directory(&self.output)?;
        }
        if path_exists(&self.output) && !self.clean {
            clone_tree(&self.output, stage.path())?;
        } else {
            // Temporary directories start as 0700. The root of a generated
            // filesystem should have the same normalized mode as its ordinary
            // directory entries.
            set_mode(stage.path(), 0o755)?;
        }
        if self.clean && path_exists(&self.output) {
            guard_clean(&self.output)?;
        }

        let report = self.apply_into(plan, stage.path(), timestamp)?;
        publish_directory(stage, &self.output)?;
        Ok(report)
    }

    /// Apply a plan to an isolated directory that is not externally visible.
    fn apply_into(
        &self,
        plan: &BundlePlan,
        output: &Path,
        timestamp: std::time::SystemTime,
    ) -> Result<RootFsReport> {
        let output = output.canonicalize().map_err(|e| io(output, e))?;

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
                    pin_times(&target, timestamp);
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
            pin_times(
                &crate::paths::join_under(&output, &file.destination),
                timestamp,
            );
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

/// Parent used for sibling staging. A bare relative output such as `rootfs`
/// lives beside a temporary directory in the current working directory.
fn output_parent(output: &Path) -> &Path {
    output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn path_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn ensure_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| io(path, e))?;
    if metadata.is_symlink() {
        return Err(Error::Config {
            message: format!("output `{}` must not be a symlink", path.display()),
        });
    }
    if metadata.is_dir() {
        return Ok(());
    }
    Err(Error::Config {
        message: format!("output `{}` is not a directory", path.display()),
    })
}

/// Clone an existing output into the stage without following symlinks. Regular
/// files are copied rather than hard-linked. A hard link would leave an
/// unplanned file in the newly published rootfs sharing an inode with the old
/// rootfs, so a later writer of the old tree could mutate the new snapshot.
fn clone_tree(source: &Path, destination: &Path) -> Result<()> {
    let mut stack = vec![(source.to_path_buf(), destination.to_path_buf())];
    let mut directories = Vec::new();

    while let Some((source_dir, destination_dir)) = stack.pop() {
        let source_metadata = std::fs::metadata(&source_dir).map_err(|e| io(&source_dir, e))?;
        directories.push((destination_dir.clone(), source_metadata));

        for entry in std::fs::read_dir(&source_dir).map_err(|e| io(&source_dir, e))? {
            let entry = entry.map_err(|e| io(&source_dir, e))?;
            let source_path = entry.path();
            let destination_path = destination_dir.join(entry.file_name());
            let metadata =
                std::fs::symlink_metadata(&source_path).map_err(|e| io(&source_path, e))?;

            if metadata.is_symlink() {
                let target = std::fs::read_link(&source_path).map_err(|e| io(&source_path, e))?;
                std::os::unix::fs::symlink(target, &destination_path)
                    .map_err(|e| io(&destination_path, e))?;
            } else if metadata.is_dir() {
                std::fs::create_dir(&destination_path).map_err(|e| io(&destination_path, e))?;
                stack.push((source_path, destination_path));
            } else if metadata.is_file() {
                std::fs::copy(&source_path, &destination_path)
                    .map_err(|e| io(&destination_path, e))?;
                set_permissions_from(&destination_path, &metadata)?;
            } else {
                return Err(Error::Config {
                    message: format!(
                        "existing output contains unsupported entry `{}`",
                        source_path.display()
                    ),
                });
            }
        }
    }

    // Creating children changes directory timestamps. Restore metadata from
    // the bottom up after the complete snapshot has been assembled.
    for (path, metadata) in directories.into_iter().rev() {
        set_permissions_from(&path, &metadata)?;
        set_times_from(&path, &metadata);
    }
    Ok(())
}

/// Replace the visible output only after the complete staged tree exists.
/// Existing outputs are atomically exchanged when the filesystem supports it;
/// otherwise a rollback-capable sequence publishes the staged tree.
fn publish_directory(stage: tempfile::TempDir, output: &Path) -> Result<()> {
    // Linux can exchange two sibling paths in one rename operation. This
    // retains a continuously visible output for readers, unlike moving the
    // old tree aside before publishing the new one. The temporary directory
    // then names the old tree and `close` removes it after the exchange.
    if path_exists(output) {
        return publish_by_exchange(stage, output);
    }

    use rustix::fs::{CWD, RenameFlags, renameat_with};

    // Do not overwrite a rootfs created between the initial existence check
    // and publication. A concurrent builder must retry rather than silently
    // discarding somebody else's output.
    renameat_with(CWD, stage.path(), CWD, output, RenameFlags::NOREPLACE)
        .map_err(|e| io(output, e.into()))
}

fn publish_by_exchange(stage: tempfile::TempDir, output: &Path) -> Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};

    let exchange = renameat_with(CWD, stage.path(), CWD, output, RenameFlags::EXCHANGE);
    finish_exchange(stage, output, exchange)
}

fn finish_exchange(
    stage: tempfile::TempDir,
    output: &Path,
    exchange: std::result::Result<(), rustix::io::Errno>,
) -> Result<()> {
    if let Err(error) = exchange {
        if matches!(
            error,
            rustix::io::Errno::INVAL | rustix::io::Errno::NOSYS | rustix::io::Errno::OPNOTSUPP
        ) {
            return publish_directory_legacy(stage, output);
        }
        return Err(io(output, error.into()));
    }

    let old_path = stage.path().to_path_buf();
    stage.close().map_err(|e| io(&old_path, e))
}

/// Portable publication path for filesystems where
/// `renameat2(RENAME_EXCHANGE)` is unavailable, such as WSL's Windows mounts.
fn publish_directory_legacy(stage: tempfile::TempDir, output: &Path) -> Result<()> {
    let backup = if path_exists(output) {
        let reservation = tempfile::Builder::new()
            .prefix(".elfpak-backup-")
            .tempdir_in(output_parent(output))
            .map_err(|e| io(output, e))?;
        let path = reservation.path().to_path_buf();
        reservation.close().map_err(|e| io(&path, e))?;
        std::fs::rename(output, &path).map_err(|e| io(output, e))?;
        Some(path)
    } else {
        None
    };

    if let Err(error) = std::fs::rename(stage.path(), output) {
        if let Some(backup) = &backup
            && let Err(rollback) = std::fs::rename(backup, output)
        {
            return Err(Error::Config {
                message: format!(
                    "failed to publish `{}` ({error}) and failed to restore its backup `{}` ({rollback})",
                    output.display(),
                    backup.display()
                ),
            });
        }
        return Err(io(output, error));
    }

    if let Some(backup) = backup {
        remove_existing(&backup)?;
    }
    Ok(())
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

/// Refuse the filesystem root as an output even when `--clean` was not
/// requested. Materializing there would turn logical bundle destinations such
/// as `/app/server` into writes to the host filesystem.
fn guard_output(output: &Path) -> Result<()> {
    // `canonicalize` cannot resolve a path that has not been created yet, so
    // normalize an absolute spelling first. Otherwise `/new/..` would evade
    // the guard and become `/` after `create_dir_all`.
    let absolute = std::path::absolute(output).map_err(|e| io(output, e))?;
    let lexical = crate::paths::normalize_absolute(&absolute);
    let resolved = output.canonicalize().unwrap_or(lexical);
    if resolved.parent().is_none() {
        return Err(Error::Config {
            message: format!(
                "refusing to materialize a bundle at filesystem root `{}`",
                resolved.display()
            ),
        });
    }
    Ok(())
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

fn set_permissions_from(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    std::fs::set_permissions(path, metadata.permissions()).map_err(|e| io(path, e))
}

fn set_times_from(path: &Path, metadata: &std::fs::Metadata) {
    let (Ok(accessed), Ok(modified), Ok(file)) = (
        metadata.accessed(),
        metadata.modified(),
        std::fs::File::open(path),
    ) else {
        return;
    };
    let _ = file.set_times(
        std::fs::FileTimes::new()
            .set_accessed(accessed)
            .set_modified(modified),
    );
}

/// Give every entry the materialization timestamp. Not every filesystem
/// supports this, and symlink timestamps cannot be set through `std` at all,
/// so this is best-effort: the tar backend is the byte-reproducible artifact.
fn pin_times(path: &Path, time: std::time::SystemTime) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let _ = file.set_times(
        std::fs::FileTimes::new()
            .set_accessed(time)
            .set_modified(time),
    );
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_directory, finish_exchange, guard_clean, guard_output, has_symlinked_ancestor,
        remove_existing,
    };
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

    #[test]
    fn materialization_refuses_a_filesystem_root() {
        let err = guard_output(Path::new("/")).unwrap_err();
        assert_eq!(err.code(), "E4001");
        let err = guard_output(Path::new("/new-rootfs/..")).unwrap_err();
        assert_eq!(err.code(), "E4001");
        let temp = tempfile::tempdir().unwrap();
        guard_output(&temp.path().join("rootfs")).unwrap();
    }

    #[test]
    fn an_output_root_symlink_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let output = temp.path().join("output");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &output).unwrap();

        assert!(ensure_directory(&output).is_err());
    }

    #[test]
    fn unsupported_atomic_exchange_falls_back_to_portable_publication() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        std::fs::create_dir(&output).unwrap();
        std::fs::write(output.join("old"), b"old").unwrap();

        let stage = tempfile::Builder::new()
            .prefix(".elfpak-rootfs-")
            .tempdir_in(temp.path())
            .unwrap();
        std::fs::write(stage.path().join("new"), b"new").unwrap();

        finish_exchange(stage, &output, Err(rustix::io::Errno::INVAL)).unwrap();

        assert_eq!(std::fs::read(output.join("new")).unwrap(), b"new");
        assert!(!output.join("old").exists());
    }

    #[test]
    fn unrelated_exchange_errors_leave_the_existing_output_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        std::fs::create_dir(&output).unwrap();
        std::fs::write(output.join("old"), b"old").unwrap();

        let stage = tempfile::Builder::new()
            .prefix(".elfpak-rootfs-")
            .tempdir_in(temp.path())
            .unwrap();
        std::fs::write(stage.path().join("new"), b"new").unwrap();

        assert!(finish_exchange(stage, &output, Err(rustix::io::Errno::PERM)).is_err());

        assert_eq!(std::fs::read(output.join("old")).unwrap(), b"old");
        assert!(!output.join("new").exists());
    }
}
