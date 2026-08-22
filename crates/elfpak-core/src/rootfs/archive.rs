//! Deterministic tar output.
//!
//! The archive is written straight from the [`BundlePlan`], never from a
//! materialized directory, so a tar and a rootfs built from the same plan
//! describe exactly the same tree. Every metadata field that could vary between
//! machines is pinned: ownership is root:root, timestamps come from
//! `SOURCE_DATE_EPOCH`, and entries are emitted in plan order.

use crate::{
    error::{Error, Result, io},
    hash::{HashingReader, ensure_matches_plan},
    plan::{BundlePlan, PlannedFile, PlannedFileKind},
};
use std::{
    io::Write,
    path::{Path, PathBuf},
};
use tar::{EntryType, Header};

#[derive(Debug)]
pub struct TarBuilder {
    path: PathBuf,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TarReport {
    pub files: u32,
    pub directories: u32,
    pub symlinks: u32,
    /// Uncompressed payload, excluding tar headers and padding.
    pub bytes: u64,
}

impl TarBuilder {
    pub fn new(path: impl Into<PathBuf>) -> TarBuilder {
        TarBuilder { path: path.into() }
    }

    pub fn apply(&self, plan: &BundlePlan) -> Result<TarReport> {
        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        let mut stage = tempfile::Builder::new()
            .prefix(".elfpak-tar-")
            .tempfile_in(parent)
            .map_err(|e| io(parent, e))?;
        set_output_permissions(stage.path(), &self.path)?;
        let mut writer = tar::Builder::new(std::io::BufWriter::new(stage.as_file_mut()));
        // Long paths and link targets get GNU extension records rather than
        // being silently truncated.
        writer.mode(tar::HeaderMode::Complete);

        let mut report = TarReport::default();
        let mtime = super::copy::source_date_epoch_secs();

        for entry in &plan.files {
            entry.assert_well_formed();
            let name = archive_name(&entry.destination)?;

            let mut header = pinned_header(entry.mode, mtime);

            match entry.kind {
                PlannedFileKind::Directory => {
                    header.set_entry_type(EntryType::Directory);
                    writer
                        .append_data(&mut header, format!("{name}/"), std::io::empty())
                        .map_err(|e| io(&self.path, e))?;
                    report.directories += 1;
                }
                PlannedFileKind::Symlink => {
                    let target = entry
                        .link_target
                        .clone()
                        .expect("validated symlinks have a target");
                    header.set_entry_type(EntryType::Symlink);
                    writer
                        .append_link(&mut header, &name, &target)
                        .map_err(|e| io(&self.path, e))?;
                    report.symlinks += 1;
                }
                _ => {
                    header.set_entry_type(EntryType::Regular);
                    append_regular(&mut writer, &mut header, &name, entry, &self.path)?;
                    report.files += 1;
                    report.bytes += entry.size;
                }
            }
        }

        writer.finish().map_err(|e| io(&self.path, e))?;
        writer
            .into_inner()
            .map_err(|e| io(&self.path, e))?
            .flush()
            .map_err(|e| io(&self.path, e))?;
        stage.as_file().sync_all().map_err(|e| io(&self.path, e))?;

        let entries = report.files + report.directories + report.symlinks;
        assert_eq!(
            entries as usize,
            plan.files.len(),
            "every entry is archived"
        );
        stage
            .persist(&self.path)
            .map_err(|e| io(&self.path, e.error))?;
        Ok(report)
    }
}

fn set_output_permissions(stage: &Path, destination: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let permissions = std::fs::metadata(destination)
        .map(|metadata| metadata.permissions())
        .unwrap_or_else(|_| std::fs::Permissions::from_mode(0o644));
    std::fs::set_permissions(stage, permissions).map_err(|e| io(stage, e))
}

/// A header with ownership and timestamp pinned.
fn pinned_header(mode: u32, mtime: u64) -> Header {
    let mut header = Header::new_gnu();
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(mtime);
    header.set_mode(mode);
    header.set_size(0);
    header
}

fn append_regular<W: Write>(
    writer: &mut tar::Builder<W>,
    header: &mut Header,
    name: &str,
    entry: &PlannedFile,
    archive: &Path,
) -> Result<()> {
    match (&entry.content, &entry.source) {
        (Some(content), None) => {
            assert_eq!(content.len() as u64, entry.size);
            header.set_size(content.len() as u64);
            writer
                .append_data(header, name, content.as_slice())
                .map_err(|e| io(archive, e))
        }
        (None, Some(source)) => {
            let file = std::fs::File::open(source).map_err(|e| io(source, e))?;
            let mut reader = HashingReader::new(std::io::BufReader::new(file));
            header.set_size(entry.size);
            let append_result = writer
                .append_data(header, name, &mut reader)
                .map_err(|e| io(archive, e));
            // `tar` stops after the header's declared size. Continue reading
            // so a source that grew is detected too, rather than silently
            // accepting its original-size prefix.
            let drain_result =
                std::io::copy(&mut reader, &mut std::io::sink()).map_err(|e| io(source, e));
            let (digest, size) = reader.finish();
            let expected = entry
                .sha256
                .as_ref()
                .expect("validated regular files have a digest");
            ensure_matches_plan(source, expected, entry.size, digest, size)?;
            append_result?;
            drain_result?;
            Ok(())
        }
        _ => unreachable!("validated regular files have exactly one content source"),
    }
}

/// Tar entries are relative paths: `/app/server` becomes `app/server`.
fn archive_name(destination: &Path) -> Result<String> {
    let normalized = crate::paths::normalize_absolute(destination);
    let relative = normalized.strip_prefix("/").unwrap_or(&normalized);
    relative
        .to_str()
        .map(str::to_string)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| Error::PathEscape {
            path: destination.to_path_buf(),
            kind: "archive",
        })
}
