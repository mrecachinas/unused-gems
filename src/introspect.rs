use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use regex::Regex;

use crate::lockfile::Lockfile;
use crate::types::{GemEntry, GemSource};

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

#[derive(Debug, Clone, PartialEq, Eq)]
enum VersionSegment {
    /// Decimal digits with leading zeros stripped ("" means zero).
    Num(String),
    Str(String),
}

impl VersionSegment {
    fn is_zero(&self) -> bool {
        matches!(self, VersionSegment::Num(n) if n.is_empty())
    }
}

/// Split a version into RubyGems-style canonical segments: numeric and alpha
/// runs, `-` treated as `.pre.`, with trailing zeros removed from both the
/// release part and the prerelease part (mirrors `Gem::Version#canonical_segments`).
fn canonical_version_segments(version: &str) -> Vec<VersionSegment> {
    let version = version.trim().replace('-', ".pre.");
    let bytes = version.as_bytes();
    let mut segments = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        if bytes[i].is_ascii_digit() {
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let digits = version[start..i].trim_start_matches('0');
            segments.push(VersionSegment::Num(digits.to_string()));
        } else if bytes[i].is_ascii_alphabetic() {
            while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            segments.push(VersionSegment::Str(version[start..i].to_string()));
        } else {
            i += 1;
        }
    }

    let split = segments
        .iter()
        .position(|segment| matches!(segment, VersionSegment::Str(_)))
        .unwrap_or(segments.len());
    let mut prerelease = segments.split_off(split);
    while segments.last().is_some_and(VersionSegment::is_zero) {
        segments.pop();
    }
    while prerelease.last().is_some_and(VersionSegment::is_zero) {
        prerelease.pop();
    }
    segments.extend(prerelease);
    segments
}

/// Compare two gem version strings with `Gem::Version` semantics:
/// numeric segments compare numerically ("1.16.8" > "1.9.1"), and any string
/// segment marks a prerelease that sorts before the corresponding release
/// ("1.0.0.rc1" < "1.0.0", "2.0.0.beta9" < "2.0.0.beta10").
fn compare_gem_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let a = canonical_version_segments(a);
    let b = canonical_version_segments(b);
    let zero = VersionSegment::Num(String::new());
    for i in 0..a.len().max(b.len()) {
        let lhs = a.get(i).unwrap_or(&zero);
        let rhs = b.get(i).unwrap_or(&zero);
        let ordering = match (lhs, rhs) {
            (VersionSegment::Num(l), VersionSegment::Num(r)) => {
                l.len().cmp(&r.len()).then_with(|| l.cmp(r))
            }
            (VersionSegment::Str(l), VersionSegment::Str(r)) => l.cmp(r),
            (VersionSegment::Str(_), VersionSegment::Num(_)) => Ordering::Less,
            (VersionSegment::Num(_), VersionSegment::Str(_)) => Ordering::Greater,
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
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

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn env_string(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn push_unique_dir(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if path.is_dir() && !paths.contains(&path) {
        paths.push(path);
    }
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
        if let Some(gem_root) = find_bundler_gem_root(&abs, project_dir) {
            return vec![gem_root];
        }
        // Some layouts put gems directly under BUNDLE_PATH
        if abs.join("gems").is_dir() {
            return vec![abs];
        }
    }

    // 2. GEM_PATH / GEM_HOME env. RubyGems searches GEM_PATH followed by
    //    GEM_HOME; without GEM_PATH it searches the Ruby's default dirs + GEM_HOME.
    let gem_path_env = std::env::var_os("GEM_PATH").filter(|v| !v.is_empty());
    let gem_home_env = env_path("GEM_HOME");
    if gem_path_env.is_some() {
        let paths = gem_paths_from_env(gem_path_env.as_deref(), gem_home_env.as_deref());
        if !paths.is_empty() {
            return paths;
        }
    } else if let Some(gem_home) = gem_home_env {
        if gem_home.is_dir() {
            let mut paths = probe_version_manager_gem_paths(project_dir).unwrap_or_default();
            paths.retain(|path| path != &gem_home);
            paths.insert(0, gem_home);
            return paths;
        }
    }

    // 3. Probe version managers (rbenv, rvm, mise, asdf, chruby, PATH) directly.
    if let Some(paths) = probe_version_manager_gem_paths(project_dir) {
        return paths;
    }

    // 4. Last resort: shell out to `gem environment gempath` from the project
    //    so shims and per-directory version files resolve like they would for
    //    the project itself.
    if let Ok(output) = std::process::Command::new("gem")
        .args(["environment", "gempath"])
        .current_dir(project_dir)
        .output()
    {
        if output.status.success() {
            let paths = parse_gem_environment_gempath(&String::from_utf8_lossy(&output.stdout));
            if !paths.is_empty() {
                return paths;
            }
        }
    }

    Vec::new()
}

fn parse_gem_environment_gempath(output: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for path in std::env::split_paths(output.trim()) {
        if !path.as_os_str().is_empty() && !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

/// Combine GEM_PATH entries with GEM_HOME (RubyGems always searches GEM_HOME
/// too), keeping order, dropping duplicates and non-directories.
fn gem_paths_from_env(gem_path: Option<&std::ffi::OsStr>, gem_home: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(gem_path) = gem_path {
        for path in std::env::split_paths(gem_path) {
            if !path.as_os_str().is_empty() {
                push_unique_dir(&mut paths, path);
            }
        }
    }
    if let Some(gem_home) = gem_home {
        push_unique_dir(&mut paths, gem_home.to_path_buf());
    }
    paths
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
/// Bundler installs to <path>/ruby/<api_version>/. When several API versions
/// are present, prefer the active Ruby's; otherwise pick the newest.
fn find_bundler_gem_root(bundle_path: &Path, project_dir: &Path) -> Option<PathBuf> {
    let candidates = bundler_api_dirs(bundle_path);
    if candidates.len() > 1 {
        if let Some(api) = active_ruby_api_version(project_dir) {
            if let Some((path, _)) = candidates.iter().find(|(_, name)| *name == api) {
                return Some(path.clone());
            }
        }
    }
    newest_api_dir(candidates)
}

fn bundler_api_dirs(bundle_path: &Path) -> Vec<(PathBuf, String)> {
    let mut candidates = Vec::new();
    if let Ok(entries) = std::fs::read_dir(bundle_path.join("ruby")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join("gems").is_dir() {
                candidates.push((path, entry.file_name().to_string_lossy().to_string()));
            }
        }
    }
    candidates
}

fn newest_api_dir(candidates: Vec<(PathBuf, String)>) -> Option<PathBuf> {
    candidates
        .into_iter()
        .max_by(|a, b| compare_gem_versions(&a.1, &b.1).then_with(|| a.1.cmp(&b.1)))
        .map(|(path, _)| path)
}

/// RubyGems API version ("3.4.0") of the Ruby the project would run, derived
/// without spawning: from a probed install prefix, else from `.ruby-version`.
fn active_ruby_api_version(project_dir: &Path) -> Option<String> {
    let home = home_dir();
    if let Some(prefix) = probe_ruby_prefix(project_dir, home.as_deref()) {
        if let Some(api) = ruby_prefix_gem_dir(&prefix)
            .and_then(|dir| dir.file_name().map(|n| n.to_string_lossy().to_string()))
        {
            return Some(api);
        }
    }
    let version = find_ruby_version_file(project_dir)?;
    api_version_from_ruby_version(&version)
}

/// "3.4.8" / "ruby-3.4.8" → "3.4.0".
fn api_version_from_ruby_version(version: &str) -> Option<String> {
    let version = version.trim().trim_start_matches("ruby-");
    let mut parts = version.split('.');
    let major = parts
        .next()
        .filter(|p| p.chars().all(|c| c.is_ascii_digit()))?;
    let minor = parts
        .next()
        .filter(|p| p.chars().all(|c| c.is_ascii_digit()))?;
    if major.is_empty() || minor.is_empty() {
        return None;
    }
    Some(format!("{major}.{minor}.0"))
}

/// `<prefix>/lib/ruby/gems/<api>` (newest API dir with a `gems/` subdir).
fn ruby_prefix_gem_dir(prefix: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(entries) = std::fs::read_dir(prefix.join("lib").join("ruby").join("gems")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join("gems").is_dir() {
                candidates.push((path, entry.file_name().to_string_lossy().to_string()));
            }
        }
    }
    newest_api_dir(candidates)
}

/// Gem dirs RubyGems searches by default for a Ruby install prefix:
/// `<prefix>/lib/ruby/gems/<api>` plus the user dir `~/.gem/ruby/<api>`.
fn gem_dirs_for_ruby_prefix(prefix: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let Some(default_dir) = ruby_prefix_gem_dir(prefix) else {
        return Vec::new();
    };
    let mut dirs = vec![default_dir.clone()];
    if let (Some(home), Some(api)) = (home, default_dir.file_name()) {
        let user_dir = home.join(".gem").join("ruby").join(api);
        if user_dir.join("gems").is_dir() {
            dirs.push(user_dir);
        }
    }
    dirs
}

/// Filesystem-only probes for the active Ruby's gem directories.
fn probe_version_manager_gem_paths(project_dir: &Path) -> Option<Vec<PathBuf>> {
    let home = home_dir();
    if let Some(prefix) = probe_rbenv(project_dir, home.as_deref()) {
        let dirs = gem_dirs_for_ruby_prefix(&prefix, home.as_deref());
        if !dirs.is_empty() {
            return Some(dirs);
        }
    }
    if let Some(path) = probe_rvm(project_dir) {
        return Some(vec![path]);
    }
    let prefix = probe_mise(project_dir, home.as_deref())
        .or_else(|| probe_asdf(project_dir, home.as_deref()))
        .or_else(|| probe_chruby(project_dir, home.as_deref()))
        .or_else(|| probe_path_ruby(home.as_deref()))?;
    let dirs = gem_dirs_for_ruby_prefix(&prefix, home.as_deref());
    (!dirs.is_empty()).then_some(dirs)
}

/// Install prefix of the project's Ruby (rbenv, mise, asdf, chruby, PATH).
fn probe_ruby_prefix(project_dir: &Path, home: Option<&Path>) -> Option<PathBuf> {
    probe_rbenv(project_dir, home)
        .or_else(|| probe_mise(project_dir, home))
        .or_else(|| probe_asdf(project_dir, home))
        .or_else(|| probe_chruby(project_dir, home))
        .or_else(|| probe_path_ruby(home))
}

/// Pick the install dir for a version spec inside `versions_dir`.
/// Exact names win; otherwise the spec is matched as a segment-aware prefix
/// (`3.3` matches `3.3.10` but not `3.30.0`) and the highest version is chosen.
/// `name_prefix` handles layouts like chruby's `ruby-3.4.8`.
fn select_version_dir(versions_dir: &Path, spec: &str, name_prefix: &str) -> Option<PathBuf> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    let mut specs = vec![spec.to_string()];
    if let Some(stripped) = spec.strip_prefix("ruby-") {
        specs.push(stripped.to_string());
    }
    for spec in &specs {
        let exact = versions_dir.join(format!("{name_prefix}{spec}"));
        if exact.is_dir() {
            return Some(exact);
        }
    }

    let entries = std::fs::read_dir(versions_dir).ok()?;
    let mut best: Option<(String, PathBuf)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(version) = name.strip_prefix(name_prefix) else {
            continue;
        };
        if !version.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let matches = specs.iter().any(|spec| {
            spec == "latest"
                || version == spec
                || version
                    .strip_prefix(spec.as_str())
                    .is_some_and(|rest| rest.starts_with('.'))
        });
        if !matches || !entry.path().is_dir() {
            continue;
        }
        let better = best
            .as_ref()
            .is_none_or(|(best_version, _)| compare_gem_versions(version, best_version).is_gt());
        if better {
            best = Some((version.to_string(), entry.path()));
        }
    }
    best.map(|(_, path)| path)
}

/// Probe rbenv filesystem to find the Ruby prefix without spawning a process.
/// Replicates rbenv's version resolution: RBENV_VERSION env → .ruby-version
/// walking up from project_dir → ~/.rbenv/version (global default).
fn probe_rbenv(project_dir: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let rbenv_root = env_path("RBENV_ROOT").or_else(|| home.map(|h| h.join(".rbenv")))?;
    let versions_dir = rbenv_root.join("versions");
    if !versions_dir.is_dir() {
        return None;
    }

    let version = env_string("RBENV_VERSION").or_else(|| {
        find_ruby_version_file(project_dir).or_else(|| {
            std::fs::read_to_string(rbenv_root.join("version"))
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        })
    })?;
    select_version_dir(&versions_dir, &version, "")
}

fn mise_data_dir(home: Option<&Path>) -> Option<PathBuf> {
    env_path("MISE_DATA_DIR")
        .or_else(|| env_path("XDG_DATA_HOME").map(|p| p.join("mise")))
        .or_else(|| home.map(|h| h.join(".local").join("share").join("mise")))
}

fn mise_global_config(home: Option<&Path>) -> Option<PathBuf> {
    env_path("MISE_GLOBAL_CONFIG_FILE")
        .or_else(|| env_path("MISE_CONFIG_DIR").map(|p| p.join("config.toml")))
        .or_else(|| env_path("XDG_CONFIG_HOME").map(|p| p.join("mise").join("config.toml")))
        .or_else(|| home.map(|h| h.join(".config").join("mise").join("config.toml")))
}

/// Probe mise installs (`<data>/installs/ruby/<version>`) using mise's config
/// resolution: MISE_RUBY_VERSION → nearest mise.toml / .tool-versions (or
/// `.ruby-version` when idiomatic files are enabled for ruby) → global config.
fn probe_mise(project_dir: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let installs = mise_data_dir(home)?.join("installs").join("ruby");
    if !installs.is_dir() {
        return None;
    }
    let global_config = mise_global_config(home);
    let global_text = global_config
        .as_ref()
        .and_then(|path| std::fs::read_to_string(path).ok());
    let idiomatic = env_string("MISE_IDIOMATIC_VERSION_FILE_ENABLE_TOOLS")
        .is_some_and(|tools| tools.split(',').any(|t| t.trim() == "ruby"))
        || global_text
            .as_deref()
            .is_some_and(mise_idiomatic_ruby_enabled);
    let version = env_string("MISE_RUBY_VERSION")
        .or_else(|| find_mise_ruby_version(project_dir, idiomatic))
        .or_else(|| global_text.as_deref().and_then(parse_mise_toml_ruby))?;
    select_version_dir(&installs, &version, "")
}

fn mise_idiomatic_ruby_enabled(config: &str) -> bool {
    config.lines().any(|line| {
        let line = line.trim();
        (line.starts_with("idiomatic_version_file_enable_tools")
            && (line.contains("\"ruby\"") || line.contains("'ruby'")))
            || line.replace(' ', "") == "legacy_version_file=true"
    })
}

/// Walk up from `start_dir` looking for the nearest mise-style ruby pin.
fn find_mise_ruby_version(start_dir: &Path, idiomatic: bool) -> Option<String> {
    const TOML_FILES: &[&str] = &[
        "mise.local.toml",
        ".mise.local.toml",
        "mise.toml",
        ".mise.toml",
        "mise/config.toml",
        ".mise/config.toml",
        ".config/mise.toml",
        ".config/mise/config.toml",
    ];
    let mut dir = start_dir
        .canonicalize()
        .unwrap_or_else(|_| start_dir.to_path_buf());
    loop {
        for file in TOML_FILES {
            if let Ok(content) = std::fs::read_to_string(dir.join(file)) {
                if let Some(version) = parse_mise_toml_ruby(&content) {
                    return Some(version);
                }
            }
        }
        if let Ok(content) = std::fs::read_to_string(dir.join(".tool-versions")) {
            if let Some(version) = parse_tool_versions_ruby(&content) {
                return Some(version);
            }
        }
        if idiomatic {
            if let Some(version) = read_ruby_version_in(&dir) {
                return Some(version);
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Extract `ruby = "x"` (or `ruby = ["x", ...]` / `ruby = { version = "x" }`)
/// from a mise TOML `[tools]` table.
fn parse_mise_toml_ruby(content: &str) -> Option<String> {
    let mut in_tools = false;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_tools = line.trim_end_matches(|c: char| c == ']' || c.is_whitespace()) == "[tools";
            continue;
        }
        if !in_tools {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_matches(|c| c == '"' || c == '\'');
        if key != "ruby" && key != "core:ruby" {
            continue;
        }
        return extract_quoted_strings(value).into_iter().find(|v| {
            let v = v.trim();
            !v.is_empty() && v != "version"
        });
    }
    None
}

/// Extract the first ruby version from a `.tool-versions` file.
fn parse_tool_versions_ruby(content: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let line = line.split('#').next().unwrap_or("");
        let mut parts = line.split_whitespace();
        (parts.next() == Some("ruby"))
            .then(|| parts.next().map(str::to_string))
            .flatten()
    })
}

/// Probe asdf installs (`<ASDF_DATA_DIR>/installs/ruby/<version>`), resolving
/// the version from ASDF_RUBY_VERSION → nearest .tool-versions → ~/.tool-versions.
fn probe_asdf(project_dir: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let installs = env_path("ASDF_DATA_DIR")
        .or_else(|| home.map(|h| h.join(".asdf")))?
        .join("installs")
        .join("ruby");
    if !installs.is_dir() {
        return None;
    }
    let version = env_string("ASDF_RUBY_VERSION")
        .or_else(|| find_tool_versions_ruby(project_dir))
        .or_else(|| {
            let content = std::fs::read_to_string(home?.join(".tool-versions")).ok()?;
            parse_tool_versions_ruby(&content)
        })?;
    select_version_dir(&installs, &version, "")
}

fn find_tool_versions_ruby(start_dir: &Path) -> Option<String> {
    let mut dir = start_dir
        .canonicalize()
        .unwrap_or_else(|_| start_dir.to_path_buf());
    loop {
        if let Ok(content) = std::fs::read_to_string(dir.join(".tool-versions")) {
            if let Some(version) = parse_tool_versions_ruby(&content) {
                return Some(version);
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Probe chruby: an active `RUBY_ROOT`, else `.ruby-version` resolved against
/// `~/.rubies/ruby-<version>` and `/opt/rubies/ruby-<version>`.
fn probe_chruby(project_dir: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(root) = env_path("RUBY_ROOT") {
        if root.join("lib").join("ruby").join("gems").is_dir() {
            return Some(root);
        }
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(home) = home {
        roots.push(home.join(".rubies"));
    }
    roots.push(PathBuf::from("/opt/rubies"));
    roots.retain(|root| root.is_dir());
    if roots.is_empty() {
        return None;
    }
    let version = find_ruby_version_file(project_dir)?;
    let version = version.trim_start_matches("ruby-");
    roots
        .iter()
        .find_map(|root| select_version_dir(root, version, "ruby-"))
}

/// Derive the Ruby prefix from a `ruby` executable on PATH when its real path
/// lives inside a known version-manager install tree (no subprocess).
fn probe_path_ruby(home: Option<&Path>) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let mut install_roots: Vec<PathBuf> = Vec::new();
    if let Some(data) = mise_data_dir(home) {
        install_roots.push(data.join("installs").join("ruby"));
    }
    if let Some(asdf) = env_path("ASDF_DATA_DIR").or_else(|| home.map(|h| h.join(".asdf"))) {
        install_roots.push(asdf.join("installs").join("ruby"));
    }
    if let Some(rbenv) = env_path("RBENV_ROOT").or_else(|| home.map(|h| h.join(".rbenv"))) {
        install_roots.push(rbenv.join("versions"));
    }
    if let Some(home) = home {
        install_roots.push(home.join(".rubies"));
        install_roots.push(home.join(".rvm").join("rubies"));
    }
    install_roots.push(PathBuf::from("/opt/rubies"));
    let install_roots: Vec<PathBuf> = install_roots
        .into_iter()
        .filter_map(|root| root.canonicalize().ok())
        .collect();
    if install_roots.is_empty() {
        return None;
    }
    ruby_prefix_from_path_entries(std::env::split_paths(&path), &install_roots)
}

fn ruby_prefix_from_path_entries(
    entries: impl Iterator<Item = PathBuf>,
    install_roots: &[PathBuf],
) -> Option<PathBuf> {
    for dir in entries {
        let candidate = dir.join("ruby");
        if !candidate.is_file() {
            continue;
        }
        let Ok(real) = candidate.canonicalize() else {
            continue;
        };
        let Some(prefix) = real.parent().and_then(Path::parent) else {
            continue;
        };
        if install_roots.iter().any(|root| prefix.starts_with(root))
            && prefix.join("lib").join("ruby").join("gems").is_dir()
        {
            return Some(prefix.to_path_buf());
        }
    }
    None
}

fn read_ruby_version_in(dir: &Path) -> Option<String> {
    let content = std::fs::read_to_string(dir.join(".ruby-version")).ok()?;
    let version = content.trim().to_string();
    (!version.is_empty()).then_some(version)
}

/// Walk from project_dir upward to find a .ruby-version file (same as rbenv shim).
fn find_ruby_version_file(start_dir: &Path) -> Option<String> {
    let mut dir = start_dir
        .canonicalize()
        .ok()
        .unwrap_or_else(|| start_dir.to_path_buf());
    loop {
        if let Some(version) = read_ruby_version_in(&dir) {
            return Some(version);
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

/// Pre-build an index of gem_name -> gem_dir for fast lookups, using the
/// latest installed version when no lockfile selection is requested.
#[cfg_attr(not(test), allow(dead_code))]
pub fn build_gem_index(gem_paths: &[PathBuf]) -> HashMap<String, PathBuf> {
    build_gem_index_for_lock(gem_paths, &Lockfile::default(), None)
}

pub fn build_gem_index_for_lock(
    gem_paths: &[PathBuf],
    lockfile: &Lockfile,
    current_platform: Option<&str>,
) -> HashMap<String, PathBuf> {
    let mut candidates: HashMap<String, Vec<PathBuf>> = HashMap::new();
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
                        candidates
                            .entry(gem_name.to_string())
                            .or_default()
                            .push(entry.path());
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
                            candidates
                                .entry(gem_name.to_string())
                                .or_default()
                                .push(entry.path());
                        }
                    }
                }
            }
        }
    }

    candidates
        .into_iter()
        .filter_map(|(gem_name, paths)| {
            select_gem_artifact(&gem_name, paths, lockfile, current_platform)
                .map(|path| (gem_name, path))
        })
        .collect()
}

fn select_gem_artifact(
    gem_name: &str,
    mut candidates: Vec<PathBuf>,
    lockfile: &Lockfile,
    current_platform: Option<&str>,
) -> Option<PathBuf> {
    candidates.sort();
    for full_name in lockfile.preferred_registry_full_names(gem_name, current_platform) {
        if let Some(path) = candidates
            .iter()
            .find(|path| candidate_full_name(path).as_deref() == Some(full_name.as_str()))
        {
            return Some(path.clone());
        }
    }

    candidates.into_iter().max_by(|a, b| {
        let a_name = candidate_full_name(a).unwrap_or_default();
        let b_name = candidate_full_name(b).unwrap_or_default();
        let a_version = extract_gem_version(&a_name).unwrap_or_default();
        let b_version = extract_gem_version(&b_name).unwrap_or_default();
        compare_gem_versions(&a_version, &b_version).then_with(|| a.cmp(b))
    })
}

fn candidate_full_name(path: &Path) -> Option<String> {
    if path.extension().and_then(|extension| extension.to_str()) == Some("gem") {
        path.file_stem()
            .map(|name| name.to_string_lossy().to_string())
    } else {
        path.file_name()
            .map(|name| name.to_string_lossy().to_string())
    }
}

pub fn index_declared_gem_sources(
    project_dir: &Path,
    gems: &[GemEntry],
    index: &mut HashMap<String, PathBuf>,
) {
    for gem in gems {
        match gem.source.as_ref() {
            Some(GemSource::Path(path)) => {
                if looks_like_gem_source(path) {
                    index.insert(gem.name.clone(), path.clone());
                } else {
                    eprintln!(
                        "Warning: path gem '{}' does not look introspectable at {}",
                        gem.name,
                        path.display()
                    );
                }
            }
            Some(GemSource::Git(_)) => {
                if let Some(path) = find_local_git_checkout(project_dir, &gem.name) {
                    index.insert(gem.name.clone(), path);
                }
            }
            None => {}
        }
    }
}

fn looks_like_gem_source(path: &Path) -> bool {
    path.is_dir()
        && (path.join("lib").is_dir()
            || glob::glob(&path.join("*.gemspec").display().to_string())
                .into_iter()
                .flatten()
                .flatten()
                .next()
                .is_some())
}

fn find_local_git_checkout(project_dir: &Path, gem_name: &str) -> Option<PathBuf> {
    let mut roots = Vec::new();
    roots.push(project_dir.join(".bundle"));
    roots.push(project_dir.join("vendor").join("bundle"));
    if let Some(bundle_path) = read_bundle_path(project_dir) {
        roots.push(if bundle_path.is_absolute() {
            bundle_path
        } else {
            project_dir.join(bundle_path)
        });
    }

    for root in roots {
        let pattern = root
            .join("ruby")
            .join("*")
            .join("bundler")
            .join("gems")
            .join(format!("{}-*", gem_name))
            .display()
            .to_string();
        let mut paths: Vec<PathBuf> = glob::glob(&pattern)
            .into_iter()
            .flatten()
            .flatten()
            .collect();
        paths.sort();
        for path in paths {
            if looks_like_gem_source(&path) {
                return Some(path);
            }
        }
    }
    None
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
        let mut candidates = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Some(ext) = path.extension() {
                        if ext == "rb" {
                            if let Some(stem) = path.file_stem() {
                                candidates.push((stem.to_string_lossy().to_string(), true));
                            }
                        }
                    }
                } else if path.is_dir() {
                    if let Some(name) = path.file_name() {
                        candidates.push((name.to_string_lossy().to_string(), false));
                    }
                }
            }
        }
        if req_dir == "lib" {
            let normalized = gem_name.replace('-', "_");
            const AUXILIARY_DIRS: &[&str] =
                &["assets", "generators", "rails", "tasks", "templates"];
            let preferred: Vec<String> = candidates
                .iter()
                .filter(|(candidate, is_file)| {
                    *is_file
                        || candidate == gem_name
                        || candidate == &normalized
                        || !AUXILIARY_DIRS.contains(&candidate.as_str())
                })
                .map(|(candidate, _)| candidate.clone())
                .collect();
            if preferred.is_empty() {
                paths.extend(candidates.into_iter().map(|(candidate, _)| candidate));
            } else {
                paths.extend(preferred);
            }
        } else {
            paths.extend(candidates.into_iter().map(|(candidate, _)| candidate));
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
            static REQ_PATHS_ARRAY_RE: Lazy<Regex> =
                Lazy::new(|| Regex::new(r#"(?s)require_paths?\s*=\s*\[([^\]]+)\]"#).unwrap());
            static REQ_PATHS_PERCENT_RE: Lazy<Regex> = Lazy::new(|| {
                Regex::new(r#"(?s)require_paths?\s*=\s*%[wW][\[\(\{]([^\]\)\}]+)[\]\)\}]"#).unwrap()
            });
            static REQ_PATH_SINGLE_RE: Lazy<Regex> =
                Lazy::new(|| Regex::new(r#"require_path\s*=\s*['"]([^'"]+)['"]"#).unwrap());
            if let Some(cap) = REQ_PATHS_ARRAY_RE.captures(&content) {
                let paths = extract_quoted_strings(&cap[1]);
                if !paths.is_empty() {
                    return Some(paths);
                }
            }
            if let Some(cap) = REQ_PATHS_PERCENT_RE.captures(&content) {
                let paths: Vec<String> = cap[1]
                    .split_whitespace()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
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

fn extract_quoted_strings(input: &str) -> Vec<String> {
    static QUOTED_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"['"]([^'"]+)['"]"#).unwrap());
    QUOTED_RE
        .captures_iter(input)
        .map(|cap| cap[1].to_string())
        .collect()
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
/// Returns gem_name → PathBuf mapping, keeping the latest version per gem name
/// when no lockfile selection is requested.
/// Does NOT open any tarballs — only inspects filenames.
pub fn build_vendor_cache_path_index(project_dir: &Path) -> HashMap<String, PathBuf> {
    build_vendor_cache_path_index_for_lock(project_dir, &Lockfile::default(), None)
}

pub fn build_vendor_cache_path_index_for_lock(
    project_dir: &Path,
    lockfile: &Lockfile,
    current_platform: Option<&str>,
) -> HashMap<String, PathBuf> {
    let cache_dir = project_dir.join("vendor").join("cache");
    if !cache_dir.is_dir() {
        return HashMap::new();
    }

    let mut candidates: HashMap<String, Vec<PathBuf>> = HashMap::new();
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
        candidates.entry(gem_name).or_default().push(path);
    }

    candidates
        .into_iter()
        .filter_map(|(gem_name, paths)| {
            select_gem_artifact(&gem_name, paths, lockfile, current_platform)
                .map(|path| (gem_name, path))
        })
        .collect()
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

    #[test]
    fn compare_versions_prerelease_sorts_before_release() {
        use std::cmp::Ordering::*;
        assert_eq!(compare_gem_versions("1.0.0.rc1", "1.0.0"), Less);
        assert_eq!(compare_gem_versions("1.0.0", "1.0.0.rc1"), Greater);
        assert_eq!(compare_gem_versions("2.0.0.beta10", "2.0.0.beta9"), Greater);
        assert_eq!(compare_gem_versions("2.0.0.beta2", "2.0.0.rc1"), Less);
        assert_eq!(compare_gem_versions("1.0", "1.0.0"), Equal);
        assert_eq!(compare_gem_versions("1.0.a", "1.0.0.a"), Equal);
        assert_eq!(compare_gem_versions("1.0.0.pre", "0.9.9"), Greater);
        assert_eq!(
            compare_gem_versions("1.99999999999999999999999", "1.2"),
            Greater
        );
    }

    #[test]
    fn build_gem_index_prefers_release_over_prerelease() {
        let dir = tempfile::tempdir().unwrap();
        make_dirs(
            &dir.path().join("gems"),
            &[
                "foo-1.0.0.rc1",
                "foo-1.0.0",
                "baz-2.0.0.beta9",
                "baz-2.0.0.beta10",
            ],
        );
        let index = build_gem_index(&[dir.path().to_path_buf()]);
        assert!(index["foo"].ends_with("foo-1.0.0"));
        assert!(index["baz"].ends_with("baz-2.0.0.beta10"));
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
    fn gemspec_require_paths_multiline_array() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("multi.gemspec"),
            "Gem::Specification.new do |s|\n  s.require_paths = [\n    'lib',\n    'ext'\n  ]\nend\n",
        )
        .unwrap();
        assert_eq!(
            read_gemspec_require_paths(dir.path()),
            Some(vec!["lib".into(), "ext".into()])
        );
    }

    #[test]
    fn gemspec_require_paths_percent_w_array() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("percent.gemspec"),
            "Gem::Specification.new do |s|\n  s.require_paths = %w[lib ext]\nend\n",
        )
        .unwrap();
        assert_eq!(
            read_gemspec_require_paths(dir.path()),
            Some(vec!["lib".into(), "ext".into()])
        );
    }

    #[test]
    fn gemspec_require_paths_percent_w_braces() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("percent_braces.gemspec"),
            "Gem::Specification.new do |s|\n  s.require_paths = %w{lib ext}\nend\n",
        )
        .unwrap();
        assert_eq!(
            read_gemspec_require_paths(dir.path()),
            Some(vec!["lib".into(), "ext".into()])
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
    fn build_gem_index_picks_locked_version_instead_of_latest() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        std::fs::create_dir_all(gems_dir.join("versioned-1.0.0")).unwrap();
        std::fs::create_dir_all(gems_dir.join("versioned-2.0.0")).unwrap();
        std::fs::write(
            dir.path().join("Gemfile.lock"),
            "GEM\n  specs:\n    versioned (1.0.0)\n\nPLATFORMS\n  ruby\n",
        )
        .unwrap();
        let lockfile = crate::lockfile::parse_gemfile_lock(dir.path());

        let index = build_gem_index_for_lock(&[dir.path().to_path_buf()], &lockfile, Some("ruby"));
        assert_eq!(index["versioned"], gems_dir.join("versioned-1.0.0"));
    }

    #[test]
    fn build_gem_index_prefers_normalized_locked_native_then_ruby_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        std::fs::create_dir_all(gems_dir.join("native-1.0.0")).unwrap();
        std::fs::create_dir_all(gems_dir.join("native-1.0.0-arm64-darwin")).unwrap();
        std::fs::write(
            dir.path().join("Gemfile.lock"),
            "\
GEM
  specs:
    native (1.0.0)
    native (1.0.0-arm64-darwin)

PLATFORMS
  arm64-darwin-25
  ruby
",
        )
        .unwrap();
        let lockfile = crate::lockfile::parse_gemfile_lock(dir.path());

        let native = build_gem_index_for_lock(
            &[dir.path().to_path_buf()],
            &lockfile,
            Some("arm64-darwin-25"),
        );
        assert_eq!(native["native"], gems_dir.join("native-1.0.0-arm64-darwin"));

        let ruby =
            build_gem_index_for_lock(&[dir.path().to_path_buf()], &lockfile, Some("x86_64-linux"));
        assert_eq!(ruby["native"], gems_dir.join("native-1.0.0"));
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

    #[test]
    fn index_declared_path_gem_sources() {
        let dir = tempfile::tempdir().unwrap();
        let gem_dir = dir.path().join("engines").join("admin_tools");
        std::fs::create_dir_all(gem_dir.join("lib")).unwrap();
        let gems = vec![GemEntry {
            name: "admin_tools".to_string(),
            groups: vec!["default".to_string()],
            platforms: Vec::new(),
            require_paths: Vec::new(),
            module_names: Vec::new(),
            require_false: false,
            source: Some(GemSource::Path(gem_dir.clone())),
        }];
        let mut index = HashMap::new();
        index_declared_gem_sources(dir.path(), &gems, &mut index);
        assert_eq!(index.get("admin_tools"), Some(&gem_dir));
    }

    #[test]
    fn index_declared_git_gem_sources_from_local_bundle_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir
            .path()
            .join("vendor/bundle/ruby/3.4.0/bundler/gems/custom_gem-abc123");
        std::fs::create_dir_all(checkout.join("lib")).unwrap();
        let gems = vec![GemEntry {
            name: "custom_gem".to_string(),
            groups: vec!["default".to_string()],
            platforms: Vec::new(),
            require_paths: Vec::new(),
            module_names: Vec::new(),
            require_false: false,
            source: Some(GemSource::Git(
                "https://example.test/custom_gem.git".to_string(),
            )),
        }];
        let mut index = HashMap::new();
        index_declared_gem_sources(dir.path(), &gems, &mut index);
        assert_eq!(index.get("custom_gem"), Some(&checkout));
    }

    #[test]
    fn index_declared_git_gem_sources_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let checkout_root = dir.path().join("vendor/bundle/ruby/3.4.0/bundler/gems");
        let first = checkout_root.join("custom_gem-aaa");
        let second = checkout_root.join("custom_gem-zzz");
        std::fs::create_dir_all(first.join("lib")).unwrap();
        std::fs::create_dir_all(second.join("lib")).unwrap();
        let gems = vec![GemEntry {
            name: "custom_gem".to_string(),
            groups: vec!["default".to_string()],
            platforms: Vec::new(),
            require_paths: Vec::new(),
            module_names: Vec::new(),
            require_false: false,
            source: Some(GemSource::Git(
                "https://example.test/custom_gem.git".to_string(),
            )),
        }];

        let mut index = HashMap::new();
        index_declared_gem_sources(dir.path(), &gems, &mut index);
        assert_eq!(index.get("custom_gem"), Some(&first));
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

    #[test]
    fn discover_require_paths_ignores_unrelated_lib_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let gem_dir = dir.path().join("factory_bot_rails-1.0.0");
        let lib_dir = gem_dir.join("lib");
        std::fs::create_dir_all(lib_dir.join("rails")).unwrap();
        std::fs::create_dir_all(lib_dir.join("generators")).unwrap();
        std::fs::write(lib_dir.join("factory_bot_rails.rb"), "").unwrap();

        let index = HashMap::from([("factory_bot_rails".to_string(), gem_dir)]);
        let paths = discover_gem_require_paths("factory_bot_rails", &index).unwrap();
        assert_eq!(paths, vec!["factory_bot_rails"]);
    }

    #[test]
    fn discover_require_paths_preserves_public_directory_alias() {
        let dir = tempfile::tempdir().unwrap();
        let gem_dir = dir.path().join("puma-1.0.0");
        let lib_dir = gem_dir.join("lib");
        std::fs::create_dir_all(lib_dir.join("rack")).unwrap();
        std::fs::write(lib_dir.join("puma.rb"), "").unwrap();

        let index = HashMap::from([("puma".to_string(), gem_dir)]);
        let paths = discover_gem_require_paths("puma", &index).unwrap();
        assert!(paths.contains(&"puma".to_string()));
        assert!(paths.contains(&"rack".to_string()));
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
        std::fs::write(lib_dir.join("unrelated.rb"), "module Unrelated\nend\n").unwrap();
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
        std::fs::write(lib_dir.join("google_cloud.rb"), "module GoogleCloud\nend\n").unwrap();

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
        let result = find_bundler_gem_root(dir.path(), dir.path());
        assert!(result.is_some());
        assert!(result.unwrap().ends_with("3.3.0"));
    }

    #[test]
    fn bundler_gem_root_no_ruby_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = find_bundler_gem_root(dir.path(), dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn bundler_gem_root_no_gems_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let ruby_dir = dir.path().join("ruby").join("3.3.0");
        std::fs::create_dir_all(&ruby_dir).unwrap();
        // No "gems" subdir inside version dir
        let result = find_bundler_gem_root(dir.path(), dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn bundler_gem_root_prefers_active_ruby_api_then_newest() {
        let dir = tempfile::tempdir().unwrap();
        for api in ["3.3.0", "3.10.0", "3.4.0"] {
            std::fs::create_dir_all(dir.path().join("ruby").join(api).join("gems")).unwrap();
        }
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(".ruby-version"), "3.4.8\n").unwrap();
        assert!(find_bundler_gem_root(dir.path(), &project)
            .unwrap()
            .ends_with("3.4.0"));
        assert!(newest_api_dir(bundler_api_dirs(dir.path()))
            .unwrap()
            .ends_with("3.10.0"));
    }

    #[test]
    fn api_version_from_ruby_version_strings() {
        assert_eq!(api_version_from_ruby_version("3.4.8"), Some("3.4.0".into()));
        assert_eq!(
            api_version_from_ruby_version("ruby-3.3.1\n"),
            Some("3.3.0".into())
        );
        assert_eq!(api_version_from_ruby_version("jruby-9.4"), None);
    }

    // ── version selection / gem path discovery ──

    fn make_dirs(root: &Path, names: &[&str]) {
        for name in names {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
    }

    #[test]
    fn select_version_dir_prefix_is_segment_aware_and_picks_highest() {
        let dir = tempfile::tempdir().unwrap();
        make_dirs(dir.path(), &["3.3.9", "3.3.10", "3.30.0", "3.4.1"]);
        assert!(select_version_dir(dir.path(), "3.3", "")
            .unwrap()
            .ends_with("3.3.10"));
        assert!(select_version_dir(dir.path(), "3.4.1", "")
            .unwrap()
            .ends_with("3.4.1"));
        assert!(select_version_dir(dir.path(), "ruby-3.4.1", "")
            .unwrap()
            .ends_with("3.4.1"));
        assert!(select_version_dir(dir.path(), "3.5", "").is_none());
        assert!(select_version_dir(dir.path(), "latest", "")
            .unwrap()
            .ends_with("3.30.0"));
    }

    #[test]
    fn select_version_dir_with_name_prefix() {
        let dir = tempfile::tempdir().unwrap();
        make_dirs(dir.path(), &["ruby-3.3.9", "ruby-3.3.10", "jruby-9.4.0"]);
        assert!(select_version_dir(dir.path(), "3.3", "ruby-")
            .unwrap()
            .ends_with("ruby-3.3.10"));
    }

    #[test]
    fn gem_dirs_for_ruby_prefix_includes_user_gem_dir() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("rubies").join("3.4.8");
        let home = dir.path().join("home");
        std::fs::create_dir_all(prefix.join("lib/ruby/gems/3.4.0/gems")).unwrap();
        std::fs::create_dir_all(home.join(".gem/ruby/3.4.0/gems")).unwrap();
        std::fs::create_dir_all(home.join(".gem/ruby/3.3.0/gems")).unwrap();
        let dirs = gem_dirs_for_ruby_prefix(&prefix, Some(&home));
        assert_eq!(
            dirs,
            vec![
                prefix.join("lib/ruby/gems/3.4.0"),
                home.join(".gem/ruby/3.4.0")
            ]
        );
    }

    #[test]
    fn gem_paths_from_env_combines_gem_path_and_gem_home() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let home = dir.path().join("home");
        for p in [&a, &b, &home] {
            std::fs::create_dir_all(p).unwrap();
        }
        let joined = std::env::join_paths([&a, &b, &a, &dir.path().join("missing")]).unwrap();
        assert_eq!(
            gem_paths_from_env(Some(&joined), Some(&home)),
            vec![a.clone(), b.clone(), home.clone()]
        );
        let joined = std::env::join_paths([&home, &a]).unwrap();
        assert_eq!(
            gem_paths_from_env(Some(&joined), Some(&home)),
            vec![home.clone(), a.clone()]
        );
    }

    #[test]
    fn parse_gem_environment_gempath_uses_platform_separator() {
        let joined = std::env::join_paths(["/x/gems", "/y/gems", "/x/gems"]).unwrap();
        let output = format!("{}\n", joined.to_string_lossy());
        assert_eq!(
            parse_gem_environment_gempath(&output),
            vec![PathBuf::from("/x/gems"), PathBuf::from("/y/gems")]
        );
    }

    #[test]
    fn parse_mise_toml_ruby_variants() {
        assert_eq!(
            parse_mise_toml_ruby(
                "[settings]\nruby = \"x\"\n[tools]\nnode = \"25\"\nruby = \"3.4.8\" # pin\n"
            ),
            Some("3.4.8".into())
        );
        assert_eq!(
            parse_mise_toml_ruby("[tools]\nruby = [\"3.3\", \"3.4\"]\n"),
            Some("3.3".into())
        );
        assert_eq!(
            parse_mise_toml_ruby("[tools]\nruby = { version = \"3.2\" }\n"),
            Some("3.2".into())
        );
        assert_eq!(parse_mise_toml_ruby("[tools]\nnode = \"25\"\n"), None);
        assert!(mise_idiomatic_ruby_enabled(
            "[settings]\nidiomatic_version_file_enable_tools = [\"node\", \"ruby\"]\n"
        ));
        assert!(!mise_idiomatic_ruby_enabled(
            "[settings]\nidiomatic_version_file_enable_tools = [\"node\"]\n"
        ));
    }

    #[test]
    fn parse_tool_versions_ruby_line() {
        assert_eq!(
            parse_tool_versions_ruby("nodejs 20.1.0\n# ruby 2.0\nruby 3.4.8 system\n"),
            Some("3.4.8".into())
        );
        assert_eq!(parse_tool_versions_ruby("nodejs 20\n"), None);
    }

    #[test]
    fn find_mise_ruby_version_walks_up_and_respects_idiomatic_setting() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("a").join("b");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(".ruby-version"), "3.2.1\n").unwrap();
        std::fs::write(
            dir.path().join("a").join("mise.toml"),
            "[tools]\nruby = \"3.4\"\n",
        )
        .unwrap();
        assert_eq!(find_mise_ruby_version(&project, false), Some("3.4".into()));
        assert_eq!(find_mise_ruby_version(&project, true), Some("3.2.1".into()));
        std::fs::write(project.join(".tool-versions"), "ruby 3.3.0\n").unwrap();
        assert_eq!(
            find_mise_ruby_version(&project, false),
            Some("3.3.0".into())
        );
    }

    #[test]
    fn ruby_prefix_from_path_requires_install_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("installs").join("ruby");
        let prefix = root.join("3.4.8");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        std::fs::create_dir_all(prefix.join("lib/ruby/gems/3.4.0/gems")).unwrap();
        std::fs::write(prefix.join("bin").join("ruby"), "").unwrap();
        let other = dir.path().join("usr");
        std::fs::create_dir_all(other.join("bin")).unwrap();
        std::fs::create_dir_all(other.join("lib/ruby/gems/2.6.0/gems")).unwrap();
        std::fs::write(other.join("bin").join("ruby"), "").unwrap();
        let roots = vec![root.canonicalize().unwrap()];
        let found = ruby_prefix_from_path_entries(
            vec![other.join("bin"), prefix.join("bin")].into_iter(),
            &roots,
        );
        assert_eq!(found, Some(prefix.canonicalize().unwrap()));
        assert_eq!(
            ruby_prefix_from_path_entries(vec![other.join("bin")].into_iter(), &roots),
            None
        );
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
        assert_eq!(
            index["aws-sdk-s3"],
            cache_dir.join("aws-sdk-s3-1.143.0.gem")
        );
        assert!(!index.contains_key("README"));
    }

    #[test]
    fn vendor_path_index_picks_normalized_locked_native_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("vendor").join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("native-1.0.0.gem"), b"").unwrap();
        std::fs::write(cache_dir.join("native-1.0.0-arm64-darwin.gem"), b"").unwrap();
        std::fs::write(cache_dir.join("native-2.0.0.gem"), b"").unwrap();
        std::fs::write(
            dir.path().join("Gemfile.lock"),
            "\
GEM
  specs:
    native (1.0.0)
    native (1.0.0-arm64-darwin)

PLATFORMS
  arm64-darwin-25
  ruby
",
        )
        .unwrap();
        let lockfile = crate::lockfile::parse_gemfile_lock(dir.path());

        let index =
            build_vendor_cache_path_index_for_lock(dir.path(), &lockfile, Some("arm64-darwin-25"));
        assert_eq!(
            index["native"],
            cache_dir.join("native-1.0.0-arm64-darwin.gem")
        );
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
