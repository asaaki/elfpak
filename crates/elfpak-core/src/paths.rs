//! Lexical path handling.
//!
//! Logical paths are interpreted relative to a source or output root, never
//! passed directly to the host OS.

use std::path::{Component, Path, PathBuf};

/// Normalize a path into an absolute logical path.
pub fn normalize_absolute(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => {}
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// Join a logical absolute path onto a real directory, refusing anything that
/// would land outside of it.
pub fn join_under(base: &Path, logical: &Path) -> PathBuf {
    let normalized = normalize_absolute(logical);
    let relative = normalized
        .strip_prefix("/")
        .expect("normalized logical paths are absolute");
    let joined = base.join(relative);
    // Containment is checked here, and again by the caller before it writes.
    assert!(joined.starts_with(base));
    joined
}

/// Absolute parent directory of a logical path (`/` for top-level entries).
pub fn logical_parent(path: &Path) -> PathBuf {
    assert!(path.is_absolute());
    path.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Every ancestor directory of a logical path, shallowest first, excluding `/`.
pub fn ancestor_dirs(path: &Path) -> Vec<PathBuf> {
    assert!(path.is_absolute());

    let mut dirs = Vec::new();
    let mut current = PathBuf::from("/");
    let parent = logical_parent(path);
    for component in parent.components() {
        if let Component::Normal(part) = component {
            current.push(part);
            dirs.push(current.clone());
        }
    }
    dirs
}

/// Whether any component between `root` and `path` is a symlink.
///
/// Checking only the immediate parent is not enough: `create_dir_all` and the
/// verification walk both follow a symlink planted higher up. A walk that
/// reaches the filesystem root without meeting `root` answers `true`, because a
/// path that is not under the root it was supposed to be under is exactly the
/// situation both callers exist to refuse.
pub fn has_symlinked_ancestor(root: &Path, path: &Path) -> bool {
    // `path` need not be under `root`: a manifest naming `/` produces a shorter
    // path than the rootfs it is checked against. Every step drops one
    // component, so the walk terminates on its own; the running of it off the
    // top is the "not under the root" answer, not a broken invariant.
    let mut current = path;
    while current != root {
        if std::fs::symlink_metadata(current).is_ok_and(|metadata| metadata.is_symlink()) {
            return true;
        }
        let Some(parent) = current.parent() else {
            return true;
        };
        if parent == current {
            return true;
        }
        current = parent;
    }
    false
}

/// Render bytes as lowercase hex (used for digests).
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // A nibble is always a valid hex digit, so neither `from_digit` can fail.
        s.push(char::from_digit(u32::from(byte >> 4), 16).expect("a nibble is one hex digit"));
        s.push(char::from_digit(u32::from(byte & 0xf), 16).expect("a nibble is one hex digit"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_dot_and_parent_components() {
        assert_eq!(
            normalize_absolute(Path::new("/a/./b/../c")),
            Path::new("/a/c")
        );
        assert_eq!(
            normalize_absolute(Path::new("/../../etc")),
            Path::new("/etc")
        );
        assert_eq!(normalize_absolute(Path::new("a/b")), Path::new("/a/b"));
    }

    #[test]
    fn join_under_cannot_escape_the_base() {
        let base = Path::new("/out");
        assert_eq!(
            join_under(base, Path::new("/etc/passwd")),
            Path::new("/out/etc/passwd")
        );
        assert_eq!(
            join_under(base, Path::new("/../../etc")),
            Path::new("/out/etc")
        );
        assert_eq!(join_under(base, Path::new("/")), Path::new("/out"));
    }

    #[test]
    fn ancestors_are_listed_shallowest_first() {
        assert_eq!(
            ancestor_dirs(Path::new("/usr/lib/x86_64-linux-gnu/libc.so.6")),
            vec![
                PathBuf::from("/usr"),
                PathBuf::from("/usr/lib"),
                PathBuf::from("/usr/lib/x86_64-linux-gnu"),
            ]
        );
        assert!(ancestor_dirs(Path::new("/libc.so.6")).is_empty());
    }

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
    }
}
