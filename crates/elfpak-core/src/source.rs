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

/// How many symlinks may be traversed while resolving one logical path.
///
/// glibc's own limit is `SYMLOOP_MAX` (40 on Linux); matching it means a path
/// that resolves here is a path the loader would also resolve.
const SYMLINK_HOPS_MAX: usize = 40;

/// Upper bound on the components still waiting to be walked. Each symlink hop
/// can push the components of its target, so a pathological sysroot could grow
/// this list without ever repeating a link; the bound turns that into an error
/// rather than into memory growth.
const PENDING_COMPONENTS_MAX: usize = 1024;

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
        let host = crate::paths::join_under(&self.path, logical);
        assert!(
            host.starts_with(&self.path),
            "the source root is never left"
        );
        host
    }

    /// Resolve a logical path, following symlinks within the root.
    ///
    /// Returns `Ok(None)` when the path does not exist. Symlinks are recorded so
    /// that the bundle can reproduce the original link structure. All control
    /// flow lives here; the helpers below only compute.
    pub fn resolve(&self, logical: &Path) -> Result<Option<Resolved>> {
        let mut pending = components_reversed(&normalize_absolute(logical));
        let mut current = PathBuf::from("/");
        let mut links: Vec<SymlinkEntry> = Vec::new();
        let mut hops = 0usize;

        while let Some(component) = pending.pop() {
            assert!(current.is_absolute());
            assert!(hops <= SYMLINK_HOPS_MAX);

            if component == ".." {
                current.pop();
                continue;
            }
            if component == "." {
                continue;
            }

            let next_logical = current.join(&component);
            let host = self.host_path(&next_logical);
            let Some(metadata) = symlink_metadata_optional(&host)? else {
                return Ok(None);
            };
            if !metadata.is_symlink() {
                current = next_logical;
                continue;
            }

            // A link is a hop: bounded above, and each hop makes progress by
            // consuming one component, so the walk always terminates.
            if hops == SYMLINK_HOPS_MAX || pending.len() > PENDING_COMPONENTS_MAX {
                return Err(Error::SymlinkLoop {
                    path: logical.to_path_buf(),
                });
            }
            hops += 1;

            let target = std::fs::read_link(&host).map_err(|e| io(&host, e))?;
            links.push(SymlinkEntry {
                logical: next_logical,
                target: target.clone(),
            });
            if target.is_absolute() {
                current = PathBuf::from("/");
            }
            pending.extend(components_reversed(&target));
        }

        assert!(links.len() <= SYMLINK_HOPS_MAX);
        self.describe(current, links)
    }

    /// Stat the destination a walk arrived at, without following any further.
    fn describe(&self, logical: PathBuf, links: Vec<SymlinkEntry>) -> Result<Option<Resolved>> {
        assert!(logical.is_absolute());

        let host = self.host_path(&logical);
        let Some(metadata) = metadata_optional(&host)? else {
            return Ok(None);
        };
        let kind = if metadata.is_dir() {
            EntryKind::Directory
        } else if metadata.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        Ok(Some(Resolved {
            logical,
            host,
            links,
            kind,
        }))
    }

    /// Read a file identified by a logical path.
    pub fn read(&self, logical: &Path) -> Result<Option<Vec<u8>>> {
        assert!(logical.is_absolute());

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
        // Directory order is whatever the filesystem feels like; sorted order
        // is what makes two runs on the same tree produce the same bundle.
        names.sort();
        assert!(names.is_sorted());
        Ok(names)
    }
}

/// Path components in pop order, i.e. reversed, with `..` kept as a component
/// so that the walk resolves it against what it has already traversed.
fn components_reversed(path: &Path) -> Vec<std::ffi::OsString> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_os_string()),
            Component::ParentDir => Some(std::ffi::OsString::from("..")),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
        .rev()
        .collect()
}

/// `None` means "not there", which is an ordinary outcome when probing search
/// directories; any other `io::Error` is a real failure and is propagated.
fn symlink_metadata_optional(host: &Path) -> Result<Option<std::fs::Metadata>> {
    match std::fs::symlink_metadata(host) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(host, e)),
    }
}

/// As [`symlink_metadata_optional`], but following a final symlink.
fn metadata_optional(host: &Path) -> Result<Option<std::fs::Metadata>> {
    match std::fs::metadata(host) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(host, e)),
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
            assert!(cached.as_ref().is_none_or(|m| m.path == host));
            return Ok(cached.clone());
        }
        let parsed = match ElfMetadata::parse_file(host) {
            Ok(metadata) => Some(metadata),
            Err(Error::NotElf { .. }) | Err(Error::Elf { .. }) => None,
            Err(e) => return Err(e),
        };
        assert!(parsed.as_ref().is_none_or(|m| m.path == host));
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
