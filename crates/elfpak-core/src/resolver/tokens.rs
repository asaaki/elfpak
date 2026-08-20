//! Expansion of the dynamic string tokens glibc understands in
//! `DT_RPATH`/`DT_RUNPATH`: `$ORIGIN`, `$LIB` and `$PLATFORM`.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct TokenContext {
    /// Directory of the object that owns the search path, as a logical path.
    pub origin: PathBuf,
    /// Value of `$LIB` (`lib` or `lib64`).
    pub lib: String,
    /// Value of `$PLATFORM` (`x86_64`, `aarch64`, ...).
    pub platform: Option<String>,
}

/// Expand dynamic string tokens. Unknown tokens are left verbatim, matching the
/// loader's behaviour of simply not substituting what it does not know.
pub fn expand(input: &str, ctx: &TokenContext) -> String {
    assert!(ctx.origin.is_absolute());

    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Every branch below advances `i` by at least one byte, so the walk is
        // bounded by the length of the input.
        let progress = i;
        if bytes[i] != b'$' {
            // Copy verbatim up to the next `$`. Copying byte by byte would
            // re-encode every non-ASCII character in the path.
            let next = input[i..].find('$').map_or(input.len(), |at| i + at);
            out.push_str(&input[i..next]);
            i = next;
            continue;
        }
        let (name, consumed) = read_token(&input[i + 1..]);
        match substitute(&name, ctx) {
            Some(value) => {
                out.push_str(&value);
                i += 1 + consumed;
            }
            None => {
                // An unknown token is not a token: the loader leaves it alone.
                out.push('$');
                i += 1;
            }
        }
        assert!(i > progress);
    }
    assert!(i >= bytes.len(), "the walk consumes the whole input");
    out
}

/// Returns the token name and how many bytes of the input it occupies.
///
/// The count includes the braces of a `${NAME}` spelling, so a caller that
/// advances by it lands just past the token either way.
fn read_token(rest: &str) -> (String, usize) {
    let (name, consumed) = if let Some(stripped) = rest.strip_prefix('{') {
        match stripped.find('}') {
            Some(end) => (stripped[..end].to_string(), end + 2),
            // Unterminated: not a token, and nothing is consumed.
            None => (String::new(), 0),
        }
    } else {
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        (rest[..end].to_string(), end)
    };
    assert!(consumed <= rest.len());
    assert!(name.len() <= consumed);
    (name, consumed)
}

fn substitute(name: &str, ctx: &TokenContext) -> Option<String> {
    match name {
        "ORIGIN" => Some(ctx.origin.to_string_lossy().into_owned()),
        "LIB" => Some(ctx.lib.clone()),
        "PLATFORM" => ctx.platform.clone(),
        _ => None,
    }
}

/// Expand a search path entry and normalize it to a logical absolute path.
///
/// Relative entries are interpreted against the requesting object's directory,
/// which is what the loader effectively does for `$ORIGIN`-style layouts.
pub fn expand_search_path(entry: &str, ctx: &TokenContext) -> PathBuf {
    assert!(ctx.origin.is_absolute());

    let expanded = expand(entry, ctx);
    let path = Path::new(&expanded);
    let logical = if path.is_absolute() {
        crate::paths::normalize_absolute(path)
    } else {
        crate::paths::normalize_absolute(&ctx.origin.join(path))
    };
    // Search paths are logical paths in the target filesystem: a relative one
    // would be resolved against this process's working directory.
    assert!(logical.is_absolute());
    logical
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> TokenContext {
        TokenContext {
            origin: PathBuf::from("/opt/app/bin"),
            lib: "lib64".to_string(),
            platform: Some("x86_64".to_string()),
        }
    }

    #[test]
    fn expands_origin_in_both_spellings() {
        assert_eq!(expand("$ORIGIN/../lib", &ctx()), "/opt/app/bin/../lib");
        assert_eq!(expand("${ORIGIN}/../lib", &ctx()), "/opt/app/bin/../lib");
    }

    #[test]
    fn expands_lib_and_platform() {
        assert_eq!(expand("/usr/$LIB", &ctx()), "/usr/lib64");
        assert_eq!(expand("/usr/lib/$PLATFORM", &ctx()), "/usr/lib/x86_64");
    }

    #[test]
    fn unknown_tokens_stay_literal() {
        assert_eq!(expand("/usr/$NOPE/lib", &ctx()), "/usr/$NOPE/lib");
        assert_eq!(expand("/usr/$", &ctx()), "/usr/$");
        assert_eq!(expand("/usr/${unterminated", &ctx()), "/usr/${unterminated");
    }

    #[test]
    fn non_ascii_path_components_survive_expansion() {
        let ctx = TokenContext {
            origin: PathBuf::from("/opt/café/bin"),
            ..ctx()
        };
        assert_eq!(expand("/opt/café/lib", &ctx), "/opt/café/lib");
        assert_eq!(expand("$ORIGIN/../lib", &ctx), "/opt/café/bin/../lib");
        assert_eq!(
            expand_search_path("$ORIGIN/../lib", &ctx),
            PathBuf::from("/opt/café/lib")
        );
    }

    #[test]
    fn search_paths_are_normalized_and_absolute() {
        assert_eq!(
            expand_search_path("$ORIGIN/../lib", &ctx()),
            PathBuf::from("/opt/app/lib")
        );
        assert_eq!(
            expand_search_path("../lib", &ctx()),
            PathBuf::from("/opt/app/lib")
        );
        assert_eq!(
            expand_search_path("/usr/lib", &ctx()),
            PathBuf::from("/usr/lib")
        );
    }
}
