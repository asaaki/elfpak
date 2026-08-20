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
use crate::paths::{ancestor_dirs, logical_parent, normalize_absolute};
use crate::resolver::Resolver;
use crate::resolver::cache::{self, CacheEntry};
use crate::rootfs::policy::{DependencyPolicy, Preset, RuntimeFeature, RuntimePolicy};
use crate::source::{EntryKind, SourceRoot};

/// Where the loader looks for its cache, and therefore where a generated one
/// has to go.
pub const LD_SO_CACHE: &str = "/etc/ld.so.cache";

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
    /// Preset the policy was derived from, when one was named.
    pub preset: Option<Preset>,
    /// Runtime policy this plan was built with, recorded for the manifest.
    pub runtime_policy: RuntimePolicy,
    pub dependency_policy: DependencyPolicy,
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
    preset: Option<Preset>,
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
            preset: None,
        }
    }

    /// Record which preset the runtime policy came from, for the manifest.
    pub fn preset(mut self, preset: Preset) -> Planner {
        self.preset = Some(preset);
        self.runtime_policy = RuntimePolicy::from_preset(preset);
        self
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
        if self.install_path.file_name().is_none() {
            return Err(Error::Config {
                message: format!(
                    "install path `{}` does not name a file",
                    self.install_path.display()
                ),
            });
        }
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
                            feature: RuntimeFeature::Nsswitch,
                        },
                    )?;
                }
            }
        }

        self.validate_dependencies(&graph)?;
        self.check_install_collision(&graph)?;

        // Two things can leave a bundle unable to load a library it contains:
        // a library outside the directories the loader searches, and an
        // executable whose $ORIGIN-relative search paths no longer point where
        // they did once it is installed somewhere else. Both are cured by the
        // one thing the loader consults besides those directories — a cache —
        // and a bundle can only have one if `elfpak` writes it.
        let unreachable: Vec<String> = resolver
            .notes()
            .iter()
            .map(|note| {
                format!(
                    "{} in {} (found through {})",
                    note.soname,
                    note.directory.display(),
                    note.origin.as_str()
                )
            })
            .collect();

        let source_dir = logical_parent(&graph.root_node().logical);
        let install_dir = logical_parent(&graph.root_node().destination);
        let relocated: Vec<String> = if install_dir == source_dir {
            Vec::new()
        } else {
            graph
                .executable_search_paths
                .iter()
                .filter(|entry| entry.contains("$ORIGIN") || entry.contains("${ORIGIN}"))
                .cloned()
                .collect()
        };

        let needs_cache = !unreachable.is_empty() || !relocated.is_empty();
        let cache = self
            .runtime_policy
            .ld_so_cache
            .applies(needs_cache)
            .then(|| self.ld_so_cache(&graph))
            .flatten();

        match cache {
            Some(bytes) => builder.push_generated(
                Path::new(LD_SO_CACHE),
                bytes,
                InclusionReason::RuntimePolicy {
                    feature: RuntimeFeature::LdSoCache,
                },
            ),
            None => {
                if !unreachable.is_empty() {
                    warnings.push(Warning {
                        code: "E2005",
                        message: match unreachable.len() {
                            1 => "a library lives outside the directories the loader searches"
                                .to_string(),
                            n => format!(
                                "{n} libraries live outside the directories the loader searches"
                            ),
                        },
                        details: unreachable
                            .into_iter()
                            .chain([if uses_glibc_loader(&graph) {
                                format!(
                                    "Without {LD_SO_CACHE} the packaged application finds these \
                                     only if its DT_RPATH/DT_RUNPATH covers them."
                                )
                            } else {
                                "This loader does not read an ld.so.cache, so the paths have to \
                                 come from the objects themselves."
                                    .to_string()
                            }])
                            .collect(),
                    });
                }
                if !relocated.is_empty() {
                    warnings.push(Warning {
                        code: "E2006",
                        message: format!(
                            "the executable declares $ORIGIN-relative search paths and moves from {} to {}",
                            source_dir.display(),
                            install_dir.display()
                        ),
                        details: relocated
                            .into_iter()
                            .chain([format!(
                                "Install it at {} to keep those paths pointing where they did.",
                                graph.root_node().logical.display()
                            )])
                            .collect(),
                    });
                }
            }
        }

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
                        DependencyReason::RuntimePolicy { feature } => {
                            InclusionReason::RuntimePolicy { feature: *feature }
                        }
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
            preset: self.preset,
            runtime_policy: self.runtime_policy.clone(),
            dependency_policy: self.dependency_policy.clone(),
            interpreter,
            interpreter_resolved,
            warnings,
        })
    }

    /// A `/etc/ld.so.cache` describing every shared object in the bundle.
    ///
    /// `None` when there is nothing to record, or when the target is one the
    /// cache format cannot describe — the caller then reports the problem
    /// instead of writing a cache the loader would reject.
    fn ld_so_cache(&self, graph: &DependencyGraph) -> Option<Vec<u8>> {
        if !uses_glibc_loader(graph) {
            // musl resolves libraries through /etc/ld-musl-<arch>.path and
            // ignores ld.so.cache entirely. Writing one would look like a fix
            // and change nothing, so the caller reports the problem instead.
            return None;
        }
        let architecture = graph.root_node().architecture;
        let entries: Vec<CacheEntry> = graph
            .nodes
            .iter()
            .filter(|node| matches!(node.kind, NodeKind::SharedObject | NodeKind::Interpreter))
            .map(|node| CacheEntry {
                // A library without DT_SONAME is looked up by file name, which
                // is also what its dependents will have recorded.
                soname: node.soname.clone().unwrap_or_else(|| {
                    node.destination
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default()
                }),
                path: node.destination.clone(),
            })
            .filter(|entry| !entry.soname.is_empty())
            .collect();
        if entries.is_empty() {
            return None;
        }
        cache::build(&architecture, &entries)
    }

    /// The executable would otherwise displace a library that has to keep its
    /// own path, leaving the bundle with a dependency it cannot load.
    fn check_install_collision(&self, graph: &DependencyGraph) -> Result<()> {
        let install = &graph.root_node().destination;
        for (id, node) in graph.nodes.iter().enumerate() {
            if id != graph.root && &node.destination == install {
                return Err(Error::Config {
                    message: format!(
                        "install path `{}` collides with `{}`, which the closure needs at that exact path",
                        self.install_path.display(),
                        node.logical.display()
                    ),
                });
            }
        }
        Ok(())
    }

    /// Enforce the dependency allow-list.
    ///
    /// Only the application's own ELF closure is policed. The interpreter is
    /// exempt because it is not a `DT_NEEDED` dependency, and so is anything
    /// runtime policy pulled in — the caller asked for those by name, and could
    /// not predict the sonames of, say, the NSS modules a source root happens to
    /// still ship.
    fn validate_dependencies(&self, graph: &DependencyGraph) -> Result<()> {
        if self.dependency_policy.allow.is_none() {
            return Ok(());
        }
        let application = graph.application_closure();
        for (id, node) in graph.nodes.iter().enumerate() {
            if node.kind != NodeKind::SharedObject || !application.contains(&id) {
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

    fn push_source_file(
        &mut self,
        source: PathBuf,
        destination: PathBuf,
        kind: PlannedFileKind,
        reason: InclusionReason,
    ) -> Result<()> {
        let (digest, size) = self.digests.get(&source)?;
        self.push_file(PlannedFile {
            source: Some(source.clone()),
            destination,
            kind,
            reason,
            mode: mode_of(&source)?,
            size,
            sha256: Some(digest),
            link_target: None,
            content: None,
        });
        Ok(())
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
                self.push_source_file(resolved.host, resolved.logical, kind, reason)?;
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
                    self.push_source_file(child_host, child_logical, kind, reason.clone())?;
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> Vec<PlannedFile> {
        self.entries.into_values().collect()
    }
}

/// Whether `PT_INTERP` is a glibc loader, and therefore whether an
/// `ld.so.cache` means anything to the packaged application.
fn uses_glibc_loader(graph: &DependencyGraph) -> bool {
    match &graph.declared_interpreter {
        Some(interpreter) => !interpreter
            .file_name()
            .map(|name| name.to_string_lossy().contains("ld-musl"))
            .unwrap_or(false),
        // A static binary has no loader, and no libraries for a cache to name.
        None => false,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elf::{ElfClass, Endianness, Machine};
    use crate::graph::{Digest, Node};

    fn graph_with_interpreter(interpreter: Option<&str>) -> DependencyGraph {
        let architecture = Architecture {
            machine: Machine::X86_64,
            class: ElfClass::Elf64,
            endianness: Endianness::Little,
        };
        let node = |kind, logical: &str, soname: Option<&str>| Node {
            source: PathBuf::from(logical),
            logical: PathBuf::from(logical),
            destination: PathBuf::from(logical),
            kind,
            soname: soname.map(str::to_string),
            architecture,
            sha256: Digest(String::new()),
            size: 0,
            links: Vec::new(),
            dlopen_references: Vec::new(),
        };

        let mut graph = DependencyGraph::new();
        graph.root = graph.insert(node(NodeKind::Executable, "/app/server", None));
        graph.declared_interpreter = interpreter.map(PathBuf::from);
        if let Some(interpreter) = interpreter {
            graph.insert(node(NodeKind::Interpreter, interpreter, Some("ld.so")));
        }
        graph.insert(node(
            NodeKind::SharedObject,
            "/opt/vendor/lib/libvendor.so.1",
            Some("libvendor.so.1"),
        ));
        graph
    }

    fn planner() -> Planner {
        Planner::new(SourceRoot::new("/"), "/app/server")
    }

    #[test]
    fn a_glibc_bundle_gets_a_cache_naming_its_libraries() {
        let graph = graph_with_interpreter(Some("/lib64/ld-linux-x86-64.so.2"));
        let bytes = planner().ld_so_cache(&graph).expect("a cache is built");

        let cache = crate::resolver::LdCache::parse(&bytes);
        assert_eq!(
            cache.lookup("libvendor.so.1"),
            [PathBuf::from("/opt/vendor/lib/libvendor.so.1")]
        );
        assert!(
            !cache.lookup("ld.so").is_empty(),
            "the interpreter is listed too, as ldconfig lists it"
        );
    }

    /// musl ignores `ld.so.cache`, so writing one would only look like a fix.
    #[test]
    fn a_musl_bundle_gets_no_cache() {
        let graph = graph_with_interpreter(Some("/lib/ld-musl-x86_64.so.1"));
        assert!(planner().ld_so_cache(&graph).is_none());
        assert!(!uses_glibc_loader(&graph));
    }

    #[test]
    fn a_static_binary_gets_no_cache() {
        let mut graph = graph_with_interpreter(None);
        graph.nodes.retain(|node| node.kind == NodeKind::Executable);
        assert!(planner().ld_so_cache(&graph).is_none());
    }
}
