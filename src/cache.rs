use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

const CACHE_VERSION: u32 = 2; // bumped: bincode format
const CACHE_FILE: &str = ".unused_gems_cache";

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    mtime_secs: u64,
    size: u64,
    requires: Vec<String>,
    modules: Vec<String>,
    bundler_groups: Vec<String>,
    bundler_require_all: bool,
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    entries: HashMap<String, CacheEntry>,
}

pub struct FileCache {
    entries: HashMap<String, CacheEntry>,
    dirty: bool,
    cache_path: std::path::PathBuf,
}

impl FileCache {
    pub fn load(dir: &Path) -> Self {
        let cache_path = dir.join(CACHE_FILE);
        let entries = if let Ok(data) = std::fs::read(&cache_path) {
            if let Ok(cf) = bincode::deserialize::<CacheFile>(&data) {
                if cf.version == CACHE_VERSION {
                    cf.entries
                } else {
                    HashMap::new()
                }
            } else {
                HashMap::new()
            }
        } else {
            HashMap::new()
        };
        FileCache {
            entries,
            dirty: false,
            cache_path,
        }
    }

    pub fn disabled() -> Self {
        FileCache {
            entries: HashMap::new(),
            dirty: false,
            cache_path: std::path::PathBuf::new(),
        }
    }

    /// Check cache for a file. Returns Some((requires, modules, bundler_groups, bundler_require_all)) on hit.
    pub fn get(
        &self,
        rel_path: &str,
        mtime: SystemTime,
        size: u64,
    ) -> Option<(Vec<String>, Vec<String>, Vec<String>, bool)> {
        let entry = self.entries.get(rel_path)?;
        let mtime_secs = mtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_secs();
        if entry.mtime_secs == mtime_secs && entry.size == size {
            Some((
                entry.requires.clone(),
                entry.modules.clone(),
                entry.bundler_groups.clone(),
                entry.bundler_require_all,
            ))
        } else {
            None
        }
    }

    /// Insert/update a cache entry.
    pub fn put(
        &mut self,
        rel_path: String,
        mtime: SystemTime,
        size: u64,
        requires: &[String],
        modules: Vec<String>,
        bundler_groups: &[String],
        bundler_require_all: bool,
    ) {
        let mtime_secs = mtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.entries.insert(
            rel_path,
            CacheEntry {
                mtime_secs,
                size,
                requires: requires.to_vec(),
                modules,
                bundler_groups: bundler_groups.to_vec(),
                bundler_require_all,
            },
        );
        self.dirty = true;
    }

    /// Write cache to disk if modified.
    pub fn save(&self) {
        if !self.dirty || self.cache_path.as_os_str().is_empty() {
            return;
        }
        let cf = CacheFile {
            version: CACHE_VERSION,
            entries: self.entries.iter().map(|(k, v)| (k.clone(), CacheEntry {
                mtime_secs: v.mtime_secs,
                size: v.size,
                requires: v.requires.clone(),
                modules: v.modules.clone(),
                bundler_groups: v.bundler_groups.clone(),
                bundler_require_all: v.bundler_require_all,
            })).collect(),
        };
        if let Ok(data) = bincode::serialize(&cf) {
            let _ = std::fs::write(&self.cache_path, data);
        }
    }

    pub fn is_enabled(&self) -> bool {
        !self.cache_path.as_os_str().is_empty()
    }
}
