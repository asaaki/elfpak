//! The source filesystem, abstracted behind `--root`.
//!
//! The source root is treated as strictly read-only, and as the logical `/` of
//! the target system. Symlinks are followed *logically* (inside the root) so a
//! sysroot can be analyzed without any chance of escaping to the host.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use crate::elf::ElfMetadata;
use crate::error::{Error, Result, io};
use crate::paths::normalize_absolute;

const SYMLINK_BUDGET: usize = 40;

/// A symlink observed while resolving a logical path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymlinkEntry {
    /// Logical location of the link itself, e.g. `/lib/x86_64-linux-gnu/libfoo.so.1`.
    pub logical: PathBuf,
    /// Raw link target, verbatim, so the relationship is preserved on output.
    pub target: PathBuf,
}

/// A logical path resolved to a real file inside the source root.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// Logical path after following symlinks, e.g. `/usr/lib/.../libfoo.so.1.4.2`.
    pub logical: PathBuf,
    /// Host path of that file (source root prepended).
    pub host: PathBuf,
    /// Symlinks traversed on the way, in traversal order.
    pub links: Vec<SymlinkEntry>,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Other,
}

#[derive(Debug, Clone)]
pub struct SourceRoot {
    path: PathBuf,
}

impl SourceRoot {
    pub fn new(path: impl Into<PathBuf>) -> SourceRoot {
        SourceRoot { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Map a logical path onto the host without following symlinks.
    pub fn host_path(&self, logical: &Path) -> PathBuf {
        crate::paths::join_under(&self.path, logical)
    }

    /// Resolve a logical path, following symlinks within the root.
    ///
    /// Returns `Ok(None)` when the path does not exist. Symlinks are recorded so
    /// that the bundle can reproduce the original link structure.
    pub fn resolve(&self, logical: &Path) -> Result<Option<Resolved>> {
        let normalized = normalize_absolute(logical);
        let mut pending: Vec<std::ffi::OsString> = normalized
            .components()
            .filter_map(|c| match c {
                Component::Normal(p) => Some(p.to_os_string()),
                _ => None,
            })
            .rev()
            .collect();

        let mut current = PathBuf::from("/");
        let mut links = Vec::new();
        let mut budget = SYMLINK_BUDGET;

        while let Some(component) = pending.pop() {
            if component == ".." {
                current.pop();
                continue;
            }
            if component == "." {
                continue;
            }
            let next_logical = current.join(&component);
            let host = self.host_path(&next_logical);
            let metadata = match std::fs::symlink_metadata(&host) {
                Ok(metadata) => metadata,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(io(&host, e)),
            };

            if metadata.is_symlink() {
                if budget == 0 {
                    return Err(Error::SymlinkLoop {
                        path: logical.to_path_buf(),
                    });
                }
                budget -= 1;
                let target = std::fs::read_link(&host).map_err(|e| io(&host, e))?;
                links.push(SymlinkEntry {
                    logical: next_logical,
                    target: target.clone(),
                });
                if target.is_absolute() {
                    current = PathBuf::from("/");
                }
                for part in target
                    .components()
                    .filter_map(|c| match c {
                        Component::Normal(p) => Some(p.to_os_string()),
                        Component::ParentDir => Some(std::ffi::OsString::from("..")),
                        _ => None,
                    })
                    .rev()
                {
                    pending.push(part);
                }
                continue;
            }

            current = next_logical;
        }

        let host = self.host_path(&current);
        let metadata = match std::fs::metadata(&host) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io(&host, e)),
        };
        let kind = if metadata.is_dir() {
            EntryKind::Directory
        } else if metadata.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };

        Ok(Some(Resolved {
            logical: current,
            host,
            links,
            kind,
        }))
    }

    /// Read a file identified by a logical path.
    pub fn read(&self, logical: &Path) -> Result<Option<Vec<u8>>> {
        match self.resolve(logical)? {
            Some(resolved) if resolved.kind == EntryKind::File => Ok(Some(
                std::fs::read(&resolved.host).map_err(|e| io(&resolved.host, e))?,
            )),
            _ => Ok(None),
        }
    }

    pub fn exists(&self, logical: &Path) -> bool {
        matches!(self.resolve(logical), Ok(Some(_)))
    }

    pub fn is_dir(&self, logical: &Path) -> bool {
        matches!(self.resolve(logical), Ok(Some(r)) if r.kind == EntryKind::Directory)
    }

    /// Directory entries (names only), sorted for deterministic output.
    pub fn read_dir(&self, logical: &Path) -> Result<Vec<std::ffi::OsString>> {
        let host = match self.resolve(logical)? {
            Some(resolved) if resolved.kind == EntryKind::Directory => resolved.host,
            _ => return Ok(Vec::new()),
        };
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&host).map_err(|e| io(&host, e))? {
            let entry = entry.map_err(|e| io(&host, e))?;
            names.push(entry.file_name());
        }
        names.sort();
        Ok(names)
    }
}

/// Parses each ELF object at most once, keyed by host path.
#[derive(Debug, Default)]
pub struct ElfCache {
    entries: HashMap<PathBuf, Option<ElfMetadata>>,
}

impl ElfCache {
    pub fn new() -> ElfCache {
        ElfCache::default()
    }

    /// Parse `host` as ELF. `Ok(None)` means "exists but is not a usable ELF
    /// object", which is a normal outcome when probing search directories.
    pub fn get(&mut self, host: &Path) -> Result<Option<ElfMetadata>> {
        if let Some(cached) = self.entries.get(host) {
            return Ok(cached.clone());
        }
        let parsed = match ElfMetadata::parse_file(host) {
            Ok(metadata) => Some(metadata),
            Err(Error::NotElf { .. }) | Err(Error::Elf { .. }) => None,
            Err(e) => return Err(e),
        };
        self.entries.insert(host.to_path_buf(), parsed.clone());
        Ok(parsed)
    }

    /// Like [`ElfCache::get`], but a parse failure is an error rather than `None`.
    pub fn require(&mut self, host: &Path) -> Result<ElfMetadata> {
        match self.get(host)? {
            Some(metadata) => Ok(metadata),
            None => ElfMetadata::parse_file(host),
        }
    }
}
