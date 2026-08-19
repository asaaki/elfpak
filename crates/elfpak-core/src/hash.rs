//! Content hashing. Every included file is hashed exactly once.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::error::{Result, io};
use crate::graph::Digest;

pub fn sha256_bytes(bytes: &[u8]) -> Digest {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Digest(crate::paths::hex(&hasher.finalize()))
}

pub fn sha256_file(path: &Path) -> Result<(Digest, u64)> {
    let bytes = std::fs::read(path).map_err(|e| io(path, e))?;
    Ok((sha256_bytes(&bytes), bytes.len() as u64))
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
