//! Content hashing. Every included file is hashed exactly once.
//!
//! A digest ties a planned entry, a manifest line and a materialized file
//! together, so it is checked on the way out of here and again on the way into
//! the graph.

use crate::{
    error::{Result, io},
    graph::Digest,
};
use sha2::{Digest as _, Sha256};
use std::{
    collections::HashMap,
    io::BufRead,
    path::{Path, PathBuf},
};

pub fn sha256_bytes(bytes: &[u8]) -> Digest {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = Digest(crate::paths::hex(&hasher.finalize()));
    assert!(digest.is_well_formed());
    digest
}

/// Read buffer for streamed hashing: large enough to amortize the read
/// syscalls, small enough to not show up in RSS.
const HASH_BUFFER_SIZE_BYTES: usize = 64 * 1024;

/// Hash a file in chunks. An `--include`d file can be arbitrarily large and
/// nothing here needs its contents in memory.
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
        // Bounded by the length of the file: every iteration consumes at least
        // one byte, so a short read cannot turn into a spin.
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

/// Hashes each path once, however many plan entries and graph nodes refer to it.
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
        assert!(previous.is_none());
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
