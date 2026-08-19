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
    out
}

/// Join a logical absolute path onto a real directory, refusing anything that
/// would land outside of it.
pub fn join_under(base: &Path, logical: &Path) -> PathBuf {
    let normalized = normalize_absolute(logical);
    match normalized.strip_prefix("/") {
        Ok(rel) if rel.as_os_str().is_empty() => base.to_path_buf(),
        Ok(rel) => base.join(rel),
        Err(_) => base.to_path_buf(),
    }
}

/// Absolute parent directory of a logical path (`/` for top-level entries).
pub fn logical_parent(path: &Path) -> PathBuf {
    path.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Every ancestor directory of a logical path, shallowest first, excluding `/`.
pub fn ancestor_dirs(path: &Path) -> Vec<PathBuf> {
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

/// Render bytes as lowercase hex (used for digests).
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        s.push(char::from_digit((byte >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((byte & 0xf) as u32, 16).unwrap());
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
