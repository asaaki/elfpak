//! Deterministic OCI image-layout directory output.

use super::model::{
    Descriptor, ImageConfiguration, ImageIndex, ImageManifest, OCI_IMAGE_CONFIG, OCI_IMAGE_INDEX,
    OCI_IMAGE_MANIFEST, OCI_LAYER_TAR, OCI_REF_NAME, OciImageConfig, Platform, ResolvedImageConfig,
    RootFs, RuntimeConfiguration,
};
use crate::{
    BundlePlan, Digest, Result,
    error::Error,
    error::io,
    hash::{HashingWriter, sha256_bytes},
    rootfs::{
        STAGE_MODE, TarBuilder, ensure_directory, guard_output, output_parent, path_exists,
        publish_directory,
    },
};
use std::{
    collections::BTreeMap,
    io::{BufWriter, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

const OCI_LAYOUT_VERSION: &str = "1.0.0";

/// Mode every file in a published layout gets, so that a consumer running as
/// another user sees one consistent set of permissions.
const LAYOUT_FILE_MODE: u32 = 0o644;

#[derive(Debug)]
pub struct OciLayoutBuilder {
    output: PathBuf,
    image: OciImageConfig,
    clean: bool,
}

impl OciLayoutBuilder {
    pub fn new(output: impl Into<PathBuf>) -> OciLayoutBuilder {
        OciLayoutBuilder {
            output: output.into(),
            image: OciImageConfig::default(),
            clean: false,
        }
    }

    pub fn image(mut self, image: OciImageConfig) -> OciLayoutBuilder {
        self.image = image;
        self
    }

    /// Permit replacing a destination directory that is not already a layout.
    pub fn clean(mut self, clean: bool) -> OciLayoutBuilder {
        self.clean = clean;
        self
    }

    pub fn apply(&self, plan: &BundlePlan) -> Result<OciReport> {
        guard_output(&self.output)?;
        let parent = output_parent(&self.output);
        std::fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
        let stage = tempfile::Builder::new()
            .prefix(".elfpak-oci-")
            .permissions(std::fs::Permissions::from_mode(STAGE_MODE))
            .tempdir_in(parent)
            .map_err(|error| io(parent, error))?;

        if path_exists(&self.output) {
            ensure_directory(&self.output)?;
            // Publication replaces the destination wholesale, so anything
            // already there is deleted. Rebuilding a layout is the ordinary
            // case; anything else has to be asked for.
            if !self.clean && !is_replaceable_layout(&self.output)? {
                return Err(Error::Config {
                    message: format!(
                        "`{}` is not an empty directory or an OCI layout; \
                         publishing there would delete its contents (use --clean)",
                        self.output.display()
                    ),
                });
            }
        }

        set_directory_mode(stage.path(), 0o755)?;
        let report = build_layout_into(stage.path(), plan, &self.image)?;
        publish_directory(stage, &self.output)?;
        Ok(report)
    }
}

/// Whether an existing destination is one this builder may replace on its own:
/// empty, or already carrying the layout marker `oci-layout`.
fn is_replaceable_layout(output: &Path) -> Result<bool> {
    if output.join("oci-layout").is_file() {
        return Ok(true);
    }
    let mut entries = std::fs::read_dir(output).map_err(|error| io(output, error))?;
    Ok(entries.next().is_none())
}

#[derive(Debug)]
pub struct OciReport {
    layer_digest: Digest,
    layer_size: u64,
    config_digest: Digest,
    config_size: u64,
    manifest_digest: Digest,
    manifest_size: u64,
    platform: String,
    image: ResolvedImageConfig,
}

impl OciReport {
    pub fn layer_digest(&self) -> &Digest {
        &self.layer_digest
    }

    pub fn layer_size(&self) -> u64 {
        self.layer_size
    }

    pub fn config_digest(&self) -> &Digest {
        &self.config_digest
    }

    pub fn config_size(&self) -> u64 {
        self.config_size
    }

    pub fn manifest_digest(&self) -> &Digest {
        &self.manifest_digest
    }

    pub fn manifest_size(&self) -> u64 {
        self.manifest_size
    }

    pub fn platform(&self) -> &str {
        &self.platform
    }

    pub fn image(&self) -> &ResolvedImageConfig {
        &self.image
    }
}

pub(crate) fn build_layout_into(
    root: &Path,
    plan: &BundlePlan,
    image: &OciImageConfig,
) -> Result<OciReport> {
    let image = image.resolve(plan)?;
    let blobs = root.join("blobs/sha256");
    std::fs::create_dir_all(&blobs).map_err(|error| io(&blobs, error))?;

    let (layer_digest, layer_size) = write_layer(&blobs, plan)?;
    let layer_descriptor = descriptor(OCI_LAYER_TAR, &layer_digest, layer_size);

    let config = ImageConfiguration {
        architecture: image.architecture.clone(),
        os: image.os.clone(),
        config: RuntimeConfiguration {
            user: image.user.clone(),
            env: image.env.clone(),
            entrypoint: image.entrypoint.clone(),
            cmd: image.cmd.clone(),
            working_dir: image.working_dir.clone(),
            labels: image.labels.clone(),
        },
        rootfs: RootFs {
            kind: "layers",
            diff_ids: vec![oci_digest(&layer_digest)],
        },
    };
    let config_bytes = serde_json::to_vec(&config).expect("OCI configuration is serializable");
    let (config_digest, config_size) = write_blob(&blobs, &config_bytes)?;

    let manifest = ImageManifest {
        schema_version: 2,
        media_type: OCI_IMAGE_MANIFEST,
        config: descriptor(OCI_IMAGE_CONFIG, &config_digest, config_size),
        layers: vec![layer_descriptor],
    };
    let manifest_bytes = serde_json::to_vec(&manifest).expect("OCI manifest is serializable");
    let (manifest_digest, manifest_size) = write_blob(&blobs, &manifest_bytes)?;

    let index = ImageIndex {
        schema_version: 2,
        media_type: OCI_IMAGE_INDEX,
        manifests: vec![Descriptor {
            media_type: OCI_IMAGE_MANIFEST,
            digest: oci_digest(&manifest_digest),
            size: manifest_size,
            annotations: Some(BTreeMap::from([(
                OCI_REF_NAME.to_string(),
                image.tag.clone(),
            )])),
            platform: Some(Platform {
                architecture: image.architecture.clone(),
                os: image.os.clone(),
            }),
        }],
    };
    write_json_document(&root.join("index.json"), &index)?;
    write_json_document(
        &root.join("oci-layout"),
        &serde_json::json!({ "imageLayoutVersion": OCI_LAYOUT_VERSION }),
    )?;

    Ok(OciReport {
        layer_digest,
        layer_size,
        config_digest,
        config_size,
        manifest_digest,
        manifest_size,
        platform: format!("{}/{}", image.os, image.architecture),
        image,
    })
}

fn write_layer(blobs: &Path, plan: &BundlePlan) -> Result<(Digest, u64)> {
    let mut stage = tempfile::NamedTempFile::new_in(blobs).map_err(|error| io(blobs, error))?;
    let stage_path = stage.path().to_path_buf();
    let writer = BufWriter::new(stage.as_file_mut());
    let writer = HashingWriter::new(writer);
    let (writer, _) = TarBuilder::new(&stage_path).write_to(writer, plan)?;
    let (mut writer, digest, size) = writer.finish();
    writer.flush().map_err(|error| io(&stage_path, error))?;
    drop(writer);
    stage
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(LAYOUT_FILE_MODE))
        .map_err(|error| io(&stage_path, error))?;
    stage
        .as_file()
        .sync_all()
        .map_err(|error| io(&stage_path, error))?;
    let destination = blobs.join(&digest.0);
    stage
        .persist(&destination)
        .map_err(|error| io(&destination, error.error))?;
    Ok((digest, size))
}

fn descriptor(media_type: &'static str, digest: &Digest, size: u64) -> Descriptor {
    Descriptor {
        media_type,
        digest: oci_digest(digest),
        size,
        annotations: None,
        platform: None,
    }
}

fn oci_digest(digest: &Digest) -> String {
    format!("sha256:{digest}")
}

fn write_blob(blobs: &Path, bytes: &[u8]) -> Result<(Digest, u64)> {
    let digest = sha256_bytes(bytes);
    let destination = blobs.join(&digest.0);
    write_layout_file(&destination, bytes)?;
    Ok((digest, bytes.len() as u64))
}

fn write_json_document(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec(value).expect("OCI metadata is serializable");
    bytes.push(b'\n');
    write_layout_file(path, &bytes)
}

/// A layout file with a fixed mode, on disk before the layout is published.
///
/// The mode is pinned because a layout is meant to be handed to another tool,
/// sometimes running as another user, and the umask would otherwise make one
/// file unreadable while the rest were fine. The sync is what keeps
/// `index.json` from naming a blob whose bytes never reached the disk.
fn write_layout_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = std::fs::File::create(path).map_err(|error| io(path, error))?;
    file.write_all(bytes).map_err(|error| io(path, error))?;
    file.set_permissions(std::fs::Permissions::from_mode(LAYOUT_FILE_MODE))
        .map_err(|error| io(path, error))?;
    file.sync_all().map_err(|error| io(path, error))
}

fn set_directory_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|error| io(path, error))
}
