//! Content hashing and a small per-run cache.

use crate::{
    error::{Error, Result, io},
    graph::Digest,
};
use sha2::{Digest as _, Sha256};
use std::{
    collections::HashMap,
    io::{BufRead, Read},
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

/// A reader which records the digest and number of bytes it has yielded.
///
/// Output backends use this while copying planned source files so the bytes
/// that were actually written are checked against the immutable plan.
#[derive(Debug)]
pub struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
    size: u64,
}

impl<R> HashingReader<R> {
    pub fn new(inner: R) -> HashingReader<R> {
        HashingReader {
            inner,
            hasher: Sha256::new(),
            size: 0,
        }
    }

    pub fn finish(self) -> (Digest, u64) {
        (
            Digest(crate::paths::hex(&self.hasher.finalize())),
            self.size,
        )
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..read]);
        self.size += read as u64;
        Ok(read)
    }
}

/// Fail when bytes copied from a source no longer match the plan.
pub fn ensure_matches_plan(
    path: &Path,
    expected_digest: &Digest,
    expected_size: u64,
    actual_digest: Digest,
    actual_size: u64,
) -> Result<()> {
    if actual_digest == *expected_digest && actual_size == expected_size {
        return Ok(());
    }
    Err(Error::SourceChanged {
        path: path.to_path_buf(),
        expected_digest: expected_digest.0.clone(),
        expected_size,
        actual_digest: actual_digest.0,
        actual_size,
    })
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
