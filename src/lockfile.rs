use std::collections::{HashMap, HashSet};
use std::path::Path;

pub fn parse_gemfile_lock(dir: &Path) -> HashMap<String, Vec<String>> {
    let lock_path = dir.join("Gemfile.lock");
    let content = match std::fs::read_to_string(lock_path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    let mut deps: HashMap<String, Vec<String>> = HashMap::new();
    let mut current_gem: Option<String> = None;
    let mut in_specs = false;

    for line in content.lines() {
        if line.starts_with("  specs:") {
            in_specs = true;
            current_gem = None;
            continue;
        }

        if in_specs && !line.is_empty() && line.as_bytes()[0].is_ascii_uppercase() {
            in_specs = false;
            current_gem = None;
            continue;
        }

        if !in_specs {
            continue;
        }

        // Top-level gem: "    gem_name (version)"
        if line.starts_with("    ") && !line.starts_with("      ") {
            if let Some(name) = line.split_whitespace().next() {
                current_gem = Some(name.to_string());
                deps.entry(name.to_string()).or_default();
            }
        }
        // Dependency: "      dep_name (version_constraint)"
        else if line.starts_with("      ") {
            if let Some(ref gem) = current_gem {
                if let Some(dep_name) = line.split_whitespace().next() {
                    deps.entry(gem.clone())
                        .or_default()
                        .push(dep_name.to_string());
                }
            }
        }
    }

    deps
}

pub fn count_transitive_deps(gem_name: &str, lock_deps: &HashMap<String, Vec<String>>) -> usize {
    let mut visited = HashSet::new();
    count_inner(gem_name, lock_deps, &mut visited)
}

fn count_inner(
    gem_name: &str,
    lock_deps: &HashMap<String, Vec<String>>,
    visited: &mut HashSet<String>,
) -> usize {
    if visited.contains(gem_name) {
        return 0;
    }
    visited.insert(gem_name.to_string());

    let direct = match lock_deps.get(gem_name) {
        Some(deps) => deps,
        None => return 0,
    };

    let mut count = direct.len();
    for d in direct {
        count += count_inner(d, lock_deps, visited);
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile.lock"),
            "\
GEM
  remote: https://rubygems.org/
  specs:
    actionpack (7.1.0)
      activesupport (= 7.1.0)
      rack (~> 3.0)
    activesupport (7.1.0)
      i18n (>= 1.6)
    i18n (1.14.1)
    rack (3.0.8)

PLATFORMS
  ruby
",
        )
        .unwrap();
        let deps = parse_gemfile_lock(dir.path());
        assert_eq!(deps["actionpack"], vec!["activesupport", "rack"]);
        assert_eq!(deps["activesupport"], vec!["i18n"]);
        assert!(deps["i18n"].is_empty());
        assert!(deps["rack"].is_empty());
    }

    #[test]
    fn missing_lockfile_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let deps = parse_gemfile_lock(dir.path());
        assert!(deps.is_empty());
    }

    #[test]
    fn count_transitive_deps_basic() {
        let mut deps = HashMap::new();
        deps.insert("a".to_string(), vec!["b".to_string(), "c".to_string()]);
        deps.insert("b".to_string(), vec!["d".to_string()]);
        deps.insert("c".to_string(), vec![]);
        deps.insert("d".to_string(), vec![]);
        // a -> b, c; b -> d; total transitive = 3 (b, c, d)
        assert_eq!(count_transitive_deps("a", &deps), 3);
    }

    #[test]
    fn count_transitive_deps_circular() {
        let mut deps = HashMap::new();
        deps.insert("a".to_string(), vec!["b".to_string()]);
        deps.insert("b".to_string(), vec!["a".to_string()]);
        // a -> b (1 direct), b -> a (visited, skip) = 2 total visits but 1 direct dep from b
        assert_eq!(count_transitive_deps("a", &deps), 2);
    }

    #[test]
    fn count_transitive_deps_missing() {
        let deps = HashMap::new();
        assert_eq!(count_transitive_deps("nonexistent", &deps), 0);
    }

    #[test]
    fn parse_multiple_gem_sections() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile.lock"),
            "\
GEM
  remote: https://rubygems.org/
  specs:
    rails (7.1.0)
      actionpack (= 7.1.0)
    actionpack (7.1.0)

BUNDLED WITH
   2.5.0
",
        )
        .unwrap();
        let deps = parse_gemfile_lock(dir.path());
        assert_eq!(deps["rails"], vec!["actionpack"]);
        assert!(deps["actionpack"].is_empty());
    }
}
