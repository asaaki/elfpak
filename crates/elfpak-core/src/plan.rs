//! Two-phase packaging: DISCOVER -> BundlePlan -> VALIDATE -> MATERIALIZE.
//!
//! A [`BundlePlan`] is immutable once built and fully describes the output, so
//! `inspect`, `--dry-run`, manifests and tests all share one code path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::elf::Architecture;
use crate::error::{Error, Result, io};
use crate::graph::{DependencyGraph, DependencyReason, Digest, NodeKind};
use crate::hash::{DigestCache, sha256_bytes};
use crate::paths::{ancestor_dirs, normalize_absolute};
use crate::resolver::Resolver;
use crate::rootfs::policy::{DependencyPolicy, RuntimeFeature, RuntimePolicy};
use crate::source::{EntryKind, SourceRoot};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlannedFileKind {
    Directory,
    Symlink,
    Executable,
    Interpreter,
    SharedObject,
    CertificateBundle,
    RuntimeConfig,
    ApplicationData,
}

impl PlannedFileKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlannedFileKind::Directory => "directory",
            PlannedFileKind::Symlink => "symlink",
            PlannedFileKind::Executable => "executable",
            PlannedFileKind::Interpreter => "interpreter",
            PlannedFileKind::SharedObject => "shared-object",
            PlannedFileKind::CertificateBundle => "certificate-bundle",
            PlannedFileKind::RuntimeConfig => "runtime-config",
            PlannedFileKind::ApplicationData => "application-data",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InclusionReason {
    Application,
    Interpreter,
    NeededBy { binary: PathBuf, soname: String },
    RuntimePolicy { feature: RuntimeFeature },
    ExplicitInclude,
}

/// One entry of the output rootfs. Nothing is written that is not planned here.
#[derive(Debug, Clone)]
pub struct PlannedFile {
    /// Host path to copy from, if the content comes from the source root.
    pub source: Option<PathBuf>,
    /// Absolute path inside the generated rootfs.
    pub destination: PathBuf,
    pub kind: PlannedFileKind,
    pub reason: InclusionReason,
    pub mode: u32,
    pub size: u64,
    pub sha256: Option<Digest>,
    /// Verbatim link target for [`PlannedFileKind::Symlink`].
    pub link_target: Option<PathBuf>,
    /// Generated content (passwd, nsswitch.conf, ...).
    pub content: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct BundlePlan {
    pub executable: PlannedFile,
    /// All entries, including the executable, sorted by destination.
    pub files: Vec<PlannedFile>,
    pub graph: DependencyGraph,
    pub architecture: Architecture,
    /// `PT_INTERP` as declared by the executable.
    pub interpreter: Option<PathBuf>,
    /// Where that interpreter actually lives after following symlinks.
    pub interpreter_resolved: Option<PathBuf>,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone)]
pub struct Warning {
    pub code: &'static str,
    pub message: String,
    pub details: Vec<String>,
}

impl BundlePlan {
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    pub fn files_of_kind(&self, kind: PlannedFileKind) -> impl Iterator<Item = &PlannedFile> {
        self.files.iter().filter(move |f| f.kind == kind)
    }
}

pub struct Planner {
    source_root: SourceRoot,
    binary: PathBuf,
    install_path: PathBuf,
    runtime_policy: RuntimePolicy,
    dependency_policy: DependencyPolicy,
    library_paths: Vec<PathBuf>,
}

impl Planner {
    pub fn new(source_root: SourceRoot, binary: impl Into<PathBuf>) -> Planner {
        let binary = binary.into();
        let install_path = PathBuf::from("/").join(
            binary
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("app")),
        );
        Planner {
            source_root,
            binary,
            install_path,
            runtime_policy: RuntimePolicy::default(),
            dependency_policy: DependencyPolicy::allow_all(),
            library_paths: Vec::new(),
        }
    }

    pub fn install_as(mut self, path: impl Into<PathBuf>) -> Planner {
        self.install_path = normalize_absolute(&path.into());
        self
    }

    pub fn runtime_policy(mut self, policy: RuntimePolicy) -> Planner {
        self.runtime_policy = policy;
        self
    }

    pub fn dependency_policy(mut self, policy: DependencyPolicy) -> Planner {
        self.dependency_policy = policy;
        self
    }

    pub fn library_paths(mut self, paths: Vec<PathBuf>) -> Planner {
        self.library_paths = paths;
        self
    }

    pub fn plan(&self) -> Result<BundlePlan> {
        let mut resolver =
            Resolver::new(self.source_root.clone()).with_library_paths(self.library_paths.clone());
        let mut graph = resolver.closure(&self.binary, &self.install_path)?;
        let architecture = graph.root_node().architecture;
        let interpreter = graph.declared_interpreter.clone();
        let interpreter_resolved = graph
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Interpreter)
            .map(|n| n.destination.clone());

        let mut warnings = Vec::new();
        let mut dlopen_libraries: Vec<String> = Vec::new();
        let mut builder = PlanBuilder::new(&self.source_root);

        // NSS modules are dlopen()ed by glibc; include them when the policy asks
        // for name-service configuration and the source root still ships them.
        if self.runtime_policy.nsswitch {
            let root_id = graph.root;
            for soname in RuntimePolicy::NSS_MODULES {
                let requester = graph.root_node().logical.clone();
                if let Some(library) =
                    resolver.resolve_extra_library(soname, architecture, &requester)?
                {
                    resolver.attach_library(
                        &mut graph,
                        &library,
                        root_id,
                        DependencyReason::RuntimePolicy {
                            feature: "nsswitch",
                        },
                    )?;
                }
            }
        }

        self.validate_dependencies(&graph)?;

        // ELF closure.
        for (id, node) in graph.nodes.iter().enumerate() {
            let reason = if id == graph.root {
                InclusionReason::Application
            } else if node.kind == NodeKind::Interpreter {
                InclusionReason::Interpreter
            } else {
                match graph.first_dependent(id) {
                    Some((edge, parent)) => match &edge.reason {
                        DependencyReason::Needed { soname } => InclusionReason::NeededBy {
                            binary: parent.destination.clone(),
                            soname: soname.clone(),
                        },
                        DependencyReason::Interpreter => InclusionReason::Interpreter,
                        DependencyReason::RuntimePolicy { .. } => InclusionReason::RuntimePolicy {
                            feature: RuntimeFeature::Nsswitch,
                        },
                    },
                    None => InclusionReason::Application,
                }
            };
            let kind = match node.kind {
                NodeKind::Executable => PlannedFileKind::Executable,
                NodeKind::Interpreter => PlannedFileKind::Interpreter,
                NodeKind::SharedObject => PlannedFileKind::SharedObject,
            };
            builder.push_file(PlannedFile {
                source: Some(node.source.clone()),
                destination: node.destination.clone(),
                kind,
                reason: reason.clone(),
                mode: mode_of(&node.source)?,
                size: node.size,
                sha256: Some(node.sha256.clone()),
                link_target: None,
                content: None,
            });
            for link in &node.links {
                builder.push_symlink(&link.logical, &link.target, reason.clone());
            }
            if !node.dlopen_references.is_empty() {
                if id == graph.root {
                    warnings.push(Warning {
                        code: "E1004",
                        message: format!("{} references dlopen()", node.destination.display()),
                        details: vec![
                            "Runtime-loaded libraries cannot be determined using static ELF dependency analysis.".to_string(),
                            "Consider adding them with --include.".to_string(),
                        ],
                    });
                } else {
                    dlopen_libraries.push(node.destination.display().to_string());
                }
            }
        }

        if !dlopen_libraries.is_empty() {
            warnings.push(Warning {
                code: "E1004",
                message: format!(
                    "{} bundled shared object(s) reference dlopen()",
                    dlopen_libraries.len()
                ),
                details: dlopen_libraries,
            });
        }

        self.apply_runtime_policy(&mut builder, &mut warnings)?;

        let files = builder.finish();
        let executable = files
            .iter()
            .find(|f| f.kind == PlannedFileKind::Executable)
            .cloned()
            .expect("plan always contains the executable");

        Ok(BundlePlan {
            executable,
            files,
            graph,
            architecture,
            interpreter,
            interpreter_resolved,
            warnings,
        })
    }

    fn validate_dependencies(&self, graph: &DependencyGraph) -> Result<()> {
        if self.dependency_policy.allow.is_none() {
            return Ok(());
        }
        for (id, node) in graph.nodes.iter().enumerate() {
            if node.kind != NodeKind::SharedObject {
                continue;
            }
            let soname = node
                .soname
                .clone()
                .or_else(|| {
                    node.logical
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                })
                .unwrap_or_default();
            if self.dependency_policy.is_allowed(&soname, &node.logical) {
                continue;
            }
            let required_by = graph
                .first_dependent(id)
                .map(|(_, parent)| parent.destination.clone())
                .unwrap_or_else(|| self.install_path.clone());
            return Err(Error::DisallowedLibrary {
                soname,
                required_by,
            });
        }
        Ok(())
    }

    fn apply_runtime_policy(
        &self,
        builder: &mut PlanBuilder<'_>,
        warnings: &mut Vec<Warning>,
    ) -> Result<()> {
        let policy = &self.runtime_policy;

        if policy.ca_certificates {
            let mut found = false;
            for candidate in RuntimePolicy::CA_BUNDLE_CANDIDATES {
                let logical = PathBuf::from(candidate);
                if builder.copy_path(
                    &logical,
                    PlannedFileKind::CertificateBundle,
                    InclusionReason::RuntimePolicy {
                        feature: RuntimeFeature::CaCertificates,
                    },
                    false,
                )? {
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(Error::MissingRuntimeFile {
                    feature: "ca-certificates",
                    searched: RuntimePolicy::CA_BUNDLE_CANDIDATES
                        .iter()
                        .map(PathBuf::from)
                        .collect(),
                });
            }
        }

        if policy.tmp {
            builder.push_dir_with_mode(
                Path::new("/tmp"),
                0o1777,
                InclusionReason::RuntimePolicy {
                    feature: RuntimeFeature::Tmp,
                },
            );
        }

        if policy.passwd_group {
            builder.push_generated(
                Path::new("/etc/passwd"),
                policy.passwd_contents(),
                InclusionReason::RuntimePolicy {
                    feature: RuntimeFeature::PasswdGroup,
                },
            );
            builder.push_generated(
                Path::new("/etc/group"),
                policy.group_contents(),
                InclusionReason::RuntimePolicy {
                    feature: RuntimeFeature::PasswdGroup,
                },
            );
        }

        if policy.nsswitch {
            builder.push_generated(
                Path::new("/etc/nsswitch.conf"),
                policy.nsswitch_contents(),
                InclusionReason::RuntimePolicy {
                    feature: RuntimeFeature::Nsswitch,
                },
            );
        }

        if policy.tzdata {
            let reason = InclusionReason::RuntimePolicy {
                feature: RuntimeFeature::Tzdata,
            };
            let zoneinfo = PathBuf::from("/usr/share/zoneinfo");
            if !builder.copy_path(
                &zoneinfo,
                PlannedFileKind::ApplicationData,
                reason.clone(),
                true,
            )? {
                return Err(Error::MissingRuntimeFile {
                    feature: "tzdata",
                    searched: vec![zoneinfo],
                });
            }
            builder.copy_path(
                Path::new("/etc/localtime"),
                PlannedFileKind::RuntimeConfig,
                reason,
                false,
            )?;
        }

        for include in &policy.includes {
            let logical = normalize_absolute(include);
            if !builder.copy_path(
                &logical,
                PlannedFileKind::ApplicationData,
                InclusionReason::ExplicitInclude,
                true,
            )? {
                return Err(Error::MissingSourcePath { path: logical });
            }
        }

        if policy.user.is_some() && !policy.passwd_group {
            warnings.push(Warning {
                code: "E1006",
                message: "--user was given without passwd/group files".to_string(),
                details: vec![
                    "Add --passwd-group (or --preset web) if the application resolves its own uid."
                        .to_string(),
                ],
            });
        }

        Ok(())
    }
}

/// Accumulates plan entries, deduplicating destinations and creating the
/// directory scaffolding each entry needs.
struct PlanBuilder<'a> {
    root: &'a SourceRoot,
    entries: BTreeMap<PathBuf, PlannedFile>,
    digests: DigestCache,
}

impl<'a> PlanBuilder<'a> {
    fn new(root: &'a SourceRoot) -> PlanBuilder<'a> {
        PlanBuilder {
            root,
            entries: BTreeMap::new(),
            digests: DigestCache::new(),
        }
    }

    /// Directories never displace real content; anything else wins over a
    /// previously planned directory placeholder.
    fn insert(&mut self, file: PlannedFile) {
        match self.entries.get(&file.destination) {
            Some(existing) if existing.kind != PlannedFileKind::Directory => {}
            Some(_) if file.kind == PlannedFileKind::Directory => {}
            _ => {
                self.entries.insert(file.destination.clone(), file);
            }
        }
    }

    fn push_parents(&mut self, path: &Path, reason: &InclusionReason) {
        for dir in ancestor_dirs(path) {
            self.insert(PlannedFile {
                source: None,
                destination: dir,
                kind: PlannedFileKind::Directory,
                reason: reason.clone(),
                mode: 0o755,
                size: 0,
                sha256: None,
                link_target: None,
                content: None,
            });
        }
    }

    fn push_file(&mut self, file: PlannedFile) {
        self.push_parents(&file.destination, &file.reason);
        self.insert(file);
    }

    fn push_symlink(&mut self, logical: &Path, target: &Path, reason: InclusionReason) {
        self.push_parents(logical, &reason);
        self.insert(PlannedFile {
            source: None,
            destination: logical.to_path_buf(),
            kind: PlannedFileKind::Symlink,
            reason,
            mode: 0o777,
            size: 0,
            sha256: None,
            link_target: Some(target.to_path_buf()),
            content: None,
        });
    }

    fn push_dir_with_mode(&mut self, path: &Path, mode: u32, reason: InclusionReason) {
        self.push_parents(path, &reason);
        self.insert(PlannedFile {
            source: None,
            destination: path.to_path_buf(),
            kind: PlannedFileKind::Directory,
            reason,
            mode,
            size: 0,
            sha256: None,
            link_target: None,
            content: None,
        });
    }

    fn push_generated(&mut self, path: &Path, content: Vec<u8>, reason: InclusionReason) {
        self.push_parents(path, &reason);
        let digest = sha256_bytes(&content);
        self.insert(PlannedFile {
            source: None,
            destination: path.to_path_buf(),
            kind: PlannedFileKind::RuntimeConfig,
            reason,
            mode: 0o644,
            size: content.len() as u64,
            sha256: Some(digest),
            link_target: None,
            content: Some(content),
        });
    }

    /// Copy a logical path from the source root, preserving its location and the
    /// symlinks leading to it. Returns `false` if the path does not exist.
    fn copy_path(
        &mut self,
        logical: &Path,
        kind: PlannedFileKind,
        reason: InclusionReason,
        recursive: bool,
    ) -> Result<bool> {
        let Some(resolved) = self.root.resolve(logical)? else {
            return Ok(false);
        };
        for link in &resolved.links {
            self.push_symlink(&link.logical, &link.target, reason.clone());
        }
        match resolved.kind {
            EntryKind::File => {
                let (digest, size) = self.digests.get(&resolved.host)?;
                self.push_file(PlannedFile {
                    source: Some(resolved.host.clone()),
                    destination: resolved.logical.clone(),
                    kind,
                    reason,
                    mode: mode_of(&resolved.host)?,
                    size,
                    sha256: Some(digest),
                    link_target: None,
                    content: None,
                });
                Ok(true)
            }
            EntryKind::Directory if recursive => {
                self.push_dir_with_mode(
                    &resolved.logical,
                    mode_of(&resolved.host)?,
                    reason.clone(),
                );
                self.copy_tree(&resolved.logical, &resolved.host, kind, &reason)?;
                Ok(true)
            }
            EntryKind::Directory => Ok(false),
            EntryKind::Other => Ok(false),
        }
    }

    /// Walk a directory without following symlinks: links are reproduced as
    /// links, which keeps the original structure intact.
    fn copy_tree(
        &mut self,
        logical: &Path,
        host: &Path,
        kind: PlannedFileKind,
        reason: &InclusionReason,
    ) -> Result<()> {
        let mut stack = vec![(logical.to_path_buf(), host.to_path_buf())];
        while let Some((logical, host)) = stack.pop() {
            let mut names = Vec::new();
            for entry in std::fs::read_dir(&host).map_err(|e| io(&host, e))? {
                let entry = entry.map_err(|e| io(&host, e))?;
                names.push(entry.file_name());
            }
            names.sort();
            for name in names {
                let child_logical = logical.join(&name);
                let child_host = host.join(&name);
                let metadata =
                    std::fs::symlink_metadata(&child_host).map_err(|e| io(&child_host, e))?;
                if metadata.is_symlink() {
                    let target = std::fs::read_link(&child_host).map_err(|e| io(&child_host, e))?;
                    self.push_symlink(&child_logical, &target, reason.clone());
                } else if metadata.is_dir() {
                    self.push_dir_with_mode(&child_logical, mode_of(&child_host)?, reason.clone());
                    stack.push((child_logical, child_host));
                } else if metadata.is_file() {
                    let (digest, size) = self.digests.get(&child_host)?;
                    self.push_file(PlannedFile {
                        source: Some(child_host.clone()),
                        destination: child_logical,
                        kind,
                        reason: reason.clone(),
                        mode: mode_of(&child_host)?,
                        size,
                        sha256: Some(digest),
                        link_target: None,
                        content: None,
                    });
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> Vec<PlannedFile> {
        self.entries.into_values().collect()
    }
}

/// Normalized permissions: executables and directories are `0755`, everything
/// else `0644`. Deterministic output matters more than exotic source modes.
fn mode_of(path: &Path) -> Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path).map_err(|e| io(path, e))?;
    let mode = metadata.permissions().mode();
    Ok(if metadata.is_dir() || mode & 0o111 != 0 {
        0o755
    } else {
        0o644
    })
}
