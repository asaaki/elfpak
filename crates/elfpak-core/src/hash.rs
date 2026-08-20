//! Content hashing and a small per-run cache.

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
    Digest(crate::paths::hex(&hasher.finalize()))
}

/// Read buffer for streamed hashing.
const HASH_BUFFER_SIZE_BYTES: usize = 64 * 1024;

/// Hash a file without loading it all at once.
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
        let consumed = chunk.len();
        hasher.update(chunk);
        size += consumed as u64;
        reader.consume(consumed);
    }
    Ok((Digest(crate::paths::hex(&hasher.finalize())), size))
}

/// Hashes a path once per run.
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
            return Ok(hit.clone());
        }
        let value = sha256_file(path)?;
        self.entries.insert(path.to_path_buf(), value.clone());
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
