use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use regex::Regex;

use crate::lockfile::Lockfile;
use crate::types::{GemEntry, GemSource};

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
    gem_paths: &[PathBuf],
    index: &mut HashMap<String, PathBuf>,
) {
    let mut git_context: Option<(Vec<PathBuf>, HashMap<String, LockedGitSource>)> = None;
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
            Some(GemSource::Git(url)) => {
                let (roots, locked) = git_context.get_or_insert_with(|| {
                    (
                        git_checkout_roots(project_dir, gem_paths),
                        read_locked_git_sources(project_dir),
                    )
                });
                if let Some(path) = find_git_checkout(roots, &gem.name, url, locked.get(&gem.name))
                {
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

/// A gem's GIT source from Gemfile.lock.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LockedGitSource {
    remote: String,
    revision: String,
}

/// Map gem name → GIT `remote:`/`revision:` from the project's Gemfile.lock.
fn read_locked_git_sources(project_dir: &Path) -> HashMap<String, LockedGitSource> {
    std::fs::read_to_string(project_dir.join("Gemfile.lock"))
        .map(|content| parse_locked_git_sources(&content))
        .unwrap_or_default()
}

fn parse_locked_git_sources(content: &str) -> HashMap<String, LockedGitSource> {
    let mut sources = HashMap::new();
    let mut current: Option<LockedGitSource> = None;
    for line in content.lines() {
        if !line.starts_with(' ') {
            current = (line.trim() == "GIT").then(LockedGitSource::default);
            continue;
        }
        let Some(source) = current.as_mut() else {
            continue;
        };
        if let Some(remote) = line.strip_prefix("  remote:") {
            source.remote = remote.trim().to_string();
        } else if let Some(revision) = line.strip_prefix("  revision:") {
            source.revision = revision.trim().to_string();
        } else if line.starts_with("    ") && !line.starts_with("     ") {
            if let Some(name) = line.split_whitespace().next() {
                sources.insert(name.to_string(), source.clone());
            }
        }
    }
    sources
}

/// Bundler's checkout dir base name for a git remote (`.../rails.git` → `rails`).
fn git_repo_basename(remote: &str) -> Option<String> {
    let trimmed = remote.trim().trim_end_matches('/');
    let last = trimmed.rsplit(['/', ':']).next()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    (!name.is_empty()).then(|| name.to_string())
}

/// All `bundler/gems` dirs where Bundler may have checked out git sources:
/// every gem path, BUNDLE_PATH, and project-local `.bundle` / `vendor/bundle`.
fn git_checkout_roots(project_dir: &Path, gem_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut bases: Vec<PathBuf> = gem_paths.to_vec();
    let mut bundle_dirs = vec![
        project_dir.join(".bundle"),
        project_dir.join("vendor").join("bundle"),
    ];
    if let Some(bundle_path) = read_bundle_path(project_dir) {
        bundle_dirs.push(if bundle_path.is_absolute() {
            bundle_path
        } else {
            project_dir.join(bundle_path)
        });
    }
    for bundle_dir in bundle_dirs {
        if let Ok(entries) = std::fs::read_dir(bundle_dir.join("ruby")) {
            bases.extend(entries.flatten().map(|entry| entry.path()));
        }
        bases.push(bundle_dir);
    }
    let mut roots = Vec::new();
    for base in bases {
        push_unique_dir(&mut roots, base.join("bundler").join("gems"));
    }
    roots
}

fn is_checkout_suffix(suffix: &str) -> bool {
    (7..=40).contains(&suffix.len()) && suffix.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Locate a git gem's checkout. Bundler names checkouts `<repo>-<12 hex>`
/// after the repository, not the gem, and one repo may hold several gems
/// (rails/activesupport/activesupport.gemspec), so candidates are matched by
/// repo basename (lockfile remote, Gemfile URL, gem name) with a hex suffix,
/// preferring the locked revision, then resolved to the dir holding
/// `<gem>.gemspec`. As a last resort every checkout is searched for it.
fn find_git_checkout(
    roots: &[PathBuf],
    gem_name: &str,
    url: &str,
    locked: Option<&LockedGitSource>,
) -> Option<PathBuf> {
    let mut basenames: Vec<String> = Vec::new();
    for remote in [locked.map(|l| l.remote.as_str()), Some(url)]
        .into_iter()
        .flatten()
    {
        if let Some(name) = git_repo_basename(remote) {
            if !basenames.contains(&name) {
                basenames.push(name);
            }
        }
    }
    if !basenames.iter().any(|name| name == gem_name) {
        basenames.push(gem_name.to_string());
    }
    let revision = locked.map(|l| l.revision.as_str()).unwrap_or("");

    let mut checkouts: Vec<PathBuf> = Vec::new();
    for root in roots {
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    checkouts.push(entry.path());
                }
            }
        }
    }
    checkouts.sort();

    let mut matching: Vec<(bool, &PathBuf)> = checkouts
        .iter()
        .filter_map(|path| {
            let dir_name = path.file_name()?.to_string_lossy().to_string();
            let suffix = basenames.iter().find_map(|base| {
                dir_name
                    .strip_prefix(base.as_str())
                    .and_then(|rest| rest.strip_prefix('-'))
                    .filter(|suffix| is_checkout_suffix(suffix))
            })?;
            let locked_rev = !revision.is_empty() && revision.starts_with(suffix);
            Some((locked_rev, path))
        })
        .collect();
    // Locked revision first; otherwise keep sorted (deterministic) order.
    matching.sort_by_key(|(locked_rev, _)| !locked_rev);
    for (_, checkout) in &matching {
        if let Some(dir) = gem_dir_in_checkout(checkout, gem_name) {
            return Some(dir);
        }
    }
    for (_, checkout) in &matching {
        if looks_like_gem_source(checkout) {
            return Some((*checkout).clone());
        }
    }
    checkouts
        .iter()
        .find_map(|checkout| gem_dir_in_checkout(checkout, gem_name))
}

/// Dir inside a checkout that holds `<gem>.gemspec` (root or up to two levels deep).
fn gem_dir_in_checkout(checkout: &Path, gem_name: &str) -> Option<PathBuf> {
    let spec = format!("{gem_name}.gemspec");
    if checkout.join(&spec).is_file() {
        return Some(checkout.to_path_buf());
    }
    let mut level = vec![checkout.to_path_buf()];
    for _ in 0..2 {
        let mut next = Vec::new();
        for dir in &level {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut children: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_dir()
                        && !path
                            .file_name()
                            .is_some_and(|name| name.to_string_lossy().starts_with('.'))
                })
                .collect();
            children.sort();
            for child in children {
                if child.join(&spec).is_file() {
                    return Some(child);
                }
                next.push(child);
            }
        }
        level = next;
    }
    None
}

/// Library folders that hold generators/rake tasks/assets rather than
/// requireable library code (unless named after the gem itself).
const AUXILIARY_DIRS: &[&str] = &["assets", "generators", "tasks", "templates"];

/// Upper bound on require paths enumerated inside namespace-extension folders
/// (e.g. `lib/rubocop/cop/performance/*.rb` for rubocop-performance).
const MAX_ENUMERATED_REQUIRE_PATHS: usize = 256;
const MAX_ENUMERATION_DEPTH: usize = 6;

/// Namespaces (require roots and constants) claimed by gems, used so that one
/// gem's namespace-extension folders or reopened constants don't claim usage
/// that belongs to another gem (e.g. solid_cache's `lib/active_support/` or
/// rails-html-sanitizer's `module ActionView`).
#[derive(Debug, Default)]
pub struct NamespaceClaims {
    /// Require root → Gemfile gems whose name forms / known paths claim it.
    require_roots: HashMap<String, Vec<String>>,
    /// Lowercased constant → gems (Gemfile, installed, known table) owning it.
    modules: HashMap<String, Vec<String>>,
    /// Every gem name we know about (Gemfile, installed, known table).
    gem_names: HashSet<String>,
    /// Proper name prefix ("google-cloud", "active_model") → number of gems
    /// whose names start with it.
    family_prefixes: HashMap<String, usize>,
    gemfile_gems: HashSet<String>,
    /// Direct lockfile dependencies per gem.
    dependencies: HashMap<String, HashSet<String>>,
}

/// Constant names a gem is conventionally expected to own: its PascalCase
/// name, its hyphen namespace (`aws-sdk-s3` → `Aws::Sdk::S3`) and curated names.
pub fn own_module_forms(gem_name: &str) -> Vec<String> {
    let mut forms = vec![gem_name
        .split(['-', '_'])
        .map(capitalize_ascii)
        .collect::<String>()];
    if gem_name.contains('-') {
        forms.push(
            gem_name
                .split('-')
                .map(|part| part.split('_').map(capitalize_ascii).collect::<String>())
                .collect::<Vec<_>>()
                .join("::"),
        );
    }
    if let Some(known) = crate::known::module_names_for(gem_name) {
        forms.extend(known.iter().map(|name| name.to_string()));
    }
    forms.retain(|form| !form.is_empty());
    forms
}

fn capitalize_ascii(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Require roots a Gemfile gem claims: its name, underscored and slashed
/// forms, and curated require paths.
fn own_require_roots(gem_name: &str) -> Vec<String> {
    let mut roots = vec![
        gem_name.to_string(),
        gem_name.replace('-', "_"),
        gem_name.replace('-', "/"),
    ];
    if let Some(known) = crate::known::require_paths_for(gem_name) {
        roots.extend(known.iter().map(|path| path.to_string()));
    }
    roots.sort();
    roots.dedup();
    roots
}

impl NamespaceClaims {
    /// Build claims from the Gemfile's gems plus every installed gem name.
    /// Constants are also claimed for every gem in the curated known table.
    pub fn build<'a>(
        gemfile_gems: &'a [String],
        installed_gems: impl IntoIterator<Item = &'a String>,
    ) -> Self {
        let mut claims = NamespaceClaims {
            gemfile_gems: gemfile_gems.iter().cloned().collect(),
            ..NamespaceClaims::default()
        };
        for gem in gemfile_gems {
            for root in own_require_roots(gem) {
                push_owner(&mut claims.require_roots, root, gem);
            }
        }
        let mut all: HashSet<&str> = gemfile_gems.iter().map(String::as_str).collect();
        all.extend(installed_gems.into_iter().map(String::as_str));
        all.extend(crate::known::MODULE_NAMES.iter().map(|(gem, _)| *gem));
        all.extend(crate::known::REQUIRE_PATHS.iter().map(|(gem, _)| *gem));
        for gem in all {
            for form in own_module_forms(gem) {
                push_owner(&mut claims.modules, form.to_lowercase(), gem);
            }
            for prefix in proper_name_prefixes(gem) {
                *claims.family_prefixes.entry(prefix).or_default() += 1;
            }
            claims.gem_names.insert(gem.to_string());
        }
        claims
    }

    /// Record Gemfile.lock direct dependencies. A namespace owned by a gem's
    /// own dependency that isn't declared in the Gemfile (factory_bot for
    /// factory_bot_rails) is reachable only through that gem, so it still
    /// counts as usage of it.
    pub fn with_lock_dependencies(mut self, dependencies: &HashMap<String, Vec<String>>) -> Self {
        self.dependencies = dependencies
            .iter()
            .map(|(gem, deps)| (gem.clone(), deps.iter().cloned().collect()))
            .collect();
        self
    }

    /// Whether `owner` should take credit for its namespace instead of `gem`.
    fn is_foreign_owner(&self, owner: &str, gem: &str) -> bool {
        owner != gem
            && (self.gemfile_gems.contains(owner)
                || !self
                    .dependencies
                    .get(gem)
                    .is_some_and(|deps| deps.contains(owner)))
    }

    /// True when `root` is claimed by a Gemfile gem other than `gem`.
    pub fn require_root_owned_by_other(&self, root: &str, gem: &str) -> bool {
        self.require_roots
            .get(root)
            .is_some_and(|owners| owners.iter().any(|owner| owner != gem))
    }

    /// True when the most specific claimed prefix of `name` (or `name`
    /// itself) belongs to gems other than `gem`.
    pub fn module_owned_by_other(&self, name: &str, gem: &str) -> bool {
        let lower = name.to_lowercase();
        let mut candidate = lower.as_str();
        loop {
            if let Some(owners) = self.modules.get(candidate) {
                return !owners.iter().any(|owner| owner == gem)
                    && owners.iter().any(|owner| self.is_foreign_owner(owner, gem));
            }
            match candidate.rfind("::") {
                Some(idx) => candidate = &candidate[..idx],
                None => return false,
            }
        }
    }

    /// True when a proper name prefix of `gem` (e.g. `google-cloud` for
    /// `google-cloud-storage`, `active_model` for `active_model_serializers`)
    /// is itself a gem name or is shared with another gem (a gem family).
    pub fn is_shared_name_prefix(&self, prefix: &str, gem: &str) -> bool {
        if prefix != gem && self.gem_names.contains(prefix) {
            return self.is_foreign_owner(prefix, gem);
        }
        let own = usize::from(
            self.gem_names.contains(gem) && proper_name_prefixes(gem).iter().any(|p| p == prefix),
        );
        self.family_prefixes
            .get(prefix)
            .is_some_and(|count| count.saturating_sub(own) > 0)
    }
}

fn push_owner(map: &mut HashMap<String, Vec<String>>, key: String, gem: &str) {
    let owners = map.entry(key).or_default();
    if !owners.iter().any(|owner| owner == gem) {
        owners.push(gem.to_string());
    }
}

/// Proper prefixes of a gem name split on `-` and `_` (`a-b_c` → `a`, `a-b`).
fn proper_name_prefixes(gem: &str) -> Vec<String> {
    gem.match_indices(['-', '_'])
        .map(|(idx, _)| gem[..idx].to_string())
        .filter(|prefix| !prefix.is_empty())
        .collect()
}

/// Discover require paths by inspecting an installed gem's require dirs.
/// Custom `require_paths` come from `<base>/specifications/<full_name>.gemspec`
/// (installed gems) or a `*.gemspec` in the gem dir (git/path gems).
pub fn discover_gem_require_paths(
    gem_name: &str,
    gem_index: &HashMap<String, PathBuf>,
    claims: &NamespaceClaims,
) -> Option<Vec<String>> {
    let gem_dir = gem_index.get(gem_name)?;
    let require_dirs =
        read_gemspec_require_paths(gem_dir).unwrap_or_else(|| vec!["lib".to_string()]);
    let list = |rel: &str| -> Vec<(String, bool)> {
        let mut entries = Vec::new();
        if let Ok(read_dir) = std::fs::read_dir(gem_dir.join(rel)) {
            for entry in read_dir.flatten() {
                let is_dir = entry.file_type().is_ok_and(|t| t.is_dir())
                    || (entry.file_type().is_ok_and(|t| t.is_symlink()) && entry.path().is_dir());
                entries.push((entry.file_name().to_string_lossy().to_string(), is_dir));
            }
        }
        entries
    };
    let paths = require_names_from_listing(gem_name, &require_dirs, &list, claims);
    (!paths.is_empty()).then_some(paths)
}

/// Turn a gem's require-dir listing into requireable names.
///
/// - Top-level `*.rb` files are requireable as-is (unless the name is another
///   Gemfile gem's require root).
/// - Folders named after the gem (`name`, `name_underscored`, or the
///   hyphen→slash form such as `rubocop/performance`) are the gem's own roots.
/// - Other folders are roots only when the gem also ships the matching
///   `<folder>.rb` entry point and no other Gemfile gem claims that root.
///   Otherwise the folder is a namespace extension (e.g. solid_cache's
///   `active_support/`, tailwindcss-rails' `puma/`), so only the files it
///   actually contains are listed (bounded), never the bare folder name.
fn require_names_from_listing(
    gem_name: &str,
    require_dirs: &[String],
    list: &dyn Fn(&str) -> Vec<(String, bool)>,
    claims: &NamespaceClaims,
) -> Vec<String> {
    let underscored = gem_name.replace('-', "_");
    let slashed = gem_name.replace('-', "/");
    let is_own = |name: &str| name == gem_name || name == underscored || name == slashed;

    let mut paths = Vec::new();
    let mut extension_dirs: Vec<(String, String)> = Vec::new();
    for req_dir in require_dirs {
        let req_dir = req_dir.trim_end_matches('/');
        let entries = list(req_dir);
        let file_stems: HashSet<String> = entries
            .iter()
            .filter(|(_, is_dir)| !is_dir)
            .filter_map(|(name, _)| name.strip_suffix(".rb").map(str::to_string))
            .collect();
        for stem in &file_stems {
            if is_own(stem) || !claims.require_root_owned_by_other(stem, gem_name) {
                paths.push(stem.clone());
            }
        }
        for (dir, is_dir) in &entries {
            if !is_dir {
                continue;
            }
            if is_own(dir) {
                paths.push(dir.clone());
            } else if AUXILIARY_DIRS.contains(&dir.as_str()) {
                continue;
            } else if file_stems.contains(dir) && !claims.require_root_owned_by_other(dir, gem_name)
            {
                paths.push(dir.clone());
            } else {
                extension_dirs.push((req_dir.to_string(), dir.clone()));
            }
        }
    }
    enumerate_extension_require_paths(list, extension_dirs, &is_own, &mut paths);
    paths.sort();
    paths.dedup();
    paths
}

/// Breadth-first listing of the `.rb` files inside namespace-extension
/// folders as full require paths (`omniauth/strategies/google_oauth2`), so
/// shallow entry points are kept before the budget runs out. Folders named
/// after the gem (e.g. `rubocop/performance`) are kept as roots, not descended.
fn enumerate_extension_require_paths(
    list: &dyn Fn(&str) -> Vec<(String, bool)>,
    roots: Vec<(String, String)>,
    is_own: &dyn Fn(&str) -> bool,
    out: &mut Vec<String>,
) {
    let mut budget = MAX_ENUMERATED_REQUIRE_PATHS;
    let mut queue: std::collections::VecDeque<(String, String, usize)> = roots
        .into_iter()
        .map(|(req_dir, rel)| (req_dir, rel, 1))
        .collect();
    while let Some((req_dir, rel, depth)) = queue.pop_front() {
        let mut entries = list(&format!("{req_dir}/{rel}"));
        entries.sort();
        for (name, is_dir) in entries {
            if budget == 0 {
                return;
            }
            if is_dir {
                let child = format!("{rel}/{name}");
                if is_own(&child) {
                    out.push(child);
                    budget -= 1;
                } else if depth < MAX_ENUMERATION_DEPTH {
                    queue.push_back((req_dir.clone(), child, depth + 1));
                }
            } else if let Some(stem) = name.strip_suffix(".rb") {
                out.push(format!("{rel}/{stem}"));
                budget -= 1;
            }
        }
    }
}

/// Read require_paths for a gem dir. Installed gems keep their spec at
/// `<base>/specifications/<full_name>.gemspec`; git/path checkouts ship a
/// `*.gemspec` in the gem dir itself.
fn read_gemspec_require_paths(gem_dir: &Path) -> Option<Vec<String>> {
    if let (Some(full_name), Some(base)) =
        (gem_dir.file_name(), gem_dir.parent().and_then(Path::parent))
    {
        let spec = base
            .join("specifications")
            .join(format!("{}.gemspec", full_name.to_string_lossy()));
        if let Ok(content) = std::fs::read_to_string(&spec) {
            return parse_gemspec_require_paths(&content);
        }
    }
    let entries = std::fs::read_dir(gem_dir).ok()?;
    let mut specs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("gemspec"))
        .collect();
    specs.sort();
    specs.into_iter().find_map(|path| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|content| parse_gemspec_require_paths(&content))
    })
}

/// Statically extract `require_paths` from gemspec Ruby source
/// (`s.require_paths = ["lib".freeze]`, `%w[lib ext]`, `s.require_path = "lib"`).
fn parse_gemspec_require_paths(content: &str) -> Option<Vec<String>> {
    static REQ_PATHS_ARRAY_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?s)require_paths?\s*=\s*\[([^\]]+)\]"#).unwrap());
    static REQ_PATHS_PERCENT_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?s)require_paths?\s*=\s*%[wW][\[\(\{]([^\]\)\}]+)[\]\)\}]"#).unwrap()
    });
    static REQ_PATH_SINGLE_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"require_path\s*=\s*['"]([^'"]+)['"]"#).unwrap());
    if let Some(cap) = REQ_PATHS_ARRAY_RE.captures(content) {
        let paths = extract_quoted_strings(&cap[1]);
        if !paths.is_empty() {
            return Some(paths);
        }
    }
    if let Some(cap) = REQ_PATHS_PERCENT_RE.captures(content) {
        let paths: Vec<String> = cap[1].split_whitespace().map(str::to_string).collect();
        if !paths.is_empty() {
            return Some(paths);
        }
    }
    REQ_PATH_SINGLE_RE
        .captures(content)
        .map(|cap| vec![cap[1].to_string()])
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
/// Only top-level declarations count (parsed with ruby-prism, so nested or
/// reopened classes such as hashie's `Hashie::Array` are ignored). Ruby
/// core/stdlib constants (`Array`, `Integer`, `TCPSocket`, ...), generic helper
/// names (see `GENERIC_MODULE_BLACKLIST`) and constants owned by another gem
/// (e.g. `ActionView` reopened by rails-html-sanitizer) are filtered. The
/// caller (main.rs) UNIONs this with the curated `KNOWN_MODULE_NAMES` table,
/// so canonical names for popular gems are still preserved.
pub fn discover_gem_module_names(
    gem_name: &str,
    gem_index: &HashMap<String, PathBuf>,
    claims: &NamespaceClaims,
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
        scan_rb_file_for_top_level_modules(&path, &mut modules);
    }

    let mut result: Vec<String> = modules
        .into_iter()
        .filter(|n| {
            !is_generic_module_name(n)
                && !is_ruby_core_constant(n)
                && !claims.module_owned_by_other(n, gem_name)
        })
        .collect();
    result.sort();
    result
}

/// Ruby core and standard-library constants that gems commonly reopen or
/// monkey-patch; they never identify a specific gem.
const RUBY_CORE_CONSTANTS: &[&str] = &[
    "Addrinfo",
    "ArgumentError",
    "Array",
    "BasicObject",
    "BasicSocket",
    "Benchmark",
    "Binding",
    "Class",
    "Comparable",
    "Complex",
    "ConditionVariable",
    "Data",
    "Date",
    "DateTime",
    "Dir",
    "Encoding",
    "Enumerable",
    "Enumerator",
    "Errno",
    "Exception",
    "FalseClass",
    "Fiber",
    "File",
    "FileTest",
    "FileUtils",
    "Float",
    "Forwardable",
    "GC",
    "Gem",
    "Hash",
    "IO",
    "IPAddr",
    "IPSocket",
    "Integer",
    "Kernel",
    "Marshal",
    "MatchData",
    "Math",
    "Method",
    "Module",
    "Monitor",
    "Mutex",
    "Net",
    "Net::HTTP",
    "NilClass",
    "NoMethodError",
    "Numeric",
    "Object",
    "ObjectSpace",
    "OpenSSL",
    "OpenStruct",
    "Pathname",
    "Proc",
    "Process",
    "Queue",
    "Ractor",
    "Random",
    "Range",
    "Rational",
    "Regexp",
    "RubyVM",
    "RuntimeError",
    "SOCKSSocket",
    "Set",
    "Signal",
    "Singleton",
    "Socket",
    "StandardError",
    "String",
    "StringIO",
    "Struct",
    "Symbol",
    "TCPServer",
    "TCPSocket",
    "Thread",
    "Time",
    "Timeout",
    "TracePoint",
    "TrueClass",
    "UDPSocket",
    "UNIXServer",
    "UNIXSocket",
    "URI",
    "UnboundMethod",
    "Warning",
];

fn is_ruby_core_constant(name: &str) -> bool {
    RUBY_CORE_CONSTANTS.contains(&name.trim_start_matches("::"))
}

/// Largest entry file we parse for top-level declarations.
const MAX_ENTRY_FILE_BYTES: u64 = 256 * 1024;

/// Parse a Ruby file with ruby-prism and collect its top-level `module` /
/// `class` declarations (also inside top-level `if`/`unless`/`begin` blocks).
fn scan_rb_file_for_top_level_modules(path: &Path, modules: &mut HashSet<String>) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let Ok(source) = read_bounded(file, MAX_ENTRY_FILE_BYTES).or_else(|too_big| {
        if too_big {
            let mut head = Vec::new();
            std::fs::File::open(path)
                .and_then(|f| f.take(MAX_ENTRY_FILE_BYTES).read_to_end(&mut head))
                .map(|_| head)
                .map_err(|_| false)
        } else {
            Err(false)
        }
    }) else {
        return;
    };
    modules.extend(top_level_constant_decls(&source));
}

fn top_level_constant_decls(source: &[u8]) -> Vec<String> {
    let result = ruby_prism::parse(source);
    let mut out = Vec::new();
    if let Some(program) = result.node().as_program_node() {
        for node in program.statements().body().iter() {
            collect_top_level_decl(&node, 0, &mut out);
        }
    }
    out
}

fn collect_top_level_decl(node: &ruby_prism::Node<'_>, depth: usize, out: &mut Vec<String>) {
    let constant_path = if let Some(module) = node.as_module_node() {
        Some(module.constant_path())
    } else {
        node.as_class_node().map(|class| class.constant_path())
    };
    if let Some(constant_path) = constant_path {
        let name = String::from_utf8_lossy(constant_path.location().as_slice())
            .trim()
            .trim_start_matches("::")
            .to_string();
        if name.starts_with(|c: char| c.is_ascii_uppercase())
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
        {
            out.push(name);
        }
        return;
    }
    if depth >= 3 {
        return;
    }
    let mut visit_statements = |statements: Option<ruby_prism::StatementsNode<'_>>| {
        if let Some(statements) = statements {
            for child in statements.body().iter() {
                collect_top_level_decl(&child, depth + 1, out);
            }
        }
    };
    if let Some(if_node) = node.as_if_node() {
        visit_statements(if_node.statements());
        if let Some(subsequent) = if_node.subsequent() {
            collect_top_level_decl(&subsequent, depth + 1, out);
        }
    } else if let Some(unless_node) = node.as_unless_node() {
        visit_statements(unless_node.statements());
        if let Some(else_node) = unless_node.else_clause() {
            visit_statements(else_node.statements());
        }
    } else if let Some(begin_node) = node.as_begin_node() {
        visit_statements(begin_node.statements());
    } else if let Some(else_node) = node.as_else_node() {
        visit_statements(else_node.statements());
    }
}

/// Limits for reading `.gem` tarballs from vendor/cache (untrusted input).
const MAX_GEM_METADATA_BYTES: u64 = 8 * 1024 * 1024;
const MAX_GEM_DATA_BYTES: u64 = 256 * 1024 * 1024;
const MAX_GEM_DATA_ENTRIES: usize = 50_000;

/// Read at most `limit` bytes; `Err(true)` when the input exceeds the limit,
/// `Err(false)` on I/O errors.
fn read_bounded(reader: impl Read, limit: u64) -> Result<Vec<u8>, bool> {
    let mut buf = Vec::new();
    reader
        .take(limit + 1)
        .read_to_end(&mut buf)
        .map_err(|_| false)?;
    if buf.len() as u64 > limit {
        Err(true)
    } else {
        Ok(buf)
    }
}

/// Decompress and parse a `.gem`'s `metadata.gz` entry with size limits.
fn read_metadata_entry(entry: impl Read, gem_path: &Path) -> Option<GemMetadata> {
    let warn_limit = || {
        eprintln!(
            "Warning: skipping {}: metadata.gz exceeds {} MiB",
            gem_path.display(),
            MAX_GEM_METADATA_BYTES / (1024 * 1024)
        );
    };
    let compressed = match read_bounded(entry, MAX_GEM_METADATA_BYTES) {
        Ok(bytes) => bytes,
        Err(true) => {
            warn_limit();
            return None;
        }
        Err(false) => return None,
    };
    let yaml = match read_bounded(
        flate2::read::GzDecoder::new(&compressed[..]),
        MAX_GEM_METADATA_BYTES,
    ) {
        Ok(bytes) => bytes,
        Err(true) => {
            warn_limit();
            return None;
        }
        Err(false) => return None,
    };
    parse_gem_metadata_yaml(&String::from_utf8_lossy(&yaml))
}

/// List file paths inside a `.gem`'s `data.tar.gz`, bounded in decompressed
/// bytes and entry count. A truncated listing is returned on overflow.
fn list_data_tar_entry(entry: impl Read, gem_path: &Path) -> Vec<String> {
    let decoder = flate2::read::GzDecoder::new(entry).take(MAX_GEM_DATA_BYTES);
    let mut archive = tar::Archive::new(decoder);
    let mut files = Vec::new();
    let Ok(entries) = archive.entries() else {
        return files;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            eprintln!(
                "Warning: {}: data.tar.gz unreadable or larger than {} MiB; listing truncated",
                gem_path.display(),
                MAX_GEM_DATA_BYTES / (1024 * 1024)
            );
            break;
        };
        if files.len() >= MAX_GEM_DATA_ENTRIES {
            eprintln!(
                "Warning: {}: more than {} files in data.tar.gz; listing truncated",
                gem_path.display(),
                MAX_GEM_DATA_ENTRIES
            );
            break;
        }
        if entry.header().entry_type().is_file() {
            if let Ok(path) = entry.path() {
                files.push(path.to_string_lossy().trim_start_matches("./").to_string());
            }
        }
    }
    files
}

/// Extract gemspec metadata from a .gem tarball (tar archive containing metadata.gz).
fn extract_gem_metadata(gem_path: &Path) -> Option<GemMetadata> {
    let file = std::fs::File::open(gem_path).ok()?;
    let mut archive = tar::Archive::new(file);
    for entry in archive.entries().ok()? {
        let entry = entry.ok()?;
        if entry.path().ok()?.to_string_lossy() == "metadata.gz" {
            return read_metadata_entry(entry, gem_path);
        }
    }
    None
}

/// Require names for a vendor/cache `.gem` that isn't installed: its
/// `require_paths` (metadata.gz) resolved against the real file listing in
/// data.tar.gz, using the same ownership rules as installed gems. Load-path
/// dirs such as `lib`/`ext` are never returned as require names themselves.
pub fn vendor_gem_require_paths(
    gem_path: &Path,
    gem_name: &str,
    claims: &NamespaceClaims,
) -> Option<Vec<String>> {
    let file = std::fs::File::open(gem_path).ok()?;
    let mut archive = tar::Archive::new(file);
    let mut metadata = None;
    let mut files = None;
    for entry in archive.entries().ok()? {
        let Ok(entry) = entry else { break };
        let path = entry.path().ok()?.to_string_lossy().to_string();
        match path.as_str() {
            "metadata.gz" => metadata = read_metadata_entry(entry, gem_path),
            "data.tar.gz" => files = Some(list_data_tar_entry(entry, gem_path)),
            _ => {}
        }
        if metadata.is_some() && files.is_some() {
            break;
        }
    }
    let files = files?;
    let require_dirs = metadata
        .map(|meta| meta.require_paths)
        .unwrap_or_else(|| vec!["lib".to_string()]);

    let mut listing: HashMap<String, HashSet<(String, bool)>> = HashMap::new();
    for file in &files {
        let mut parent = String::new();
        let mut parts = file.split('/').filter(|part| !part.is_empty()).peekable();
        while let Some(part) = parts.next() {
            let is_dir = parts.peek().is_some();
            listing
                .entry(parent.clone())
                .or_default()
                .insert((part.to_string(), is_dir));
            if !parent.is_empty() {
                parent.push('/');
            }
            parent.push_str(part);
        }
    }
    let list = |rel: &str| -> Vec<(String, bool)> {
        listing
            .get(rel.trim_matches('/'))
            .map(|entries| entries.iter().cloned().collect())
            .unwrap_or_default()
    };
    let paths = require_names_from_listing(gem_name, &require_dirs, &list, claims);
    (!paths.is_empty()).then_some(paths)
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
            declarations: Vec::new(),
        }];
        let mut index = HashMap::new();
        index_declared_gem_sources(dir.path(), &gems, &[], &mut index);
        assert_eq!(index.get("admin_tools"), Some(&gem_dir));
    }

    #[test]
    fn index_declared_git_gem_sources_from_local_bundle_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir
            .path()
            .join("vendor/bundle/ruby/3.4.0/bundler/gems/custom_gem-abc123def456");
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
            declarations: Vec::new(),
        }];
        let mut index = HashMap::new();
        index_declared_gem_sources(dir.path(), &gems, &[], &mut index);
        assert_eq!(index.get("custom_gem"), Some(&checkout));
    }

    #[test]
    fn index_declared_git_gem_sources_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let checkout_root = dir.path().join("vendor/bundle/ruby/3.4.0/bundler/gems");
        let first = checkout_root.join("custom_gem-aaaaaaaaaaaa");
        let second = checkout_root.join("custom_gem-ffffffffffff");
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
            declarations: Vec::new(),
        }];

        let mut index = HashMap::new();
        index_declared_gem_sources(dir.path(), &gems, &[], &mut index);
        assert_eq!(index.get("custom_gem"), Some(&first));
    }

    fn git_gem(name: &str, url: &str) -> GemEntry {
        GemEntry {
            name: name.to_string(),
            groups: vec!["default".to_string()],
            platforms: Vec::new(),
            require_paths: Vec::new(),
            module_names: Vec::new(),
            require_false: false,
            source: Some(GemSource::Git(url.to_string())),
        }
    }

    #[test]
    fn index_git_gem_requires_hex_revision_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("vendor/bundle/ruby/3.4.0/bundler/gems");
        std::fs::create_dir_all(root.join("foo-bar-0a1b2c3d4e5f/lib")).unwrap();
        std::fs::create_dir_all(root.join("foo-c0ffee123456/lib")).unwrap();
        let gems = vec![
            git_gem("foo", "https://example.com/foo.git"),
            git_gem("foo-bar", "https://example.com/foo-bar.git"),
        ];
        let mut index = HashMap::new();
        index_declared_gem_sources(dir.path(), &gems, &[], &mut index);
        assert_eq!(index["foo"], root.join("foo-c0ffee123456"));
        assert_eq!(index["foo-bar"], root.join("foo-bar-0a1b2c3d4e5f"));
    }

    #[test]
    fn index_git_gem_from_gem_path_and_monorepo_gemspec() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("app");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("Gemfile.lock"),
            "GIT\n  remote: https://github.com/rails/rails.git\n  revision: 1234567890abcdef1234567890abcdef12345678\n  specs:\n    activesupport (8.1.0.alpha)\n      tzinfo (~> 2.0)\n\nGEM\n  remote: https://rubygems.org/\n  specs:\n    tzinfo (2.0.6)\n",
        )
        .unwrap();
        let gem_home = dir.path().join("gem_home");
        let checkouts = gem_home.join("bundler/gems");
        let old = checkouts.join("rails-aaaaaaaaaaaa/activesupport");
        let locked = checkouts.join("rails-1234567890ab/activesupport");
        for path in [&old, &locked] {
            std::fs::create_dir_all(path.join("lib")).unwrap();
            std::fs::write(path.join("activesupport.gemspec"), "").unwrap();
        }
        let gems = vec![git_gem("activesupport", "https://github.com/rails/rails")];
        let mut index = HashMap::new();
        index_declared_gem_sources(&project, &gems, &[gem_home], &mut index);
        assert_eq!(index["activesupport"], locked);
    }

    #[test]
    fn index_git_gem_found_by_gemspec_when_repo_name_differs() {
        let dir = tempfile::tempdir().unwrap();
        let gem_home = dir.path().join("gem_home");
        let checkout = gem_home.join("bundler/gems/ruby-client-abcdefabcdef");
        std::fs::create_dir_all(checkout.join("lib")).unwrap();
        std::fs::write(checkout.join("acme.gemspec"), "").unwrap();
        let gems = vec![git_gem("acme", "git@example.com:org/acme-sdk.git")];
        let mut index = HashMap::new();
        index_declared_gem_sources(dir.path(), &gems, &[gem_home], &mut index);
        assert_eq!(index["acme"], checkout);
    }

    #[test]
    fn parse_locked_git_sources_and_basename() {
        let sources = parse_locked_git_sources(
            "GIT\n  remote: https://github.com/a/b.git\n  revision: abc\n  specs:\n    one (1.0)\n      dep (>= 0)\n    two (1.0)\n\nPATH\n  remote: .\n  specs:\n    three (1.0)\n",
        );
        assert_eq!(sources["one"].revision, "abc");
        assert_eq!(sources["two"].remote, "https://github.com/a/b.git");
        assert!(!sources.contains_key("dep"));
        assert!(!sources.contains_key("three"));
        assert_eq!(
            git_repo_basename("git@github.com:org/repo.git/"),
            Some("repo".into())
        );
        assert_eq!(git_repo_basename("https://x/y/z"), Some("z".into()));
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
        let paths = discover_gem_require_paths("mygem", &index, &NamespaceClaims::default());
        assert!(paths.is_some());
        let paths = paths.unwrap();
        assert!(paths.contains(&"mygem".to_string()));
        assert!(paths.contains(&"helpers".to_string()));
    }

    #[test]
    fn discover_require_paths_missing_gem() {
        let index = HashMap::new();
        assert!(
            discover_gem_require_paths("nonexistent", &index, &NamespaceClaims::default())
                .is_none()
        );
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
        let paths =
            discover_gem_require_paths("factory_bot_rails", &index, &NamespaceClaims::default())
                .unwrap();
        assert_eq!(paths, vec!["factory_bot_rails"]);
    }

    #[test]
    fn discover_require_paths_lists_files_in_extension_dirs() {
        // puma's lib/rack/ extends rack's namespace: only the files it ships
        // are require paths, never the bare `rack` root.
        let dir = tempfile::tempdir().unwrap();
        let gem_dir = dir.path().join("puma-1.0.0");
        let lib_dir = gem_dir.join("lib");
        std::fs::create_dir_all(lib_dir.join("rack/handler")).unwrap();
        std::fs::write(lib_dir.join("rack/handler/puma.rb"), "").unwrap();
        std::fs::write(lib_dir.join("puma.rb"), "").unwrap();

        let index = HashMap::from([("puma".to_string(), gem_dir)]);
        let paths =
            discover_gem_require_paths("puma", &index, &NamespaceClaims::default()).unwrap();
        assert!(paths.contains(&"puma".to_string()));
        assert!(paths.contains(&"rack/handler/puma".to_string()));
        assert!(!paths.contains(&"rack".to_string()));
    }

    fn write_files(root: &Path, files: &[&str]) {
        for file in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
    }

    #[test]
    fn discover_require_paths_namespace_extensions_do_not_claim_other_roots() {
        let dir = tempfile::tempdir().unwrap();
        let gems = dir.path().join("gems");
        write_files(
            &gems.join("solid_cache-1.0.0/lib"),
            &[
                "solid_cache.rb",
                "solid_cache/store.rb",
                "active_support/cache/solid_cache_store.rb",
                "generators/solid_cache/install/install_generator.rb",
            ],
        );
        write_files(
            &gems.join("rubocop-performance-1.0.0/lib"),
            &[
                "rubocop-performance.rb",
                "rubocop/performance.rb",
                "rubocop/performance/version.rb",
                "rubocop/cop/performance/caller.rb",
            ],
        );
        write_files(
            &gems.join("minitest-5.0.0/lib"),
            &["minitest.rb", "minitest/autorun.rb", "hoe/minitest.rb"],
        );
        write_files(
            &gems.join("activesupport-8.0.0/lib"),
            &["active_support.rb", "active_support/all.rb"],
        );
        let index = build_gem_index(&[dir.path().to_path_buf()]);
        let gemfile: Vec<String> = [
            "solid_cache",
            "rubocop-performance",
            "minitest",
            "activesupport",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let claims = NamespaceClaims::build(&gemfile, index.keys());

        let paths = discover_gem_require_paths("solid_cache", &index, &claims).unwrap();
        assert_eq!(
            paths,
            vec!["active_support/cache/solid_cache_store", "solid_cache"]
        );

        let paths = discover_gem_require_paths("rubocop-performance", &index, &claims).unwrap();
        assert!(paths.contains(&"rubocop-performance".to_string()));
        assert!(paths.contains(&"rubocop/performance".to_string()));
        assert!(paths.contains(&"rubocop/cop/performance/caller".to_string()));
        assert!(!paths.contains(&"rubocop".to_string()));

        let paths = discover_gem_require_paths("minitest", &index, &claims).unwrap();
        assert_eq!(paths, vec!["hoe/minitest", "minitest"]);

        // A folder with its own `<folder>.rb` entry point is the gem's root.
        let paths = discover_gem_require_paths("activesupport", &index, &claims).unwrap();
        assert_eq!(paths, vec!["active_support"]);
    }

    #[test]
    fn discover_require_paths_owned_root_yields_to_other_gemfile_gem() {
        let dir = tempfile::tempdir().unwrap();
        write_files(
            &dir.path().join("gems/shim-1.0.0/lib"),
            &["shim.rb", "puma.rb", "puma/plugin/shim.rb"],
        );
        let index = build_gem_index(&[dir.path().to_path_buf()]);
        let lone = NamespaceClaims::build(&["shim".to_string()], index.keys());
        let paths = discover_gem_require_paths("shim", &index, &lone).unwrap();
        assert!(paths.contains(&"puma".to_string()));

        let both = NamespaceClaims::build(&["shim".to_string(), "puma".to_string()], index.keys());
        let paths = discover_gem_require_paths("shim", &index, &both).unwrap();
        assert_eq!(paths, vec!["puma/plugin/shim", "shim"]);
    }

    #[test]
    fn discover_require_paths_reads_installed_specification() {
        let dir = tempfile::tempdir().unwrap();
        write_files(
            &dir.path()
                .join("gems/concurrent-ruby-1.3.6/lib/concurrent-ruby"),
            &["concurrent.rb", "concurrent/map.rb"],
        );
        std::fs::create_dir_all(dir.path().join("specifications")).unwrap();
        std::fs::write(
            dir.path().join("specifications/concurrent-ruby-1.3.6.gemspec"),
            "Gem::Specification.new do |s|\n  s.name = \"concurrent-ruby\".freeze\n  s.require_paths = [\"lib/concurrent-ruby\".freeze]\nend\n",
        )
        .unwrap();
        let index = build_gem_index(&[dir.path().to_path_buf()]);
        let paths =
            discover_gem_require_paths("concurrent-ruby", &index, &NamespaceClaims::default())
                .unwrap();
        assert_eq!(paths, vec!["concurrent"]);
    }

    #[test]
    fn namespace_claims_longest_prefix_wins() {
        let claims = NamespaceClaims::build(
            &["rack".to_string(), "rack-attack".to_string()],
            std::iter::empty(),
        );
        assert!(claims.module_owned_by_other("Rack", "rack-attack"));
        assert!(claims.module_owned_by_other("Rack::Request", "rack-attack"));
        assert!(!claims.module_owned_by_other("Rack::Attack", "rack-attack"));
        assert!(!claims.module_owned_by_other("Rack", "rack"));
        assert!(!claims.module_owned_by_other("Unclaimed", "rack"));
        // Known table claims apply even when the owner isn't in the Gemfile.
        assert!(claims.module_owned_by_other("ActionView::Helpers", "rack"));
        assert!(claims.is_shared_name_prefix("google-cloud", "google-cloud-storage"));
        assert!(claims.is_shared_name_prefix("rack", "rack-attack"));
        assert!(!claims.is_shared_name_prefix("acme-widget", "acme-widget-tools"));
    }

    #[test]
    fn namespace_claims_credit_undeclared_dependencies() {
        let gemfile = vec!["factory_bot_rails".to_string()];
        let deps = HashMap::from([(
            "factory_bot_rails".to_string(),
            vec!["factory_bot".to_string(), "railties".to_string()],
        )]);
        let claims =
            NamespaceClaims::build(&gemfile, std::iter::empty()).with_lock_dependencies(&deps);
        assert!(!claims.module_owned_by_other("FactoryBot", "factory_bot_rails"));
        assert!(!claims.is_shared_name_prefix("factory_bot", "factory_bot_rails"));
        assert!(claims.module_owned_by_other("ActionView", "factory_bot_rails"));

        // Once factory_bot is declared in the Gemfile it takes the credit.
        let gemfile = vec!["factory_bot_rails".to_string(), "factory_bot".to_string()];
        let claims =
            NamespaceClaims::build(&gemfile, std::iter::empty()).with_lock_dependencies(&deps);
        assert!(claims.module_owned_by_other("FactoryBot", "factory_bot_rails"));
        assert!(claims.is_shared_name_prefix("factory_bot", "factory_bot_rails"));
    }

    // ── discover_gem_module_names ──

    #[test]
    fn discover_module_names_only_top_level_and_not_core_or_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let gems = dir.path().join("gems");
        let write = |gem: &str, file: &str, body: &str| {
            let lib = gems.join(gem).join("lib");
            std::fs::create_dir_all(&lib).unwrap();
            std::fs::write(lib.join(file), body).unwrap();
        };
        write(
            "hashie-5.0.0",
            "hashie.rb",
            "module Hashie\n  module Extensions\n  end\n  module Array\n  end\nend\n",
        );
        write(
            "abbrev-0.1.0",
            "abbrev.rb",
            "module Abbrev\nend\n\nclass Array\n  def abbrev; end\nend\n",
        );
        write(
            "parallel-1.0.0",
            "parallel.rb",
            "module Parallel\nclass Worker\nend\nend\n",
        );
        write(
            "resolv-replace-0.1.0",
            "resolv-replace.rb",
            "class TCPSocket < IPSocket\nend\nclass SOCKSSocket < TCPSocket\nend\n",
        );
        write(
            "rails-html-sanitizer-1.0.0",
            "rails-html-sanitizer.rb",
            "module Rails\n  module HTML\n  end\nend\n\nmodule ActionView\n  module Helpers\n  end\nend\n",
        );
        write(
            "guarded-1.0.0",
            "guarded.rb",
            "unless defined?(Guarded)\n  module Guarded\n    class Inner; end\n  end\nend\nclass Foo::Bar\nend\n",
        );
        let index = build_gem_index(&[dir.path().to_path_buf()]);
        let gemfile: Vec<String> = index.keys().cloned().collect();
        let claims = NamespaceClaims::build(&gemfile, index.keys());
        let names = |gem: &str| discover_gem_module_names(gem, &index, &claims);
        assert_eq!(names("hashie"), vec!["Hashie"]);
        assert_eq!(names("abbrev"), vec!["Abbrev"]);
        assert_eq!(names("parallel"), vec!["Parallel"]);
        assert!(names("resolv-replace").is_empty());
        assert!(names("rails-html-sanitizer").is_empty());
        assert_eq!(names("guarded"), vec!["Foo::Bar", "Guarded"]);
    }

    #[test]
    fn discover_module_names_basic() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        let gem_dir = gems_dir.join("mygem-1.0.0");
        let lib_dir = gem_dir.join("lib");
        std::fs::create_dir_all(&lib_dir).unwrap();
        std::fs::write(lib_dir.join("mygem.rb"), "module MyGem\nend\n").unwrap();

        let index = build_gem_index(&[dir.path().to_path_buf()]);
        let modules = discover_gem_module_names("mygem", &index, &NamespaceClaims::default());
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
        let modules =
            discover_gem_module_names("google-cloud", &index, &NamespaceClaims::default());
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
        let modules = discover_gem_module_names("mygem", &index, &NamespaceClaims::default());
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
        let modules = discover_gem_module_names("mygem", &index, &NamespaceClaims::default());
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
        let modules =
            discover_gem_module_names("google-cloud", &index, &NamespaceClaims::default());
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
        let modules = discover_gem_module_names("aws-sdk-s3", &index, &NamespaceClaims::default());
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
        let modules = discover_gem_module_names("mygem", &index, &NamespaceClaims::default());
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
        let modules = discover_gem_module_names("mygem", &index, &NamespaceClaims::default());
        // Only top-level scanned; version.rb is not.
        assert_eq!(modules, vec!["MyGem".to_string()]);
    }

    // ── select_rvm_gemset (deterministic RVM selection) ──

    #[test]
    fn discover_module_names_missing_gem() {
        let index = HashMap::new();
        let modules = discover_gem_module_names("nonexistent", &index, &NamespaceClaims::default());
        assert!(modules.is_empty());
    }

    #[test]
    fn discover_module_names_no_lib() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        std::fs::create_dir_all(gems_dir.join("mygem-1.0.0")).unwrap();

        let index = build_gem_index(&[dir.path().to_path_buf()]);
        let modules = discover_gem_module_names("mygem", &index, &NamespaceClaims::default());
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
        let paths =
            discover_gem_require_paths("mygem", &index, &NamespaceClaims::default()).unwrap();
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
        let paths = discover_gem_require_paths("mygem", &index, &NamespaceClaims::default());
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
        let paths =
            discover_gem_require_paths("mygem", &index, &NamespaceClaims::default()).unwrap();
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

    fn gz(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn tar_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_path(path).unwrap();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn extract_gem_metadata_rejects_decompression_bomb() {
        let dir = tempfile::tempdir().unwrap();
        let gem_path = dir.path().join("bomb-1.0.0.gem");
        let mut yaml = b"require_paths:\n- lib\n".to_vec();
        yaml.resize((MAX_GEM_METADATA_BYTES + 1024) as usize, b' ');
        let metadata = gz(&yaml);
        assert!(metadata.len() < 1024 * 1024);
        std::fs::write(&gem_path, tar_of(&[("metadata.gz", &metadata)])).unwrap();
        assert!(extract_gem_metadata(&gem_path).is_none());
        assert!(matches!(read_bounded(&b"abcdef"[..], 3), Err(true)));
        assert_eq!(read_bounded(&b"abc"[..], 3).unwrap(), b"abc");
    }

    #[test]
    fn vendor_gem_require_paths_lists_files_not_load_path_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let gem_path = dir.path().join("weird-1.0.0.gem");
        let metadata =
            gz(b"--- !ruby/object:Gem::Specification\nname: weird\nrequire_paths:\n- lib\n- ext\n");
        let data = gz(&tar_of(&[
            ("lib/weird_core.rb", b""),
            ("lib/weird/version.rb", b""),
            ("lib/rack/handler/weird.rb", b""),
            ("ext/weird_ext/extconf.rb", b""),
        ]));
        std::fs::write(
            &gem_path,
            tar_of(&[("metadata.gz", &metadata), ("data.tar.gz", &data)]),
        )
        .unwrap();
        let paths =
            vendor_gem_require_paths(&gem_path, "weird", &NamespaceClaims::default()).unwrap();
        assert_eq!(
            paths,
            vec![
                "rack/handler/weird",
                "weird",
                "weird_core",
                "weird_ext/extconf"
            ]
        );
        assert!(!paths.iter().any(|p| p == "ext" || p == "lib"));
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
