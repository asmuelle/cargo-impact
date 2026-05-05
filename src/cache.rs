//! Content-hash cache for incremental analysis.
//!
//! Caches computed results keyed on file content hashes so re-parsing
//! unchanged files can be skipped across `cargo-impact` invocations.
//!
//! The cache lives at `target/cargo-impact/cache/` within the workspace.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use sha2::{Digest, Sha256};

/// Lightweight cache for file modification tracking.
///
/// Stores `file_path -> (content_hash, timestamp)` mappings so
/// the analyzer can skip re-parsing files whose content hasn't changed
/// since the last run.
#[derive(Debug, Clone, Default)]
pub struct FileCache {
    /// Maps workspace-relative file path to its last-known hash.
    entries: HashMap<PathBuf, CachedEntry>,
    /// Cache directory path.
    cache_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CachedEntry {
    hash: String,
    /// Last known modification time.
    mtime: u64,
}

impl FileCache {
    /// Create a new cache, loading any saved state from disk.
    pub fn new(workspace_root: &Path) -> Self {
        let cache_dir = workspace_root.join("target").join("cargo-impact").join("cache");
        let entries = Self::load_from_disk(&cache_dir).unwrap_or_default();
        Self {
            entries,
            cache_dir: Some(cache_dir),
        }
    }

    /// Check if a file has changed since it was last cached.
    /// Returns `true` if the file needs re-parsing (new or modified).
    pub fn has_changed(&self, workspace_root: &Path, file: &Path) -> bool {
        let full_path = workspace_root.join(file);
        match self.compute_hash(&full_path) {
            Some(hash) => {
                match self.entries.get(file) {
                    Some(entry) => entry.hash != hash,
                    None => true, // Not cached — needs parsing
                }
            }
            None => false, // Can't read file — skip
        }
    }

    /// Record a file's current state in the cache.
    pub fn record(&mut self, workspace_root: &Path, file: &Path) {
        let full_path = workspace_root.join(file);
        if let Some(hash) = self.compute_hash(&full_path) {
            let mtime = std::fs::metadata(&full_path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            self.entries.insert(
                file.to_path_buf(),
                CachedEntry { hash, mtime },
            );
        }
    }

    /// Save cache state to disk.
    pub fn save(&self) {
        let Some(ref cache_dir) = self.cache_dir else { return };
        let _ = std::fs::create_dir_all(cache_dir);
        let path = cache_dir.join("file-cache.json");
        if let Ok(json) = serde_json::to_string(&self.entries) {
            let _ = std::fs::write(&path, json);
        }
    }

    fn compute_hash(&self, path: &Path) -> Option<String> {
        let contents = std::fs::read(path).ok()?;
        let hash = Sha256::digest(&contents);
        Some(format!("{:x}", hash))
    }

    fn load_from_disk(cache_dir: &Path) -> Option<HashMap<PathBuf, CachedEntry>> {
        let path = cache_dir.join("file-cache.json");
        let contents = std::fs::read_to_string(&path).ok()?;
        // Custom deserialization because PathBuf keys in JSON are strings
        let raw: HashMap<String, serde_json::Value> = serde_json::from_str(&contents).ok()?;
        let mut entries = HashMap::new();
        for (k, v) in raw {
            let entry: CachedEntry = serde_json::from_value(v).ok()?;
            entries.insert(PathBuf::from(k), entry);
        }
        Some(entries)
    }
}

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
        let cache_dir = workspace_root.join("target").join("cargo-impact").join("cache");
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
        let Some(ref dir) = self.cache_dir else { return };
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
    fn cache_tracks_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("test.rs");
        std::fs::write(&file, "fn a() {}").unwrap();

        let mut cache = FileCache::new(tmp.path());
        assert!(cache.has_changed(tmp.path(), Path::new("test.rs")));

        cache.record(tmp.path(), Path::new("test.rs"));
        assert!(!cache.has_changed(tmp.path(), Path::new("test.rs")));

        std::fs::write(&file, "fn b() {}").unwrap();
        assert!(cache.has_changed(tmp.path(), Path::new("test.rs")));
    }

    #[test]
    fn save_and_reload_preserves_state() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("test.rs");
        std::fs::write(&file, "fn a() {}").unwrap();

        let mut cache = FileCache::new(tmp.path());
        cache.record(tmp.path(), Path::new("test.rs"));
        cache.save();

        let cache2 = FileCache::new(tmp.path());
        assert!(!cache2.has_changed(tmp.path(), Path::new("test.rs")));
    }
}
