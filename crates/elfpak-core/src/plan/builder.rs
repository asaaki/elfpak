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

/// Accumulates plan entries, deduplicating destinations and creating the
/// directory scaffolding each entry needs.
#[derive(Debug)]
pub(super) struct PlanBuilder<'a> {
    root: &'a SourceRoot,
    entries: BTreeMap<PathBuf, PlannedFile>,
    digests: DigestCache,
}

impl<'a> PlanBuilder<'a> {
    pub(super) fn new(root: &'a SourceRoot) -> PlanBuilder<'a> {
        PlanBuilder {
            root,
            entries: BTreeMap::new(),
            digests: DigestCache::new(),
        }
    }

    /// Add an entry, settling a destination that two of them want.
    ///
    /// Directories never displace real content, and real content always
    /// displaces a directory that was only scaffolding. Between two entries
    /// that both carry content the first one planned wins, and the phases run
    /// in the order their paths are fixed: the ELF closure, whose objects must
    /// sit where the loader will look, then runtime policy, then `--include`.
    /// A generated `/etc/passwd` therefore keeps its place against an
    /// `--include` of the source root's `/etc`.
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
    /// precede everything inside it when the plan is written out.
    pub(super) fn finish(self) -> Vec<PlannedFile> {
        self.entries.into_values().collect()
    }
}
