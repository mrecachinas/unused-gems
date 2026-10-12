use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

const CACHE_VERSION: u32 = 11; // bumped: Ruby shebang files use full lexical filtering
const CACHE_FILE: &str = ".unused_gems_cache";

#[derive(Serialize, Deserialize)]
pub(crate) struct CacheEntry {
    mtime_secs: u64,
    mtime_nanos: u32,
    size: u64,
    #[cfg(not(unix))]
    content_fingerprint: u64,
    pub(crate) requires: Vec<String>,
    pub(crate) modules: Vec<String>,
    pub(crate) bundler_groups: Vec<String>,
    pub(crate) bundler_require_all: bool,
    pub(crate) storage_services: Vec<String>,
}

/// Scan result for one file, produced by the walker on a cache miss and
/// applied to the cache afterwards.
#[derive(Debug, Clone)]
pub struct CacheUpdate {
    pub rel_path: String,
    pub mtime: SystemTime,
    pub size: u64,
    pub requires: Vec<String>,
    pub modules: Vec<String>,
    pub bundler_groups: Vec<String>,
    pub bundler_require_all: bool,
    pub storage_services: Vec<String>,
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

#[derive(Default)]
struct CacheState {
    entries: HashMap<String, CacheEntry>,
    /// Files looked up by the source walk. `None` until the first lookup; when
    /// set, `save` drops entries for files the walk no longer visits.
    visited: Option<HashSet<String>>,
}

pub struct FileCache {
    /// Wrapped in Mutex so we can `take()` (consume) entries on cache hit
    /// without cloning. Lock is held only briefly per file.
    state: Mutex<CacheState>,
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
            state: Mutex::new(CacheState {
                entries,
                visited: None,
            }),
            dirty: std::sync::atomic::AtomicBool::new(false),
            cache_path,
        }
    }

    pub fn disabled() -> Self {
        FileCache {
            state: Mutex::new(CacheState::default()),
            dirty: std::sync::atomic::AtomicBool::new(false),
            cache_path: std::path::PathBuf::new(),
        }
    }

    /// Check cache for a file. Returns a borrowed CacheEntry clone on hit.
    /// Kept for tests / external readers; scanner uses `take()` to avoid clones.
    #[allow(dead_code)]
    pub(crate) fn get(&self, rel_path: &str, mtime: SystemTime, size: u64) -> Option<CacheEntry> {
        let state = self.state.lock().unwrap();
        let entry = state.entries.get(rel_path)?;
        let duration = mtime.duration_since(SystemTime::UNIX_EPOCH).ok()?;
        let mtime_secs = duration.as_secs();
        let mtime_nanos = duration.subsec_nanos();
        if entry.mtime_secs == mtime_secs && entry.mtime_nanos == mtime_nanos && entry.size == size
        {
            Some(CacheEntry {
                mtime_secs: entry.mtime_secs,
                mtime_nanos: entry.mtime_nanos,
                size: entry.size,
                #[cfg(not(unix))]
                content_fingerprint: entry.content_fingerprint,
                requires: entry.requires.clone(),
                modules: entry.modules.clone(),
                bundler_groups: entry.bundler_groups.clone(),
                bundler_require_all: entry.bundler_require_all,
                storage_services: entry.storage_services.clone(),
            })
        } else {
            None
        }
    }

    /// Consume the cache entry on hit (removes from internal map, returns owned CacheEntry).
    /// This avoids cloning the inner Vec<String>s on the hot path.
    /// If validation fails, the entry is left in place.
    /// Every lookup records `rel_path` as visited so `save` can prune entries
    /// for files that no longer exist (or are no longer scanned).
    pub(crate) fn take(
        &self,
        rel_path: &str,
        mtime: SystemTime,
        size: u64,
        _path: &Path,
    ) -> Option<CacheEntry> {
        let mut state = self.state.lock().unwrap();
        state
            .visited
            .get_or_insert_with(HashSet::new)
            .insert(rel_path.to_string());
        // Validate before removing
        let entry = state.entries.get(rel_path)?;
        let duration = mtime.duration_since(SystemTime::UNIX_EPOCH).ok()?;
        let mtime_secs = duration.as_secs();
        let mtime_nanos = duration.subsec_nanos();
        if entry.mtime_secs != mtime_secs || entry.mtime_nanos != mtime_nanos || entry.size != size
        {
            return None;
        }
        #[cfg(not(unix))]
        if entry.content_fingerprint != content_fingerprint(_path) {
            return None;
        }
        state.entries.remove(rel_path)
    }

    /// Re-insert a previously `take()`n entry back into the cache without marking
    /// it dirty. Used by the scanner cache-hit path to preserve durability while
    /// avoiding spurious cache_buf round-trips.
    pub(crate) fn reinsert(&self, rel_path: String, entry: CacheEntry) {
        self.state.lock().unwrap().entries.insert(rel_path, entry);
    }

    /// Insert/update a cache entry.
    pub fn put(&mut self, update: CacheUpdate) {
        #[cfg(not(unix))]
        let content_fingerprint = self
            .cache_path
            .parent()
            .map(|root| content_fingerprint(&root.join(&update.rel_path)))
            .unwrap_or_default();
        let duration = update
            .mtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let mtime_secs = duration.as_secs();
        let mtime_nanos = duration.subsec_nanos();
        self.state.lock().unwrap().entries.insert(
            update.rel_path,
            CacheEntry {
                mtime_secs,
                mtime_nanos,
                size: update.size,
                #[cfg(not(unix))]
                content_fingerprint,
                requires: update.requires,
                modules: update.modules,
                bundler_groups: update.bundler_groups,
                bundler_require_all: update.bundler_require_all,
                storage_services: update.storage_services,
            },
        );
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Write cache to disk if modified, dropping entries for files the walk did
    /// not visit. The file is written to a temporary sibling and renamed into
    /// place so readers never observe a partially written cache.
    pub fn save(&self) {
        if self.cache_path.as_os_str().is_empty() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        let CacheState { entries, visited } = &mut *state;
        let mut pruned = false;
        if let Some(visited) = visited {
            let before = entries.len();
            entries.retain(|rel_path, _| visited.contains(rel_path));
            pruned = entries.len() != before;
        }
        if !pruned && !self.dirty.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let cf = CacheFileRef {
            version: CACHE_VERSION,
            entries,
        };
        let data = match bincode::serialize(&cf) {
            Ok(data) => data,
            Err(err) => {
                eprintln!("Failed to serialize cache: {err}");
                return;
            }
        };
        for attempt in 0..100 {
            let name = if attempt == 0 {
                format!("{CACHE_FILE}.{}.tmp", std::process::id())
            } else {
                format!("{CACHE_FILE}.{}.{attempt}.tmp", std::process::id())
            };
            let tmp_path = self.cache_path.with_file_name(name);
            let mut file = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)
            {
                Ok(file) => file,
                Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    eprintln!(
                        "Failed to create cache temp file {}: {err}",
                        tmp_path.display()
                    );
                    return;
                }
            };
            let written = file.write_all(&data);
            drop(file);
            let written = written.and_then(|()| std::fs::rename(&tmp_path, &self.cache_path));
            // Cache writes are best-effort (e.g. read-only checkouts).
            if let Err(err) = written {
                eprintln!("Failed to save cache {}: {err}", self.cache_path.display());
                if let Err(err) = std::fs::remove_file(&tmp_path) {
                    eprintln!(
                        "Failed to remove cache temp file {}: {err}",
                        tmp_path.display()
                    );
                }
            }
            return;
        }
        eprintln!(
            "Failed to save cache {}: exhausted temp file name retries",
            self.cache_path.display()
        );
    }

    pub fn is_enabled(&self) -> bool {
        !self.cache_path.as_os_str().is_empty()
    }
}

pub(crate) fn cache_timestamp(metadata: &std::fs::Metadata) -> SystemTime {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let secs = metadata.ctime().max(0) as u64;
        let nanos = metadata.ctime_nsec().clamp(0, 999_999_999) as u32;
        SystemTime::UNIX_EPOCH + Duration::new(secs, nanos)
    }

    #[cfg(not(unix))]
    {
        metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH)
    }
}

#[cfg(not(unix))]
fn content_fingerprint(path: &Path) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    std::fs::read(path)
        .map(|bytes| {
            bytes.iter().fold(OFFSET, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
            })
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn update(rel_path: &str, mtime: SystemTime, size: u64) -> CacheUpdate {
        CacheUpdate {
            rel_path: rel_path.to_string(),
            mtime,
            size,
            requires: Vec::new(),
            modules: Vec::new(),
            bundler_groups: Vec::new(),
            bundler_require_all: false,
            storage_services: Vec::new(),
        }
    }

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
        cache.put(CacheUpdate {
            requires: vec!["foo".to_string()],
            modules: vec!["Foo".to_string()],
            bundler_groups: vec!["default".to_string()],
            storage_services: vec!["amazon".to_string()],
            ..update("app/foo.rb", mtime, 42)
        });

        let entry = cache.get("app/foo.rb", mtime, 42);
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.requires, vec!["foo"]);
        assert_eq!(entry.modules, vec!["Foo"]);
        assert_eq!(entry.bundler_groups, vec!["default"]);
        assert!(!entry.bundler_require_all);
        assert_eq!(entry.storage_services, vec!["amazon"]);
    }

    #[test]
    fn cache_miss_different_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = FileCache::load(dir.path());
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1000000);
        cache.put(update("f.rb", mtime, 42));

        let other_mtime = mtime + Duration::from_secs(1);
        assert!(cache.get("f.rb", other_mtime, 42).is_none());
    }

    #[test]
    fn cache_miss_different_size() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = FileCache::load(dir.path());
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1000000);
        cache.put(update("f.rb", mtime, 42));

        assert!(cache.get("f.rb", mtime, 99).is_none());
    }

    #[test]
    fn cache_miss_different_nanos() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = FileCache::load(dir.path());
        let mtime = SystemTime::UNIX_EPOCH + Duration::new(1000000, 1000);
        cache.put(update("f.rb", mtime, 42));

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
            cache.put(CacheUpdate {
                requires: vec!["bar".to_string()],
                modules: vec!["Bar".to_string()],
                bundler_require_all: true,
                ..update("app/bar.rb", mtime, 100)
            });
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
                        #[cfg(not(unix))]
                        content_fingerprint: 0,
                        requires: vec!["old".to_string()],
                        modules: vec![],
                        bundler_groups: vec![],
                        bundler_require_all: false,
                        storage_services: vec![],
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
    fn save_prunes_entries_for_files_not_visited() {
        let dir = tempfile::tempdir().unwrap();
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1000000);
        {
            let mut cache = FileCache::load(dir.path());
            cache.put(update("kept.rb", mtime, 1));
            cache.put(update("deleted.rb", mtime, 1));
            cache.save();
        }

        // A later walk only looks up kept.rb (deleted.rb no longer exists).
        let cache = FileCache::load(dir.path());
        let hit = cache
            .take("kept.rb", mtime, 1, Path::new("kept.rb"))
            .unwrap();
        cache.reinsert("kept.rb".to_string(), hit);
        cache.save();

        let cache = FileCache::load(dir.path());
        assert!(cache.get("kept.rb", mtime, 1).is_some());
        assert!(cache.get("deleted.rb", mtime, 1).is_none());
    }

    #[test]
    fn save_without_walk_does_not_prune() {
        let dir = tempfile::tempdir().unwrap();
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1000000);
        {
            let mut cache = FileCache::load(dir.path());
            cache.put(update("a.rb", mtime, 1));
            cache.save();
        }
        let mut cache = FileCache::load(dir.path());
        cache.put(update("b.rb", mtime, 1));
        cache.save();

        let cache = FileCache::load(dir.path());
        assert!(cache.get("a.rb", mtime, 1).is_some());
        assert!(cache.get("b.rb", mtime, 1).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn save_replaces_cache_file_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1000000);
        let mut cache = FileCache::load(dir.path());
        cache.put(update("a.rb", mtime, 1));
        cache.save();

        // A hard link keeps pointing at the old inode only if the cache was
        // replaced via rename rather than truncated and rewritten in place.
        let snapshot = dir.path().join("snapshot");
        std::fs::hard_link(dir.path().join(CACHE_FILE), &snapshot).unwrap();
        let before = std::fs::read(&snapshot).unwrap();
        cache.put(update("b.rb", mtime, 2));
        cache.save();

        assert_eq!(std::fs::read(&snapshot).unwrap(), before);
        assert_ne!(std::fs::read(dir.path().join(CACHE_FILE)).unwrap(), before);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn unmodified_cache_save_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::load(dir.path());
        cache.save();
        // No cache file created since nothing was put
        assert!(!dir.path().join(CACHE_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn save_does_not_follow_temp_file_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"keep intact").unwrap();
        let tmp_path = dir
            .path()
            .join(format!("{CACHE_FILE}.{}.tmp", std::process::id()));
        std::os::unix::fs::symlink(&target, &tmp_path).unwrap();

        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1000000);
        let mut cache = FileCache::load(dir.path());
        cache.put(update("a.rb", mtime, 1));
        cache.save();

        assert_eq!(std::fs::read(&target).unwrap(), b"keep intact");
        assert_eq!(std::fs::read_link(&tmp_path).unwrap(), target);
        assert!(FileCache::load(dir.path()).get("a.rb", mtime, 1).is_some());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3);
    }

    #[test]
    fn save_cleans_up_temp_file_on_rename_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(CACHE_FILE)).unwrap();
        let mut cache = FileCache::load(dir.path());
        cache.put(update("a.rb", SystemTime::UNIX_EPOCH, 1));
        cache.save();

        assert!(dir.path().join(CACHE_FILE).is_dir());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
