//! Lexical path handling.
//!
//! All logical paths inside `elfpak` are absolute paths *as seen by the target
//! process*. They are never handed to the OS directly; they are always joined
//! onto a source root or an output root first.

use std::path::{Component, Path, PathBuf};

/// Lexically normalize an absolute path: drop `.`, resolve `..` textually and
/// never allow escaping above `/`.
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
    assert!(out.is_absolute());
    assert!(!out.components().any(|c| c == Component::ParentDir));
    out
}

/// Join a logical absolute path onto a real directory, refusing anything that
/// would land outside of it.
pub fn join_under(base: &Path, logical: &Path) -> PathBuf {
    let normalized = normalize_absolute(logical);
    let joined = match normalized.strip_prefix("/") {
        Ok(rel) if rel.as_os_str().is_empty() => base.to_path_buf(),
        Ok(rel) => base.join(rel),
        // `normalize_absolute` always returns a path below `/`, so this arm is
        // unreachable; falling back to the base keeps the write inside it.
        Err(_) => base.to_path_buf(),
    };
    // The whole point of this function: containment is a postcondition, not a
    // hope. It is checked here, and again by the caller before it writes.
    assert!(joined.starts_with(base));
    joined
}

/// Absolute parent directory of a logical path (`/` for top-level entries).
pub fn logical_parent(path: &Path) -> PathBuf {
    assert!(path.is_absolute());
    let parent = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"));
    assert!(parent.is_absolute());
    parent
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
    // Shallowest first is what lets a caller create directories in list order.
    if let Some(last) = dirs.last() {
        assert_eq!(last, &parent);
        assert!(dirs[0].parent() == Some(Path::new("/")));
    }
    dirs
}

/// Render bytes as lowercase hex (used for digests).
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // A nibble is always a valid hex digit, so neither `from_digit` can fail.
        s.push(char::from_digit(u32::from(byte >> 4), 16).expect("a nibble is one hex digit"));
        s.push(char::from_digit(u32::from(byte & 0xf), 16).expect("a nibble is one hex digit"));
    }
    assert_eq!(s.len(), bytes.len() * 2);
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
