//! Deterministic tar transport for an OCI image layout.

use super::{OciImageConfig, OciReport, layout::build_layout_into};
use crate::{
    BundlePlan, Result,
    error::io,
    rootfs::{STAGE_MODE, output_parent, set_output_permissions},
};
use std::{
    io::{BufWriter, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
use tar::{EntryType, Header};

#[derive(Debug)]
pub struct OciArchiveBuilder {
    output: PathBuf,
    image: OciImageConfig,
}

impl OciArchiveBuilder {
    pub fn new(output: impl Into<PathBuf>) -> OciArchiveBuilder {
        OciArchiveBuilder {
            output: output.into(),
            image: OciImageConfig::default(),
        }
    }

    pub fn image(mut self, image: OciImageConfig) -> OciArchiveBuilder {
        self.image = image;
        self
    }

    pub fn apply(&self, plan: &BundlePlan) -> Result<OciReport> {
        let parent = output_parent(&self.output);
        std::fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
        let layout = tempfile::Builder::new()
            .prefix(".elfpak-oci-layout-")
            .permissions(std::fs::Permissions::from_mode(STAGE_MODE))
            .tempdir_in(parent)
            .map_err(|error| io(parent, error))?;
        let report = build_layout_into(layout.path(), plan, &self.image)?;

        let mut stage = tempfile::Builder::new()
            .prefix(".elfpak-oci-archive-")
            .tempfile_in(parent)
            .map_err(|error| io(parent, error))?;
        let stage_path = stage.path().to_path_buf();
        set_output_permissions(&stage_path, &self.output)?;
        write_archive(stage.as_file_mut(), &stage_path, layout.path())?;
        stage
            .as_file()
            .sync_all()
            .map_err(|error| io(&stage_path, error))?;
        stage
            .persist(&self.output)
            .map_err(|error| io(&self.output, error.error))?;
        Ok(report)
    }
}

fn write_archive(output: &mut std::fs::File, output_path: &Path, layout: &Path) -> Result<()> {
    let timestamp = crate::rootfs::copy::source_date_epoch_secs()?;
    let writer = BufWriter::new(output);
    let mut archive = tar::Builder::new(writer);
    archive.mode(tar::HeaderMode::Complete);

    append_file(
        &mut archive,
        output_path,
        "oci-layout",
        &layout.join("oci-layout"),
        timestamp,
    )?;
    append_file(
        &mut archive,
        output_path,
        "index.json",
        &layout.join("index.json"),
        timestamp,
    )?;
    append_directory(&mut archive, output_path, "blobs/", timestamp)?;
    append_directory(&mut archive, output_path, "blobs/sha256/", timestamp)?;

    let mut blobs = std::fs::read_dir(layout.join("blobs/sha256"))
        .map_err(|error| io(layout, error))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| io(layout, error))?;
    blobs.sort();
    for blob in blobs {
        let name = blob
            .file_name()
            .expect("blob path has a file name")
            .to_string_lossy();
        append_file(
            &mut archive,
            output_path,
            &format!("blobs/sha256/{name}"),
            &blob,
            timestamp,
        )?;
    }

    archive.finish().map_err(|error| io(output_path, error))?;
    let mut writer = archive
        .into_inner()
        .map_err(|error| io(output_path, error))?;
    writer.flush().map_err(|error| io(output_path, error))
}

fn append_file<W: Write>(
    archive: &mut tar::Builder<W>,
    output: &Path,
    name: &str,
    source: &Path,
    timestamp: u64,
) -> Result<()> {
    let file = std::fs::File::open(source).map_err(|error| io(source, error))?;
    let size = file.metadata().map_err(|error| io(source, error))?.len();
    let mut reader = std::io::BufReader::new(file);
    let mut header = pinned_header(EntryType::Regular, 0o644, size, timestamp);
    archive
        .append_data(&mut header, name, &mut reader)
        .map_err(|error| io(output, error))
}

fn append_directory<W: Write>(
    archive: &mut tar::Builder<W>,
    output: &Path,
    name: &str,
    timestamp: u64,
) -> Result<()> {
    let mut header = pinned_header(EntryType::Directory, 0o755, 0, timestamp);
    archive
        .append_data(&mut header, name, std::io::empty())
        .map_err(|error| io(output, error))
}

fn pinned_header(kind: EntryType, mode: u32, size: u64, timestamp: u64) -> Header {
    let mut header = Header::new_gnu();
    header.set_entry_type(kind);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(timestamp);
    header.set_mode(mode);
    header.set_size(size);
    header.set_cksum();
    header
}
