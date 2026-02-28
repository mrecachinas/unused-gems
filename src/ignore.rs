use std::collections::HashSet;
use std::path::Path;

pub struct IgnoreConfig {
    pub ignored: HashSet<String>,
    pub path: Option<String>,
}

pub fn load_ignore_file(dir: &Path, override_path: Option<&str>) -> IgnoreConfig {
    let path = match override_path {
        Some(p) => std::path::PathBuf::from(p),
        None => dir.join(".unused_gems.yml"),
    };

    if !path.exists() {
        return IgnoreConfig {
            ignored: HashSet::new(),
            path: None,
        };
    }

    let path_str = path.display().to_string();

    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_yaml::from_str::<serde_yaml::Value>(&content) {
            Ok(yaml) => {
                let mut ignored = HashSet::new();
                if let Some(ignore_list) = yaml.get("ignore").and_then(|v| v.as_sequence()) {
                    for item in ignore_list {
                        if let Some(s) = item.as_str() {
                            ignored.insert(s.to_string());
                        }
                    }
                }
                IgnoreConfig {
                    ignored,
                    path: Some(path_str),
                }
            }
            Err(e) => {
                eprintln!("Warning: Could not parse {}: {}", path_str, e);
                IgnoreConfig {
                    ignored: HashSet::new(),
                    path: Some(path_str),
                }
            }
        },
        Err(e) => {
            eprintln!("Warning: Could not read {}: {}", path_str, e);
            IgnoreConfig {
                ignored: HashSet::new(),
                path: Some(path_str),
            }
        }
    }
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
}
