//! Direct `/etc/ld.so.cache` parser.
//!
//! `ldconfig` is never invoked. Both the historical `ld.so-1.7.0` layout and the
//! current `glibc-ld.so.cache1.1` layout are understood, including the common
//! case where a new-format cache is appended after an old-format one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

const OLD_MAGIC: &[u8] = b"ld.so-1.7.0";
const NEW_MAGIC: &[u8] = b"glibc-ld.so.cache";
const NEW_VERSION: &[u8] = b"1.1";

/// Header size of `struct cache_file` including alignment padding.
const OLD_HEADER_LEN: usize = 16;
const OLD_ENTRY_LEN: usize = 12;
/// Header size of `struct cache_file_new`.
const NEW_HEADER_LEN: usize = 48;
const NEW_ENTRY_LEN: usize = 24;

#[derive(Debug, Clone, Default)]
pub struct LdCache {
    /// soname -> candidate absolute paths, in cache order.
    entries: HashMap<String, Vec<PathBuf>>,
    len: usize,
}

impl LdCache {
    /// Parse a cache image. Malformed caches degrade to "no entries" rather than
    /// failing the build: the cache is only a hint, the search paths remain.
    pub fn parse(bytes: &[u8]) -> LdCache {
        let mut cache = LdCache::default();
        let pairs = if starts_with(bytes, 0, NEW_MAGIC) {
            parse_new(bytes, 0)
        } else if starts_with(bytes, 0, OLD_MAGIC) {
            let nlibs = match read_u32(bytes, 12) {
                Some(n) => n as usize,
                None => return cache,
            };
            let new_offset = align8(OLD_HEADER_LEN + nlibs.saturating_mul(OLD_ENTRY_LEN));
            if starts_with(bytes, new_offset, NEW_MAGIC) {
                parse_new(bytes, new_offset)
            } else {
                parse_old(bytes, nlibs)
            }
        } else {
            Vec::new()
        };

        cache.len = pairs.len();
        for (soname, path) in pairs {
            let list = cache.entries.entry(soname).or_default();
            if !list.contains(&path) {
                list.push(path);
            }
        }
        cache
    }

    pub fn load(path: &Path) -> Option<LdCache> {
        let bytes = std::fs::read(path).ok()?;
        let cache = LdCache::parse(&bytes);
        if cache.entries.is_empty() {
            None
        } else {
            Some(cache)
        }
    }

    pub fn lookup(&self, soname: &str) -> &[PathBuf] {
        self.entries.get(soname).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn entry_count(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn align8(value: usize) -> usize {
    (value + 7) & !7
}

fn starts_with(bytes: &[u8], offset: usize, magic: &[u8]) -> bool {
    bytes.len() >= offset + magic.len() && &bytes[offset..offset + magic.len()] == magic
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes(slice.try_into().ok()?))
}

/// Strings are NUL terminated and addressed relative to `base`.
fn read_string(bytes: &[u8], base: usize, offset: u32) -> Option<String> {
    let start = base.checked_add(offset as usize)?;
    let rest = bytes.get(start..)?;
    let end = rest.iter().position(|&b| b == 0)?;
    std::str::from_utf8(&rest[..end]).ok().map(str::to_string)
}

fn parse_old(bytes: &[u8], nlibs: usize) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for index in 0..nlibs {
        let offset = OLD_HEADER_LEN + index * OLD_ENTRY_LEN;
        let (Some(key), Some(value)) = (read_u32(bytes, offset + 4), read_u32(bytes, offset + 8))
        else {
            break;
        };
        if let (Some(soname), Some(path)) =
            (read_string(bytes, 0, key), read_string(bytes, 0, value))
        {
            out.push((soname, PathBuf::from(path)));
        }
    }
    out
}

fn parse_new(bytes: &[u8], base: usize) -> Vec<(String, PathBuf)> {
    if !starts_with(bytes, base + NEW_MAGIC.len(), NEW_VERSION) {
        return Vec::new();
    }
    let nlibs = match read_u32(bytes, base + 20) {
        Some(n) => n as usize,
        None => return Vec::new(),
    };
    let mut out = Vec::with_capacity(nlibs);
    for index in 0..nlibs {
        let offset = base + NEW_HEADER_LEN + index * NEW_ENTRY_LEN;
        let (Some(key), Some(value)) = (read_u32(bytes, offset + 4), read_u32(bytes, offset + 8))
        else {
            break;
        };
        if let (Some(soname), Some(path)) = (
            read_string(bytes, base, key),
            read_string(bytes, base, value),
        ) {
            out.push((soname, PathBuf::from(path)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `glibc-ld.so.cache1.1` image with the given (soname, path) pairs.
    fn new_format(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut strings = Vec::new();
        let mut offsets = Vec::new();
        for (soname, path) in entries {
            let key = strings.len() as u32;
            strings.extend_from_slice(soname.as_bytes());
            strings.push(0);
            let value = strings.len() as u32;
            strings.extend_from_slice(path.as_bytes());
            strings.push(0);
            offsets.push((key, value));
        }
        let header_len = NEW_HEADER_LEN + entries.len() * NEW_ENTRY_LEN;

        let mut out = Vec::new();
        out.extend_from_slice(NEW_MAGIC);
        out.extend_from_slice(NEW_VERSION);
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        out.extend_from_slice(&(strings.len() as u32).to_le_bytes());
        out.push(0);
        out.extend_from_slice(&[0, 0, 0]);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&[0u8; 12]);
        assert_eq!(out.len(), NEW_HEADER_LEN);

        for (key, value) in &offsets {
            out.extend_from_slice(&0x0300_0003u32.to_le_bytes());
            out.extend_from_slice(&(key + header_len as u32).to_le_bytes());
            out.extend_from_slice(&(value + header_len as u32).to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&0u64.to_le_bytes());
        }
        out.extend_from_slice(&strings);
        out
    }

    #[test]
    fn parses_the_new_format() {
        let bytes = new_format(&[
            ("libc.so.6", "/lib/x86_64-linux-gnu/libc.so.6"),
            ("libm.so.6", "/lib/x86_64-linux-gnu/libm.so.6"),
        ]);
        let cache = LdCache::parse(&bytes);
        assert_eq!(cache.entry_count(), 2);
        assert_eq!(
            cache.lookup("libc.so.6"),
            [PathBuf::from("/lib/x86_64-linux-gnu/libc.so.6")]
        );
        assert!(cache.lookup("libnope.so.1").is_empty());
    }

    #[test]
    fn parses_an_old_format_header_with_an_appended_new_cache() {
        let new = new_format(&[("libz.so.1", "/usr/lib/libz.so.1")]);
        let nlibs = 1usize;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(OLD_MAGIC);
        bytes.push(0); // padding to the u32 boundary
        bytes.extend_from_slice(&(nlibs as u32).to_le_bytes());
        // One old entry pointing at strings we never read.
        bytes.extend_from_slice(&[0u8; OLD_ENTRY_LEN]);
        while bytes.len() < align8(OLD_HEADER_LEN + nlibs * OLD_ENTRY_LEN) {
            bytes.push(0);
        }
        bytes.extend_from_slice(&new);

        let cache = LdCache::parse(&bytes);
        assert_eq!(
            cache.lookup("libz.so.1"),
            [PathBuf::from("/usr/lib/libz.so.1")]
        );
    }

    #[test]
    fn garbage_degrades_to_an_empty_cache() {
        assert!(LdCache::parse(b"not a cache at all").is_empty());
        assert!(LdCache::parse(&[]).is_empty());
    }

    #[test]
    fn truncated_entries_do_not_panic() {
        let mut bytes = new_format(&[("libc.so.6", "/lib/libc.so.6")]);
        bytes.truncate(NEW_HEADER_LEN + 4);
        assert!(LdCache::parse(&bytes).is_empty());
    }
}
