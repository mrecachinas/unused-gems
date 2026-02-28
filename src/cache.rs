use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

const CACHE_VERSION: u32 = 3; // bumped: nanosecond mtime precision
const CACHE_FILE: &str = ".unused_gems_cache";

/// Return type for cache hits: (requires, modules, bundler_groups, bundler_require_all)
type CacheHit = (Vec<String>, Vec<String>, Vec<String>, bool);

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    mtime_secs: u64,
    mtime_nanos: u32,
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
    pub fn get(&self, rel_path: &str, mtime: SystemTime, size: u64) -> Option<CacheHit> {
        let entry = self.entries.get(rel_path)?;
        let duration = mtime.duration_since(SystemTime::UNIX_EPOCH).ok()?;
        let mtime_secs = duration.as_secs();
        let mtime_nanos = duration.subsec_nanos();
        if entry.mtime_secs == mtime_secs && entry.mtime_nanos == mtime_nanos && entry.size == size
        {
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
    #[allow(clippy::too_many_arguments)]
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
        let duration = mtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let mtime_secs = duration.as_secs();
        let mtime_nanos = duration.subsec_nanos();
        self.entries.insert(
            rel_path,
            CacheEntry {
                mtime_secs,
                mtime_nanos,
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
            entries: self
                .entries
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        CacheEntry {
                            mtime_secs: v.mtime_secs,
                            mtime_nanos: v.mtime_nanos,
                            size: v.size,
                            requires: v.requires.clone(),
                            modules: v.modules.clone(),
                            bundler_groups: v.bundler_groups.clone(),
                            bundler_require_all: v.bundler_require_all,
                        },
                    )
                })
                .collect(),
        };
        if let Ok(data) = bincode::serialize(&cf) {
            let _ = std::fs::write(&self.cache_path, data);
        }
    }

    pub fn is_enabled(&self) -> bool {
        !self.cache_path.as_os_str().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn disabled_cache() {
        let cache = FileCache::disabled();
        assert!(!cache.is_enabled());
        let mtime = SystemTime::now();
        assert!(cache.get("file.rb", mtime, 100).is_none());
    }

    #[test]
    fn put_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = FileCache::load(dir.path());
        assert!(cache.is_enabled());

        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1000000);
        cache.put(
            "app/foo.rb".to_string(),
            mtime,
            42,
            &["foo".to_string()],
            vec!["Foo".to_string()],
            &["default".to_string()],
            false,
        );

        let result = cache.get("app/foo.rb", mtime, 42);
        assert!(result.is_some());
        let (requires, modules, groups, breq) = result.unwrap();
        assert_eq!(requires, vec!["foo"]);
        assert_eq!(modules, vec!["Foo"]);
        assert_eq!(groups, vec!["default"]);
        assert!(!breq);
    }

    #[test]
    fn cache_miss_different_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = FileCache::load(dir.path());
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1000000);
        cache.put("f.rb".to_string(), mtime, 42, &[], vec![], &[], false);

        let other_mtime = mtime + Duration::from_secs(1);
        assert!(cache.get("f.rb", other_mtime, 42).is_none());
    }

    #[test]
    fn cache_miss_different_size() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = FileCache::load(dir.path());
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1000000);
        cache.put("f.rb".to_string(), mtime, 42, &[], vec![], &[], false);

        assert!(cache.get("f.rb", mtime, 99).is_none());
    }

    #[test]
    fn cache_miss_different_nanos() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = FileCache::load(dir.path());
        let mtime = SystemTime::UNIX_EPOCH + Duration::new(1000000, 1000);
        cache.put("f.rb".to_string(), mtime, 42, &[], vec![], &[], false);

        // Use a difference >= 200ns to exceed Windows FILETIME 100ns granularity
        let other_mtime = SystemTime::UNIX_EPOCH + Duration::new(1000000, 1200);
        assert!(cache.get("f.rb", other_mtime, 42).is_none());
    }

    #[test]
    fn save_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1000000);

        {
            let mut cache = FileCache::load(dir.path());
            cache.put(
                "app/bar.rb".to_string(),
                mtime,
                100,
                &["bar".to_string()],
                vec!["Bar".to_string()],
                &[],
                true,
            );
            cache.save();
        }

        let cache = FileCache::load(dir.path());
        let result = cache.get("app/bar.rb", mtime, 100);
        assert!(result.is_some());
        let (requires, modules, _, breq) = result.unwrap();
        assert_eq!(requires, vec!["bar"]);
        assert_eq!(modules, vec!["Bar"]);
        assert!(breq);
    }

    #[test]
    fn version_mismatch_clears_cache() {
        let dir = tempfile::tempdir().unwrap();
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1000000);

        // Write a cache with a wrong version
        let cf = CacheFile {
            version: 999,
            entries: {
                let mut m = HashMap::new();
                m.insert(
                    "test.rb".to_string(),
                    CacheEntry {
                        mtime_secs: 1000000,
                        mtime_nanos: 0,
                        size: 10,
                        requires: vec!["old".to_string()],
                        modules: vec![],
                        bundler_groups: vec![],
                        bundler_require_all: false,
                    },
                );
                m
            },
        };
        let data = bincode::serialize(&cf).unwrap();
        std::fs::write(dir.path().join(CACHE_FILE), data).unwrap();

        let cache = FileCache::load(dir.path());
        assert!(cache.get("test.rb", mtime, 10).is_none());
    }

    #[test]
    fn empty_cache_file_handled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CACHE_FILE), b"").unwrap();
        let cache = FileCache::load(dir.path());
        assert!(cache.is_enabled());
    }

    #[test]
    fn corrupted_cache_file_handled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CACHE_FILE), b"not valid bincode").unwrap();
        let cache = FileCache::load(dir.path());
        assert!(cache.is_enabled());
    }

    #[test]
    fn disabled_cache_save_is_noop() {
        let cache = FileCache::disabled();
        cache.save(); // should not panic or write anything
    }

    #[test]
    fn unmodified_cache_save_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::load(dir.path());
        cache.save();
        // No cache file created since nothing was put
        assert!(!dir.path().join(CACHE_FILE).exists());
    }
}
