//! Content hashing. Every included file is hashed exactly once.
//!
//! A digest is the only thing that ties a planned entry, a manifest line and a
//! materialized file together, so digests are asserted well-formed on the way
//! out of this module and again on the way into the graph.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::error::{Result, io};
use crate::graph::Digest;

pub fn sha256_bytes(bytes: &[u8]) -> Digest {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = Digest(crate::paths::hex(&hasher.finalize()));
    assert!(digest.is_well_formed());
    digest
}

/// Read buffer for streamed hashing. Large enough to amortize the syscall over
/// a whole page cluster, small enough that it stays a rounding error in RSS.
const HASH_BUFFER_SIZE_BYTES: usize = 64 * 1024;

/// Hash a file in chunks: an `--include`d file may be arbitrarily large, and
/// nothing here ever needs its contents in memory.
pub fn sha256_file(path: &Path) -> Result<(Digest, u64)> {
    let file = std::fs::File::open(path).map_err(|e| io(path, e))?;
    let mut reader = std::io::BufReader::with_capacity(HASH_BUFFER_SIZE_BYTES, file);
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    loop {
        let chunk = reader.fill_buf().map_err(|e| io(path, e))?;
        if chunk.is_empty() {
            break;
        }
        // Every iteration consumes at least one byte, so the loop is bounded by
        // the length of the file and a short read cannot turn into a spin.
        let consumed = chunk.len();
        assert!(consumed > 0);
        assert!(consumed <= HASH_BUFFER_SIZE_BYTES);
        hasher.update(chunk);
        size += consumed as u64;
        reader.consume(consumed);
    }
    let digest = Digest(crate::paths::hex(&hasher.finalize()));
    assert!(digest.is_well_formed());
    Ok((digest, size))
}

/// Every included file is hashed exactly once, however many plan entries and
/// graph nodes end up referring to it.
#[derive(Debug, Default)]
pub struct DigestCache {
    entries: HashMap<PathBuf, (Digest, u64)>,
}

impl DigestCache {
    pub fn new() -> DigestCache {
        DigestCache::default()
    }

    pub fn get(&mut self, path: &Path) -> Result<(Digest, u64)> {
        if let Some(hit) = self.entries.get(path) {
            assert!(hit.0.is_well_formed());
            return Ok(hit.clone());
        }
        let value = sha256_file(path)?;
        assert!(value.0.is_well_formed());
        let previous = self.entries.insert(path.to_path_buf(), value.clone());
        assert!(previous.is_none(), "a miss cannot have had an entry");
        Ok(value)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_and_in_memory_hashing_agree() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("blob");
        // Larger than the read buffer, so more than one chunk is hashed.
        let bytes: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &bytes).unwrap();

        let (digest, size) = sha256_file(&path).unwrap();
        assert_eq!(size, bytes.len() as u64);
        assert_eq!(digest, sha256_bytes(&bytes));

        let empty = temp.path().join("empty");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(sha256_file(&empty).unwrap(), (sha256_bytes(b""), 0));
    }

    #[test]
    fn the_cache_hashes_each_path_once() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("blob");
        std::fs::write(&path, b"one").unwrap();

        let mut cache = DigestCache::new();
        let first = cache.get(&path).unwrap();
        std::fs::write(&path, b"two").unwrap();
        assert_eq!(
            cache.get(&path).unwrap(),
            first,
            "the cached digest is reused"
        );
    }
}
