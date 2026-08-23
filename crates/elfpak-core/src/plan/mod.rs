//! Two-phase packaging: DISCOVER -> BundlePlan -> VALIDATE -> MATERIALIZE.
//!
//! A [`BundlePlan`] is immutable once built and fully describes the output, so
//! `inspect`, `--dry-run`, manifests and tests all share one code path.

use crate::{
    diagnostics::warning as code,
    error::{Error, Result, io},
    graph::{DependencyGraph, DependencyReason, Node, NodeId, NodeKind},
    paths::{logical_parent, normalize_absolute},
    policy::{DependencyPolicy, Preset, RuntimeFeature, RuntimePolicy},
    resolver::{
        Resolver,
        cache::{self, CacheEntry},
    },
    source::SourceRoot,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

mod builder;
mod model;

use builder::{Authority, Conflict, PlanBuilder};
pub use model::{
    ApplicationPlan, BundlePlan, InclusionReason, PlannedFile, PlannedFileKind, Warning,
};

/// Where the loader looks for its cache, and therefore where a generated one
/// has to go.
pub const LD_SO_CACHE: &str = "/etc/ld.so.cache";

/// Upper bound on the entries in one plan.
///
/// A minimal rootfs is tens of entries, while timezone data or an explicit
/// application tree can add thousands. This bound catches an accidental walk
/// into an unexpectedly large filesystem before the plan consumes unbounded
/// memory.
pub const PLAN_ENTRIES_MAX: usize = 1 << 20;

#[derive(Debug)]
pub struct Planner {
    source_root: SourceRoot,
    binaries: Vec<PlannerInput>,
    runtime_policy: RuntimePolicy,
    dependency_policy: DependencyPolicy,
    library_paths: Vec<PathBuf>,
    preset: Option<Preset>,
}

#[derive(Debug)]
struct PlannerInput {
    binary: PathBuf,
    install_path: PathBuf,
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
            binaries: vec![PlannerInput {
                binary,
                install_path,
            }],
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
        self.binaries[0].install_path = normalize_absolute(&path.into());
        self
    }

    /// Add another executable and its destination to this bundle.
    pub fn add_binary(
        mut self,
        binary: impl Into<PathBuf>,
        install_path: impl Into<PathBuf>,
    ) -> Planner {
        self.binaries.push(PlannerInput {
            binary: binary.into(),
            install_path: normalize_absolute(&install_path.into()),
        });
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
        let mut resolved = Vec::with_capacity(self.binaries.len());
        let mut closure_entries = 0usize;
        for input in &self.binaries {
            self.check_install_path(input)?;

            let mut resolver = Resolver::new(self.source_root.clone())
                .with_library_paths(self.library_paths.clone());
            let mut graph = resolver.closure(&input.binary, &input.install_path)?;
            if self.runtime_policy.nsswitch {
                self.attach_nss_modules(&mut resolver, &mut graph)?;
            }
            self.validate_dependencies(&graph, &input.install_path)?;
            self.check_install_collision(&graph, &input.install_path)?;
            closure_entries = closure_entries
                .checked_add(graph.node_count())
                .and_then(|count| {
                    graph
                        .nodes
                        .iter()
                        .try_fold(count, |total, node| total.checked_add(node.links.len()))
                })
                .ok_or_else(|| Error::Config {
                    message: format!("bundle plan exceeds {PLAN_ENTRIES_MAX} closure entries"),
                })?;
            if closure_entries > PLAN_ENTRIES_MAX {
                return Err(Error::Config {
                    message: format!("bundle plan exceeds {PLAN_ENTRIES_MAX} closure entries"),
                });
            }
            resolved.push((resolver, graph));
        }
        let architecture = self.check_architectures(&resolved)?;
        check_closure_collisions(&resolved)?;

        let mut warnings: Vec<Warning> = Vec::new();
        let mut builder = PlanBuilder::new(&self.source_root);
        // The closure is planned first: its objects have to sit exactly where
        // the loader will look, so nothing may displace them.
        builder.acting_as(Authority::Closure);
        for (_, graph) in &resolved {
            self.plan_closure(graph, &mut builder, &mut warnings)?;
        }
        builder.acting_as(Authority::RuntimePolicy);
        self.plan_loader_cache(&resolved, &mut builder, &mut warnings)?;
        self.apply_runtime_policy(&mut builder, &mut warnings)?;
        deduplicate_warnings(&mut warnings);

        let (files, conflicts) = builder.finish();
        if files.len() > PLAN_ENTRIES_MAX {
            return Err(Error::Config {
                message: format!("bundle plan exceeds {PLAN_ENTRIES_MAX} entries"),
            });
        }
        check_destination_conflicts(&conflicts)?;
        check_nesting(&files)?;
        let applications = resolved
            .into_iter()
            .map(|(_, graph)| {
                let destination = &graph.root_node().destination;
                let executable = files
                    .iter()
                    .find(|file| {
                        file.kind == PlannedFileKind::Executable && &file.destination == destination
                    })
                    .cloned()
                    .expect("every graph root has one planned executable");
                ApplicationPlan {
                    executable,
                    interpreter: graph.declared_interpreter.clone(),
                    interpreter_resolved: graph
                        .nodes
                        .iter()
                        .find(|node| node.kind == NodeKind::Interpreter)
                        .map(|node| node.destination.clone()),
                    graph,
                }
            })
            .collect();

        Ok(BundlePlan {
            applications,
            architecture,
            files,
            preset: self.preset,
            runtime_policy: self.runtime_policy.clone(),
            dependency_policy: self.dependency_policy.clone(),
            warnings,
        })
    }

    /// The executable has to land somewhere the rootfs can name.
    fn check_install_path(&self, input: &PlannerInput) -> Result<()> {
        assert!(input.install_path.is_absolute());

        if input.install_path.file_name().is_some() {
            return Ok(());
        }
        Err(Error::Config {
            message: format!(
                "install path `{}` does not name a file",
                input.install_path.display()
            ),
        })
    }

    fn check_architectures(
        &self,
        resolved: &[(Resolver, DependencyGraph)],
    ) -> Result<crate::Architecture> {
        let first = resolved
            .first()
            .expect("a planner always has at least one binary")
            .1
            .root_node()
            .architecture;
        for (_, graph) in &resolved[1..] {
            let architecture = graph.root_node().architecture;
            if architecture != first {
                return Err(Error::Config {
                    message: format!(
                        "executable `{}` has architecture {architecture}, expected {first}",
                        graph.root_node().logical.display()
                    ),
                });
            }
        }
        Ok(first)
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
        resolved: &[(Resolver, DependencyGraph)],
        builder: &mut PlanBuilder<'_>,
        warnings: &mut Vec<Warning>,
    ) -> Result<()> {
        let needs_cache = resolved.iter().any(|(resolver, graph)| {
            !unreachable_libraries(resolver).is_empty() || !relocated_search_paths(graph).is_empty()
        });

        let writing_cache = self.runtime_policy.ld_so_cache.applies(needs_cache);
        if writing_cache {
            warn_ambiguous_sonames(resolved.iter().map(|(_, graph)| graph), warnings);
        }
        let cache = writing_cache
            .then(|| self.ld_so_cache_many(resolved.iter().map(|(_, graph)| graph)))
            .flatten();

        let wrote_cache = if let Some(bytes) = cache {
            builder.push_generated(
                Path::new(LD_SO_CACHE),
                bytes,
                InclusionReason::RuntimePolicy {
                    feature: RuntimeFeature::LdSoCache,
                },
            );
            true
        } else {
            false
        };

        // A generated cache serves glibc applications only. Other loaders still
        // need a warning for paths they cannot reproduce inside the bundle.
        for (resolver, graph) in resolved {
            if wrote_cache && uses_glibc_loader(graph) {
                continue;
            }
            let unreachable = unreachable_libraries(resolver);
            let relocated = relocated_search_paths(graph);
            if !unreachable.is_empty() {
                warnings.push(warn_unreachable(unreachable, uses_glibc_loader(graph)));
            }
            if !relocated.is_empty() {
                warnings.push(warn_relocated(relocated, graph));
            }
        }
        Ok(())
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
                code: code::DLOPEN,
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
    #[cfg(test)]
    fn ld_so_cache(&self, graph: &DependencyGraph) -> Option<Vec<u8>> {
        self.ld_so_cache_many(std::iter::once(graph))
    }

    fn ld_so_cache_many<'a>(
        &self,
        graphs: impl IntoIterator<Item = &'a DependencyGraph>,
    ) -> Option<Vec<u8>> {
        let graphs: Vec<_> = graphs
            .into_iter()
            .filter(|graph| uses_glibc_loader(graph))
            .collect();
        let architecture = graphs.first()?.root_node().architecture;
        let entries: Vec<CacheEntry> = graphs
            .iter()
            .flat_map(|graph| graph.nodes.iter())
            .filter(|node| matches!(node.kind, NodeKind::SharedObject | NodeKind::Interpreter))
            .map(|node| CacheEntry {
                soname: cache_soname(node),
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
    fn check_install_collision(&self, graph: &DependencyGraph, install_path: &Path) -> Result<()> {
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
                    install_path.display(),
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
    fn validate_dependencies(&self, graph: &DependencyGraph, install_path: &Path) -> Result<()> {
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
                .unwrap_or_else(|| install_path.to_path_buf());
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
        builder.acting_as(Authority::IncludedTree);
        for include in &policy.includes {
            self.plan_include(builder, include)?;
        }
        builder.acting_as(Authority::RuntimePolicy);

        if policy.user.is_some() && !policy.passwd_group {
            warnings.push(Warning {
                code: code::USER_WITHOUT_PASSWD_GROUP,
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

/// Report objects the generated `/etc/ld.so.cache` cannot describe unambiguously.
///
/// One cache serves every application in the bundle. If two closures hold
/// different files under the same soname, the cache can name only one, and an
/// application that reaches the cache loads a library it was never analyzed
/// against. This is a warning rather than an error because the cache is the
/// *last* place glibc looks: a closure that finds its own copy through
/// `DT_RPATH` or `DT_RUNPATH` never consults it and is unaffected.
fn warn_ambiguous_sonames<'a>(
    graphs: impl IntoIterator<Item = &'a DependencyGraph>,
    warnings: &mut Vec<Warning>,
) {
    let mut by_soname: BTreeMap<String, Vec<(&Path, &crate::graph::Digest)>> = BTreeMap::new();
    for graph in graphs {
        if !uses_glibc_loader(graph) {
            continue;
        }
        for node in &graph.nodes {
            if !matches!(node.kind, NodeKind::SharedObject | NodeKind::Interpreter) {
                continue;
            }
            let soname = cache_soname(node);
            if soname.is_empty() {
                continue;
            }
            let candidates = by_soname.entry(soname).or_default();
            let candidate = (node.destination.as_path(), &node.sha256);
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }

    let details: Vec<String> = by_soname
        .into_iter()
        .filter(|(_, candidates)| {
            // Identical bytes at two paths are not ambiguous: either answer is
            // the same library.
            candidates
                .iter()
                .any(|(_, digest)| *digest != candidates[0].1)
        })
        .map(|(soname, candidates)| {
            let paths: Vec<String> = candidates
                .iter()
                .map(|(path, _)| path.display().to_string())
                .collect();
            format!("{soname}: {}", paths.join(", "))
        })
        .collect();

    if details.is_empty() {
        return;
    }
    warnings.push(Warning {
        code: code::LOADER_CACHE_AMBIGUOUS,
        message: format!(
            "{} soname(s) name different files; the generated /etc/ld.so.cache lists one of each",
            details.len()
        ),
        details,
    });
}

/// The name a cache entry is looked up by: `DT_SONAME`, or the file name for a
/// library that declares none, which is what its dependents recorded.
///
/// This is the single definition of that rule; [`Planner::ld_so_cache_many`]
/// builds its entries from it, so the warning above and the cache itself can
/// never disagree about what a library is called.
fn cache_soname(node: &Node) -> String {
    match node.soname.as_deref() {
        Some(soname) if !soname.is_empty() => soname.to_string(),
        _ => node
            .destination
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// Reject a destination two non-scaffolding entries wanted incompatibly.
///
/// One case is legitimate and stays silent: runtime policy keeps its place
/// against the same path arriving inside an `--include` tree, which is what
/// lets `--include /etc` coexist with generated files and policy directories.
/// Everything else means the caller asked for two different things in one
/// place, and the bundle can only express one of them.
fn check_destination_conflicts(conflicts: &[Conflict]) -> Result<()> {
    for conflict in conflicts {
        let documented = conflict.kept.1 == Authority::RuntimePolicy
            && conflict.dropped.1 == Authority::IncludedTree;
        if documented {
            continue;
        }
        return Err(Error::Config {
            message: format!(
                "`{}` is planned both as {} and as {}; only one entry can occupy a path",
                conflict.destination.display(),
                conflict.kept.0.as_str(),
                conflict.dropped.0.as_str(),
            ),
        });
    }
    Ok(())
}

/// Reject a plan whose entries nest inside something that is not a directory.
///
/// Directory output would fail part-way through and tar output would silently
/// write through the symlink or file instead, so the two backends would stop
/// describing the same tree. Entries are sorted by destination, so every parent
/// has already been seen by the time its children are.
fn check_nesting(files: &[PlannedFile]) -> Result<()> {
    let mut by_destination: BTreeMap<&Path, PlannedFileKind> = BTreeMap::new();
    for file in files {
        let parent = logical_parent(&file.destination);
        if let Some(&kind) = by_destination.get(parent.as_path())
            && kind != PlannedFileKind::Directory
        {
            return Err(Error::Config {
                message: format!(
                    "`{}` would be created inside `{}`, which is planned as {}",
                    file.destination.display(),
                    parent.display(),
                    kind.as_str(),
                ),
            });
        }
        by_destination.insert(&file.destination, file.kind);
    }
    Ok(())
}

#[derive(Debug)]
enum ClosureEntry {
    Regular {
        digest: String,
        kind: NodeKind,
        source: PathBuf,
    },
    Symlink {
        target: PathBuf,
        source: PathBuf,
    },
}

impl ClosureEntry {
    fn is_compatible_with(&self, other: &ClosureEntry) -> bool {
        match (self, other) {
            (
                ClosureEntry::Regular {
                    digest: left,
                    kind: left_kind,
                    ..
                },
                ClosureEntry::Regular {
                    digest: right,
                    kind: right_kind,
                    ..
                },
            ) => left_kind == right_kind && *left_kind != NodeKind::Executable && left == right,
            (
                ClosureEntry::Symlink { target: left, .. },
                ClosureEntry::Symlink { target: right, .. },
            ) => left == right,
            _ => false,
        }
    }

    fn source(&self) -> &Path {
        match self {
            ClosureEntry::Regular { source, .. } | ClosureEntry::Symlink { source, .. } => source,
        }
    }
}

/// Every application closure shares one output namespace. Identical libraries
/// and links are deduplicated, while executable or content collisions would
/// make at least one application differ from the plan and are rejected.
fn check_closure_collisions(resolved: &[(Resolver, DependencyGraph)]) -> Result<()> {
    let mut entries = BTreeMap::<PathBuf, ClosureEntry>::new();
    for (_, graph) in resolved {
        for (_, node) in graph.iter() {
            insert_closure_entry(
                &mut entries,
                node.destination.clone(),
                ClosureEntry::Regular {
                    digest: node.sha256.0.clone(),
                    kind: node.kind,
                    source: node.logical.clone(),
                },
            )?;
            for link in &node.links {
                insert_closure_entry(
                    &mut entries,
                    link.logical.clone(),
                    ClosureEntry::Symlink {
                        target: link.target.clone(),
                        source: link.logical.clone(),
                    },
                )?;
            }
        }
    }
    Ok(())
}

fn insert_closure_entry(
    entries: &mut BTreeMap<PathBuf, ClosureEntry>,
    destination: PathBuf,
    incoming: ClosureEntry,
) -> Result<()> {
    if let Some(existing) = entries.get(&destination) {
        if existing.is_compatible_with(&incoming) {
            return Ok(());
        }
        return Err(Error::Config {
            message: format!(
                "bundle path `{}` collides between `{}` and `{}`",
                destination.display(),
                existing.source().display(),
                incoming.source().display()
            ),
        });
    }
    entries.insert(destination, incoming);
    Ok(())
}

fn deduplicate_warnings(warnings: &mut Vec<Warning>) {
    let mut seen = BTreeSet::new();
    warnings.retain(|warning| {
        seen.insert((
            warning.code,
            warning.message.clone(),
            warning.details.clone(),
        ))
    });
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
        code: code::LIBRARY_UNREACHABLE,
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
        code: code::EXECUTABLE_RELOCATED,
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
        code: code::DLOPEN,
        message: format!("{} references dlopen()", node.destination.display()),
        details: vec![
            "Runtime-loaded libraries cannot be determined using static ELF dependency analysis."
                .to_string(),
            "Consider adding them with --include.".to_string(),
        ],
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
        elf::{Architecture, ElfClass, Endianness, Machine},
        graph::Node,
        hash::sha256_bytes,
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
    fn closure_entries_require_matching_regular_file_kinds() {
        let digest = sha256_bytes(b"same bytes").0;
        let mut entries = BTreeMap::new();
        insert_closure_entry(
            &mut entries,
            PathBuf::from("/lib/same.so"),
            ClosureEntry::Regular {
                digest: digest.clone(),
                kind: NodeKind::Interpreter,
                source: PathBuf::from("/lib/ld.so"),
            },
        )
        .unwrap();

        let error = insert_closure_entry(
            &mut entries,
            PathBuf::from("/lib/same.so"),
            ClosureEntry::Regular {
                digest,
                kind: NodeKind::SharedObject,
                source: PathBuf::from("/lib/libsame.so"),
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("/lib/same.so"), "{error}");
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
