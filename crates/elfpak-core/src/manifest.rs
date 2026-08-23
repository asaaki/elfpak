//! Machine-readable record of a bundle: what was included and why.

use crate::{
    error::{Error, Result, io},
    graph::Digest,
    hash::sha256_file,
    oci::ResolvedImageConfig,
    plan::{BundlePlan, InclusionReason, PLAN_ENTRIES_MAX, PlannedFileKind},
};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub const MANIFEST_VERSION: u32 = 4;

/// Upper bound on a manifest file `verify` will read.
///
/// One entry costs a few hundred bytes, so this comfortably covers a plan at
/// [`crate::plan::PLAN_ENTRIES_MAX`] while keeping an arbitrary file handed to
/// `verify` from being loaded whole.
const MANIFEST_BYTES_MAX: usize = 512 * 1024 * 1024;
const MANIFEST_SHA256_VERSION: u32 = 2;
/// Name of the manifest written beside a bundle.
pub const MANIFEST_NAME_DEFAULT: &str = "elfpak-manifest.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub manifest_version: u32,
    pub elfpak_version: String,
    /// Install path of the application inside the rootfs.
    pub binary: String,
    /// Install paths of every application. Empty in manifests before version 3.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub binaries: Vec<String>,
    pub architecture: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interpreter: Option<String>,
    pub source_root: String,
    /// Where the rootfs was written, used by `elfpak verify`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rootfs: Option<String>,
    /// Where the tar archive was written, when one was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tar: Option<String>,
    /// Where an OCI image layout directory was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oci_layout: Option<String>,
    /// Where an OCI image layout archive was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oci_archive: Option<String>,
    /// Resolved OCI metadata and the published manifest digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ManifestImage>,
    /// Resolved runtime and dependency policy. Reproducing a bundle requires
    /// the same configuration, so the configuration is part of the record.
    #[serde(default)]
    pub policy: ManifestPolicy,
    #[serde(deserialize_with = "deserialize_manifest_files")]
    pub files: Vec<ManifestFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestPolicy {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    pub ca_certificates: bool,
    pub tmp: bool,
    pub passwd_group: bool,
    pub nsswitch: bool,
    pub tzdata: bool,
    /// `auto`, `always` or `never`; absent in manifests written before the
    /// bundle could carry a generated loader cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ld_so_cache: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub includes: Vec<String>,
    /// `None` means the dependency allow-list was not enforced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_libraries: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestFile {
    pub path: String,
    pub kind: String,
    pub reason: Reason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub size: u64,
    /// Octal permission bits, e.g. `0755`.
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

const MANIFEST_ENTRY_LIMIT_MESSAGE: &str = "manifest entries exceed the supported limit";

fn deserialize_manifest_files<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<ManifestFile>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_manifest_files(deserializer, PLAN_ENTRIES_MAX)
}

/// Deserialize manifest entries without first trusting the sequence's size.
///
/// The post-deserialization validation remains as defense in depth, but this
/// visitor is the allocation boundary: it retains at most `limit` entries and
/// rejects the first additional one.
fn deserialize_bounded_manifest_files<'de, D>(
    deserializer: D,
    limit: usize,
) -> std::result::Result<Vec<ManifestFile>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct BoundedManifestFilesVisitor {
        limit: usize,
    }

    impl<'de> serde::de::Visitor<'de> for BoundedManifestFilesVisitor {
        type Value = Vec<ManifestFile>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an array of manifest entries")
        }

        fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let capacity = sequence.size_hint().unwrap_or(0).min(self.limit);
            let mut files = Vec::with_capacity(capacity);
            while let Some(file) = sequence.next_element()? {
                if files.len() == self.limit {
                    return Err(serde::de::Error::custom(format_args!(
                        "{MANIFEST_ENTRY_LIMIT_MESSAGE} of {}",
                        self.limit
                    )));
                }
                files.push(file);
            }
            Ok(files)
        }
    }

    deserializer.deserialize_seq(BoundedManifestFilesVisitor { limit })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestImage {
    pub tag: String,
    pub os: String,
    pub architecture: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    pub entrypoint: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cmd: Vec<String>,
    pub working_dir: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub labels: std::collections::BTreeMap<String, String>,
    pub manifest_digest: String,
}

impl ManifestImage {
    pub fn from_oci(image: &ResolvedImageConfig, manifest_digest: &Digest) -> ManifestImage {
        ManifestImage {
            tag: image.tag().to_string(),
            os: image.os().to_string(),
            architecture: image.architecture().to_string(),
            user: image.user().map(str::to_string),
            entrypoint: image.entrypoint().to_vec(),
            cmd: image.cmd().to_vec(),
            working_dir: image.working_dir().to_string(),
            env: image.env().to_vec(),
            labels: image.labels().clone(),
            manifest_digest: format!("sha256:{manifest_digest}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ManifestOutputs<'a> {
    pub rootfs: Option<&'a Path>,
    pub tar: Option<&'a Path>,
    pub oci_layout: Option<&'a Path>,
    pub oci_archive: Option<&'a Path>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Reason {
    Simple(String),
    NeededBy { needed_by: String, soname: String },
    RuntimePolicy { runtime_policy: String },
}

impl From<&InclusionReason> for Reason {
    fn from(reason: &InclusionReason) -> Reason {
        match reason {
            InclusionReason::Application => Reason::Simple("application".to_string()),
            InclusionReason::Interpreter => Reason::Simple("interpreter".to_string()),
            InclusionReason::ExplicitInclude => Reason::Simple("include".to_string()),
            InclusionReason::NeededBy { binary, soname } => Reason::NeededBy {
                needed_by: binary.display().to_string(),
                soname: soname.clone(),
            },
            InclusionReason::RuntimePolicy { feature } => Reason::RuntimePolicy {
                runtime_policy: feature.as_str().to_string(),
            },
        }
    }
}

impl Manifest {
    /// A manifest of a plan, without recording where the bundle was written.
    pub fn from_plan(plan: &BundlePlan, source_root: &Path, rootfs: Option<&Path>) -> Manifest {
        Manifest::from_plan_with_artifacts(
            plan,
            source_root,
            ManifestOutputs {
                rootfs,
                ..ManifestOutputs::default()
            },
            None,
        )
    }

    pub fn from_plan_with_outputs(
        plan: &BundlePlan,
        source_root: &Path,
        rootfs: Option<&Path>,
        tar: Option<&Path>,
    ) -> Manifest {
        Manifest::from_plan_with_artifacts(
            plan,
            source_root,
            ManifestOutputs {
                rootfs,
                tar,
                ..ManifestOutputs::default()
            },
            None,
        )
    }

    pub fn from_plan_with_artifacts(
        plan: &BundlePlan,
        source_root: &Path,
        outputs: ManifestOutputs<'_>,
        image: Option<ManifestImage>,
    ) -> Manifest {
        let files: Vec<ManifestFile> = plan
            .files
            .iter()
            .map(|file| ManifestFile {
                path: file.destination.display().to_string(),
                kind: file.kind.as_str().to_string(),
                reason: Reason::from(&file.reason),
                sha256: file.sha256.as_ref().map(|d| d.0.clone()),
                size: file.size,
                mode: format!("{:04o}", file.mode),
                target: file.link_target.as_ref().map(|t| t.display().to_string()),
            })
            .collect();

        Manifest {
            manifest_version: MANIFEST_VERSION,
            elfpak_version: env!("CARGO_PKG_VERSION").to_string(),
            binary: plan.executable().destination.display().to_string(),
            binaries: plan
                .executables()
                .map(|file| file.destination.display().to_string())
                .collect(),
            architecture: plan.architecture.machine.to_string(),
            interpreter: plan.interpreter().map(|p| p.display().to_string()),
            source_root: source_root.display().to_string(),
            rootfs: outputs.rootfs.map(|p| p.display().to_string()),
            tar: outputs.tar.map(|p| p.display().to_string()),
            oci_layout: outputs.oci_layout.map(|p| p.display().to_string()),
            oci_archive: outputs.oci_archive.map(|p| p.display().to_string()),
            image,
            policy: ManifestPolicy {
                preset: plan.preset.map(|p| p.to_string()),
                ca_certificates: plan.runtime_policy.ca_certificates,
                tmp: plan.runtime_policy.tmp,
                passwd_group: plan.runtime_policy.passwd_group,
                nsswitch: plan.runtime_policy.nsswitch,
                tzdata: plan.runtime_policy.tzdata,
                ld_so_cache: Some(plan.runtime_policy.ld_so_cache.to_string()),
                user: plan.runtime_policy.user.as_ref().map(|u| u.to_string()),
                includes: plan
                    .runtime_policy
                    .includes
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect(),
                allow_libraries: plan.dependency_policy.allow.clone(),
            },
            files,
            warnings: plan
                .warnings
                .iter()
                .map(|w| format!("{}: {}", w.code, w.message))
                .collect(),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("a manifest is plain data")
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        let mut json = self.to_json();
        json.push('\n');
        let mut stage = tempfile::Builder::new()
            .prefix(".elfpak-manifest-")
            .tempfile_in(parent)
            .map_err(|e| io(parent, e))?;
        crate::rootfs::set_output_permissions(stage.path(), path)?;
        stage.write_all(json.as_bytes()).map_err(|e| io(path, e))?;
        stage.as_file().sync_all().map_err(|e| io(path, e))?;
        stage.persist(path).map_err(|e| io(path, e.error))?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Manifest> {
        // A manifest is an index of a bundle, not a data container. `verify`
        // is pointed at files this process did not write, so the size is
        // checked before the bytes are held in memory.
        let metadata = std::fs::metadata(path).map_err(|e| io(path, e))?;
        if !metadata.is_file() {
            // A FIFO reports a length of zero and then yields bytes forever, so
            // the size check below would wave it through.
            return Err(invalid_manifest(path, "not a regular file".to_string()));
        }
        let bytes = read_manifest_with_limit(path, MANIFEST_BYTES_MAX)?;
        let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|error| {
            let message = error.to_string();
            if message.contains(MANIFEST_ENTRY_LIMIT_MESSAGE) {
                Error::LimitExceeded {
                    resource: "manifest entries",
                    limit: PLAN_ENTRIES_MAX,
                }
            } else {
                Error::Manifest {
                    path: path.to_path_buf(),
                    message,
                }
            }
        })?;
        manifest.validate(path)?;
        Ok(manifest)
    }

    /// Reject malformed untrusted manifest data before verification relies on
    /// it. `verify` is also public, so it still treats an unknown kind as a
    /// problem, but normal CLI use gets a clear load-time error.
    fn validate(&self, manifest_path: &Path) -> Result<()> {
        if self.manifest_version == 0 || self.manifest_version > MANIFEST_VERSION {
            return Err(invalid_manifest(
                manifest_path,
                format!("unsupported manifest version {}", self.manifest_version),
            ));
        }
        validate_manifest_entry_count(self.files.len())?;
        let binaries = self.validate_binaries(manifest_path)?;
        let mut paths = std::collections::HashSet::new();
        for file in &self.files {
            let path = Path::new(&file.path);
            if !path.is_absolute()
                || path != crate::paths::normalize_absolute(path)
                || !paths.insert(path.to_path_buf())
            {
                return Err(invalid_manifest(
                    manifest_path,
                    format!("invalid or duplicate path `{}`", file.path),
                ));
            }
            let mode = u32::from_str_radix(&file.mode, 8).ok();
            if mode.is_none_or(|mode| mode > 0o7777) {
                return Err(invalid_manifest(
                    manifest_path,
                    format!("invalid mode `{}` for `{}`", file.mode, file.path),
                ));
            }
            match file.kind.as_str() {
                "directory" if file.size == 0 && file.sha256.is_none() && file.target.is_none() => {
                }
                "symlink" if file.size == 0 && file.sha256.is_none() && file.target.is_some() => {}
                "executable" | "interpreter" | "shared-object" | "certificate-bundle"
                | "runtime-config" | "application-data"
                    if file.target.is_none()
                        && file.sha256.as_ref().is_some_and(|digest| {
                            self.manifest_version < MANIFEST_SHA256_VERSION || is_sha256(digest)
                        }) => {}
                _ => {
                    return Err(invalid_manifest(
                        manifest_path,
                        format!("inconsistent entry `{}`", file.path),
                    ));
                }
            }
        }
        if self.manifest_version >= 3 {
            let executables: std::collections::HashSet<PathBuf> = self
                .files
                .iter()
                .filter(|file| file.kind == "executable")
                .map(|file| crate::paths::normalize_absolute(Path::new(&file.path)))
                .collect();
            if binaries != executables {
                return Err(invalid_manifest(
                    manifest_path,
                    "binaries must list every executable manifest entry exactly once".to_string(),
                ));
            }
        }
        self.validate_image(manifest_path)?;
        Ok(())
    }

    fn validate_image(&self, manifest_path: &Path) -> Result<()> {
        let has_oci_output = self.oci_layout.is_some() || self.oci_archive.is_some();
        if self.manifest_version < 4 && (has_oci_output || self.image.is_some()) {
            return Err(invalid_manifest(
                manifest_path,
                "OCI fields require manifest version 4".to_string(),
            ));
        }
        if has_oci_output != self.image.is_some() {
            return Err(invalid_manifest(
                manifest_path,
                "OCI destinations and image metadata must be recorded together".to_string(),
            ));
        }
        if let Some(image) = &self.image {
            let digest = image
                .manifest_digest
                .strip_prefix("sha256:")
                .filter(|digest| is_sha256(digest));
            if digest.is_none() {
                return Err(invalid_manifest(
                    manifest_path,
                    "image manifest_digest must be sha256:<64 lowercase hex>".to_string(),
                ));
            }
        }
        Ok(())
    }

    fn validate_binaries(
        &self,
        manifest_path: &Path,
    ) -> Result<std::collections::HashSet<PathBuf>> {
        if self.manifest_version >= 3 && self.binaries.is_empty() {
            return Err(invalid_manifest(
                manifest_path,
                "manifest version 3 or newer requires a non-empty binaries list".to_string(),
            ));
        }
        let binaries: Vec<&str> = if self.binaries.is_empty() {
            vec![self.binary.as_str()]
        } else {
            if self.binaries.first().map(String::as_str) != Some(self.binary.as_str()) {
                return Err(invalid_manifest(
                    manifest_path,
                    "binary must be the first entry in binaries".to_string(),
                ));
            }
            self.binaries.iter().map(String::as_str).collect()
        };
        let mut unique = std::collections::HashSet::new();
        for binary in binaries {
            let path = Path::new(binary);
            if !path.is_absolute()
                || path != crate::paths::normalize_absolute(path)
                || !unique.insert(path.to_path_buf())
            {
                return Err(invalid_manifest(
                    manifest_path,
                    format!("invalid or duplicate binary path `{binary}`"),
                ));
            }
        }
        Ok(unique)
    }

    /// Check a materialized rootfs against this manifest. An entry can be
    /// missing, of the wrong kind or contents, or, under `--strict`, have
    /// permissions that changed.
    pub fn verify(&self, rootfs: &Path, options: &VerifyOptions) -> VerifyReport {
        let mut report = VerifyReport::default();
        match std::fs::symlink_metadata(rootfs) {
            Ok(metadata) if metadata.is_symlink() => {
                report.problems.push(Problem {
                    path: "/".to_string(),
                    detail: "verification root must not be a symlink".to_string(),
                });
                return report;
            }
            Ok(metadata) if !metadata.is_dir() => {
                report.problems.push(Problem {
                    path: "/".to_string(),
                    detail: "verification root is not a directory".to_string(),
                });
                return report;
            }
            Ok(_) | Err(_) => {}
        }
        for file in &self.files {
            report.checked += 1;
            let target = crate::paths::join_under(rootfs, Path::new(&file.path));
            assert!(target.starts_with(rootfs));

            if crate::paths::has_symlinked_ancestor(rootfs, target.parent().unwrap_or(rootfs)) {
                report.problems.push(Problem {
                    path: file.path.clone(),
                    detail: "path traverses a symlinked directory inside the rootfs".to_string(),
                });
                continue;
            }

            let Ok(metadata) = std::fs::symlink_metadata(&target) else {
                report.problems.push(Problem {
                    path: file.path.clone(),
                    detail: "missing".to_string(),
                });
                continue;
            };

            if let Some(problem) = verify_entry(file, &target, &metadata) {
                report.problems.push(problem);
                continue;
            }

            // Permission bits are part of the record, and a mode change is a
            // change the digests cannot see. Symlink modes are not meaningful.
            if options.strict
                && file.kind != "symlink"
                && let Some(problem) = mode_problem(file, &metadata)
            {
                report.problems.push(problem);
            }
        }

        if options.strict {
            self.report_unexpected(rootfs, &mut report);
        }
        report
    }

    /// Anything present in the rootfs that the manifest does not list. Without
    /// this, `verify` can only prove that nothing was removed or altered.
    fn report_unexpected(&self, rootfs: &Path, report: &mut VerifyReport) {
        let expected: std::collections::HashSet<PathBuf> = self
            .files
            .iter()
            .map(|f| crate::paths::normalize_absolute(Path::new(&f.path)))
            .collect();

        let mut budget = VerificationBudget::new(PLAN_ENTRIES_MAX);
        let mut stack = vec![rootfs.to_path_buf()];
        while let Some(current) = stack.pop() {
            assert!(current.starts_with(rootfs), "the walk stays in the rootfs");

            let entries = match std::fs::read_dir(&current) {
                Ok(entries) => entries,
                Err(error) => {
                    // Strict verification claims the rootfs holds nothing but
                    // planned entries. A subtree it could not enumerate is a
                    // gap in that claim, not an absence of problems.
                    report.problems.push(Problem {
                        path: logical_within(rootfs, &current),
                        detail: format!(
                            "could not be read while checking for unlisted entries: {error}"
                        ),
                    });
                    continue;
                }
            };
            // Keep the deterministic ordering without allowing one hostile
            // directory to allocate an unbounded list before the walk's limit
            // has a chance to stop it. One extra entry proves the limit was
            // crossed, and no further names need to be retained or inspected.
            let mut found = Vec::new();
            for entry in entries {
                match entry {
                    Ok(entry) => found.push(entry.path()),
                    Err(error) => {
                        if !budget.visit(report) {
                            return;
                        }
                        report.problems.push(Problem {
                            path: logical_within(rootfs, &current),
                            detail: format!(
                                "could not be read while checking for unlisted entries: {error}"
                            ),
                        });
                        continue;
                    }
                }
                if found.len() > budget.remaining() {
                    budget.exhaust(report);
                    return;
                }
            }
            // Sorted, so that the problems of a failing verification are
            // reported in the same order on every run.
            found.sort();

            for path in found {
                if !budget.visit(report) {
                    return;
                }
                let Ok(relative) = path.strip_prefix(rootfs) else {
                    continue;
                };
                let logical = crate::paths::normalize_absolute(&Path::new("/").join(relative));
                let metadata = match std::fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        report.problems.push(Problem {
                            path: logical.display().to_string(),
                            detail: format!("could not be inspected: {error}"),
                        });
                        continue;
                    }
                };
                // Never descend into a symlink: it is an entry in its own right,
                // and its target is checked where the target lives.
                if metadata.is_dir() && !metadata.is_symlink() {
                    stack.push(path.clone());
                }
                if !expected.contains(&logical) {
                    report.unexpected += 1;
                    report.problems.push(Problem {
                        path: logical.display().to_string(),
                        detail: "present in the rootfs but not listed in the manifest".to_string(),
                    });
                }
            }
        }
    }

    /// Entries that carry content, i.e. everything but the directory scaffolding.
    pub fn file_count(&self) -> usize {
        self.files
            .iter()
            .filter(|f| f.kind != PlannedFileKind::Directory.as_str())
            .count()
    }
}

fn read_manifest_with_limit(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let limit_u64 = u64::try_from(limit).unwrap_or(u64::MAX);
    let metadata = std::fs::metadata(path).map_err(|error| io(path, error))?;
    if metadata.len() > limit_u64 {
        return Err(Error::LimitExceeded {
            resource: "manifest",
            limit,
        });
    }

    let file = std::fs::File::open(path).map_err(|error| io(path, error))?;
    let capacity = usize::try_from(metadata.len()).unwrap_or(limit).min(limit);
    let mut bytes = Vec::with_capacity(capacity);
    file.take(limit_u64.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| io(path, error))?;
    if bytes.len() > limit {
        return Err(Error::LimitExceeded {
            resource: "manifest",
            limit,
        });
    }
    Ok(bytes)
}

/// Reject a manifest that could not have been emitted from a valid plan.
///
/// The byte limit on the JSON document prevents one oversized allocation; this
/// entry limit prevents a small-enough document made from millions of tiny
/// entries from consuming unbounded memory and verification time.
fn validate_manifest_entry_count(entry_count: usize) -> Result<()> {
    if entry_count > PLAN_ENTRIES_MAX {
        return Err(Error::LimitExceeded {
            resource: "manifest entries",
            limit: PLAN_ENTRIES_MAX,
        });
    }
    Ok(())
}

/// Shared budget for strict filesystem discovery.
///
/// A manifest already bounds verification of recorded entries. The strict
/// walk additionally sees entries the manifest does not name, so it needs the
/// same cap before retaining directory names or growing the problem report.
struct VerificationBudget {
    remaining: usize,
    exhausted: bool,
}

impl VerificationBudget {
    fn new(limit: usize) -> VerificationBudget {
        VerificationBudget {
            remaining: limit,
            exhausted: false,
        }
    }

    fn remaining(&self) -> usize {
        self.remaining
    }

    /// Consume one discovered filesystem entry. `false` means verification
    /// has recorded the limit problem and must stop the walk.
    fn visit(&mut self, report: &mut VerifyReport) -> bool {
        if self.remaining == 0 {
            self.exhaust(report);
            return false;
        }
        self.remaining -= 1;
        true
    }

    fn exhaust(&mut self, report: &mut VerifyReport) {
        if self.exhausted {
            return;
        }
        self.exhausted = true;
        report.problems.push(Problem {
            path: "/".to_string(),
            detail: format!(
                "strict verification exceeded the supported limit of {PLAN_ENTRIES_MAX} verification entries"
            ),
        });
    }
}

/// A path inside the rootfs, named the way the manifest names it, so every
/// problem in one report is keyed in the same path space.
fn logical_within(rootfs: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(rootfs).unwrap_or(path);
    crate::paths::normalize_absolute(&Path::new("/").join(relative))
        .display()
        .to_string()
}

fn invalid_manifest(path: &Path, message: String) -> Error {
    Error::Manifest {
        path: path.to_path_buf(),
        message,
    }
}

fn is_sha256(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// Check one entry against what the manifest recorded for it. `None` means the
/// entry is what it should be.
fn verify_entry(
    file: &ManifestFile,
    target: &Path,
    metadata: &std::fs::Metadata,
) -> Option<Problem> {
    match file.kind.as_str() {
        "directory" => (!metadata.is_dir()).then(|| Problem {
            path: file.path.clone(),
            detail: "expected a directory".to_string(),
        }),
        "symlink" => verify_symlink(file, target, metadata),
        "executable" | "interpreter" | "shared-object" | "certificate-bundle"
        | "runtime-config" | "application-data" => verify_regular(file, target, metadata),
        _ => Some(Problem {
            path: file.path.clone(),
            detail: format!("unknown manifest entry kind `{}`", file.kind),
        }),
    }
}

/// A symlink is verified by its target, verbatim: the bundle preserves link
/// structure, so a link that now points elsewhere is a changed bundle.
fn verify_symlink(
    file: &ManifestFile,
    target: &Path,
    metadata: &std::fs::Metadata,
) -> Option<Problem> {
    if !metadata.is_symlink() {
        return Some(Problem {
            path: file.path.clone(),
            detail: "expected a symlink".to_string(),
        });
    }
    let actual = std::fs::read_link(target).unwrap_or_default();
    let expected = file.target.clone().unwrap_or_default();
    if actual.as_os_str() == expected.as_str() {
        return None;
    }
    Some(Problem {
        path: file.path.clone(),
        detail: format!(
            "link target is `{}`, expected `{}`",
            actual.display(),
            expected
        ),
    })
}

/// A regular file is verified by its digest.
fn verify_regular(
    file: &ManifestFile,
    target: &Path,
    metadata: &std::fs::Metadata,
) -> Option<Problem> {
    if !metadata.is_file() {
        return Some(Problem {
            path: file.path.clone(),
            detail: "expected a regular file".to_string(),
        });
    }
    let Some(expected) = file.sha256.as_ref() else {
        return Some(Problem {
            path: file.path.clone(),
            detail: "regular file has no sha256 digest".to_string(),
        });
    };
    match sha256_file(target) {
        Ok((actual, size)) if &actual.0 == expected && size == file.size => None,
        Ok((_actual, size)) if size != file.size => Some(Problem {
            path: file.path.clone(),
            detail: format!("size is {size} bytes, expected {}", file.size),
        }),
        Ok((actual, _)) => Some(Problem {
            path: file.path.clone(),
            detail: format!("sha256 mismatch (found {}, expected {expected})", actual.0),
        }),
        Err(e) => Some(Problem {
            path: file.path.clone(),
            detail: format!("unreadable: {e}"),
        }),
    }
}

/// Compare recorded and actual permission bits.
fn mode_problem(file: &ManifestFile, metadata: &std::fs::Metadata) -> Option<Problem> {
    use std::os::unix::fs::PermissionsExt;
    let expected = u32::from_str_radix(&file.mode, 8).ok()?;
    let actual = metadata.permissions().mode() & 0o7777;
    (actual != expected).then(|| Problem {
        path: file.path.clone(),
        detail: format!("mode is {actual:04o}, expected {expected:04o}"),
    })
}

/// What `verify` should check beyond "every recorded entry still matches".
#[derive(Debug, Default, Clone, Copy)]
pub struct VerifyOptions {
    /// Also fail on files present in the rootfs but absent from the manifest,
    /// and on entries whose permission bits changed.
    pub strict: bool,
}

#[derive(Debug, Default)]
pub struct VerifyReport {
    pub checked: u32,
    /// Entries found in the rootfs that the manifest does not list.
    pub unexpected: u32,
    pub problems: Vec<Problem>,
}

#[derive(Debug)]
pub struct Problem {
    pub path: String,
    pub detail: String,
}

impl VerifyReport {
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }

    /// Problems found, saturated at `u32::MAX`.
    pub fn failure_count(&self) -> u32 {
        u32::try_from(self.problems.len()).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_entry_validation_rejects_a_count_over_the_plan_limit() {
        let error = validate_manifest_entry_count(PLAN_ENTRIES_MAX + 1).unwrap_err();
        assert!(matches!(
            error,
            Error::LimitExceeded {
                resource: "manifest entries",
                limit: PLAN_ENTRIES_MAX,
            }
        ));
    }

    #[test]
    fn manifest_file_deserialization_stops_at_its_entry_limit() {
        let json = r#"[
            {"path":"/one","kind":"directory","reason":"include","size":0,"mode":"0755"},
            {"path":"/two","kind":"directory","reason":"include","size":0,"mode":"0755"}
        ]"#;
        let mut deserializer = serde_json::Deserializer::from_str(json);

        let error = deserialize_bounded_manifest_files(&mut deserializer, 1).unwrap_err();
        assert!(error.to_string().contains("manifest entries"), "{error}");
    }

    #[test]
    fn manifest_read_stops_at_its_byte_limit() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), b"12345").unwrap();

        let error = read_manifest_with_limit(temp.path(), 4).unwrap_err();
        assert!(matches!(
            error,
            Error::LimitExceeded {
                resource: "manifest",
                limit: 4,
            }
        ));
    }

    #[test]
    fn verification_budget_stops_before_recording_unbounded_problems() {
        let mut budget = VerificationBudget::new(1);
        let mut report = VerifyReport::default();

        assert!(budget.visit(&mut report));
        assert!(!budget.visit(&mut report));
        assert_eq!(report.problems.len(), 1);
        assert!(report.problems[0].detail.contains("verification entries"));
    }
}
