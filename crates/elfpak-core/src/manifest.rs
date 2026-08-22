//! Machine-readable record of a bundle: what was included and why.

use crate::{
    error::{Error, Result, io},
    hash::sha256_file,
    plan::{BundlePlan, InclusionReason, PlannedFileKind},
};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

pub const MANIFEST_VERSION: u32 = 2;
/// Name of the manifest written beside a bundle.
pub const MANIFEST_NAME_DEFAULT: &str = "elfpak-manifest.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub manifest_version: u32,
    pub elfpak_version: String,
    /// Install path of the application inside the rootfs.
    pub binary: String,
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
    /// Resolved runtime and dependency policy. Reproducing a bundle requires
    /// the same configuration, so the configuration is part of the record.
    #[serde(default)]
    pub policy: ManifestPolicy,
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
        Manifest::from_plan_with_outputs(plan, source_root, rootfs, None)
    }

    pub fn from_plan_with_outputs(
        plan: &BundlePlan,
        source_root: &Path,
        rootfs: Option<&Path>,
        tar: Option<&Path>,
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
            binary: plan.executable.destination.display().to_string(),
            architecture: plan.architecture.machine.to_string(),
            interpreter: plan.interpreter.as_ref().map(|p| p.display().to_string()),
            source_root: source_root.display().to_string(),
            rootfs: rootfs.map(|p| p.display().to_string()),
            tar: tar.map(|p| p.display().to_string()),
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
        set_output_permissions(stage.path(), path)?;
        stage.write_all(json.as_bytes()).map_err(|e| io(path, e))?;
        stage.as_file().sync_all().map_err(|e| io(path, e))?;
        stage.persist(path).map_err(|e| io(path, e.error))?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Manifest> {
        let bytes = std::fs::read(path).map_err(|e| io(path, e))?;
        let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| Error::Manifest {
            path: path.to_path_buf(),
            message: e.to_string(),
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
                            self.manifest_version < MANIFEST_VERSION || is_sha256(digest)
                        }) => {}
                _ => {
                    return Err(invalid_manifest(
                        manifest_path,
                        format!("inconsistent entry `{}`", file.path),
                    ));
                }
            }
        }
        Ok(())
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

            if has_symlinked_ancestor(rootfs, &target) {
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

        let mut stack = vec![rootfs.to_path_buf()];
        while let Some(current) = stack.pop() {
            assert!(current.starts_with(rootfs), "the walk stays in the rootfs");

            let Ok(entries) = std::fs::read_dir(&current) else {
                continue;
            };
            // Sorted, so that the problems of a failing verification are
            // reported in the same order on every run.
            let mut found: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            found.sort();

            for path in found {
                let Ok(relative) = path.strip_prefix(rootfs) else {
                    continue;
                };
                let logical = crate::paths::normalize_absolute(&Path::new("/").join(relative));
                let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                    continue;
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

/// A final symlink is a valid manifest entry; only ancestor symlinks would
/// redirect metadata reads or hashing outside the supplied rootfs.
fn has_symlinked_ancestor(rootfs: &Path, target: &Path) -> bool {
    let mut current = target.parent();
    while let Some(path) = current {
        if path == rootfs {
            return false;
        }
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_symlink() => return true,
            Ok(_) | Err(_) => {}
        }
        current = path.parent();
    }
    true
}

fn set_output_permissions(stage: &Path, destination: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let permissions = std::fs::metadata(destination)
        .map(|metadata| metadata.permissions())
        .unwrap_or_else(|_| std::fs::Permissions::from_mode(0o644));
    std::fs::set_permissions(stage, permissions).map_err(|e| io(stage, e))
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
