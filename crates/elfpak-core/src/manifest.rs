//! Machine-readable record of a bundle: what was included and why.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, io};
use crate::hash::sha256_file;
use crate::plan::{BundlePlan, InclusionReason, PlannedFileKind};

pub const MANIFEST_VERSION: u32 = 2;
pub const DEFAULT_MANIFEST_NAME: &str = "elfpak-manifest.json";

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
    pub fn from_plan(plan: &BundlePlan, source_root: &Path, rootfs: Option<&Path>) -> Manifest {
        Manifest::from_plan_with_outputs(plan, source_root, rootfs, None)
    }

    pub fn from_plan_with_outputs(
        plan: &BundlePlan,
        source_root: &Path,
        rootfs: Option<&Path>,
        tar: Option<&Path>,
    ) -> Manifest {
        let files = plan
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
        serde_json::to_string_pretty(self).expect("manifest serializes")
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
        let mut json = self.to_json();
        json.push('\n');
        std::fs::write(path, json).map_err(|e| io(path, e))
    }

    pub fn load(path: &Path) -> Result<Manifest> {
        let bytes = std::fs::read(path).map_err(|e| io(path, e))?;
        serde_json::from_slice(&bytes).map_err(|e| Error::Manifest {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
    }

    /// Check a materialized rootfs against this manifest.
    pub fn verify(&self, rootfs: &Path, options: &VerifyOptions) -> VerifyReport {
        let mut report = VerifyReport::default();
        for file in &self.files {
            report.checked += 1;
            let target = crate::paths::join_under(rootfs, Path::new(&file.path));
            let Ok(metadata) = std::fs::symlink_metadata(&target) else {
                report.problems.push(Problem {
                    path: file.path.clone(),
                    detail: "missing".to_string(),
                });
                continue;
            };

            match file.kind.as_str() {
                "directory" => {
                    if !metadata.is_dir() {
                        report.problems.push(Problem {
                            path: file.path.clone(),
                            detail: "expected a directory".to_string(),
                        });
                    }
                }
                "symlink" => {
                    if !metadata.is_symlink() {
                        report.problems.push(Problem {
                            path: file.path.clone(),
                            detail: "expected a symlink".to_string(),
                        });
                        continue;
                    }
                    let actual = std::fs::read_link(&target).unwrap_or_default();
                    let expected = file.target.clone().unwrap_or_default();
                    if actual.as_os_str() != expected.as_str() {
                        report.problems.push(Problem {
                            path: file.path.clone(),
                            detail: format!(
                                "link target is `{}`, expected `{}`",
                                actual.display(),
                                expected
                            ),
                        });
                    }
                }
                _ => {
                    if !metadata.is_file() {
                        report.problems.push(Problem {
                            path: file.path.clone(),
                            detail: "expected a regular file".to_string(),
                        });
                        continue;
                    }
                    let Some(expected) = &file.sha256 else {
                        continue;
                    };
                    match sha256_file(&target) {
                        Ok((actual, _)) => {
                            if &actual.0 != expected {
                                report.problems.push(Problem {
                                    path: file.path.clone(),
                                    detail: format!(
                                        "sha256 mismatch (found {}, expected {expected})",
                                        actual.0
                                    ),
                                });
                            }
                        }
                        Err(e) => report.problems.push(Problem {
                            path: file.path.clone(),
                            detail: format!("unreadable: {e}"),
                        }),
                    }
                }
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
            let Ok(entries) = std::fs::read_dir(&current) else {
                continue;
            };
            let mut found: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            found.sort();
            for path in found {
                let Ok(relative) = path.strip_prefix(rootfs) else {
                    continue;
                };
                let logical = crate::paths::normalize_absolute(&Path::new("/").join(relative));
                let metadata = match std::fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(_) => continue,
                };
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

    pub fn file_count(&self) -> usize {
        self.files
            .iter()
            .filter(|f| f.kind != PlannedFileKind::Directory.as_str())
            .count()
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
    pub checked: usize,
    /// Entries found in the rootfs that the manifest does not list.
    pub unexpected: usize,
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
}
