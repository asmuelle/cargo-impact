//! Content-hash cache for incremental analysis.
//!
//! Caches computed results keyed on file content hashes so re-parsing
//! unchanged files can be skipped across `cargo-impact` invocations.
//!
//! The cache lives at `target/cargo-impact/cache/` within the workspace.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Expensive-operation cache using content hashing.
///
/// Example: caching `syn::File` parse results by file content hash.
/// On subsequent runs, if a file's SHA-256 hasn't changed, the cached
/// AST is reused without re-parsing.
pub struct ContentHashCache<V> {
    /// Hash -> value mapping.
    store: HashMap<String, V>,
    /// Cache directory for persistence.
    cache_dir: Option<PathBuf>,
    /// Cache name (used as filename prefix).
    name: String,
}

impl<V: serde::de::DeserializeOwned + serde::Serialize> ContentHashCache<V> {
    /// Create a new content-hash cache.
    pub fn new(workspace_root: &Path, name: &str) -> Self {
        let cache_dir = workspace_root
            .join("target")
            .join("cargo-impact")
            .join("cache");
        let store = Self::load(&cache_dir, name).unwrap_or_default();
        Self {
            store,
            cache_dir: Some(cache_dir),
            name: name.to_string(),
        }
    }

    /// Get a cached value by content hash.
    pub fn get(&self, hash: &str) -> Option<&V> {
        self.store.get(hash)
    }

    /// Insert a value into the cache.
    pub fn insert(&mut self, hash: String, value: V) {
        self.store.insert(hash, value);
    }

    /// Save cache to disk.
    pub fn save(&self) {
        let Some(ref dir) = self.cache_dir else {
            return;
        };
        let _ = std::fs::create_dir_all(dir);
        let path = dir.join(format!("{}.json", self.name));
        if let Ok(json) = serde_json::to_string(&self.store) {
            let _ = std::fs::write(&path, json);
        }
    }

    fn load(dir: &Path, name: &str) -> Option<HashMap<String, V>> {
        let path = dir.join(format!("{}.json", name));
        let contents = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&contents).ok()
    }
}

/// Compute SHA-256 hash of a file's contents.
pub fn file_hash(path: &Path) -> Option<String> {
    let contents = std::fs::read(path).ok()?;
    let hash = Sha256::digest(&contents);
    Some(format!("{:x}", hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_cache_round_trips_values() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cache = ContentHashCache::new(tmp.path(), "test-cache");
        cache.insert("abc".to_string(), vec!["symbol".to_string()]);
        cache.save();

        let reloaded = ContentHashCache::<Vec<String>>::new(tmp.path(), "test-cache");
        assert_eq!(reloaded.get("abc"), Some(&vec!["symbol".to_string()]));
    }

    #[test]
    fn file_hash_changes_with_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("test.rs");
        std::fs::write(&file, "fn a() {}").unwrap();
        let first = file_hash(&file).unwrap();

        std::fs::write(&file, "fn b() {}").unwrap();
        let second = file_hash(&file).unwrap();
        assert_ne!(first, second);
    }
}
