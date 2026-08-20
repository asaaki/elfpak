//! Two-phase packaging: DISCOVER -> BundlePlan -> VALIDATE -> MATERIALIZE.
//!
//! A [`BundlePlan`] is immutable once built and fully describes the output, so
//! `inspect`, `--dry-run`, manifests and tests all share one code path.

use crate::{
    elf::Architecture,
    error::{Error, Result, io},
    graph::{DependencyGraph, DependencyReason, Digest, Node, NodeId, NodeKind},
    hash::{DigestCache, sha256_bytes},
    paths::{ancestor_dirs, logical_parent, normalize_absolute},
    resolver::{
        Resolver,
        cache::{self, CacheEntry},
    },
    rootfs::policy::{DependencyPolicy, Preset, RuntimeFeature, RuntimePolicy},
    source::{EntryKind, SourceRoot},
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Where the loader looks for its cache, and therefore where a generated one
/// has to go.
pub const LD_SO_CACHE: &str = "/etc/ld.so.cache";

/// Upper bound on the entries in one plan.
///
/// A minimal rootfs is tens of entries, and the timezone database, the largest
/// thing any preset contributes, is a few thousand. A plan past this bound is an
/// `--include` that named far more than it meant to.
pub const PLAN_ENTRIES_MAX: usize = 1 << 20;

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

impl PlannedFile {
    /// Invariants every entry holds, checked when it enters a plan and again
    /// before it is written.
    pub fn assert_well_formed(&self) {
        assert!(self.destination.is_absolute());
        assert!(self.mode <= 0o7777);

        match self.kind {
            PlannedFileKind::Directory => {
                assert!(self.source.is_none());
                assert!(self.link_target.is_none());
                assert!(self.content.is_none());
                assert!(self.sha256.is_none());
                assert_eq!(self.size, 0);
            }
            PlannedFileKind::Symlink => {
                assert!(self.source.is_none());
                assert!(self.link_target.is_some());
                assert!(self.content.is_none());
                assert!(self.sha256.is_none());
                assert_eq!(self.size, 0);
            }
            // Everything else is a regular file, and its bytes come either from
            // the source root or from this process. Either way they are hashed.
            _ => {
                assert!(self.link_target.is_none());
                assert_ne!(self.source.is_some(), self.content.is_some());
                assert!(self.sha256.as_ref().is_some_and(Digest::is_well_formed));
                if let Some(content) = &self.content {
                    assert_eq!(self.size, content.len() as u64);
                }
            }
        }
    }
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

#[derive(Debug)]
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

    /// Phase one: resolve, validate, then describe the output. Nothing is
    /// written until [`crate::RootFsBuilder`] or [`crate::TarBuilder`] gets the
    /// plan.
    pub fn plan(&self) -> Result<BundlePlan> {
        self.check_install_path()?;

        let mut resolver =
            Resolver::new(self.source_root.clone()).with_library_paths(self.library_paths.clone());
        let mut graph = resolver.closure(&self.binary, &self.install_path)?;
        if self.runtime_policy.nsswitch {
            self.attach_nss_modules(&mut resolver, &mut graph)?;
        }

        self.validate_dependencies(&graph)?;
        self.check_install_collision(&graph)?;

        let mut warnings: Vec<Warning> = Vec::new();
        let mut builder = PlanBuilder::new(&self.source_root);
        self.plan_loader_cache(&graph, &resolver, &mut builder, &mut warnings);
        self.plan_closure(&graph, &mut builder, &mut warnings)?;
        self.apply_runtime_policy(&mut builder, &mut warnings)?;

        let files = builder.finish();
        let executable = files
            .iter()
            .find(|f| f.kind == PlannedFileKind::Executable)
            .cloned()
            .expect("the plan always contains the executable");

        // Every object in the closure is an entry, and every entry needs its
        // parent directories, so a plan is never smaller than the graph.
        assert_eq!(executable.destination, graph.root_node().destination);
        assert!(files.len() >= graph.node_count());

        Ok(BundlePlan {
            executable,
            architecture: graph.root_node().architecture,
            interpreter: graph.declared_interpreter.clone(),
            interpreter_resolved: graph
                .nodes
                .iter()
                .find(|n| n.kind == NodeKind::Interpreter)
                .map(|n| n.destination.clone()),
            files,
            graph,
            preset: self.preset,
            runtime_policy: self.runtime_policy.clone(),
            dependency_policy: self.dependency_policy.clone(),
            warnings,
        })
    }

    /// The executable has to land somewhere the rootfs can name.
    fn check_install_path(&self) -> Result<()> {
        assert!(self.install_path.is_absolute());

        if self.install_path.file_name().is_some() {
            return Ok(());
        }
        Err(Error::Config {
            message: format!(
                "install path `{}` does not name a file",
                self.install_path.display()
            ),
        })
    }

    /// NSS modules are `dlopen`ed by glibc rather than named by `DT_NEEDED`, so
    /// they are included when the policy asks for name-service configuration
    /// and the source root still ships them.
    fn attach_nss_modules(
        &self,
        resolver: &mut Resolver,
        graph: &mut DependencyGraph,
    ) -> Result<()> {
        assert!(self.runtime_policy.nsswitch);

        let root_id = graph.root;
        let architecture = graph.root_node().architecture;
        for soname in RuntimePolicy::NSS_MODULES {
            let requester = graph.root_node().logical.clone();
            let Some(library) = resolver.resolve_extra_library(soname, architecture, &requester)?
            else {
                continue;
            };
            resolver.attach_library(
                graph,
                &library,
                root_id,
                DependencyReason::RuntimePolicy {
                    feature: RuntimeFeature::Nsswitch,
                },
            )?;
        }
        Ok(())
    }

    /// Decide whether the bundle needs a generated `/etc/ld.so.cache`, and warn
    /// about what it cannot load when it does not get one.
    ///
    /// Two things leave a bundle unable to load a library it contains: a library
    /// outside the directories the loader searches, and an executable whose
    /// `$ORIGIN`-relative search paths point elsewhere once it is installed
    /// somewhere else. A cache fixes both, and only `elfpak` can write it.
    fn plan_loader_cache(
        &self,
        graph: &DependencyGraph,
        resolver: &Resolver,
        builder: &mut PlanBuilder<'_>,
        warnings: &mut Vec<Warning>,
    ) {
        let unreachable = unreachable_libraries(resolver);
        let relocated = relocated_search_paths(graph);
        let needs_cache = !unreachable.is_empty() || !relocated.is_empty();

        let cache = self
            .runtime_policy
            .ld_so_cache
            .applies(needs_cache)
            .then(|| self.ld_so_cache(graph))
            .flatten();

        if let Some(bytes) = cache {
            builder.push_generated(
                Path::new(LD_SO_CACHE),
                bytes,
                InclusionReason::RuntimePolicy {
                    feature: RuntimeFeature::LdSoCache,
                },
            );
            return;
        }

        // No cache: say exactly what the packaged loader will not find.
        if !unreachable.is_empty() {
            warnings.push(warn_unreachable(unreachable, uses_glibc_loader(graph)));
        }
        if !relocated.is_empty() {
            warnings.push(warn_relocated(relocated, graph));
        }
    }

    /// Turn every object in the closure into a plan entry, together with the
    /// symlinks it is reached through and any `dlopen` warning it earns.
    fn plan_closure(
        &self,
        graph: &DependencyGraph,
        builder: &mut PlanBuilder<'_>,
        warnings: &mut Vec<Warning>,
    ) -> Result<()> {
        let mut dlopen_libraries: Vec<String> = Vec::new();

        for (id, node) in graph.iter() {
            let reason = inclusion_reason(graph, id, node);
            builder.push_file(PlannedFile {
                source: Some(node.source.clone()),
                destination: node.destination.clone(),
                kind: planned_kind(node.kind),
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

            if node.dlopen_references.is_empty() {
                continue;
            }
            if id == graph.root {
                warnings.push(warn_dlopen_executable(node));
            } else {
                dlopen_libraries.push(node.destination.display().to_string());
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
        Ok(())
    }

    /// A `/etc/ld.so.cache` describing every shared object in the bundle.
    ///
    /// `None` when there is nothing to record, or when the target is one the
    /// cache format cannot describe — the caller then reports the problem
    /// instead of writing a cache the loader would reject.
    fn ld_so_cache(&self, graph: &DependencyGraph) -> Option<Vec<u8>> {
        if !uses_glibc_loader(graph) {
            // musl resolves libraries through /etc/ld-musl-<arch>.path and
            // ignores ld.so.cache entirely, so the caller reports instead.
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
        assert!(install.is_absolute());

        for (id, node) in graph.iter() {
            if id == graph.root {
                continue;
            }
            if &node.destination != install {
                continue;
            }
            return Err(Error::Config {
                message: format!(
                    "install path `{}` collides with `{}`, which the closure \
                     needs at that exact path",
                    self.install_path.display(),
                    node.logical.display()
                ),
            });
        }
        Ok(())
    }

    /// Enforce the dependency allow-list.
    ///
    /// Only the application's own ELF closure is policed. The interpreter is
    /// exempt because it is not a `DT_NEEDED` dependency, and so is anything
    /// runtime policy pulled in: the caller asked for those by name, and cannot
    /// be expected to know the sonames of the NSS modules a source root ships.
    fn validate_dependencies(&self, graph: &DependencyGraph) -> Result<()> {
        if self.dependency_policy.allow.is_none() {
            // No allow-list means no contract to enforce.
            return Ok(());
        }

        let application = graph.application_closure();

        for (id, node) in graph.iter() {
            if node.kind != NodeKind::SharedObject {
                continue;
            }
            if !application.contains(&id) {
                continue;
            }
            let soname = library_name(node);
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

    /// Everything runtime policy contributes, in one place.
    fn apply_runtime_policy(
        &self,
        builder: &mut PlanBuilder<'_>,
        warnings: &mut Vec<Warning>,
    ) -> Result<()> {
        let policy = &self.runtime_policy;

        if policy.ca_certificates {
            self.plan_ca_certificates(builder)?;
        }
        if policy.tmp {
            // 1777: every user may write, only the owner may unlink.
            builder.push_dir_with_mode(
                Path::new("/tmp"),
                0o1777,
                InclusionReason::RuntimePolicy {
                    feature: RuntimeFeature::Tmp,
                },
            );
        }
        if policy.passwd_group {
            self.plan_passwd_group(builder);
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
            self.plan_tzdata(builder)?;
        }
        for include in &policy.includes {
            self.plan_include(builder, include)?;
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

    /// The first CA bundle the source root actually has. A `web` preset that
    /// silently shipped no trust store would fail at the first HTTPS request.
    fn plan_ca_certificates(&self, builder: &mut PlanBuilder<'_>) -> Result<()> {
        for candidate in RuntimePolicy::CA_BUNDLE_CANDIDATES {
            let logical = PathBuf::from(candidate);
            let found = builder.copy_path(
                &logical,
                PlannedFileKind::CertificateBundle,
                InclusionReason::RuntimePolicy {
                    feature: RuntimeFeature::CaCertificates,
                },
                false,
            )?;
            if found {
                return Ok(());
            }
        }
        Err(Error::MissingRuntimeFile {
            feature: "ca-certificates",
            searched: RuntimePolicy::CA_BUNDLE_CANDIDATES
                .iter()
                .map(PathBuf::from)
                .collect(),
        })
    }

    fn plan_passwd_group(&self, builder: &mut PlanBuilder<'_>) {
        let reason = InclusionReason::RuntimePolicy {
            feature: RuntimeFeature::PasswdGroup,
        };
        builder.push_generated(
            Path::new("/etc/passwd"),
            self.runtime_policy.passwd_contents(),
            reason.clone(),
        );
        builder.push_generated(
            Path::new("/etc/group"),
            self.runtime_policy.group_contents(),
            reason,
        );
    }

    /// The zone database, plus `/etc/localtime` when the source root sets one.
    fn plan_tzdata(&self, builder: &mut PlanBuilder<'_>) -> Result<()> {
        let reason = InclusionReason::RuntimePolicy {
            feature: RuntimeFeature::Tzdata,
        };
        let zoneinfo = PathBuf::from("/usr/share/zoneinfo");
        let found = builder.copy_path(
            &zoneinfo,
            PlannedFileKind::ApplicationData,
            reason.clone(),
            true,
        )?;
        if !found {
            return Err(Error::MissingRuntimeFile {
                feature: "tzdata",
                searched: vec![zoneinfo],
            });
        }
        // A missing /etc/localtime is not an error: UTC is a valid default.
        builder.copy_path(
            Path::new("/etc/localtime"),
            PlannedFileKind::RuntimeConfig,
            reason,
            false,
        )?;
        Ok(())
    }

    fn plan_include(&self, builder: &mut PlanBuilder<'_>, include: &Path) -> Result<()> {
        let logical = normalize_absolute(include);
        let found = builder.copy_path(
            &logical,
            PlannedFileKind::ApplicationData,
            InclusionReason::ExplicitInclude,
            true,
        )?;
        if found {
            return Ok(());
        }
        Err(Error::MissingSourcePath { path: logical })
    }
}

/// Why an object is in the bundle, as recorded for the manifest.
fn inclusion_reason(graph: &DependencyGraph, id: NodeId, node: &Node) -> InclusionReason {
    if id == graph.root {
        return InclusionReason::Application;
    }
    if node.kind == NodeKind::Interpreter {
        return InclusionReason::Interpreter;
    }
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
        // Unreachable in a graph built by the resolver: only the executable has
        // no dependent, and it was handled above.
        None => InclusionReason::Application,
    }
}

fn planned_kind(kind: NodeKind) -> PlannedFileKind {
    match kind {
        NodeKind::Executable => PlannedFileKind::Executable,
        NodeKind::Interpreter => PlannedFileKind::Interpreter,
        NodeKind::SharedObject => PlannedFileKind::SharedObject,
    }
}

/// How a library is named on the command line: its `DT_SONAME` when it has one,
/// otherwise its file name.
fn library_name(node: &Node) -> String {
    node.soname
        .clone()
        .or_else(|| {
            node.logical
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

/// Libraries that resolved through something the bundle does not reproduce.
fn unreachable_libraries(resolver: &Resolver) -> Vec<String> {
    resolver
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
        .collect()
}

/// `$ORIGIN`-relative search paths of an executable that is being installed
/// somewhere other than where it was built. They point somewhere else now.
fn relocated_search_paths(graph: &DependencyGraph) -> Vec<String> {
    let source_dir = logical_parent(&graph.root_node().logical);
    let install_dir = logical_parent(&graph.root_node().destination);
    if install_dir == source_dir {
        return Vec::new();
    }
    graph
        .executable_search_paths
        .iter()
        .filter(|entry| entry.contains("$ORIGIN") || entry.contains("${ORIGIN}"))
        .cloned()
        .collect()
}

fn warn_unreachable(libraries: Vec<String>, glibc: bool) -> Warning {
    assert!(!libraries.is_empty());

    let explanation = if glibc {
        format!(
            "Without {LD_SO_CACHE} the packaged application finds these \
             only if its DT_RPATH/DT_RUNPATH covers them."
        )
    } else {
        "This loader does not read an ld.so.cache, so the paths have to \
         come from the objects themselves."
            .to_string()
    };
    Warning {
        code: "E2005",
        message: match libraries.len() {
            1 => "a library lives outside the directories the loader searches".to_string(),
            n => format!("{n} libraries live outside the directories the loader searches"),
        },
        details: libraries.into_iter().chain([explanation]).collect(),
    }
}

fn warn_relocated(paths: Vec<String>, graph: &DependencyGraph) -> Warning {
    assert!(!paths.is_empty());

    let source_dir = logical_parent(&graph.root_node().logical);
    let install_dir = logical_parent(&graph.root_node().destination);
    assert_ne!(source_dir, install_dir);

    let advice = format!(
        "Install it at {} to keep those paths pointing where they did.",
        graph.root_node().logical.display()
    );
    Warning {
        code: "E2006",
        message: format!(
            "the executable declares $ORIGIN-relative search paths and moves from {} to {}",
            source_dir.display(),
            install_dir.display()
        ),
        details: paths.into_iter().chain([advice]).collect(),
    }
}

fn warn_dlopen_executable(node: &Node) -> Warning {
    assert!(!node.dlopen_references.is_empty());

    Warning {
        code: "E1004",
        message: format!("{} references dlopen()", node.destination.display()),
        details: vec![
            "Runtime-loaded libraries cannot be determined using static ELF dependency analysis."
                .to_string(),
            "Consider adding them with --include.".to_string(),
        ],
    }
}

/// Accumulates plan entries, deduplicating destinations and creating the
/// directory scaffolding each entry needs.
#[derive(Debug)]
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
        file.assert_well_formed();

        match self.entries.get(&file.destination) {
            Some(existing) if existing.kind != PlannedFileKind::Directory => {}
            Some(_) if file.kind == PlannedFileKind::Directory => {}
            _ => {
                self.entries.insert(file.destination.clone(), file);
            }
        }
    }

    /// Directory scaffolding for an entry. Parents are planned shallowest
    /// first, which is the order they have to be created in.
    fn push_parents(&mut self, path: &Path, reason: &InclusionReason) {
        assert!(path.is_absolute());

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
        assert!(logical.is_absolute());
        assert!(!target.as_os_str().is_empty());

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

    /// An entry whose bytes this process produced rather than copied.
    fn push_generated(&mut self, path: &Path, content: Vec<u8>, reason: InclusionReason) {
        assert!(path.is_absolute());
        assert!(!content.is_empty());

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
        assert!(destination.is_absolute());

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
            assert!(logical.is_absolute());
            self.check_size(&logical)?;

            let mut names = Vec::new();
            let remaining = PLAN_ENTRIES_MAX.saturating_sub(self.entries.len());
            for entry in std::fs::read_dir(&host).map_err(|e| io(&host, e))? {
                if names.len() == remaining {
                    return Err(Self::size_error(&logical));
                }
                let entry = entry.map_err(|e| io(&host, e))?;
                names.push(entry.file_name());
            }
            names.sort();
            for name in names {
                let child_logical = logical.join(&name);
                self.check_size(&child_logical)?;
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
                self.check_size(&logical)?;
            }
        }
        Ok(())
    }

    fn check_size(&self, logical: &Path) -> Result<()> {
        if self.entries.len() <= PLAN_ENTRIES_MAX {
            return Ok(());
        }
        Err(Self::size_error(logical))
    }

    fn size_error(logical: &Path) -> Error {
        Error::Config {
            message: format!(
                "bundle plan exceeds {PLAN_ENTRIES_MAX} entries at `{}`; narrow --include",
                logical.display()
            ),
        }
    }

    /// Entries sorted by destination, which is what makes a parent directory
    /// precede everything inside it when the plan is written out.
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
/// else `0644`.
fn mode_of(path: &Path) -> Result<u32> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::metadata(path).map_err(|e| io(path, e))?;
    let mode = metadata.permissions().mode();
    let normalized = if metadata.is_dir() || mode & 0o111 != 0 {
        0o755
    } else {
        0o644
    };
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        elf::{ElfClass, Endianness, Machine},
        graph::Node,
    };

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
            // A real digest: the graph asserts that every node carries one.
            sha256: sha256_bytes(logical.as_bytes()),
            size: 0,
            links: Vec::new(),
            dlopen_references: Vec::new(),
        };

        let mut graph = DependencyGraph::new();
        graph.root = graph
            .insert(node(NodeKind::Executable, "/app/server", None))
            .unwrap();
        graph.declared_interpreter = interpreter.map(PathBuf::from);
        if let Some(interpreter) = interpreter {
            graph
                .insert(node(NodeKind::Interpreter, interpreter, Some("ld.so")))
                .unwrap();
        }
        graph
            .insert(node(
                NodeKind::SharedObject,
                "/opt/vendor/lib/libvendor.so.1",
                Some("libvendor.so.1"),
            ))
            .unwrap();
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
