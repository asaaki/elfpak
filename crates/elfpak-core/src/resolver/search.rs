//! Library search directories: `ld.so.conf` and the architecture defaults.

use std::path::{Path, PathBuf};

use crate::elf::{Architecture, ElfClass};
use crate::source::SourceRoot;

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
    paths
}

/// Read `/etc/ld.so.conf`, following `include` directives (with `*` globs).
pub fn parse_ld_so_conf(root: &SourceRoot) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut visited = Vec::new();
    read_conf(
        root,
        Path::new("/etc/ld.so.conf"),
        &mut paths,
        &mut visited,
        0,
    );
    paths
}

fn read_conf(
    root: &SourceRoot,
    logical: &Path,
    paths: &mut Vec<PathBuf>,
    visited: &mut Vec<PathBuf>,
    depth: usize,
) {
    if depth > 8 || visited.contains(&logical.to_path_buf()) {
        return;
    }
    visited.push(logical.to_path_buf());
    let Ok(Some(bytes)) = root.read(logical) else {
        return;
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return;
    };

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
                for included in expand_include(root, logical, pattern) {
                    read_conf(root, &included, paths, visited, depth + 1);
                }
            }
            continue;
        }
        if line.starts_with("hwcap ") {
            continue;
        }
        let candidate = crate::paths::normalize_absolute(Path::new(line));
        if !paths.contains(&candidate) {
            paths.push(candidate);
        }
    }
}

/// Resolve an `include` pattern; only the final component may contain `*`.
fn expand_include(root: &SourceRoot, current: &Path, pattern: &str) -> Vec<PathBuf> {
    let pattern_path = if Path::new(pattern).is_absolute() {
        PathBuf::from(pattern)
    } else {
        crate::paths::logical_parent(current).join(pattern)
    };
    let pattern_path = crate::paths::normalize_absolute(&pattern_path);

    let Some(name) = pattern_path.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    if !name.contains('*') {
        return vec![pattern_path];
    }

    let dir = crate::paths::logical_parent(&pattern_path);
    let Ok(entries) = root.read_dir(&dir) else {
        return Vec::new();
    };
    let (prefix, suffix) = name.split_once('*').unwrap_or((name, ""));
    entries
        .into_iter()
        .filter_map(|entry| {
            let entry = entry.to_str()?.to_string();
            (entry.len() >= prefix.len() + suffix.len()
                && entry.starts_with(prefix)
                && entry.ends_with(suffix))
            .then(|| dir.join(entry))
        })
        .collect()
}
