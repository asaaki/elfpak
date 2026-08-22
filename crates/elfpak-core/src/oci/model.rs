//! OCI image metadata defaults, validation, and serialized models.

use crate::{BundlePlan, Error, Machine, PlannedFileKind, Result};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

/// Upper bound for independently supplied environment variables or labels.
pub(super) const OCI_METADATA_ENTRIES_MAX: usize = 4096;
/// Upper bound for one image-metadata string, in UTF-8 bytes.
pub(super) const OCI_METADATA_VALUE_BYTES_MAX: usize = 1 << 20;

pub(super) const OCI_IMAGE_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub(super) const OCI_IMAGE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub(super) const OCI_IMAGE_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
pub(super) const OCI_LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";
pub(super) const OCI_REF_NAME: &str = "org.opencontainers.image.ref.name";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Descriptor {
    pub media_type: &'static str,
    pub digest: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
}

#[derive(Debug, Serialize)]
pub(super) struct Platform {
    pub architecture: String,
    pub os: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ImageIndex {
    pub schema_version: u32,
    pub media_type: &'static str,
    pub manifests: Vec<Descriptor>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ImageManifest {
    pub schema_version: u32,
    pub media_type: &'static str,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
}

#[derive(Debug, Serialize)]
pub(super) struct ImageConfiguration {
    pub architecture: String,
    pub os: String,
    pub config: RuntimeConfiguration,
    pub rootfs: RootFs,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct RuntimeConfiguration {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    pub entrypoint: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cmd: Vec<String>,
    pub working_dir: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub(super) struct RootFs {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub diff_ids: Vec<String>,
}

/// User-selected process and descriptive metadata for an OCI image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciImageConfig {
    pub tag: String,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub working_dir: Option<String>,
    pub env: Vec<String>,
    pub labels: BTreeMap<String, String>,
}

impl Default for OciImageConfig {
    fn default() -> OciImageConfig {
        OciImageConfig {
            tag: "latest".to_string(),
            entrypoint: Vec::new(),
            cmd: Vec::new(),
            working_dir: None,
            env: Vec::new(),
            labels: BTreeMap::new(),
        }
    }
}

/// Fully defaulted and validated OCI metadata for one bundle plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImageConfig {
    pub(crate) tag: String,
    pub(crate) os: String,
    pub(crate) architecture: String,
    pub(crate) user: Option<String>,
    pub(crate) entrypoint: Vec<String>,
    pub(crate) cmd: Vec<String>,
    pub(crate) working_dir: String,
    pub(crate) env: Vec<String>,
    pub(crate) labels: BTreeMap<String, String>,
}

impl OciImageConfig {
    pub fn resolve(&self, plan: &BundlePlan) -> Result<ResolvedImageConfig> {
        validate_tag(&self.tag)?;
        validate_count("environment", self.env.len())?;
        validate_count("labels", self.labels.len())?;

        let entrypoint = if self.entrypoint.is_empty() {
            if plan.applications().len() != 1 {
                return Err(config_error(
                    "OCI entrypoint is required for a multi-binary bundle",
                ));
            }
            vec![plan.executable().destination().display().to_string()]
        } else {
            self.entrypoint.clone()
        };
        validate_process_args("entrypoint", &entrypoint)?;
        validate_process_args("command", &self.cmd)?;
        validate_entrypoint(plan, &entrypoint[0])?;

        let working_dir = self.working_dir.as_deref().unwrap_or("/").to_string();
        validate_working_dir(plan, &working_dir)?;
        validate_environment(&self.env)?;
        validate_labels(&self.labels)?;

        let architecture = match plan.architecture().machine {
            Machine::X86_64 => "amd64",
            Machine::Aarch64 => "arm64",
            other => {
                return Err(config_error(format!(
                    "cannot map architecture `{other}` to an OCI platform"
                )));
            }
        };
        let user = plan
            .runtime_policy()
            .user
            .as_ref()
            .map(|user| format!("{}:{}", user.uid(), user.gid()));

        Ok(ResolvedImageConfig {
            tag: self.tag.clone(),
            os: "linux".to_string(),
            architecture: architecture.to_string(),
            user,
            entrypoint,
            cmd: self.cmd.clone(),
            working_dir,
            env: self.env.clone(),
            labels: self.labels.clone(),
        })
    }
}

impl ResolvedImageConfig {
    pub fn tag(&self) -> &str {
        &self.tag
    }

    pub fn os(&self) -> &str {
        &self.os
    }

    pub fn architecture(&self) -> &str {
        &self.architecture
    }

    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    pub fn entrypoint(&self) -> &[String] {
        &self.entrypoint
    }

    pub fn cmd(&self) -> &[String] {
        &self.cmd
    }

    pub fn working_dir(&self) -> &str {
        &self.working_dir
    }

    pub fn env(&self) -> &[String] {
        &self.env
    }

    pub fn labels(&self) -> &BTreeMap<String, String> {
        &self.labels
    }
}

fn validate_tag(tag: &str) -> Result<()> {
    validate_value("image tag", tag)?;
    let mut bytes = tag.bytes();
    let Some(first) = bytes.next() else {
        return Err(config_error("OCI image tag cannot be empty"));
    };
    if tag.len() > 128
        || !(first.is_ascii_alphanumeric() || first == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    {
        return Err(config_error(format!(
            "invalid OCI image tag `{tag}` (expected 1-128 characters from [A-Za-z0-9_.-])"
        )));
    }
    Ok(())
}

fn validate_process_args(kind: &str, values: &[String]) -> Result<()> {
    for value in values {
        validate_value(kind, value)?;
    }
    Ok(())
}

fn validate_entrypoint(plan: &BundlePlan, entrypoint: &str) -> Result<()> {
    let path = Path::new(entrypoint);
    if !is_normalized_absolute(path)
        || !plan
            .files()
            .iter()
            .any(|file| file.destination() == path && file.kind() != PlannedFileKind::Directory)
    {
        return Err(config_error(format!(
            "OCI entrypoint `{entrypoint}` must name an absolute planned file"
        )));
    }
    Ok(())
}

fn validate_working_dir(plan: &BundlePlan, working_dir: &str) -> Result<()> {
    validate_value("working directory", working_dir)?;
    let path = Path::new(working_dir);
    let planned = path == Path::new("/")
        || plan
            .files()
            .iter()
            .any(|file| file.destination() == path && file.kind() == PlannedFileKind::Directory);
    if !is_normalized_absolute(path) || !planned {
        return Err(config_error(format!(
            "OCI working directory `{working_dir}` must name an absolute planned directory"
        )));
    }
    Ok(())
}

fn is_normalized_absolute(path: &Path) -> bool {
    path.is_absolute() && crate::paths::normalize_absolute(path) == path
}

fn validate_environment(env: &[String]) -> Result<()> {
    let mut keys = BTreeSet::new();
    for value in env {
        validate_value("environment", value)?;
        let Some((key, _)) = value.split_once('=') else {
            return Err(config_error(format!(
                "invalid OCI environment value `{value}` (expected KEY=VALUE)"
            )));
        };
        if key.is_empty() || key.contains('\0') || !keys.insert(key) {
            return Err(config_error(format!(
                "OCI environment keys must be non-empty and unique (`{key}`)"
            )));
        }
    }
    Ok(())
}

fn validate_labels(labels: &BTreeMap<String, String>) -> Result<()> {
    for (key, value) in labels {
        validate_value("label key", key)?;
        validate_value("label value", value)?;
        if key.is_empty() {
            return Err(config_error("OCI label key cannot be empty"));
        }
    }
    Ok(())
}

fn validate_count(kind: &str, count: usize) -> Result<()> {
    if count > OCI_METADATA_ENTRIES_MAX {
        return Err(config_error(format!(
            "OCI {kind} has {count} entries; the supported limit is 4,096"
        )));
    }
    Ok(())
}

fn validate_value(kind: &str, value: &str) -> Result<()> {
    if value.contains('\0') {
        return Err(config_error(format!("OCI {kind} contains a NUL byte")));
    }
    if value.len() > OCI_METADATA_VALUE_BYTES_MAX {
        return Err(config_error(format!(
            "OCI {kind} exceeds the supported limit of 1,048,576 bytes"
        )));
    }
    Ok(())
}

fn config_error(message: impl Into<String>) -> Error {
    Error::Config {
        message: message.into(),
    }
}
