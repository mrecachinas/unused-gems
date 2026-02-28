use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use regex::Regex;

static MODULE_DECL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^\s*(?:module|class)\s+([A-Z]\w+)").unwrap());

/// Compare two gem version strings numerically (e.g. "1.16.8" > "1.9.1").
fn compare_gem_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> Vec<u64> {
        s.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect()
    };
    parse(a).cmp(&parse(b))
}

/// Extract version from a gem directory name, handling platform suffixes.
fn extract_gem_version(dir_name: &str) -> Option<String> {
    let mut search_end = dir_name.len();
    while let Some(idx) = dir_name[..search_end].rfind('-') {
        let ver = &dir_name[idx + 1..search_end];
        if ver.starts_with(|c: char| c.is_ascii_digit()) && ver.contains('.') {
            return Some(ver.to_string());
        }
        search_end = idx;
    }
    // Fallback: simple rfind
    if let Some(idx) = dir_name.rfind('-') {
        let ver = &dir_name[idx + 1..];
        if ver.starts_with(|c: char| c.is_ascii_digit()) {
            return Some(ver.to_string());
        }
    }
    None
}

/// Find gem installation directories. Tries fast filesystem probes first,
/// only falling back to the `gem` subprocess as a last resort.
pub fn find_gem_paths(project_dir: &Path) -> Vec<PathBuf> {
    // 1. Check .bundle/config for BUNDLE_PATH (vendored gems)
    if let Some(bundle_path) = read_bundle_path(project_dir) {
        let abs = if bundle_path.is_absolute() {
            bundle_path
        } else {
            project_dir.join(bundle_path)
        };
        // Bundler installs under <BUNDLE_PATH>/ruby/<api_version>/
        if let Some(gem_root) = find_bundler_gem_root(&abs) {
            return vec![gem_root];
        }
        // Some layouts put gems directly under BUNDLE_PATH
        if abs.join("gems").is_dir() {
            return vec![abs];
        }
    }

    // 2. GEM_PATH env (colon-separated) — often set by rbenv/rvm init scripts
    if let Ok(gem_path) = std::env::var("GEM_PATH") {
        let paths: Vec<PathBuf> = gem_path
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .collect();
        if !paths.is_empty() {
            return paths;
        }
    }

    // 3. GEM_HOME env
    if let Ok(gem_home) = std::env::var("GEM_HOME") {
        let p = PathBuf::from(&gem_home);
        if p.is_dir() {
            return vec![p];
        }
    }

    // 4. Probe rbenv filesystem directly (no subprocess)
    if let Some(path) = probe_rbenv(project_dir) {
        return vec![path];
    }

    // 5. Probe rvm filesystem directly
    if let Some(path) = probe_rvm() {
        return vec![path];
    }

    // 6. Last resort: shell out to `gem environment gempath`
    if let Ok(output) = std::process::Command::new("gem")
        .args(["environment", "gempath"])
        .output()
    {
        if output.status.success() {
            let path_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let paths: Vec<PathBuf> = path_str
                .split(':')
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect();
            if !paths.is_empty() {
                return paths;
            }
        }
    }

    Vec::new()
}

/// Read BUNDLE_PATH from .bundle/config in the project directory.
fn read_bundle_path(project_dir: &Path) -> Option<PathBuf> {
    let config_path = project_dir.join(".bundle").join("config");
    let content = std::fs::read_to_string(config_path).ok()?;
    for line in content.lines() {
        let line = line.trim();
        // Format: BUNDLE_PATH: "vendor/gems" or BUNDLE_PATH: vendor/gems
        if let Some(rest) = line.strip_prefix("BUNDLE_PATH:") {
            let val = rest.trim().trim_matches('"').trim();
            if !val.is_empty() {
                return Some(PathBuf::from(val));
            }
        }
    }
    None
}

/// Find the gem root inside a Bundler bundle path.
/// Bundler typically installs to <path>/ruby/<api_version>/.
fn find_bundler_gem_root(bundle_path: &Path) -> Option<PathBuf> {
    let ruby_dir = bundle_path.join("ruby");
    if !ruby_dir.is_dir() {
        return None;
    }
    // Find the API version directory (e.g., "3.4.0")
    if let Ok(entries) = std::fs::read_dir(&ruby_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() && p.join("gems").is_dir() {
                return Some(p);
            }
        }
    }
    None
}

/// Probe rbenv filesystem to find gem path without spawning a process.
/// Replicates rbenv's version resolution: RBENV_VERSION env → .ruby-version
/// walking up from project_dir → ~/.rbenv/version (global default).
fn probe_rbenv(project_dir: &Path) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok().map(PathBuf::from)?;
    let rbenv_root = std::env::var("RBENV_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".rbenv"));

    let versions_dir = rbenv_root.join("versions");
    if !versions_dir.is_dir() {
        return None;
    }

    // Resolve version the same way rbenv does
    let version = std::env::var("RBENV_VERSION").ok().or_else(|| {
        // Walk from project_dir upward looking for .ruby-version (matches rbenv shim behavior)
        find_ruby_version_file(project_dir).or_else(|| {
            // Global default
            std::fs::read_to_string(rbenv_root.join("version"))
                .ok()
                .map(|v| v.trim().to_string())
        })
    })?;

    // Find exact or prefix-matching version directory
    let version_dir = versions_dir.join(&version);
    let resolved_dir = if version_dir.is_dir() {
        version_dir
    } else {
        // Prefix match (e.g., "3.3" matches "3.3.9")
        let mut best: Option<PathBuf> = None;
        if let Ok(entries) = std::fs::read_dir(&versions_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(&version) && entry.path().is_dir() {
                    best = Some(entry.path());
                }
            }
        }
        best?
    };

    // Gem path: <ruby_dir>/lib/ruby/gems/<api_version>/
    let gems_base = resolved_dir.join("lib").join("ruby").join("gems");
    if let Ok(entries) = std::fs::read_dir(&gems_base) {
        for entry in entries.flatten() {
            if entry.path().is_dir() && entry.path().join("gems").is_dir() {
                return Some(entry.path());
            }
        }
    }
    None
}

/// Walk from project_dir upward to find a .ruby-version file (same as rbenv shim).
fn find_ruby_version_file(start_dir: &Path) -> Option<String> {
    let mut dir = start_dir.canonicalize().ok().unwrap_or_else(|| start_dir.to_path_buf());
    loop {
        let candidate = dir.join(".ruby-version");
        if let Ok(content) = std::fs::read_to_string(&candidate) {
            let v = content.trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Probe rvm filesystem to find gem path without spawning a process.
fn probe_rvm() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok().map(PathBuf::from)?;
    let rvm_dir = std::env::var("rvm_path")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".rvm"));

    let gems_dir = rvm_dir.join("gems");
    if !gems_dir.is_dir() {
        return None;
    }

    // Find the default or current gemset
    if let Ok(entries) = std::fs::read_dir(&gems_dir) {
        let mut best: Option<PathBuf> = None;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip @global and non-directories
            if !entry.path().is_dir() || name.contains("@global") || name.starts_with('.') {
                continue;
            }
            if entry.path().join("gems").is_dir() {
                best = Some(entry.path());
            }
        }
        return best;
    }
    None
}

/// Pre-build an index of gem_name -> gem_dir for fast lookups.
pub fn build_gem_index(gem_paths: &[PathBuf]) -> HashMap<String, PathBuf> {
    let mut index = HashMap::new();
    for base in gem_paths {
        let gems_dir = base.join("gems");
        if !gems_dir.is_dir() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&gems_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !entry.path().is_dir() {
                    continue;
                }
                // Parse "gem_name-version" where version starts with a digit.
                // Try rfind('-') positions from right to left to handle platform suffixes
                // like "nokogiri-1.16.0-x86_64-linux".
                let mut parsed = false;
                let mut search_end = name.len();
                while let Some(idx) = name[..search_end].rfind('-') {
                    let gem_name = &name[..idx];
                    let version_part = &name[idx + 1..search_end];
                    if !gem_name.is_empty()
                        && version_part.starts_with(|c: char| c.is_ascii_digit())
                        && version_part.contains('.')
                    {
                        index
                            .entry(gem_name.to_string())
                            .and_modify(|existing: &mut PathBuf| {
                                let existing_name = existing
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .to_string();
                                // Re-extract version from existing using same approach
                                let existing_ver = extract_gem_version(&existing_name)
                                    .unwrap_or_default();
                                if compare_gem_versions(version_part, &existing_ver)
                                    == std::cmp::Ordering::Greater
                                {
                                    *existing = entry.path();
                                }
                            })
                            .or_insert_with(|| entry.path());
                        parsed = true;
                        break;
                    }
                    search_end = idx;
                }
                if !parsed {
                    // Fallback: simple rfind for gems without dots in version (rare)
                    if let Some(idx) = name.rfind('-') {
                        let gem_name = &name[..idx];
                        let version_part = &name[idx + 1..];
                        if version_part.starts_with(|c: char| c.is_ascii_digit()) {
                            index
                                .entry(gem_name.to_string())
                                .and_modify(|existing: &mut PathBuf| {
                                    let existing_name = existing
                                        .file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy()
                                        .to_string();
                                    let existing_ver =
                                        extract_gem_version(&existing_name).unwrap_or_default();
                                    if compare_gem_versions(version_part, &existing_ver)
                                        == std::cmp::Ordering::Greater
                                    {
                                        *existing = entry.path();
                                    }
                                })
                                .or_insert_with(|| entry.path());
                        }
                    }
                }
            }
        }
    }
    index
}

/// Discover actual require paths by inspecting the installed gem's lib/ directory.
/// Also checks gemspec for custom require_paths (e.g., ["lib", "ext"]).
pub fn discover_gem_require_paths(
    gem_name: &str,
    gem_index: &HashMap<String, PathBuf>,
) -> Option<Vec<String>> {
    let gem_dir = gem_index.get(gem_name)?;

    // Try reading require_paths from the gem's gemspec first
    let require_dirs = read_gemspec_require_paths(gem_dir).unwrap_or_else(|| vec!["lib".to_string()]);

    let mut paths = Vec::new();
    for req_dir in &require_dirs {
        let dir = gem_dir.join(req_dir);
        if !dir.is_dir() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Some(ext) = path.extension() {
                        if ext == "rb" {
                            if let Some(stem) = path.file_stem() {
                                paths.push(stem.to_string_lossy().to_string());
                            }
                        }
                    }
                } else if path.is_dir() {
                    if let Some(name) = path.file_name() {
                        paths.push(name.to_string_lossy().to_string());
                    }
                }
            }
        }
    }

    if paths.is_empty() {
        None
    } else {
        paths.sort();
        paths.dedup();
        Some(paths)
    }
}

/// Read require_paths from a gem's .gemspec file (static parse).
fn read_gemspec_require_paths(gem_dir: &Path) -> Option<Vec<String>> {
    let pattern = gem_dir.join("*.gemspec").display().to_string();
    for spec_path in glob::glob(&pattern).into_iter().flatten().flatten() {
        if let Ok(content) = std::fs::read_to_string(&spec_path) {
            // Match: s.require_paths = ["lib", "ext"] or s.require_path = "lib"
            static REQ_PATHS_RE: Lazy<Regex> = Lazy::new(|| {
                Regex::new(r#"require_paths?\s*=\s*\[([^\]]+)\]"#).unwrap()
            });
            static REQ_PATH_SINGLE_RE: Lazy<Regex> = Lazy::new(|| {
                Regex::new(r#"require_path\s*=\s*['"]([^'"]+)['"]"#).unwrap()
            });
            if let Some(cap) = REQ_PATHS_RE.captures(&content) {
                let inner = &cap[1];
                let paths: Vec<String> = inner
                    .split(',')
                    .filter_map(|s| {
                        let trimmed = s.trim().trim_matches(|c| c == '\'' || c == '"').trim();
                        if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
                    })
                    .collect();
                if !paths.is_empty() {
                    return Some(paths);
                }
            }
            if let Some(cap) = REQ_PATH_SINGLE_RE.captures(&content) {
                return Some(vec![cap[1].to_string()]);
            }
        }
    }
    None
}

/// Extract top-level module/class names from a gem's lib/*.rb files using regex.
pub fn discover_gem_module_names(gem_name: &str, gem_index: &HashMap<String, PathBuf>) -> Vec<String> {
    let gem_dir = match gem_index.get(gem_name) {
        Some(d) => d,
        None => return Vec::new(),
    };
    let lib_dir = gem_dir.join("lib");
    if !lib_dir.is_dir() {
        return Vec::new();
    }

    let mut modules = HashSet::new();

    // Scan top-level lib/*.rb
    scan_rb_files_for_modules(&lib_dir, &MODULE_DECL_RE, &mut modules);

    // If nothing found, try one level deeper
    if modules.is_empty() {
        if let Ok(entries) = std::fs::read_dir(&lib_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    scan_rb_files_for_modules(&entry.path(), &MODULE_DECL_RE, &mut modules);
                }
            }
        }
    }

    let mut result: Vec<String> = modules.into_iter().collect();
    result.sort();
    result
}

fn scan_rb_files_for_modules(dir: &Path, re: &Regex, modules: &mut HashSet<String>) {
    let pattern = dir.join("*.rb").display().to_string();
    for entry in glob::glob(&pattern).into_iter().flatten().flatten() {
        if let Ok(content) = std::fs::read_to_string(&entry) {
            for cap in re.captures_iter(&content) {
                modules.insert(cap[1].to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_gem_version ──

    #[test]
    fn version_simple() {
        assert_eq!(extract_gem_version("rails-7.1.3"), Some("7.1.3".into()));
    }

    #[test]
    fn version_with_platform() {
        assert_eq!(extract_gem_version("nokogiri-1.16.0-x86_64-linux"), Some("1.16.0".into()));
    }

    #[test]
    fn version_with_arm_platform() {
        assert_eq!(extract_gem_version("nokogiri-1.16.0-arm64-darwin"), Some("1.16.0".into()));
    }

    #[test]
    fn version_hyphenated_gem_name() {
        assert_eq!(extract_gem_version("ruby-kafka-1.5.0"), Some("1.5.0".into()));
    }

    #[test]
    fn version_multi_hyphen_gem() {
        assert_eq!(extract_gem_version("aws-sdk-s3-1.143.0"), Some("1.143.0".into()));
    }

    #[test]
    fn version_no_version() {
        assert_eq!(extract_gem_version("just-a-name"), None);
    }

    #[test]
    fn version_pre_release() {
        // Full version string includes pre-release suffix since it's all after the '-'
        assert_eq!(extract_gem_version("rails-7.2.0.beta1"), Some("7.2.0.beta1".into()));
    }

    // ── compare_gem_versions ──

    #[test]
    fn compare_versions_greater() {
        assert_eq!(compare_gem_versions("1.16.8", "1.9.1"), std::cmp::Ordering::Greater);
    }

    #[test]
    fn compare_versions_less() {
        assert_eq!(compare_gem_versions("1.9.1", "1.16.8"), std::cmp::Ordering::Less);
    }

    #[test]
    fn compare_versions_equal() {
        assert_eq!(compare_gem_versions("2.0.0", "2.0.0"), std::cmp::Ordering::Equal);
    }

    #[test]
    fn compare_versions_different_lengths() {
        assert_eq!(compare_gem_versions("1.0.0.1", "1.0.0"), std::cmp::Ordering::Greater);
    }

    // ── read_bundle_path ──

    #[test]
    fn bundle_path_from_config() {
        let dir = tempfile::tempdir().unwrap();
        let bundle_dir = dir.path().join(".bundle");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(bundle_dir.join("config"), "---\nBUNDLE_PATH: \"vendor/gems\"\n").unwrap();
        assert_eq!(read_bundle_path(dir.path()), Some(PathBuf::from("vendor/gems")));
    }

    #[test]
    fn bundle_path_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_bundle_path(dir.path()), None);
    }

    // ── read_gemspec_require_paths ──

    #[test]
    fn gemspec_require_paths_array() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("foo.gemspec"),
            "Gem::Specification.new do |s|\n  s.require_paths = [\"lib\", \"ext\"]\nend\n"
        ).unwrap();
        assert_eq!(read_gemspec_require_paths(dir.path()), Some(vec!["lib".into(), "ext".into()]));
    }

    #[test]
    fn gemspec_require_path_single() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bar.gemspec"),
            "Gem::Specification.new do |s|\n  s.require_path = 'src'\nend\n"
        ).unwrap();
        assert_eq!(read_gemspec_require_paths(dir.path()), Some(vec!["src".into()]));
    }

    #[test]
    fn gemspec_require_paths_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("baz.gemspec"),
            "Gem::Specification.new do |s|\n  s.name = 'baz'\nend\n"
        ).unwrap();
        assert_eq!(read_gemspec_require_paths(dir.path()), None);
    }
}
