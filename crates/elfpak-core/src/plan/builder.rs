//! Plan-entry construction, destination precedence, and source-tree copying.

use super::{InclusionReason, PLAN_ENTRIES_MAX, PlannedFile, PlannedFileKind, mode_of};
use crate::{
    error::{Error, Result, io},
    hash::{DigestCache, sha256_bytes},
    paths::ancestor_dirs,
    source::{EntryKind, SourceRoot},
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Who asked for a destination, which is what settles a contest between two
/// entries that both want it.
///
/// The order is the order of authority, weakest first. Scaffolding exists only
/// to hold something else, an `--include` tree is a bulk request, runtime
/// policy names its files one at a time, and the closure's objects must sit
/// exactly where the loader will look for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Authority {
    Scaffolding,
    IncludedTree,
    RuntimePolicy,
    Closure,
}

/// Two entries that both carry content wanted the same destination.
#[derive(Debug, Clone)]
pub(super) struct Conflict {
    pub(super) destination: PathBuf,
    pub(super) kept: (PlannedFileKind, Authority),
    pub(super) dropped: (PlannedFileKind, Authority),
}

/// Accumulates plan entries, deduplicating destinations and creating the
/// directory scaffolding each entry needs.
#[derive(Debug)]
pub(super) struct PlanBuilder<'a> {
    root: &'a SourceRoot,
    entries: BTreeMap<PathBuf, (PlannedFile, Authority)>,
    digests: DigestCache,
    /// Destinations two content entries wanted, in insertion order. The planner
    /// decides which of these are legitimate precedence and which are errors.
    conflicts: Vec<Conflict>,
    /// Authority applied to entries pushed from here on, so that each planning
    /// phase does not have to pass it to every call.
    authority: Authority,
}

impl<'a> PlanBuilder<'a> {
    pub(super) fn new(root: &'a SourceRoot) -> PlanBuilder<'a> {
        PlanBuilder {
            root,
            entries: BTreeMap::new(),
            digests: DigestCache::new(),
            conflicts: Vec::new(),
            authority: Authority::Closure,
        }
    }

    /// Set the authority the next planning phase's entries carry.
    pub(super) fn acting_as(&mut self, authority: Authority) {
        self.authority = authority;
    }

    /// Add an entry, settling a destination that two of them want.
    ///
    /// Content always displaces a directory, because a directory is either
    /// scaffolding or an empty shell and the entry with bytes is the one the
    /// caller asked for. Otherwise the stronger [`Authority`] wins, and between
    /// equals the entry planned first keeps its place.
    ///
    /// Whenever two entries that both carry content want one destination, the
    /// loser is recorded as a [`Conflict`] whichever way it went. The planner
    /// decides which of those are legitimate precedence and which mean the
    /// bundle cannot express what was asked for.
    fn insert(&mut self, file: PlannedFile) {
        self.insert_with(file, self.authority);
    }

    fn insert_with(&mut self, file: PlannedFile, authority: Authority) {
        file.assert_well_formed();

        let Some((existing, existing_authority)) = self.entries.get(&file.destination) else {
            self.entries
                .insert(file.destination.clone(), (file, authority));
            return;
        };

        let existing_is_dir = existing.kind == PlannedFileKind::Directory;
        let candidate_is_dir = file.kind == PlannedFileKind::Directory;
        let wins = match (existing_is_dir, candidate_is_dir) {
            (true, false) => true,
            (false, true) => false,
            _ => authority > *existing_authority,
        };

        if !existing_is_dir && !candidate_is_dir && !describes_the_same_entry(existing, &file) {
            let (kept, dropped) = if wins {
                ((file.kind, authority), (existing.kind, *existing_authority))
            } else {
                ((existing.kind, *existing_authority), (file.kind, authority))
            };
            self.conflicts.push(Conflict {
                destination: file.destination.clone(),
                kept,
                dropped,
            });
        }

        if wins {
            self.entries
                .insert(file.destination.clone(), (file, authority));
        }
    }

    /// Directory scaffolding for an entry. Parents are planned shallowest
    /// first, which is the order they have to be created in.
    fn push_parents(&mut self, path: &Path, reason: &InclusionReason) {
        assert!(path.is_absolute());

        for dir in ancestor_dirs(path) {
            self.insert_with(
                PlannedFile {
                    source: None,
                    destination: dir,
                    kind: PlannedFileKind::Directory,
                    reason: reason.clone(),
                    mode: 0o755,
                    size: 0,
                    sha256: None,
                    link_target: None,
                    content: None,
                },
                Authority::Scaffolding,
            );
        }
    }

    pub(super) fn push_file(&mut self, file: PlannedFile) {
        self.push_parents(&file.destination, &file.reason);
        self.insert(file);
    }

    pub(super) fn push_symlink(&mut self, logical: &Path, target: &Path, reason: InclusionReason) {
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

    pub(super) fn push_dir_with_mode(&mut self, path: &Path, mode: u32, reason: InclusionReason) {
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
    pub(super) fn push_generated(
        &mut self,
        path: &Path,
        content: Vec<u8>,
        reason: InclusionReason,
    ) {
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
    pub(super) fn copy_path(
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
            EntryKind::Directory | EntryKind::Other => Ok(false),
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
    /// precede everything inside it when the plan is written out, together with
    /// the destinations two content entries contested.
    pub(super) fn finish(self) -> (Vec<PlannedFile>, Vec<Conflict>) {
        let entries = self.entries.into_values().map(|(file, _)| file).collect();
        (entries, self.conflicts)
    }
}

/// Whether re-planning `candidate` over `existing` would change nothing.
///
/// What lands on disk is the mode, the bytes and, for a link, its target.
/// `kind` and `reason` say why an entry is in the bundle, not what it is, and
/// one file legitimately arrives under two of them: `/etc/localtime` resolves
/// into the zone database `--tzdata` already copied, and an `--include` of a
/// library directory covers objects the closure also needs. Those are the same
/// file planned twice, not a contest over a destination.
fn describes_the_same_entry(existing: &PlannedFile, candidate: &PlannedFile) -> bool {
    existing.mode == candidate.mode
        && existing.size == candidate.size
        && existing.sha256 == candidate.sha256
        && existing.link_target == candidate.link_target
}
