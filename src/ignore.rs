use std::collections::HashSet;
use std::path::Path;

use crate::adapters::AdapterOverrides;

pub struct IgnoreConfig {
    pub ignored: HashSet<String>,
    pub adapter_overrides: AdapterOverrides,
    pub path: Option<String>,
}

pub fn load_ignore_file(dir: &Path, override_path: Option<&str>) -> IgnoreConfig {
    let path = match override_path {
        Some(p) => resolve_override_path(dir, p),
        None => dir.join(".unused_gems.yml"),
    };

    if !path.exists() {
        if override_path.is_some() {
            eprintln!("Warning: Ignore file not found: {}", path.display());
        }
        return IgnoreConfig {
            ignored: HashSet::new(),
            adapter_overrides: AdapterOverrides::default(),
            path: None,
        };
    }

    let path_str = path.display().to_string();

    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_yaml::from_str::<serde_yaml::Value>(&content) {
            Ok(yaml) => {
                let ignored = parse_ignore_list(&yaml, &path_str);
                let adapter_overrides = parse_adapter_overrides(&yaml, &path_str);
                IgnoreConfig {
                    ignored,
                    adapter_overrides,
                    path: Some(path_str),
                }
            }
            Err(e) => {
                eprintln!("Warning: Could not parse {}: {}", path_str, e);
                IgnoreConfig {
                    ignored: HashSet::new(),
                    adapter_overrides: AdapterOverrides::default(),
                    path: Some(path_str),
                }
            }
        },
        Err(e) => {
            eprintln!("Warning: Could not read {}: {}", path_str, e);
            IgnoreConfig {
                ignored: HashSet::new(),
                adapter_overrides: AdapterOverrides::default(),
                path: Some(path_str),
            }
        }
    }
}

/// Relative `--ignore-file` paths resolve against the scanned directory (like
/// `--gemfile`), falling back to the working directory for compatibility.
fn resolve_override_path(dir: &Path, override_path: &str) -> std::path::PathBuf {
    let path = std::path::PathBuf::from(override_path);
    if path.is_absolute() {
        return path;
    }
    let in_dir = dir.join(&path);
    if in_dir.exists() || !path.exists() {
        in_dir
    } else {
        path
    }
}

fn parse_ignore_list(yaml: &serde_yaml::Value, path: &str) -> HashSet<String> {
    let mut ignored = HashSet::new();
    match yaml.get("ignore") {
        None | Some(serde_yaml::Value::Null) => {}
        Some(serde_yaml::Value::String(name)) => {
            ignored.insert(name.clone());
        }
        Some(serde_yaml::Value::Sequence(items)) => {
            for item in items {
                match item.as_str() {
                    Some(name) => {
                        ignored.insert(name.to_string());
                    }
                    None => eprintln!(
                        "Warning: Ignoring non-string entry in ignore list of {}",
                        path
                    ),
                }
            }
        }
        Some(_) => eprintln!(
            "Warning: Ignoring invalid ignore section in {} (expected a list of gem names)",
            path
        ),
    }
    if !yaml.is_mapping() && !yaml.is_null() {
        eprintln!("Warning: Expected a mapping at the top level of {}", path);
    }
    ignored
}

fn parse_adapter_overrides(yaml: &serde_yaml::Value, path: &str) -> AdapterOverrides {
    let mut overrides = AdapterOverrides::default();
    let Some(adapters) = yaml.get("adapters") else {
        return overrides;
    };
    let Some(mapping) = adapters.as_mapping() else {
        eprintln!("Warning: Ignoring invalid adapters section in {}", path);
        return overrides;
    };

    for (key, value) in mapping {
        let Some(section) = key.as_str() else {
            eprintln!("Warning: Ignoring invalid adapter section key in {}", path);
            continue;
        };
        let Some(entries) = value.as_mapping() else {
            eprintln!(
                "Warning: Ignoring invalid adapters.{} section in {}",
                section, path
            );
            continue;
        };
        let target = match section {
            "database" => &mut overrides.database,
            "cable" => &mut overrides.cable,
            "storage" => &mut overrides.storage,
            "cache" => &mut overrides.cache,
            other => {
                eprintln!(
                    "Warning: Ignoring unknown adapters.{} section in {}",
                    other, path
                );
                continue;
            }
        };
        for (adapter, gem) in entries {
            match (adapter.as_str(), gem.as_str()) {
                (Some(adapter), Some(gem)) if !adapter.is_empty() && !gem.is_empty() => {
                    target.insert(adapter.to_string(), gem.to_string());
                }
                _ => eprintln!(
                    "Warning: Ignoring invalid adapter override in adapters.{} from {}",
                    section, path
                ),
            }
        }
    }

    overrides
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let config = load_ignore_file(dir.path(), None);
        assert!(config.ignored.is_empty());
        assert!(config.path.is_none());
    }

    #[test]
    fn loads_default_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".unused_gems.yml"),
            "ignore:\n  - rails\n  - puma\n",
        )
        .unwrap();
        let config = load_ignore_file(dir.path(), None);
        assert!(config.ignored.contains("rails"));
        assert!(config.ignored.contains("puma"));
        assert_eq!(config.ignored.len(), 2);
        assert!(config.path.is_some());
    }

    #[test]
    fn loads_custom_path() {
        let dir = tempfile::tempdir().unwrap();
        let custom = dir.path().join("custom_ignore.yml");
        std::fs::write(&custom, "ignore:\n  - rspec\n").unwrap();
        let config = load_ignore_file(dir.path(), Some(custom.to_str().unwrap()));
        assert!(config.ignored.contains("rspec"));
        assert_eq!(config.ignored.len(), 1);
    }

    #[test]
    fn empty_ignore_list() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".unused_gems.yml"), "ignore: []\n").unwrap();
        let config = load_ignore_file(dir.path(), None);
        assert!(config.ignored.is_empty());
        assert!(config.path.is_some());
    }

    #[test]
    fn no_ignore_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".unused_gems.yml"), "other_key: value\n").unwrap();
        let config = load_ignore_file(dir.path(), None);
        assert!(config.ignored.is_empty());
        assert!(config.path.is_some());
    }

    #[test]
    fn invalid_yaml_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".unused_gems.yml"), "{{invalid yaml").unwrap();
        let config = load_ignore_file(dir.path(), None);
        assert!(config.ignored.is_empty());
        assert!(config.path.is_some());
    }

    #[test]
    fn custom_path_not_readable() {
        // Create a file but make it a directory to cause a read error
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unreadable.yml");
        std::fs::create_dir(&path).unwrap(); // directory can't be read as file
        let config = load_ignore_file(dir.path(), Some(path.to_str().unwrap()));
        assert!(config.ignored.is_empty());
        assert!(config.path.is_some());
    }

    #[test]
    fn loads_adapter_overrides() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".unused_gems.yml"),
            "adapters:\n  database:\n    clickhouse: clickhouse-activerecord\n  storage:\n    CustomStorage: custom-storage-gem\n  cache:\n    custom_cache_store: custom-cache-gem\n",
        )
        .unwrap();
        let config = load_ignore_file(dir.path(), None);
        assert_eq!(
            config.adapter_overrides.database.get("clickhouse"),
            Some(&"clickhouse-activerecord".to_string())
        );
        assert_eq!(
            config.adapter_overrides.storage.get("CustomStorage"),
            Some(&"custom-storage-gem".to_string())
        );
        assert_eq!(
            config.adapter_overrides.cache.get("custom_cache_store"),
            Some(&"custom-cache-gem".to_string())
        );
    }

    #[test]
    fn relative_override_resolves_against_target_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("custom.yml"), "ignore:\n  - bar-unused\n").unwrap();
        let config = load_ignore_file(dir.path(), Some("custom.yml"));
        assert!(config.ignored.contains("bar-unused"));
        assert_eq!(
            config.path.as_deref(),
            Some(dir.path().join("custom.yml").display().to_string().as_str())
        );
    }

    #[test]
    fn missing_explicit_override_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let config = load_ignore_file(dir.path(), Some("does-not-exist.yml"));
        assert!(config.ignored.is_empty());
        assert!(config.path.is_none());
    }

    #[test]
    fn scalar_ignore_is_single_entry() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".unused_gems.yml"), "ignore: foo-unused\n").unwrap();
        let config = load_ignore_file(dir.path(), None);
        assert_eq!(config.ignored, HashSet::from(["foo-unused".to_string()]));
    }
}
