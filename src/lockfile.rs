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
            if let Some(name) = line.trim().split_whitespace().next() {
                current_gem = Some(name.to_string());
                deps.entry(name.to_string()).or_default();
            }
        }
        // Dependency: "      dep_name (version_constraint)"
        else if line.starts_with("      ") {
            if let Some(ref gem) = current_gem {
                if let Some(dep_name) = line.trim().split_whitespace().next() {
                    deps.entry(gem.clone()).or_default().push(dep_name.to_string());
                }
            }
        }
    }

    deps
}

pub fn count_transitive_deps(
    gem_name: &str,
    lock_deps: &HashMap<String, Vec<String>>,
) -> usize {
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
