//! Content hashing. Every included file is hashed exactly once.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::error::{Result, io};
use crate::graph::Digest;

pub fn sha256_bytes(bytes: &[u8]) -> Digest {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Digest(crate::paths::hex(&hasher.finalize()))
}

/// Hash a file in chunks: an `--include`d file may be arbitrarily large, and
/// nothing here ever needs its contents in memory.
pub fn sha256_file(path: &Path) -> Result<(Digest, u64)> {
    let file = std::fs::File::open(path).map_err(|e| io(path, e))?;
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    loop {
        let chunk = reader.fill_buf().map_err(|e| io(path, e))?;
        if chunk.is_empty() {
            break;
        }
        hasher.update(chunk);
        size += chunk.len() as u64;
        let consumed = chunk.len();
        reader.consume(consumed);
    }
    Ok((Digest(crate::paths::hex(&hasher.finalize())), size))
}

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
        assert_eq!(cache.get(&path).unwrap(), first, "the cached digest is reused");
    }
}
