use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

const CACHE_VERSION: u32 = 4; // bumped: Bundler.require splat detection + middleware string refs
const CACHE_FILE: &str = ".unused_gems_cache";

#[derive(Serialize, Deserialize)]
pub(crate) struct CacheEntry {
    mtime_secs: u64,
    mtime_nanos: u32,
    size: u64,
    pub(crate) requires: Vec<String>,
    pub(crate) modules: Vec<String>,
    pub(crate) bundler_groups: Vec<String>,
    pub(crate) bundler_require_all: bool,
}

#[derive(Deserialize, Serialize)]
struct CacheFile {
    version: u32,
    entries: HashMap<String, CacheEntry>,
}

#[derive(Serialize)]
struct CacheFileRef<'a> {
    version: u32,
    entries: &'a HashMap<String, CacheEntry>,
}

pub struct FileCache {
    /// Wrapped in Mutex so we can `take()` (consume) entries on cache hit
    /// without cloning. Lock is held only briefly per file.
    entries: Mutex<HashMap<String, CacheEntry>>,
    dirty: std::sync::atomic::AtomicBool,
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
            entries: Mutex::new(entries),
            dirty: std::sync::atomic::AtomicBool::new(false),
            cache_path,
        }
    }

    pub fn disabled() -> Self {
        FileCache {
            entries: Mutex::new(HashMap::new()),
            dirty: std::sync::atomic::AtomicBool::new(false),
            cache_path: std::path::PathBuf::new(),
        }
    }

    /// Check cache for a file. Returns a borrowed CacheEntry clone on hit.
    /// Kept for tests / external readers; scanner uses `take()` to avoid clones.
    #[allow(dead_code)]
    pub fn get(&self, rel_path: &str, mtime: SystemTime, size: u64) -> Option<CacheEntry> {
        let entries = self.entries.lock().unwrap();
        let entry = entries.get(rel_path)?;
        let duration = mtime.duration_since(SystemTime::UNIX_EPOCH).ok()?;
        let mtime_secs = duration.as_secs();
        let mtime_nanos = duration.subsec_nanos();
        if entry.mtime_secs == mtime_secs && entry.mtime_nanos == mtime_nanos && entry.size == size
        {
            Some(CacheEntry {
                mtime_secs: entry.mtime_secs,
                mtime_nanos: entry.mtime_nanos,
                size: entry.size,
                requires: entry.requires.clone(),
                modules: entry.modules.clone(),
                bundler_groups: entry.bundler_groups.clone(),
                bundler_require_all: entry.bundler_require_all,
            })
        } else {
            None
        }
    }

    /// Consume the cache entry on hit (removes from internal map, returns owned CacheEntry).
    /// This avoids cloning the inner Vec<String>s on the hot path.
    /// If validation fails, the entry is left in place.
    pub fn take(&self, rel_path: &str, mtime: SystemTime, size: u64) -> Option<CacheEntry> {
        let mut entries = self.entries.lock().unwrap();
        // Validate before removing
        let entry = entries.get(rel_path)?;
        let duration = mtime.duration_since(SystemTime::UNIX_EPOCH).ok()?;
        let mtime_secs = duration.as_secs();
        let mtime_nanos = duration.subsec_nanos();
        if entry.mtime_secs != mtime_secs || entry.mtime_nanos != mtime_nanos || entry.size != size
        {
            return None;
        }
        entries.remove(rel_path)
    }

    /// Re-insert a previously `take()`n entry back into the cache without marking
    /// it dirty. Used by the scanner cache-hit path to preserve durability while
    /// avoiding spurious cache_buf round-trips.
    pub(crate) fn reinsert(&self, rel_path: String, entry: CacheEntry) {
        self.entries.lock().unwrap().insert(rel_path, entry);
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
        self.entries.lock().unwrap().insert(
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
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Write cache to disk if modified.
    pub fn save(&self) {
        if !self.dirty.load(std::sync::atomic::Ordering::Relaxed)
            || self.cache_path.as_os_str().is_empty()
        {
            return;
        }
        let entries = self.entries.lock().unwrap();
        let cf = CacheFileRef {
            version: CACHE_VERSION,
            entries: &entries,
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

        let entry = cache.get("app/foo.rb", mtime, 42);
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.requires, vec!["foo"]);
        assert_eq!(entry.modules, vec!["Foo"]);
        assert_eq!(entry.bundler_groups, vec!["default"]);
        assert!(!entry.bundler_require_all);
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
        let entry = cache.get("app/bar.rb", mtime, 100);
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.requires, vec!["bar"]);
        assert_eq!(entry.modules, vec!["Bar"]);
        assert!(entry.bundler_require_all);
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
