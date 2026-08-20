//! Deterministic tar output.
//!
//! The archive is written straight from the [`BundlePlan`], never from a
//! materialized directory, so a tar and a rootfs built from the same plan
//! describe exactly the same tree. Every metadata field that could vary between
//! machines is pinned: ownership is root:root, timestamps come from
//! `SOURCE_DATE_EPOCH`, and entries are emitted in plan order.

use crate::{
    error::{Error, Result, io},
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
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
        let file = std::fs::File::create(&self.path).map_err(|e| io(&self.path, e))?;
        let mut writer = tar::Builder::new(std::io::BufWriter::new(file));
        // Long paths and link targets get GNU extension records rather than
        // being silently truncated.
        writer.mode(tar::HeaderMode::Complete);

        let mut report = TarReport::default();
        let mtime = super::copy::source_date_epoch_secs();

        for entry in &plan.files {
            entry.assert_well_formed();
            let name = archive_name(&entry.destination)?;
            assert!(!name.is_empty());
            assert!(!name.starts_with('/'), "tar entries are relative");

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
                        .unwrap_or_else(|| PathBuf::from("/"));
                    header.set_entry_type(EntryType::Symlink);
                    writer
                        .append_link(&mut header, &name, &target)
                        .map_err(|e| io(&self.path, e))?;
                    report.symlinks += 1;
                }
                _ => {
                    header.set_entry_type(EntryType::Regular);
                    append_regular(&mut writer, &mut header, &name, entry)
                        .map_err(|e| io(&self.path, e))?;
                    report.files += 1;
                    report.bytes += entry.size;
                }
            }
        }

        writer
            .into_inner()
            .map_err(|e| io(&self.path, e))?
            .flush()
            .map_err(|e| io(&self.path, e))?;

        let entries = report.files + report.directories + report.symlinks;
        assert_eq!(
            entries as usize,
            plan.files.len(),
            "every entry is archived"
        );
        Ok(report)
    }
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
) -> std::io::Result<()> {
    match (&entry.content, &entry.source) {
        (Some(content), _) => {
            assert_eq!(content.len() as u64, entry.size);
            header.set_size(content.len() as u64);
            writer.append_data(header, name, content.as_slice())
        }
        (None, Some(source)) => {
            let file = std::fs::File::open(source)?;
            header.set_size(file.metadata()?.len());
            writer.append_data(header, name, file)
        }
        (None, None) => {
            header.set_size(0);
            writer.append_data(header, name, std::io::empty())
        }
    }
}

/// Tar entries are relative paths: `/app/server` becomes `app/server`.
fn archive_name(destination: &Path) -> Result<String> {
    assert!(destination.is_absolute());

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
