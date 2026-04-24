use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use regex::Regex;

static MODULE_DECL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^\s*(?:module|class)\s+([A-Z][\w:]+)").unwrap());

/// Generic module/class names that appear in many gems and would over-match
/// when used as a usage signal (e.g. any `Configuration` reference in app code
/// would mark the gem Used). These are filtered out of discovered module names;
/// canonical names are still preserved via the curated `KNOWN_MODULE_NAMES`
/// table in main.rs which is unioned with the discovery output.
const GENERIC_MODULE_BLACKLIST: &[&str] = &[
    "VERSION",
    "Error",
    "Errors",
    "Base",
    "Configuration",
    "Config",
    "Engine",
    "Railtie",
    "ClassMethods",
    "Cli",
    "Util",
    "Utils",
    "Helpers",
    "Helper",
    "Result",
    "Response",
    "Request",
    "Client",
    "Server",
];

fn is_generic_module_name(name: &str) -> bool {
    GENERIC_MODULE_BLACKLIST.contains(&name)
}

/// Metadata extracted from a .gem tarball's metadata.gz
#[derive(Debug, Default)]
pub struct GemMetadata {
    pub require_paths: Vec<String>,
    pub executables: Vec<String>,
}

/// Compare two gem version strings numerically (e.g. "1.16.8" > "1.9.1").
fn compare_gem_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> Vec<u64> {
        s.split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
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
    if let Some(path) = probe_rvm(project_dir) {
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
    let mut dir = start_dir
        .canonicalize()
        .ok()
        .unwrap_or_else(|| start_dir.to_path_buf());
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

/// Walk from project_dir upward to find a `.ruby-gemset` file (RVM convention).
fn find_ruby_gemset_file(start_dir: &Path) -> Option<String> {
    let mut dir = start_dir
        .canonicalize()
        .ok()
        .unwrap_or_else(|| start_dir.to_path_buf());
    loop {
        let candidate = dir.join(".ruby-gemset");
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
///
/// Selection (deterministic):
///   1. If both `.ruby-version` and `.ruby-gemset` are present in/above
///      `project_dir`, prefer `<rvm>/gems/ruby-<version>@<gemset>`.
///   2. Else if `.ruby-version` is present, prefer `<rvm>/gems/ruby-<version>`.
///   3. Else fall back to a deterministic scan: skip `@global` and dotfiles,
///      prefer entries with no `@gemset` suffix, then sort by longest version
///      string (newest), breaking ties alphabetically (descending).
fn probe_rvm(project_dir: &Path) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok().map(PathBuf::from)?;
    let rvm_dir = std::env::var("rvm_path")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".rvm"));

    let gems_dir = rvm_dir.join("gems");
    if !gems_dir.is_dir() {
        return None;
    }

    select_rvm_gemset(&gems_dir, project_dir)
}

/// Pure selection logic over an RVM-style `gems/` directory. Extracted for
/// testability — no env or HOME access here.
fn select_rvm_gemset(gems_dir: &Path, project_dir: &Path) -> Option<PathBuf> {
    let version = find_ruby_version_file(project_dir);
    let gemset = find_ruby_gemset_file(project_dir);

    // 1. version + gemset → exact match
    if let (Some(v), Some(g)) = (version.as_ref(), gemset.as_ref()) {
        let candidate = gems_dir.join(format!("ruby-{}@{}", v, g));
        if candidate.join("gems").is_dir() {
            return Some(candidate);
        }
    }

    // 2. version only → exact match without @gemset
    if let Some(v) = version.as_ref() {
        let candidate = gems_dir.join(format!("ruby-{}", v));
        if candidate.join("gems").is_dir() {
            return Some(candidate);
        }
    }

    // 3. Deterministic fallback: collect candidates, sort, return best.
    let mut candidates: Vec<(PathBuf, String)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(gems_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !entry.path().is_dir() || name.contains("@global") || name.starts_with('.') {
                continue;
            }
            if !entry.path().join("gems").is_dir() {
                continue;
            }
            candidates.push((entry.path(), name));
        }
    }

    // Order: plain (no @) before @gemset; then longest name (proxy for newest);
    // then descending lexical order. Fully deterministic.
    candidates.sort_by(|a, b| {
        let a_plain = !a.1.contains('@');
        let b_plain = !b.1.contains('@');
        b_plain
            .cmp(&a_plain)
            .then_with(|| b.1.len().cmp(&a.1.len()))
            .then_with(|| b.1.cmp(&a.1))
    });

    candidates.into_iter().next().map(|(p, _)| p)
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
                                let existing_ver =
                                    extract_gem_version(&existing_name).unwrap_or_default();
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
    let require_dirs =
        read_gemspec_require_paths(gem_dir).unwrap_or_else(|| vec!["lib".to_string()]);

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
    let entries = std::fs::read_dir(gem_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("gemspec") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            // Match: s.require_paths = ["lib", "ext"] or s.require_path = "lib"
            static REQ_PATHS_RE: Lazy<Regex> =
                Lazy::new(|| Regex::new(r#"require_paths?\s*=\s*\[([^\]]+)\]"#).unwrap());
            static REQ_PATH_SINGLE_RE: Lazy<Regex> =
                Lazy::new(|| Regex::new(r#"require_path\s*=\s*['"]([^'"]+)['"]"#).unwrap());
            if let Some(cap) = REQ_PATHS_RE.captures(&content) {
                let inner = &cap[1];
                let paths: Vec<String> = inner
                    .split(',')
                    .filter_map(|s| {
                        let trimmed = s.trim().trim_matches(|c| c == '\'' || c == '"').trim();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        }
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

/// Extract top-level module/class names from a gem's `lib/<gem>.rb` entry
/// file(s) only. We deliberately skip nested files (`lib/<gem>/...`) because
/// those tend to declare generic helper names (`Configuration`, `Error`,
/// `ClassMethods`, ...) that over-match against unrelated app code.
///
/// Discovery is restricted to the conventional entry-point file names:
///   - `lib/<gem_name>.rb`
///   - `lib/<gem_name with - replaced by _>.rb`
///   - `lib/<first hyphen segment>.rb`
///
/// Generic helper names (see `GENERIC_MODULE_BLACKLIST`) are filtered. The
/// caller (main.rs) UNIONs this with the curated `KNOWN_MODULE_NAMES` table,
/// so canonical names for popular gems are still preserved when they would
/// otherwise be filtered.
pub fn discover_gem_module_names(
    gem_name: &str,
    gem_index: &HashMap<String, PathBuf>,
) -> Vec<String> {
    let gem_dir = match gem_index.get(gem_name) {
        Some(d) => d,
        None => return Vec::new(),
    };
    let lib_dir = gem_dir.join("lib");
    if !lib_dir.is_dir() {
        return Vec::new();
    }

    let mut candidate_files: Vec<String> = Vec::with_capacity(3);
    candidate_files.push(format!("{}.rb", gem_name));
    let underscored = gem_name.replace('-', "_");
    if underscored != gem_name {
        candidate_files.push(format!("{}.rb", underscored));
    }
    if let Some(first) = gem_name.split('-').next() {
        let first_rb = format!("{}.rb", first);
        if !candidate_files.contains(&first_rb) {
            candidate_files.push(first_rb);
        }
    }

    let mut modules: HashSet<String> = HashSet::new();
    for fname in &candidate_files {
        let path = lib_dir.join(fname);
        if !path.is_file() {
            continue;
        }
        scan_rb_file_head_for_modules(&path, &MODULE_DECL_RE, &mut modules);
    }

    let mut result: Vec<String> = modules
        .into_iter()
        .filter(|n| !is_generic_module_name(n))
        .collect();
    result.sort();
    result
}

/// Read up to ~4KB from the head of a Ruby file and extract top-level
/// `module`/`class` declarations. Top-level declarations conventionally
/// appear at the file's top, so this avoids loading huge generated files.
fn scan_rb_file_head_for_modules(path: &Path, re: &Regex, modules: &mut HashSet<String>) {
    const HEAD_BYTES: usize = 4096;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let mut buf = vec![0u8; HEAD_BYTES];
    let n = match file.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return,
    };
    buf.truncate(n);
    let content = String::from_utf8_lossy(&buf);
    for cap in re.captures_iter(&content) {
        modules.insert(cap[1].to_string());
    }
}

/// Extract gemspec metadata from a .gem tarball (tar archive containing metadata.gz).
fn extract_gem_metadata(gem_path: &Path) -> Option<GemMetadata> {
    let file = std::fs::File::open(gem_path).ok()?;
    let mut archive = tar::Archive::new(file);
    for entry in archive.entries().ok()? {
        let mut entry = entry.ok()?;
        let path = entry.path().ok()?.to_string_lossy().to_string();
        if path == "metadata.gz" {
            let mut compressed = Vec::new();
            entry.read_to_end(&mut compressed).ok()?;
            let mut decoder = flate2::read::GzDecoder::new(&compressed[..]);
            let mut yaml = String::new();
            decoder.read_to_string(&mut yaml).ok()?;
            return parse_gem_metadata_yaml(&yaml);
        }
    }
    None
}

/// Parse the YAML gemspec metadata for require_paths and executables.
fn parse_gem_metadata_yaml(yaml: &str) -> Option<GemMetadata> {
    let mut meta = GemMetadata::default();
    let mut in_require_paths = false;
    let mut in_executables = false;

    for line in yaml.lines() {
        // Detect section starts
        if line.starts_with("require_paths:") {
            in_require_paths = true;
            in_executables = false;
            // Check inline value: require_paths: ["lib"]
            if let Some(rest) = line.strip_prefix("require_paths:") {
                let rest = rest.trim();
                if rest.starts_with('[') {
                    for item in rest.trim_matches(|c| c == '[' || c == ']').split(',') {
                        let item = item.trim().trim_matches('"').trim_matches('\'');
                        if !item.is_empty() {
                            meta.require_paths.push(item.to_string());
                        }
                    }
                    in_require_paths = false;
                }
            }
            continue;
        }
        if line.starts_with("executables:") {
            in_executables = true;
            in_require_paths = false;
            if let Some(rest) = line.strip_prefix("executables:") {
                let rest = rest.trim();
                if rest == "[]" {
                    in_executables = false;
                } else if rest.starts_with('[') {
                    for item in rest.trim_matches(|c| c == '[' || c == ']').split(',') {
                        let item = item.trim().trim_matches('"').trim_matches('\'');
                        if !item.is_empty() {
                            meta.executables.push(item.to_string());
                        }
                    }
                    in_executables = false;
                }
            }
            continue;
        }
        // New top-level key ends current section
        if !line.starts_with(' ') && !line.starts_with('-') && line.contains(':') {
            in_require_paths = false;
            in_executables = false;
            continue;
        }
        // Collect list items
        let trimmed = line.trim();
        if trimmed.starts_with("- ") {
            let value = trimmed
                .strip_prefix("- ")
                .unwrap_or(trimmed)
                .trim()
                .trim_matches('"')
                .trim_matches('\'');
            if in_require_paths && !value.is_empty() {
                meta.require_paths.push(value.to_string());
            }
            if in_executables && !value.is_empty() {
                meta.executables.push(value.to_string());
            }
        }
    }

    if meta.require_paths.is_empty() {
        meta.require_paths.push("lib".to_string());
    }
    Some(meta)
}

/// Lightweight directory scan of `vendor/cache/*.gem`.
/// Returns gem_name → PathBuf mapping, keeping the latest version per gem name.
/// Does NOT open any tarballs — only inspects filenames.
pub fn build_vendor_cache_path_index(project_dir: &Path) -> HashMap<String, PathBuf> {
    let cache_dir = project_dir.join("vendor").join("cache");
    if !cache_dir.is_dir() {
        return HashMap::new();
    }

    let mut best: HashMap<String, (String, PathBuf)> = HashMap::new();
    let entries = match std::fs::read_dir(&cache_dir) {
        Ok(rd) => rd,
        Err(_) => return HashMap::new(),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "gem") {
            continue;
        }
        let filename = match path.file_stem() {
            Some(s) => s.to_string_lossy().to_string(),
            None => continue,
        };
        let gem_name = match extract_gem_name_from_filename(&filename) {
            Some(n) => n,
            None => continue,
        };
        let version = extract_gem_version(&filename).unwrap_or_default();
        match best.entry(gem_name) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert((version, path));
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                if compare_gem_versions(&version, &e.get().0) == std::cmp::Ordering::Greater {
                    e.insert((version, path));
                }
            }
        }
    }
    best.into_iter().map(|(name, (_, path))| (name, path)).collect()
}

/// Extract metadata (require_paths, executables) from a single `.gem` tarball.
/// This is the expensive operation — opens the tar, decompresses metadata.gz, parses YAML.
pub fn extract_vendor_gem_metadata(gem_path: &Path) -> Option<GemMetadata> {
    extract_gem_metadata(gem_path)
}

/// Build a gem metadata index from vendor/cache/*.gem tarballs.
/// Returns gem_name → GemMetadata with require_paths and executables.
///
/// Prefer `build_vendor_cache_path_index` + on-demand `extract_vendor_gem_metadata`
/// for lazy evaluation — this eagerly extracts every gem.
#[allow(dead_code)]
pub fn build_vendor_cache_index(project_dir: &Path) -> HashMap<String, GemMetadata> {
    let paths = build_vendor_cache_path_index(project_dir);
    use rayon::prelude::*;
    paths
        .into_par_iter()
        .filter_map(|(name, path)| extract_vendor_gem_metadata(&path).map(|m| (name, m)))
        .collect()
}

/// Extract gem name from a "name-version" filename (e.g., "activesupport-8.1.1" → "activesupport").
fn extract_gem_name_from_filename(filename: &str) -> Option<String> {
    let mut search_end = filename.len();
    while let Some(idx) = filename[..search_end].rfind('-') {
        let version_part = &filename[idx + 1..search_end];
        if version_part.starts_with(|c: char| c.is_ascii_digit()) && version_part.contains('.') {
            return Some(filename[..idx].to_string());
        }
        search_end = idx;
    }
    None
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
        assert_eq!(
            extract_gem_version("nokogiri-1.16.0-x86_64-linux"),
            Some("1.16.0".into())
        );
    }

    #[test]
    fn version_with_arm_platform() {
        assert_eq!(
            extract_gem_version("nokogiri-1.16.0-arm64-darwin"),
            Some("1.16.0".into())
        );
    }

    #[test]
    fn version_hyphenated_gem_name() {
        assert_eq!(
            extract_gem_version("ruby-kafka-1.5.0"),
            Some("1.5.0".into())
        );
    }

    #[test]
    fn version_multi_hyphen_gem() {
        assert_eq!(
            extract_gem_version("aws-sdk-s3-1.143.0"),
            Some("1.143.0".into())
        );
    }

    #[test]
    fn version_no_version() {
        assert_eq!(extract_gem_version("just-a-name"), None);
    }

    #[test]
    fn version_pre_release() {
        // Full version string includes pre-release suffix since it's all after the '-'
        assert_eq!(
            extract_gem_version("rails-7.2.0.beta1"),
            Some("7.2.0.beta1".into())
        );
    }

    // ── compare_gem_versions ──

    #[test]
    fn compare_versions_greater() {
        assert_eq!(
            compare_gem_versions("1.16.8", "1.9.1"),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn compare_versions_less() {
        assert_eq!(
            compare_gem_versions("1.9.1", "1.16.8"),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn compare_versions_equal() {
        assert_eq!(
            compare_gem_versions("2.0.0", "2.0.0"),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn compare_versions_different_lengths() {
        assert_eq!(
            compare_gem_versions("1.0.0.1", "1.0.0"),
            std::cmp::Ordering::Greater
        );
    }

    // ── read_bundle_path ──

    #[test]
    fn bundle_path_from_config() {
        let dir = tempfile::tempdir().unwrap();
        let bundle_dir = dir.path().join(".bundle");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join("config"),
            "---\nBUNDLE_PATH: \"vendor/gems\"\n",
        )
        .unwrap();
        assert_eq!(
            read_bundle_path(dir.path()),
            Some(PathBuf::from("vendor/gems"))
        );
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
        std::fs::write(
            dir.path().join("foo.gemspec"),
            "Gem::Specification.new do |s|\n  s.require_paths = [\"lib\", \"ext\"]\nend\n",
        )
        .unwrap();
        assert_eq!(
            read_gemspec_require_paths(dir.path()),
            Some(vec!["lib".into(), "ext".into()])
        );
    }

    #[test]
    fn gemspec_require_path_single() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("bar.gemspec"),
            "Gem::Specification.new do |s|\n  s.require_path = 'src'\nend\n",
        )
        .unwrap();
        assert_eq!(
            read_gemspec_require_paths(dir.path()),
            Some(vec!["src".into()])
        );
    }

    #[test]
    fn gemspec_require_paths_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("baz.gemspec"),
            "Gem::Specification.new do |s|\n  s.name = 'baz'\nend\n",
        )
        .unwrap();
        assert_eq!(read_gemspec_require_paths(dir.path()), None);
    }

    // ── build_gem_index ──

    #[test]
    fn build_gem_index_basic() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        std::fs::create_dir_all(gems_dir.join("rails-7.1.0")).unwrap();
        std::fs::create_dir_all(gems_dir.join("puma-6.4.0")).unwrap();
        let index = build_gem_index(&[dir.path().to_path_buf()]);
        assert!(index.contains_key("rails"));
        assert!(index.contains_key("puma"));
    }

    #[test]
    fn build_gem_index_picks_latest_version() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        std::fs::create_dir_all(gems_dir.join("rails-7.0.0")).unwrap();
        std::fs::create_dir_all(gems_dir.join("rails-7.1.0")).unwrap();
        let index = build_gem_index(&[dir.path().to_path_buf()]);
        assert!(index["rails"].to_string_lossy().contains("rails-7.1.0"));
    }

    #[test]
    fn build_gem_index_platform_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        std::fs::create_dir_all(gems_dir.join("nokogiri-1.16.0-arm64-darwin")).unwrap();
        let index = build_gem_index(&[dir.path().to_path_buf()]);
        assert!(index.contains_key("nokogiri"));
    }

    #[test]
    fn build_gem_index_hyphenated_name() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        std::fs::create_dir_all(gems_dir.join("aws-sdk-s3-1.143.0")).unwrap();
        let index = build_gem_index(&[dir.path().to_path_buf()]);
        assert!(index.contains_key("aws-sdk-s3"));
    }

    #[test]
    fn build_gem_index_empty() {
        let dir = tempfile::tempdir().unwrap();
        let index = build_gem_index(&[dir.path().to_path_buf()]);
        assert!(index.is_empty());
    }

    // ── discover_gem_require_paths ──

    #[test]
    fn discover_require_paths_from_lib() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        let gem_dir = gems_dir.join("mygem-1.0.0");
        let lib_dir = gem_dir.join("lib");
        std::fs::create_dir_all(&lib_dir).unwrap();
        std::fs::write(lib_dir.join("mygem.rb"), "# entry").unwrap();
        std::fs::write(lib_dir.join("helpers.rb"), "# helpers").unwrap();
        std::fs::create_dir(lib_dir.join("mygem")).unwrap();

        let index = build_gem_index(&[dir.path().to_path_buf()]);
        let paths = discover_gem_require_paths("mygem", &index);
        assert!(paths.is_some());
        let paths = paths.unwrap();
        assert!(paths.contains(&"mygem".to_string()));
        assert!(paths.contains(&"helpers".to_string()));
    }

    #[test]
    fn discover_require_paths_missing_gem() {
        let index = HashMap::new();
        assert!(discover_gem_require_paths("nonexistent", &index).is_none());
    }

    // ── discover_gem_module_names ──

    #[test]
    fn discover_module_names_basic() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        let gem_dir = gems_dir.join("mygem-1.0.0");
        let lib_dir = gem_dir.join("lib");
        std::fs::create_dir_all(&lib_dir).unwrap();
        std::fs::write(lib_dir.join("mygem.rb"), "module MyGem\nend\n").unwrap();

        let index = build_gem_index(&[dir.path().to_path_buf()]);
        let modules = discover_gem_module_names("mygem", &index);
        assert!(modules.contains(&"MyGem".to_string()));
    }

    #[test]
    fn discover_module_names_namespaced() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        let gem_dir = gems_dir.join("google-cloud-1.0.0");
        let lib_dir = gem_dir.join("lib");
        std::fs::create_dir_all(&lib_dir).unwrap();
        std::fs::write(
            lib_dir.join("google-cloud.rb"),
            "module Google::Cloud\nend\n",
        )
        .unwrap();

        let index = build_gem_index(&[dir.path().to_path_buf()]);
        let modules = discover_gem_module_names("google-cloud", &index);
        assert!(modules.contains(&"Google::Cloud".to_string()));
    }

    #[test]
    fn discover_module_names_deeper() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        let gem_dir = gems_dir.join("mygem-1.0.0");
        let lib_dir = gem_dir.join("lib");
        let sub_dir = lib_dir.join("mygem");
        std::fs::create_dir_all(&sub_dir).unwrap();
        // No top-level rb files — discovery is restricted to lib/<gem>.rb,
        // so nested files must NOT be scanned (they tend to declare generic
        // helper names that over-match in app code). The curated
        // KNOWN_MODULE_NAMES table in main.rs is the safety net.
        std::fs::write(sub_dir.join("core.rb"), "class MyGem::Core\nend\n").unwrap();

        let index = build_gem_index(&[dir.path().to_path_buf()]);
        let modules = discover_gem_module_names("mygem", &index);
        assert!(modules.is_empty());
    }

    #[test]
    fn discover_module_names_only_top_level_entry_file() {
        // lib/mygem.rb declares MyGem; lib/mygem/internal.rb declares
        // generic helpers. Only top-level should be picked up.
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        let gem_dir = gems_dir.join("mygem-1.0.0");
        let lib_dir = gem_dir.join("lib");
        let sub_dir = lib_dir.join("mygem");
        std::fs::create_dir_all(&sub_dir).unwrap();
        std::fs::write(lib_dir.join("mygem.rb"), "module MyGem\nend\n").unwrap();
        // A sibling top-level rb file that is NOT the gem entry — must be ignored.
        std::fs::write(
            lib_dir.join("unrelated.rb"),
            "module Unrelated\nend\n",
        )
        .unwrap();
        std::fs::write(
            sub_dir.join("base.rb"),
            "class MyGem::Base\nend\nclass MyGem::Configuration\nend\n",
        )
        .unwrap();

        let index = build_gem_index(&[dir.path().to_path_buf()]);
        let modules = discover_gem_module_names("mygem", &index);
        assert_eq!(modules, vec!["MyGem".to_string()]);
    }

    #[test]
    fn discover_module_names_underscore_variant() {
        // Hyphenated gem name: lib/google_cloud.rb is also considered.
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        let gem_dir = gems_dir.join("google-cloud-1.0.0");
        let lib_dir = gem_dir.join("lib");
        std::fs::create_dir_all(&lib_dir).unwrap();
        std::fs::write(
            lib_dir.join("google_cloud.rb"),
            "module GoogleCloud\nend\n",
        )
        .unwrap();

        let index = build_gem_index(&[dir.path().to_path_buf()]);
        let modules = discover_gem_module_names("google-cloud", &index);
        assert!(modules.contains(&"GoogleCloud".to_string()));
    }

    #[test]
    fn discover_module_names_first_segment_variant() {
        // Hyphenated gem: lib/<first-segment>.rb is also considered.
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        let gem_dir = gems_dir.join("aws-sdk-s3-1.0.0");
        let lib_dir = gem_dir.join("lib");
        std::fs::create_dir_all(&lib_dir).unwrap();
        std::fs::write(lib_dir.join("aws.rb"), "module Aws\nend\n").unwrap();

        let index = build_gem_index(&[dir.path().to_path_buf()]);
        let modules = discover_gem_module_names("aws-sdk-s3", &index);
        assert!(modules.contains(&"Aws".to_string()));
    }

    #[test]
    fn discover_module_names_filters_generic_helpers() {
        // Even in the top-level entry file, generic names like Configuration
        // / Error / VERSION must be filtered out.
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        let gem_dir = gems_dir.join("mygem-1.0.0");
        let lib_dir = gem_dir.join("lib");
        std::fs::create_dir_all(&lib_dir).unwrap();
        std::fs::write(
            lib_dir.join("mygem.rb"),
            "module MyGem\nend\nclass Configuration\nend\nclass Error\nend\nVERSION = '1'\nmodule VERSION\nend\n",
        )
        .unwrap();

        let index = build_gem_index(&[dir.path().to_path_buf()]);
        let modules = discover_gem_module_names("mygem", &index);
        assert!(modules.contains(&"MyGem".to_string()));
        assert!(!modules.contains(&"Configuration".to_string()));
        assert!(!modules.contains(&"Error".to_string()));
        assert!(!modules.contains(&"VERSION".to_string()));
    }

    #[test]
    fn discover_module_names_skips_version_subfile() {
        // lib/<gem>/version.rb must NOT be scanned (it only contains VERSION).
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        let gem_dir = gems_dir.join("mygem-1.0.0");
        let lib_dir = gem_dir.join("lib");
        let sub_dir = lib_dir.join("mygem");
        std::fs::create_dir_all(&sub_dir).unwrap();
        std::fs::write(lib_dir.join("mygem.rb"), "module MyGem\nend\n").unwrap();
        std::fs::write(
            sub_dir.join("version.rb"),
            "module MyGem\n  VERSION = '1.0.0'\nend\n",
        )
        .unwrap();

        let index = build_gem_index(&[dir.path().to_path_buf()]);
        let modules = discover_gem_module_names("mygem", &index);
        // Only top-level scanned; version.rb is not.
        assert_eq!(modules, vec!["MyGem".to_string()]);
    }

    // ── select_rvm_gemset (deterministic RVM selection) ──

    #[test]
    fn discover_module_names_missing_gem() {
        let index = HashMap::new();
        let modules = discover_gem_module_names("nonexistent", &index);
        assert!(modules.is_empty());
    }

    #[test]
    fn discover_module_names_no_lib() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        std::fs::create_dir_all(gems_dir.join("mygem-1.0.0")).unwrap();

        let index = build_gem_index(&[dir.path().to_path_buf()]);
        let modules = discover_gem_module_names("mygem", &index);
        assert!(modules.is_empty());
    }

    #[test]
    fn rvm_select_prefers_version_plus_gemset() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let gems = dir.path().join("gems");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(".ruby-version"), "3.3.0\n").unwrap();
        std::fs::write(project.join(".ruby-gemset"), "myapp\n").unwrap();
        // Multiple candidates exist
        std::fs::create_dir_all(gems.join("ruby-3.3.0").join("gems")).unwrap();
        std::fs::create_dir_all(gems.join("ruby-3.3.0@myapp").join("gems")).unwrap();
        std::fs::create_dir_all(gems.join("ruby-3.4.0").join("gems")).unwrap();

        let chosen = select_rvm_gemset(&gems, &project).unwrap();
        assert_eq!(chosen, gems.join("ruby-3.3.0@myapp"));
    }

    #[test]
    fn rvm_select_prefers_version_only_when_no_gemset_file() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let gems = dir.path().join("gems");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(".ruby-version"), "3.2.1\n").unwrap();
        std::fs::create_dir_all(gems.join("ruby-3.2.1").join("gems")).unwrap();
        std::fs::create_dir_all(gems.join("ruby-3.4.0").join("gems")).unwrap();
        std::fs::create_dir_all(gems.join("ruby-3.2.1@other").join("gems")).unwrap();

        let chosen = select_rvm_gemset(&gems, &project).unwrap();
        assert_eq!(chosen, gems.join("ruby-3.2.1"));
    }

    #[test]
    fn rvm_select_deterministic_fallback() {
        // No project hints — fallback must be deterministic regardless of
        // filesystem read order.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let gems = dir.path().join("gems");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(gems.join("ruby-3.2.0").join("gems")).unwrap();
        std::fs::create_dir_all(gems.join("ruby-3.3.10").join("gems")).unwrap();
        std::fs::create_dir_all(gems.join("ruby-3.3.10@global")).unwrap(); // skipped
        std::fs::create_dir_all(gems.join("ruby-3.3.10@app").join("gems")).unwrap();
        std::fs::create_dir_all(gems.join("ruby-3.4.0").join("gems")).unwrap();

        // Run twice — must return the same result both times.
        let a = select_rvm_gemset(&gems, &project).unwrap();
        let b = select_rvm_gemset(&gems, &project).unwrap();
        assert_eq!(a, b);
        // Plain (no @gemset) is preferred; among plain entries, longest name wins,
        // ties broken by descending alphabetic order ⇒ "ruby-3.3.10".
        assert_eq!(a, gems.join("ruby-3.3.10"));
    }

    #[test]
    fn rvm_select_fallback_picks_at_gemset_when_no_plain() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let gems = dir.path().join("gems");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(gems.join("ruby-3.3.0@app").join("gems")).unwrap();
        std::fs::create_dir_all(gems.join("ruby-3.3.0@global")).unwrap();

        let chosen = select_rvm_gemset(&gems, &project).unwrap();
        assert_eq!(chosen, gems.join("ruby-3.3.0@app"));
    }

    // ── parse_gem_metadata_yaml ──

    #[test]
    fn parse_metadata_yaml_basic() {
        let yaml = "--- !ruby/object:Gem::Specification\nname: rails\nversion: !ruby/object:Gem::Version\n  version: 7.1.0\nrequire_paths:\n- lib\nexecutables:\n- rails\n";
        let meta = parse_gem_metadata_yaml(yaml);
        assert!(meta.is_some());
        let meta = meta.unwrap();
        assert_eq!(meta.require_paths, vec!["lib"]);
        assert_eq!(meta.executables, vec!["rails"]);
    }

    #[test]
    fn parse_metadata_yaml_inline() {
        let yaml = "require_paths: [\"lib\", \"ext\"]\nexecutables: [\"rake\"]\n";
        let meta = parse_gem_metadata_yaml(yaml);
        assert!(meta.is_some());
        let meta = meta.unwrap();
        assert_eq!(meta.require_paths, vec!["lib", "ext"]);
        assert_eq!(meta.executables, vec!["rake"]);
    }

    #[test]
    fn parse_metadata_yaml_empty_executables() {
        let yaml = "require_paths:\n- lib\nexecutables: []\n";
        let meta = parse_gem_metadata_yaml(yaml);
        assert!(meta.is_some());
        let meta = meta.unwrap();
        assert_eq!(meta.require_paths, vec!["lib"]);
        assert!(meta.executables.is_empty());
    }

    #[test]
    fn parse_metadata_yaml_defaults_to_lib() {
        let yaml = "name: simple_gem\nversion: 1.0.0\n";
        let meta = parse_gem_metadata_yaml(yaml);
        assert!(meta.is_some());
        assert_eq!(meta.unwrap().require_paths, vec!["lib"]);
    }

    // ── extract_gem_name_from_filename ──

    #[test]
    fn gem_name_from_filename_basic() {
        assert_eq!(
            extract_gem_name_from_filename("rails-7.1.0"),
            Some("rails".to_string())
        );
    }

    #[test]
    fn gem_name_from_filename_hyphenated() {
        assert_eq!(
            extract_gem_name_from_filename("aws-sdk-s3-1.143.0"),
            Some("aws-sdk-s3".to_string())
        );
    }

    #[test]
    fn gem_name_from_filename_no_version() {
        assert_eq!(extract_gem_name_from_filename("just-a-name"), None);
    }

    // ── find_gem_paths edge cases ──

    #[test]
    fn find_gem_paths_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        // No gems installed, no env vars — should return empty or fall back
        let paths = find_gem_paths(dir.path());
        // This may return something from the system or empty depending on env
        // Just verify it doesn't panic
        let _ = paths;
    }

    #[test]
    fn find_gem_paths_with_bundle_config() {
        let dir = tempfile::tempdir().unwrap();
        let bundle_dir = dir.path().join(".bundle");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        let vendor = dir.path().join("vendor/gems/ruby/3.3.0");
        std::fs::create_dir_all(vendor.join("gems")).unwrap();
        std::fs::write(
            bundle_dir.join("config"),
            "---\nBUNDLE_PATH: \"vendor/gems\"\n",
        )
        .unwrap();

        let paths = find_gem_paths(dir.path());
        assert!(!paths.is_empty());
    }

    // ── build_vendor_cache_index ──

    #[test]
    fn vendor_cache_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let index = build_vendor_cache_index(dir.path());
        assert!(index.is_empty());
    }

    // ── find_bundler_gem_root ──

    #[test]
    fn bundler_gem_root_found() {
        let dir = tempfile::tempdir().unwrap();
        let ruby_dir = dir.path().join("ruby").join("3.3.0");
        std::fs::create_dir_all(ruby_dir.join("gems")).unwrap();
        let result = find_bundler_gem_root(dir.path());
        assert!(result.is_some());
        assert!(result.unwrap().ends_with("3.3.0"));
    }

    #[test]
    fn bundler_gem_root_no_ruby_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = find_bundler_gem_root(dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn bundler_gem_root_no_gems_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let ruby_dir = dir.path().join("ruby").join("3.3.0");
        std::fs::create_dir_all(&ruby_dir).unwrap();
        // No "gems" subdir inside version dir
        let result = find_bundler_gem_root(dir.path());
        assert!(result.is_none());
    }

    // ── find_ruby_version_file ──

    #[test]
    fn ruby_version_file_in_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".ruby-version"), "3.3.0\n").unwrap();
        let result = find_ruby_version_file(dir.path());
        assert_eq!(result, Some("3.3.0".to_string()));
    }

    #[test]
    fn ruby_version_file_in_parent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".ruby-version"), "3.2.0\n").unwrap();
        let child = dir.path().join("subdir");
        std::fs::create_dir_all(&child).unwrap();
        let result = find_ruby_version_file(&child);
        assert_eq!(result, Some("3.2.0".to_string()));
    }

    #[test]
    fn ruby_version_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".ruby-version"), "\n").unwrap();
        // Empty version string — should continue walking up
        let result = find_ruby_version_file(dir.path());
        // May or may not find one higher up; just check it doesn't panic
        let _ = result;
    }

    #[test]
    fn ruby_version_file_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("deep").join("nested");
        std::fs::create_dir_all(&child).unwrap();
        // No .ruby-version anywhere in temp dir
        // This tests the "walk to root" path
        let _ = find_ruby_version_file(&child);
    }

    // ── build_gem_index edge cases ──

    #[test]
    fn build_gem_index_fallback_no_dot_version() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        // Version without a dot (rare, hits fallback path)
        std::fs::create_dir_all(gems_dir.join("mygem-1")).unwrap();
        let index = build_gem_index(&[dir.path().to_path_buf()]);
        assert!(index.contains_key("mygem"));
    }

    #[test]
    fn build_gem_index_non_dir_entry() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        std::fs::create_dir_all(&gems_dir).unwrap();
        // A file instead of a directory — should be skipped
        std::fs::write(gems_dir.join("rails-7.1.0"), "not a dir").unwrap();
        let index = build_gem_index(&[dir.path().to_path_buf()]);
        assert!(!index.contains_key("rails"));
    }

    #[test]
    fn build_gem_index_no_version() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        std::fs::create_dir_all(gems_dir.join("my-extension")).unwrap();
        let index = build_gem_index(&[dir.path().to_path_buf()]);
        // "my-extension" doesn't have a numeric version, so should not be indexed
        assert!(!index.contains_key("my"));
    }

    #[test]
    fn build_gem_index_multiple_paths() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir1.path().join("gems").join("rails-7.0.0")).unwrap();
        std::fs::create_dir_all(dir2.path().join("gems").join("puma-6.0.0")).unwrap();
        let index = build_gem_index(&[dir1.path().to_path_buf(), dir2.path().to_path_buf()]);
        assert!(index.contains_key("rails"));
        assert!(index.contains_key("puma"));
    }

    // ── discover_gem_require_paths edge cases ──

    #[test]
    fn discover_require_paths_with_custom_gemspec() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        let gem_dir = gems_dir.join("mygem-1.0.0");
        std::fs::create_dir_all(&gem_dir).unwrap();
        // Gemspec with custom require_paths
        std::fs::write(
            gem_dir.join("mygem.gemspec"),
            "Gem::Specification.new do |s|\n  s.require_paths = [\"lib\", \"ext\"]\nend\n",
        )
        .unwrap();
        let lib_dir = gem_dir.join("lib");
        let ext_dir = gem_dir.join("ext");
        std::fs::create_dir_all(&lib_dir).unwrap();
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(lib_dir.join("mygem.rb"), "# main").unwrap();
        std::fs::write(ext_dir.join("native.rb"), "# native").unwrap();

        let index = build_gem_index(&[dir.path().to_path_buf()]);
        let paths = discover_gem_require_paths("mygem", &index).unwrap();
        assert!(paths.contains(&"mygem".to_string()));
        assert!(paths.contains(&"native".to_string()));
    }

    #[test]
    fn discover_require_paths_nonexistent_require_dir() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        let gem_dir = gems_dir.join("mygem-1.0.0");
        std::fs::create_dir_all(&gem_dir).unwrap();
        // Gemspec points to "ext" but that dir doesn't exist
        std::fs::write(
            gem_dir.join("mygem.gemspec"),
            "Gem::Specification.new do |s|\n  s.require_path = 'ext'\nend\n",
        )
        .unwrap();

        let index = build_gem_index(&[dir.path().to_path_buf()]);
        let paths = discover_gem_require_paths("mygem", &index);
        assert!(paths.is_none());
    }

    #[test]
    fn discover_require_paths_with_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        let gem_dir = gems_dir.join("mygem-1.0.0");
        let lib_dir = gem_dir.join("lib");
        std::fs::create_dir_all(&lib_dir).unwrap();
        // A subdirectory in lib/ (like "mygem/")
        std::fs::create_dir(lib_dir.join("mygem")).unwrap();
        // A non-.rb file should be ignored
        std::fs::write(lib_dir.join("README.md"), "# readme").unwrap();

        let index = build_gem_index(&[dir.path().to_path_buf()]);
        let paths = discover_gem_require_paths("mygem", &index).unwrap();
        assert!(paths.contains(&"mygem".to_string()));
        assert!(!paths.contains(&"README".to_string()));
    }

    // ── extract_gem_metadata + build_vendor_cache_index ──

    #[test]
    fn extract_gem_metadata_from_tarball() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let gem_path = dir.path().join("test-1.0.0.gem");

        // Create a minimal .gem file (tar archive with metadata.gz)
        let yaml = "--- !ruby/object:Gem::Specification\nname: test\nversion: 1.0.0\nrequire_paths:\n- lib\nexecutables:\n- test_exe\n";

        // Compress yaml with gzip
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(yaml.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();

        // Create tar archive
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_path("metadata.gz").unwrap();
        header.set_size(compressed.len() as u64);
        header.set_cksum();
        builder.append(&header, &compressed[..]).unwrap();
        let tar_data = builder.into_inner().unwrap();

        std::fs::write(&gem_path, &tar_data).unwrap();

        let meta = extract_gem_metadata(&gem_path);
        assert!(meta.is_some());
        let meta = meta.unwrap();
        assert_eq!(meta.require_paths, vec!["lib"]);
        assert_eq!(meta.executables, vec!["test_exe"]);
    }

    #[test]
    fn build_vendor_cache_index_with_gem() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("vendor").join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        let yaml = "--- !ruby/object:Gem::Specification\nname: mygem\nrequire_paths:\n- lib\nexecutables:\n- myexe\n";
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(yaml.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();

        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_path("metadata.gz").unwrap();
        header.set_size(compressed.len() as u64);
        header.set_cksum();
        builder.append(&header, &compressed[..]).unwrap();
        let tar_data = builder.into_inner().unwrap();

        std::fs::write(cache_dir.join("mygem-1.0.0.gem"), &tar_data).unwrap();

        let index = build_vendor_cache_index(dir.path());
        assert!(index.contains_key("mygem"));
        let meta = &index["mygem"];
        assert_eq!(meta.executables, vec!["myexe"]);
    }

    // ── find_gem_paths with GEM_HOME bundle path ──

    #[test]
    fn find_gem_paths_bundle_path_direct_gems() {
        let dir = tempfile::tempdir().unwrap();
        let bundle_dir = dir.path().join(".bundle");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        // BUNDLE_PATH with gems directly under it (no ruby/<version> subdir)
        let vendor = dir.path().join("vendor/gems");
        std::fs::create_dir_all(vendor.join("gems")).unwrap();
        std::fs::write(
            bundle_dir.join("config"),
            "---\nBUNDLE_PATH: \"vendor/gems\"\n",
        )
        .unwrap();

        let paths = find_gem_paths(dir.path());
        assert!(!paths.is_empty());
    }

    // ── bundle_path edge cases ──

    #[test]
    fn bundle_path_empty_value() {
        let dir = tempfile::tempdir().unwrap();
        let bundle_dir = dir.path().join(".bundle");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(bundle_dir.join("config"), "---\nBUNDLE_PATH: \"\"\n").unwrap();
        assert_eq!(read_bundle_path(dir.path()), None);
    }

    #[test]
    fn bundle_path_unquoted() {
        let dir = tempfile::tempdir().unwrap();
        let bundle_dir = dir.path().join(".bundle");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join("config"),
            "---\nBUNDLE_PATH: vendor/bundle\n",
        )
        .unwrap();
        assert_eq!(
            read_bundle_path(dir.path()),
            Some(PathBuf::from("vendor/bundle"))
        );
    }

    // ── parse_gem_metadata_yaml edge cases ──

    #[test]
    fn parse_metadata_yaml_multiline_executables() {
        let yaml = "name: mygem\nrequire_paths:\n- lib\n- ext\nexecutables:\n- exe1\n- exe2\nother_field: value\n";
        let meta = parse_gem_metadata_yaml(yaml).unwrap();
        assert_eq!(meta.require_paths, vec!["lib", "ext"]);
        assert_eq!(meta.executables, vec!["exe1", "exe2"]);
    }

    #[test]
    fn parse_metadata_no_sections() {
        // Just random YAML with no require_paths or executables
        let yaml = "name: test\nversion: 1.0\nauthors:\n- Alice\n";
        let meta = parse_gem_metadata_yaml(yaml).unwrap();
        assert_eq!(meta.require_paths, vec!["lib"]); // defaults
        assert!(meta.executables.is_empty());
    }

    // ── extract_gem_name_from_filename edge cases ──

    #[test]
    fn gem_name_platform_suffix() {
        assert_eq!(
            extract_gem_name_from_filename("nokogiri-1.16.0-x86_64-linux"),
            Some("nokogiri".to_string())
        );
    }

    #[test]
    fn gem_name_pre_release() {
        assert_eq!(
            extract_gem_name_from_filename("rails-7.2.0.beta1"),
            Some("rails".to_string())
        );
    }

    // ── build_vendor_cache_path_index ──

    #[test]
    fn path_index_returns_correct_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("vendor").join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        // Create empty .gem files — path index should not care about contents
        std::fs::write(cache_dir.join("rails-7.1.3.gem"), b"").unwrap();
        std::fs::write(cache_dir.join("nokogiri-1.16.0-x86_64-linux.gem"), b"").unwrap();
        std::fs::write(cache_dir.join("aws-sdk-s3-1.143.0.gem"), b"").unwrap();
        // Non-gem file should be ignored
        std::fs::write(cache_dir.join("README.txt"), b"").unwrap();

        let index = build_vendor_cache_path_index(dir.path());
        assert_eq!(index.len(), 3);
        assert_eq!(index["rails"], cache_dir.join("rails-7.1.3.gem"));
        assert_eq!(
            index["nokogiri"],
            cache_dir.join("nokogiri-1.16.0-x86_64-linux.gem")
        );
        assert_eq!(index["aws-sdk-s3"], cache_dir.join("aws-sdk-s3-1.143.0.gem"));
        assert!(!index.contains_key("README"));
    }

    #[test]
    fn path_index_keeps_latest_version() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("vendor").join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        std::fs::write(cache_dir.join("rails-6.1.0.gem"), b"").unwrap();
        std::fs::write(cache_dir.join("rails-7.1.3.gem"), b"").unwrap();
        std::fs::write(cache_dir.join("rails-7.0.8.gem"), b"").unwrap();

        let index = build_vendor_cache_path_index(dir.path());
        assert_eq!(index.len(), 1);
        assert_eq!(index["rails"], cache_dir.join("rails-7.1.3.gem"));
    }
}
