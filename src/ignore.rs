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
