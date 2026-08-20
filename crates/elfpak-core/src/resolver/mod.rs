//! The dynamic linker resolver.
//!
//! This models the glibc loader's search algorithm rather than looking for
//! matching filenames. The target binary is never executed and `ldd` is never
//! called.

pub mod cache;
pub mod search;
pub mod tokens;

use std::path::{Path, PathBuf};

use crate::elf::{Architecture, ElfMetadata, ObjectType};
use crate::error::{Error, Result};
use crate::graph::{DependencyGraph, DependencyReason, Node, NodeId, NodeKind};
use crate::hash::DigestCache;
use crate::paths::{logical_parent, normalize_absolute};
use crate::source::{ElfCache, EntryKind, Resolved, SourceRoot};

pub use cache::LdCache;
pub use tokens::TokenContext;

/// A single `DT_NEEDED` lookup, with all loader state it depends on.
#[derive(Debug, Clone)]
pub struct LibraryRequest {
    pub soname: String,
    /// Logical path of the object that needs the library.
    pub requester: PathBuf,
    /// `DT_RPATH` lists of the requester and its loaders, nearest first.
    /// Entries whose object declared `DT_RUNPATH` are already filtered out.
    pub rpath_chain: Vec<Vec<String>>,
    /// `DT_RUNPATH` of the requester (never inherited).
    pub runpath: Vec<String>,
    pub nodeflib: bool,
    pub architecture: Architecture,
}

#[derive(Debug, Clone)]
pub struct ResolvedLibrary {
    pub resolved: Resolved,
    pub metadata: ElfMetadata,
}

/// Where a lookup succeeded. Only some of these survive into the bundle: the
/// generated rootfs has no `ld.so.cache`, and `--library-path` is a hint to
/// `elfpak`, not something the packaged application inherits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchOrigin {
    /// `DT_RPATH`/`DT_RUNPATH` of the requesting object, or an absolute soname.
    ObjectPath,
    /// `--library-path`, the `LD_LIBRARY_PATH` equivalent.
    LibraryPath,
    /// `/etc/ld.so.cache`.
    Cache,
    /// A directory the loader searches without being told to.
    DefaultDirectory,
    /// A directory that only `/etc/ld.so.conf` named.
    ConfiguredDirectory,
}

/// A library the loader inside the bundle would not find on its own.
///
/// `elfpak` never runs `ldconfig`, so a bundle carries no `ld.so.cache`; a
/// library that was only reachable through the build host's cache, its
/// `ld.so.conf` or `--library-path` keeps its original path but nothing points
/// the loader at it any more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionNote {
    pub soname: String,
    pub directory: PathBuf,
    pub origin: SearchOrigin,
}

impl SearchOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            SearchOrigin::ObjectPath => "DT_RPATH/DT_RUNPATH",
            SearchOrigin::LibraryPath => "--library-path",
            SearchOrigin::Cache => "/etc/ld.so.cache",
            SearchOrigin::DefaultDirectory => "a default directory",
            SearchOrigin::ConfiguredDirectory => "/etc/ld.so.conf",
        }
    }

    /// Whether the packaged application can still find a library that was
    /// located this way.
    fn survives_packaging(&self) -> bool {
        matches!(
            self,
            SearchOrigin::ObjectPath | SearchOrigin::DefaultDirectory
        )
    }
}

/// Loader-specific resolution, kept behind a trait so the implementation can be
/// replaced (or wrapped for tracing) without touching the planner.
pub trait DynamicLinkerResolver {
    fn resolve(&mut self, request: &LibraryRequest) -> Result<ResolvedLibrary>;
}

pub struct Resolver {
    root: SourceRoot,
    /// Explicit search paths, equivalent to `LD_LIBRARY_PATH`.
    library_paths: Vec<PathBuf>,
    /// Paths configured through `/etc/ld.so.conf`.
    conf_paths: Vec<PathBuf>,
    cache: Option<LdCache>,
    elf: ElfCache,
    digests: DigestCache,
    notes: Vec<ResolutionNote>,
}

impl Resolver {
    pub fn new(root: SourceRoot) -> Resolver {
        let cache = root
            .resolve(Path::new("/etc/ld.so.cache"))
            .ok()
            .flatten()
            .filter(|r| r.kind == EntryKind::File)
            .and_then(|r| LdCache::load(&r.host));
        let conf_paths = search::parse_ld_so_conf(&root);
        Resolver {
            root,
            library_paths: Vec::new(),
            conf_paths,
            cache,
            elf: ElfCache::new(),
            digests: DigestCache::new(),
            notes: Vec::new(),
        }
    }

    pub fn with_library_paths(mut self, paths: Vec<PathBuf>) -> Resolver {
        self.library_paths = paths.iter().map(|p| normalize_absolute(p)).collect();
        self
    }

    pub fn root(&self) -> &SourceRoot {
        &self.root
    }

    pub fn ld_cache(&self) -> Option<&LdCache> {
        self.cache.as_ref()
    }

    /// Libraries that resolved through something the bundle does not reproduce.
    pub fn notes(&self) -> &[ResolutionNote] {
        &self.notes
    }

    fn note(&mut self, request: &LibraryRequest, directory: &Path, origin: SearchOrigin) {
        if origin.survives_packaging()
            || search::default_library_paths(&request.architecture)
                .iter()
                .any(|default| default == directory)
        {
            return;
        }
        let note = ResolutionNote {
            soname: request.soname.clone(),
            directory: directory.to_path_buf(),
            origin,
        };
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }

    /// Map a host path to its logical path inside the source root.
    pub fn logical_of_host(&self, host: &Path) -> PathBuf {
        let host = std::path::absolute(host).unwrap_or_else(|_| host.to_path_buf());
        let root = self.root.path();
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let host = host.canonicalize().unwrap_or(host);
        match host.strip_prefix(&root) {
            Ok(rest) => normalize_absolute(&Path::new("/").join(rest)),
            Err(_) => normalize_absolute(&host),
        }
    }

    /// Build the full runtime closure of an executable.
    ///
    /// `binary` is a host path; `install` is where the executable will live in
    /// the generated rootfs. Every other object keeps its original location.
    pub fn closure(&mut self, binary: &Path, install: &Path) -> Result<DependencyGraph> {
        let metadata = ElfMetadata::parse_file(binary)?;
        if !metadata.architecture.machine.is_supported_target() {
            return Err(Error::UnsupportedArchitecture {
                path: binary.to_path_buf(),
                architecture: metadata.architecture.to_string(),
                machine: metadata.e_machine,
            });
        }
        let architecture = metadata.architecture;
        let logical = self.logical_of_host(binary);
        let mut graph = DependencyGraph::new();
        graph.declared_interpreter = metadata
            .interpreter
            .as_deref()
            .map(crate::paths::normalize_absolute);
        graph.executable_search_paths = metadata
            .rpath
            .iter()
            .chain(metadata.runpath.iter())
            .cloned()
            .collect();

        let (digest, size) = self.digests.get(binary)?;
        let root_id = graph.insert(Node {
            source: binary.to_path_buf(),
            logical: logical.clone(),
            destination: normalize_absolute(install),
            kind: NodeKind::Executable,
            soname: metadata.soname.clone(),
            architecture,
            sha256: digest,
            size,
            links: Vec::new(),
            dlopen_references: metadata.dlopen_references.clone(),
        });
        graph.root = root_id;

        // PT_INTERP: the loader is a hard runtime dependency of the image.
        if let Some(interp) = &metadata.interpreter {
            let resolved = self
                .root
                .resolve(interp)?
                .filter(|r| r.kind == EntryKind::File);
            match resolved {
                Some(resolved) => {
                    let interp_meta = self.elf.require(&resolved.host)?;
                    self.check_architecture(&interp_meta, &architecture, interp, &resolved)?;
                    let id = self.insert_object(
                        &mut graph,
                        &resolved,
                        &interp_meta,
                        NodeKind::Interpreter,
                    )?;
                    graph.connect(root_id, id, DependencyReason::Interpreter);
                }
                None => {
                    return Err(Error::UnresolvedLibrary {
                        soname: interp.to_string_lossy().into_owned(),
                        required_by: logical,
                        searched: vec![self.root.host_path(interp)],
                    });
                }
            }
        }

        self.walk_needed(&mut graph, root_id, metadata, Vec::new())?;

        Ok(graph)
    }

    /// Add everything reachable from `start` through `DT_NEEDED`, depth first.
    ///
    /// `inherited` is the `DT_RPATH` chain of the objects that loaded `start`,
    /// nearest first; the loader consults it for every lookup further down the
    /// chain, which is the whole difference between `DT_RPATH` and `DT_RUNPATH`.
    fn walk_needed(
        &mut self,
        graph: &mut DependencyGraph,
        start: NodeId,
        metadata: ElfMetadata,
        inherited: Vec<Vec<String>>,
    ) -> Result<()> {
        let architecture = metadata.architecture;
        let mut queue = vec![(start, metadata, inherited)];
        while let Some((id, meta, inherited)) = queue.pop() {
            let mut chain = Vec::new();
            if !meta.runpath_is_authoritative() && !meta.rpath.is_empty() {
                chain.push(meta.rpath.clone());
            }
            chain.extend(inherited);

            let requester = graph.node(id).logical.clone();
            for soname in &meta.needed {
                let request = LibraryRequest {
                    soname: soname.clone(),
                    requester: requester.clone(),
                    rpath_chain: chain.clone(),
                    runpath: meta.runpath.clone(),
                    nodeflib: meta.nodeflib,
                    architecture,
                };
                let library = self.resolve(&request)?;
                let known = graph.find(&library.resolved.logical);
                let child = self.insert_object(
                    graph,
                    &library.resolved,
                    &library.metadata,
                    NodeKind::SharedObject,
                )?;
                graph.connect(
                    id,
                    child,
                    DependencyReason::Needed {
                        soname: soname.clone(),
                    },
                );
                if known.is_none() {
                    queue.push((child, library.metadata, chain.clone()));
                }
            }
        }
        Ok(())
    }

    /// Resolve a soname that is already known to be a library the policy wants,
    /// e.g. NSS modules pulled in by runtime policy rather than by `DT_NEEDED`.
    pub fn resolve_extra_library(
        &mut self,
        soname: &str,
        architecture: Architecture,
        requester: &Path,
    ) -> Result<Option<ResolvedLibrary>> {
        let request = LibraryRequest {
            soname: soname.to_string(),
            requester: requester.to_path_buf(),
            rpath_chain: Vec::new(),
            runpath: Vec::new(),
            nodeflib: false,
            architecture,
        };
        match self.resolve(&request) {
            Ok(library) => Ok(Some(library)),
            Err(Error::UnresolvedLibrary { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Add an already-resolved object (and its own `DT_NEEDED` closure) to a graph.
    pub fn attach_library(
        &mut self,
        graph: &mut DependencyGraph,
        library: &ResolvedLibrary,
        from: NodeId,
        reason: DependencyReason,
    ) -> Result<NodeId> {
        let existing = graph.find(&library.resolved.logical);
        let id = self.insert_object(
            graph,
            &library.resolved,
            &library.metadata,
            NodeKind::SharedObject,
        )?;
        graph.connect(from, id, reason);
        if existing.is_none() {
            // A policy-loaded module is opened by `dlopen` from libc, not by the
            // application, so it inherits no RPATH from the loading chain.
            self.walk_needed(graph, id, library.metadata.clone(), Vec::new())?;
        }
        Ok(id)
    }

    fn insert_object(
        &mut self,
        graph: &mut DependencyGraph,
        resolved: &Resolved,
        metadata: &ElfMetadata,
        kind: NodeKind,
    ) -> Result<NodeId> {
        let (digest, size) = self.digests.get(&resolved.host)?;
        Ok(graph.insert(Node {
            source: resolved.host.clone(),
            logical: resolved.logical.clone(),
            destination: resolved.logical.clone(),
            kind,
            soname: metadata.soname.clone(),
            architecture: metadata.architecture,
            sha256: digest,
            size,
            links: resolved.links.clone(),
            dlopen_references: metadata.dlopen_references.clone(),
        }))
    }

    fn check_architecture(
        &self,
        metadata: &ElfMetadata,
        expected: &Architecture,
        soname: &Path,
        resolved: &Resolved,
    ) -> Result<()> {
        if metadata.architecture.is_compatible_with(expected) {
            return Ok(());
        }
        Err(Error::IncompatibleArchitecture {
            soname: soname.to_string_lossy().into_owned(),
            expected: expected.to_string(),
            found: resolved.logical.clone(),
            found_architecture: metadata.architecture.to_string(),
        })
    }

    fn token_context(&self, requester: &Path, architecture: &Architecture) -> TokenContext {
        TokenContext {
            origin: logical_parent(requester),
            lib: architecture.lib_token().to_string(),
            platform: architecture.machine.platform_token().map(str::to_string),
        }
    }

    /// Candidate directories in glibc's documented order, each tagged with
    /// where it came from so the planner can tell what survives packaging.
    fn search_directories(&self, request: &LibraryRequest) -> Vec<(PathBuf, SearchOrigin)> {
        let ctx = self.token_context(&request.requester, &request.architecture);
        let mut dirs: Vec<(PathBuf, SearchOrigin)> = Vec::new();
        let push = |dirs: &mut Vec<(PathBuf, SearchOrigin)>, dir: PathBuf, origin| {
            if !dirs.iter().any(|(known, _)| known == &dir) {
                dirs.push((dir, origin));
            }
        };

        // 1. DT_RPATH of the object and, transitively, of its loaders.
        for level in &request.rpath_chain {
            for entry in level {
                push(
                    &mut dirs,
                    tokens::expand_search_path(entry, &ctx),
                    SearchOrigin::ObjectPath,
                );
            }
        }
        // 2. LD_LIBRARY_PATH equivalent.
        for dir in &self.library_paths {
            push(&mut dirs, dir.clone(), SearchOrigin::LibraryPath);
        }
        // 3. DT_RUNPATH of the requesting object only.
        for entry in &request.runpath {
            push(
                &mut dirs,
                tokens::expand_search_path(entry, &ctx),
                SearchOrigin::ObjectPath,
            );
        }
        dirs
    }

    fn default_directories(&self, architecture: &Architecture) -> Vec<(PathBuf, SearchOrigin)> {
        let mut dirs: Vec<(PathBuf, SearchOrigin)> = Vec::new();
        let configured = self
            .conf_paths
            .iter()
            .cloned()
            .map(|dir| (dir, SearchOrigin::ConfiguredDirectory));
        let builtin = search::default_library_paths(architecture)
            .into_iter()
            .map(|dir| (dir, SearchOrigin::DefaultDirectory));
        for (dir, origin) in configured.chain(builtin) {
            if !dirs.iter().any(|(known, _)| known == &dir) {
                dirs.push((dir, origin));
            }
        }
        dirs
    }

    /// glibc-hwcaps subdirectories, highest priority first.
    fn hwcaps_subdirs(architecture: &Architecture) -> &'static [&'static str] {
        match architecture.machine {
            crate::elf::Machine::X86_64 => &["x86-64-v4", "x86-64-v3", "x86-64-v2"],
            _ => &[],
        }
    }

    fn try_directory(
        &mut self,
        dir: &Path,
        request: &LibraryRequest,
        searched: &mut Vec<PathBuf>,
        mismatch: &mut Option<(PathBuf, Architecture)>,
    ) -> Result<Option<ResolvedLibrary>> {
        for hwcap in Self::hwcaps_subdirs(&request.architecture) {
            let hwcap_dir = dir.join("glibc-hwcaps").join(hwcap);
            if let Some(found) = self.try_path(
                &hwcap_dir.join(&request.soname),
                request,
                searched,
                mismatch,
            )? {
                return Ok(Some(found));
            }
        }
        self.try_path(&dir.join(&request.soname), request, searched, mismatch)
    }

    fn try_path(
        &mut self,
        logical: &Path,
        request: &LibraryRequest,
        searched: &mut Vec<PathBuf>,
        mismatch: &mut Option<(PathBuf, Architecture)>,
    ) -> Result<Option<ResolvedLibrary>> {
        let dir = logical_parent(logical);
        if !searched.contains(&dir) {
            searched.push(dir);
        }
        let Some(resolved) = self.root.resolve(logical)? else {
            return Ok(None);
        };
        if resolved.kind != EntryKind::File {
            return Ok(None);
        }
        let Some(metadata) = self.elf.get(&resolved.host)? else {
            return Ok(None);
        };
        if !matches!(
            metadata.object_type,
            ObjectType::SharedObject | ObjectType::Executable
        ) {
            return Ok(None);
        }
        if !metadata
            .architecture
            .is_compatible_with(&request.architecture)
        {
            if mismatch.is_none() {
                *mismatch = Some((resolved.logical.clone(), metadata.architecture));
            }
            return Ok(None);
        }
        Ok(Some(ResolvedLibrary { resolved, metadata }))
    }
}

impl DynamicLinkerResolver for Resolver {
    fn resolve(&mut self, request: &LibraryRequest) -> Result<ResolvedLibrary> {
        let mut searched = Vec::new();
        let mut mismatch = None;

        // A soname containing a slash is a path, not a search request.
        if request.soname.contains('/') {
            let ctx = self.token_context(&request.requester, &request.architecture);
            let path = tokens::expand_search_path(&request.soname, &ctx);
            if let Some(found) = self.try_path(&path, request, &mut searched, &mut mismatch)? {
                return Ok(found);
            }
        } else {
            for (dir, origin) in self.search_directories(request) {
                if let Some(found) =
                    self.try_directory(&dir, request, &mut searched, &mut mismatch)?
                {
                    self.note(request, &dir, origin);
                    return Ok(found);
                }
            }

            // 4. /etc/ld.so.cache
            let cached: Vec<PathBuf> = self
                .cache
                .as_ref()
                .map(|c| c.lookup(&request.soname).to_vec())
                .unwrap_or_default();
            for candidate in cached {
                if let Some(found) =
                    self.try_path(&candidate, request, &mut searched, &mut mismatch)?
                {
                    self.note(request, &logical_parent(&candidate), SearchOrigin::Cache);
                    return Ok(found);
                }
            }

            // 5. Default directories, unless the object opted out.
            if !request.nodeflib {
                for (dir, origin) in self.default_directories(&request.architecture) {
                    if let Some(found) =
                        self.try_directory(&dir, request, &mut searched, &mut mismatch)?
                    {
                        self.note(request, &dir, origin);
                        return Ok(found);
                    }
                }
            }
        }

        if let Some((found, architecture)) = mismatch {
            return Err(Error::IncompatibleArchitecture {
                soname: request.soname.clone(),
                expected: request.architecture.to_string(),
                found,
                found_architecture: architecture.to_string(),
            });
        }

        Err(Error::UnresolvedLibrary {
            soname: request.soname.clone(),
            required_by: request.requester.clone(),
            searched,
        })
    }
}
