//! Library search directories: `ld.so.conf` and the architecture defaults.

use crate::{
    elf::{Architecture, ElfClass},
    paths::{logical_parent, normalize_absolute},
    source::SourceRoot,
};
use std::path::{Path, PathBuf};

/// How deeply `include` directives may nest.
///
/// `ld.so.conf` files include a directory of fragments, and no distribution
/// nests those any further. Eight levels is generous.
const CONF_DEPTH_MAX: usize = 8;

/// Upper bound on the files one `ld.so.conf` may pull in, and on the
/// directories it may name. Both bound the work a hostile sysroot can ask for.
const CONF_FILES_MAX: usize = 256;
const CONF_DIRECTORIES_MAX: usize = 256;

/// glibc's built-in trusted directories, plus the Debian/Fedora conventions that
/// are configured on every mainstream distribution.
pub fn default_library_paths(architecture: &Architecture) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(tuple) = architecture.machine.debian_multiarch() {
        paths.push(PathBuf::from(format!("/lib/{tuple}")));
        paths.push(PathBuf::from(format!("/usr/lib/{tuple}")));
    }
    if architecture.class == ElfClass::Elf64 {
        paths.push(PathBuf::from("/lib64"));
        paths.push(PathBuf::from("/usr/lib64"));
    }
    paths.push(PathBuf::from("/lib"));
    paths.push(PathBuf::from("/usr/lib"));

    assert!(!paths.is_empty());
    assert!(paths.iter().all(|p| p.is_absolute()));
    paths
}

/// One meaningful line of an `ld.so.conf`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Directive {
    /// A directory to add to the search list.
    Directory(PathBuf),
    /// Another configuration file to read, already expanded to a concrete path.
    Include(PathBuf),
}

/// Read `/etc/ld.so.conf`, following `include` directives (with `*` globs).
///
/// Directives are pushed in reverse and popped in order, which reproduces the
/// depth-first, in-file-order traversal of the loader's own reader.
pub fn parse_ld_so_conf(root: &SourceRoot) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut visited: Vec<PathBuf> = Vec::new();
    let mut pending = vec![(Directive::Include(PathBuf::from("/etc/ld.so.conf")), 0usize)];

    while let Some((directive, depth)) = pending.pop() {
        assert!(depth <= CONF_DEPTH_MAX + 1);
        assert!(visited.len() <= CONF_FILES_MAX);

        let file = match directive {
            Directive::Directory(dir) => {
                assert!(dir.is_absolute());
                if !paths.contains(&dir) && paths.len() < CONF_DIRECTORIES_MAX {
                    paths.push(dir);
                }
                continue;
            }
            Directive::Include(file) => file,
        };

        // A file is read once, so an include cycle cannot become a loop.
        if depth > CONF_DEPTH_MAX || visited.contains(&file) || visited.len() == CONF_FILES_MAX {
            continue;
        }
        visited.push(file.clone());

        for directive in read_conf(root, &file).into_iter().rev() {
            pending.push((directive, depth + 1));
        }
    }

    assert!(paths.len() <= CONF_DIRECTORIES_MAX);
    assert!(paths.iter().all(|p| p.is_absolute()));
    paths
}

/// Directives of a single file, in file order. Unreadable or non-UTF-8 files
/// yield nothing: the configuration is a hint, and the default directories
/// remain either way.
fn read_conf(root: &SourceRoot, logical: &Path) -> Vec<Directive> {
    assert!(logical.is_absolute());

    let Ok(Some(bytes)) = root.read(logical) else {
        return Vec::new();
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return Vec::new();
    };

    let mut directives = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line
            .strip_prefix("include ")
            .or_else(|| line.strip_prefix("include\t"))
        {
            for pattern in rest.split_whitespace() {
                let included = expand_include(root, logical, pattern);
                directives.extend(included.into_iter().map(Directive::Include));
            }
            continue;
        }
        if line.starts_with("hwcap ") {
            // Obsolete since glibc 2.33 and never a directory.
            continue;
        }
        directives.push(Directive::Directory(normalize_absolute(Path::new(line))));
    }
    directives
}

/// Resolve an `include` pattern; only the final component may contain `*`.
fn expand_include(root: &SourceRoot, current: &Path, pattern: &str) -> Vec<PathBuf> {
    assert!(current.is_absolute());

    let pattern_path = if Path::new(pattern).is_absolute() {
        PathBuf::from(pattern)
    } else {
        logical_parent(current).join(pattern)
    };
    let pattern_path = normalize_absolute(&pattern_path);

    let Some(name) = pattern_path.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    if !name.contains('*') {
        return vec![pattern_path];
    }

    let dir = logical_parent(&pattern_path);
    let Ok(entries) = root.read_dir(&dir) else {
        return Vec::new();
    };
    let (prefix, suffix) = name.split_once('*').unwrap_or((name, ""));
    entries
        .into_iter()
        .filter_map(|entry| {
            let entry = entry.to_str()?.to_string();
            // The two halves must not overlap, or `lib*.conf` would match
            // `lib.conf` twice over the same bytes.
            let fits = entry.len() >= prefix.len() + suffix.len();
            let matches = entry.starts_with(prefix) && entry.ends_with(suffix);
            (fits && matches).then(|| dir.join(entry))
        })
        .collect()
}
