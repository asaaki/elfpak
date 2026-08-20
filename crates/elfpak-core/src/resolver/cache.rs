//! Direct `/etc/ld.so.cache` reading and writing.
//!
//! `ldconfig` is never invoked. Both the historical `ld.so-1.7.0` layout and the
//! current `glibc-ld.so.cache1.1` layout are understood on the way in, including
//! the common case where a new-format cache is appended after an old-format one.
//!
//! On the way out, [`build`] emits a `glibc-ld.so.cache1.1` image for the
//! bundle. That is the only way a packaged application can find a library that
//! does not live in a directory the loader searches by default: the rootfs
//! carries no `ldconfig` to generate one, and copying the build host's cache
//! would describe the host's filesystem rather than the bundle's.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::elf::{Architecture, ElfClass, Endianness, Machine};

const OLD_MAGIC: &[u8] = b"ld.so-1.7.0";
const NEW_MAGIC: &[u8] = b"glibc-ld.so.cache";
const NEW_VERSION: &[u8] = b"1.1";

/// Header size of `struct cache_file` including alignment padding.
const OLD_HEADER_LEN: usize = 16;
const OLD_ENTRY_LEN: usize = 12;
/// Header size of `struct cache_file_new`.
const NEW_HEADER_LEN: usize = 48;
const NEW_ENTRY_LEN: usize = 24;

/// Upper bound on the entries taken from a cache image.
///
/// A distribution cache holds a few thousand libraries. The bound is on what is
/// read, not on what exists: `nlibs` comes out of the file and is never trusted.
const CACHE_ENTRIES_MAX: usize = 65_536;

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
            let new_offset = align8(
                nlibs
                    .saturating_mul(OLD_ENTRY_LEN)
                    .saturating_add(OLD_HEADER_LEN),
            );
            if starts_with(bytes, new_offset, NEW_MAGIC) {
                parse_new(bytes, new_offset)
            } else {
                parse_old(bytes, nlibs)
            }
        } else {
            Vec::new()
        };

        assert!(pairs.len() <= CACHE_ENTRIES_MAX);

        cache.len = pairs.len();
        for (soname, path) in pairs {
            // ldconfig only ever records absolute paths. A relative one would
            // be interpreted against the working directory further downstream,
            // which is exactly the kind of ambient state a packaging tool must
            // not depend on.
            if !path.is_absolute() {
                continue;
            }
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
        let candidates = self.entries.get(soname).map(Vec::as_slice).unwrap_or(&[]);
        assert!(candidates.iter().all(|p| p.is_absolute()));
        candidates
    }

    pub fn entry_count(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn align8(value: usize) -> usize {
    value.saturating_add(7) & !7
}

fn starts_with(bytes: &[u8], offset: usize, magic: &[u8]) -> bool {
    bytes.len() >= offset + magic.len() && &bytes[offset..offset + magic.len()] == magic
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(slice.try_into().ok()?))
}

/// Strings are NUL terminated and addressed relative to `base`.
fn read_string(bytes: &[u8], base: usize, offset: u32) -> Option<String> {
    let start = base.checked_add(offset as usize)?;
    let rest = bytes.get(start..)?;
    let end = rest.iter().position(|&b| b == 0)?;
    std::str::from_utf8(&rest[..end]).ok().map(str::to_string)
}

/// How many entries `bytes` can actually hold from `base` on.
///
/// `nlibs` comes straight out of the file, so it is never trusted for sizing:
/// a 48-byte header claiming four billion entries must not reserve memory for
/// four billion entries.
fn entry_capacity(bytes: &[u8], base: usize, header: usize, entry: usize) -> usize {
    bytes
        .len()
        .saturating_sub(base.saturating_add(header))
        .saturating_div(entry)
}

/// `struct cache_file`: a header followed by `nlibs` fixed-size entries whose
/// string offsets are relative to the start of the image.
fn parse_old(bytes: &[u8], nlibs: usize) -> Vec<(String, PathBuf)> {
    let capacity = entry_capacity(bytes, 0, OLD_HEADER_LEN, OLD_ENTRY_LEN);
    let count = nlibs.min(capacity).min(CACHE_ENTRIES_MAX);
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let Some(offset) = index
            .checked_mul(OLD_ENTRY_LEN)
            .and_then(|at| at.checked_add(OLD_HEADER_LEN))
        else {
            break;
        };
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
    let capacity = entry_capacity(bytes, base, NEW_HEADER_LEN, NEW_ENTRY_LEN);
    let count = nlibs.min(capacity).min(CACHE_ENTRIES_MAX);
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let Some(offset) = base
            .checked_add(NEW_HEADER_LEN)
            .and_then(|start| index.checked_mul(NEW_ENTRY_LEN)?.checked_add(start))
        else {
            break;
        };
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

/// One `soname -> path` mapping, as the loader inside the bundle will see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub soname: String,
    /// Absolute path *inside the generated rootfs*.
    pub path: PathBuf,
}

/// glibc's `_DL_CACHE_DEFAULT_ID` for the target.
///
/// `_dl_cache_check_flags` compares the entry flags against this value exactly,
/// so an entry carrying anything else is silently ignored by the loader.
fn entry_flags(architecture: &Architecture) -> Option<i32> {
    const FLAG_ELF_LIBC6: i32 = 0x0003;
    const FLAG_X8664_LIB64: i32 = 0x0300;
    const FLAG_AARCH64_LIB64: i32 = 0x0a00;
    match (architecture.machine, architecture.class) {
        (Machine::X86_64, ElfClass::Elf64) => Some(FLAG_X8664_LIB64 | FLAG_ELF_LIBC6),
        (Machine::Aarch64, ElfClass::Elf64) => Some(FLAG_AARCH64_LIB64 | FLAG_ELF_LIBC6),
        _ => None,
    }
}

/// `cache_file_new_flags_endian_little`. The header records the byte order the
/// entries were written in, and glibc refuses a cache that disagrees with the
/// architecture it is running on.
const FLAGS_ENDIAN_LITTLE: u8 = 2;

/// The string table of a cache image, and the `(soname, path)` offset pair each
/// entry record points at.
struct StringTable {
    bytes: Vec<u8>,
    offsets: Vec<(u32, u32)>,
}

/// Encode the strings of `entries`. Offsets are absolute within the image,
/// hence `base`.
///
/// `None` for anything that cannot be encoded faithfully: a path that is not
/// UTF-8, a NUL that would truncate a string, or an image too large to address
/// with the `u32` offsets the format uses.
fn encode_strings(entries: &[&CacheEntry], base: usize) -> Option<StringTable> {
    let mut strings: Vec<u8> = Vec::new();
    let mut offsets: Vec<(u32, u32)> = Vec::with_capacity(entries.len());

    for entry in entries {
        // A NUL in either string would truncate it; such a name cannot come
        // from an ELF string table, but the file names could in principle.
        let path = entry.path.to_str()?;
        if entry.soname.contains('\0') || path.contains('\0') {
            return None;
        }
        let key = u32::try_from(base + strings.len()).ok()?;
        strings.extend_from_slice(entry.soname.as_bytes());
        strings.push(0);
        let value = u32::try_from(base + strings.len()).ok()?;
        strings.extend_from_slice(path.as_bytes());
        strings.push(0);
        assert!(key < value, "the soname precedes the path it names");
        offsets.push((key, value));
    }
    Some(StringTable {
        bytes: strings,
        offsets,
    })
}

/// Build a `glibc-ld.so.cache1.1` image for `entries`.
///
/// Returns `None` for a target this function cannot encode faithfully, so the
/// caller can fall back to reporting the problem instead of writing a cache the
/// loader would reject.
pub fn build(architecture: &Architecture, entries: &[CacheEntry]) -> Option<Vec<u8>> {
    assert!(entries.iter().all(|e| e.path.is_absolute()));

    let flags = entry_flags(architecture)?;
    if architecture.endianness != Endianness::Little {
        // The header records one byte order and glibc refuses a cache that
        // disagrees with the architecture reading it.
        return None;
    }

    let mut entries: Vec<&CacheEntry> = entries.iter().collect();
    // glibc looks entries up with a binary search that walks *down* the table,
    // so it has to be sorted in descending `_dl_cache_libcmp` order. Ascending
    // order parses fine and then fails to resolve. The path breaks ties, purely
    // so that the same plan always produces the same bytes.
    entries.sort_by(|a, b| libcmp(&b.soname, &a.soname).then_with(|| a.path.cmp(&b.path)));
    entries.dedup_by(|a, b| a.soname == b.soname && a.path == b.path);
    assert!(
        entries
            .windows(2)
            .all(|pair| libcmp(&pair[0].soname, &pair[1].soname) != Ordering::Less),
        "the loader binary-searches downwards and needs descending order"
    );

    let base = NEW_HEADER_LEN + entries.len() * NEW_ENTRY_LEN;
    let StringTable {
        bytes: strings,
        offsets,
    } = encode_strings(&entries, base)?;
    assert_eq!(offsets.len(), entries.len());

    let mut out = Vec::with_capacity(base + strings.len());
    out.extend_from_slice(NEW_MAGIC);
    out.extend_from_slice(NEW_VERSION);
    out.extend_from_slice(&u32::try_from(entries.len()).ok()?.to_le_bytes());
    out.extend_from_slice(&u32::try_from(strings.len()).ok()?.to_le_bytes());
    out.push(FLAGS_ENDIAN_LITTLE);
    out.extend_from_slice(&[0, 0, 0]); // `padding_unsed` (sic), reserved
    out.extend_from_slice(&0u32.to_le_bytes()); // extension_offset: none
    out.extend_from_slice(&[0u8; 12]); // `unused`, reserved
    assert_eq!(out.len(), NEW_HEADER_LEN);

    for (key, value) in offsets {
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&key.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // osversion, unused
        out.extend_from_slice(&0u64.to_le_bytes()); // hwcap: none required
    }
    assert_eq!(out.len(), base);
    out.extend_from_slice(&strings);
    assert_eq!(out.len(), base + strings.len());

    // Pair assertion: what was just written is read back with the same reader
    // the loader's format is modelled on. A cache the bundle cannot use is
    // worse than no cache, because it looks like the problem was solved.
    let written = LdCache::parse(&out);
    assert_eq!(written.entry_count(), entries.len());
    assert!(
        entries
            .iter()
            .all(|entry| written.lookup(&entry.soname).contains(&entry.path))
    );
    Some(out)
}

/// glibc's `_dl_cache_libcmp`: like `strcmp`, except that runs of digits compare
/// numerically, so `libfoo.so.9` sorts before `libfoo.so.10`.
///
/// Bytes are compared unsigned. glibc compares them as `char`, whose signedness
/// is architecture-dependent, so the two can only disagree about sonames that
/// are not ASCII — which no toolchain produces.
fn libcmp(left: &str, right: &str) -> Ordering {
    let (p1, p2) = (left.as_bytes(), right.as_bytes());
    let (mut i, mut j) = (0usize, 0usize);
    let at = |s: &[u8], k: usize| s.get(k).copied().unwrap_or(0);

    while at(p1, i) != 0 {
        let (c1, c2) = (at(p1, i), at(p2, j));
        if c1.is_ascii_digit() {
            if !c2.is_ascii_digit() {
                return Ordering::Greater;
            }
            let mut v1 = 0u64;
            while at(p1, i).is_ascii_digit() {
                v1 = v1
                    .saturating_mul(10)
                    .saturating_add(u64::from(at(p1, i) - b'0'));
                i += 1;
            }
            let mut v2 = 0u64;
            while at(p2, j).is_ascii_digit() {
                v2 = v2
                    .saturating_mul(10)
                    .saturating_add(u64::from(at(p2, j) - b'0'));
                j += 1;
            }
            if v1 != v2 {
                return v1.cmp(&v2);
            }
        } else if c2.is_ascii_digit() {
            return Ordering::Less;
        } else if c1 != c2 {
            return c1.cmp(&c2);
        } else {
            i += 1;
            j += 1;
        }
    }
    at(p1, i).cmp(&at(p2, j))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Offsets in the cache format are `u32`; a fixture that did not fit would
    /// be a broken fixture, not a truncated one.
    fn offset(value: usize) -> u32 {
        u32::try_from(value).expect("fixture offsets fit in u32")
    }

    /// Build a `glibc-ld.so.cache1.1` image with the given (soname, path) pairs.
    fn new_format(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut strings = Vec::new();
        let mut offsets = Vec::new();
        for (soname, path) in entries {
            let key = offset(strings.len());
            strings.extend_from_slice(soname.as_bytes());
            strings.push(0);
            let value = offset(strings.len());
            strings.extend_from_slice(path.as_bytes());
            strings.push(0);
            offsets.push((key, value));
        }
        let header_len = NEW_HEADER_LEN + entries.len() * NEW_ENTRY_LEN;

        let mut out = Vec::new();
        out.extend_from_slice(NEW_MAGIC);
        out.extend_from_slice(NEW_VERSION);
        out.extend_from_slice(&offset(entries.len()).to_le_bytes());
        out.extend_from_slice(&offset(strings.len()).to_le_bytes());
        out.push(0);
        out.extend_from_slice(&[0, 0, 0]);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&[0u8; 12]);
        assert_eq!(out.len(), NEW_HEADER_LEN);

        for (key, value) in &offsets {
            out.extend_from_slice(&0x0300_0003u32.to_le_bytes());
            out.extend_from_slice(&(key + offset(header_len)).to_le_bytes());
            out.extend_from_slice(&(value + offset(header_len)).to_le_bytes());
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
        bytes.extend_from_slice(&offset(nlibs).to_le_bytes());
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

    fn x86_64() -> Architecture {
        Architecture {
            machine: Machine::X86_64,
            class: ElfClass::Elf64,
            endianness: Endianness::Little,
        }
    }

    fn entry(soname: &str, path: &str) -> CacheEntry {
        CacheEntry {
            soname: soname.to_string(),
            path: PathBuf::from(path),
        }
    }

    /// Read a generated image back the way glibc does: header fields at their
    /// fixed offsets, entries in file order.
    fn header(bytes: &[u8]) -> (u32, u32, u8, u32) {
        (
            read_u32(bytes, 20).unwrap(),
            read_u32(bytes, 24).unwrap(),
            bytes[28],
            read_u32(bytes, 32).unwrap(),
        )
    }

    fn keys_in_file_order(bytes: &[u8]) -> Vec<String> {
        let nlibs = read_u32(bytes, 20).unwrap() as usize;
        (0..nlibs)
            .map(|index| {
                let offset = NEW_HEADER_LEN + index * NEW_ENTRY_LEN;
                read_string(bytes, 0, read_u32(bytes, offset + 4).unwrap()).unwrap()
            })
            .collect()
    }

    #[test]
    fn a_generated_cache_round_trips_through_the_parser() {
        let bytes = build(
            &x86_64(),
            &[
                entry("libcached.so.1", "/opt/cached/libcached.so.1"),
                entry("libc.so.6", "/usr/lib/x86_64-linux-gnu/libc.so.6"),
            ],
        )
        .unwrap();

        let (nlibs, len_strings, flags, extension_offset) = header(&bytes);
        assert_eq!(nlibs, 2);
        assert_eq!(flags, FLAGS_ENDIAN_LITTLE, "glibc checks the byte order");
        assert_eq!(extension_offset, 0, "no extension section is written");
        assert_eq!(
            bytes.len(),
            NEW_HEADER_LEN + 2 * NEW_ENTRY_LEN + len_strings as usize
        );

        let cache = LdCache::parse(&bytes);
        assert_eq!(cache.entry_count(), 2);
        assert_eq!(
            cache.lookup("libcached.so.1"),
            [PathBuf::from("/opt/cached/libcached.so.1")]
        );
        assert!(cache.lookup("libnope.so.1").is_empty());
    }

    /// The loader binary-searches downwards, so the table has to descend.
    #[test]
    fn entries_are_written_in_descending_libcmp_order() {
        let bytes = build(
            &x86_64(),
            &[
                entry("libaaa.so.1", "/a"),
                entry("libzzz.so.1", "/z"),
                entry("libmmm.so.9", "/m9"),
                entry("libmmm.so.10", "/m10"),
            ],
        )
        .unwrap();
        assert_eq!(
            keys_in_file_order(&bytes),
            [
                "libzzz.so.1",
                // 10 is numerically greater than 9, which plain strcmp gets wrong.
                "libmmm.so.10",
                "libmmm.so.9",
                "libaaa.so.1",
            ]
        );
    }

    #[test]
    fn digit_runs_compare_numerically() {
        assert_eq!(libcmp("libfoo.so.9", "libfoo.so.10"), Ordering::Less);
        assert_eq!(libcmp("libfoo.so.10", "libfoo.so.9"), Ordering::Greater);
        assert_eq!(libcmp("libfoo.so.1", "libfoo.so.1"), Ordering::Equal);
        // A prefix is smaller than what extends it.
        assert_eq!(libcmp("libfoo.so", "libfoo.so.1"), Ordering::Less);
        // Digits sort after anything that is not a digit, as in glibc.
        assert_eq!(libcmp("lib1", "liba"), Ordering::Greater);
        assert_eq!(libcmp("liba", "lib1"), Ordering::Less);
        // Absurd digit runs saturate rather than overflow.
        let huge = format!("lib{}", "9".repeat(40));
        assert_eq!(libcmp(&huge, &huge), Ordering::Equal);
    }

    #[test]
    fn identical_entries_collapse_and_output_is_stable() {
        let entries = [
            entry("libc.so.6", "/lib/libc.so.6"),
            entry("libc.so.6", "/lib/libc.so.6"),
            entry("libc.so.6", "/other/libc.so.6"),
        ];
        let bytes = build(&x86_64(), &entries).unwrap();
        assert_eq!(read_u32(&bytes, 20).unwrap(), 2, "duplicates collapse");

        let mut shuffled = entries.to_vec();
        shuffled.reverse();
        assert_eq!(
            build(&x86_64(), &shuffled).unwrap(),
            bytes,
            "input order must not change the image"
        );
    }

    #[test]
    fn an_architecture_without_a_known_cache_id_is_refused() {
        let unsupported = Architecture {
            machine: Machine::RiscV64,
            ..x86_64()
        };
        assert!(build(&unsupported, &[entry("libc.so.6", "/lib/libc.so.6")]).is_none());
        let big_endian = Architecture {
            endianness: Endianness::Big,
            ..x86_64()
        };
        assert!(build(&big_endian, &[entry("libc.so.6", "/lib/libc.so.6")]).is_none());
        // An empty cache is still a valid cache.
        assert!(build(&x86_64(), &[]).is_some());
    }

    #[test]
    fn garbage_degrades_to_an_empty_cache() {
        assert!(LdCache::parse(b"not a cache at all").is_empty());
        assert!(LdCache::parse(&[]).is_empty());
    }

    /// A header may claim any entry count at all; the file size is the only
    /// bound that can be trusted for allocation.
    #[test]
    fn an_absurd_entry_count_allocates_nothing() {
        assert_eq!(
            entry_capacity(&[0u8; 48], 0, NEW_HEADER_LEN, NEW_ENTRY_LEN),
            0
        );
        assert_eq!(
            entry_capacity(&[0u8; 48 + 24], 0, NEW_HEADER_LEN, NEW_ENTRY_LEN),
            1
        );
        assert_eq!(entry_capacity(&[], 4096, NEW_HEADER_LEN, NEW_ENTRY_LEN), 0);

        let mut bytes = new_format(&[("libc.so.6", "/lib/libc.so.6")]);
        bytes[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        let cache = LdCache::parse(&bytes);
        assert_eq!(cache.lookup("libc.so.6"), [PathBuf::from("/lib/libc.so.6")]);
        assert_eq!(
            cache.entry_count(),
            1,
            "parsing stops at the end of the file"
        );
    }

    #[test]
    fn truncated_entries_do_not_panic() {
        let mut bytes = new_format(&[("libc.so.6", "/lib/libc.so.6")]);
        bytes.truncate(NEW_HEADER_LEN + 4);
        assert!(LdCache::parse(&bytes).is_empty());
    }
}
