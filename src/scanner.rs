use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use memchr::memmem;
use once_cell::sync::Lazy;
use regex::Regex;

use crate::types::{FileResult, ScanResults};

// YAML-specific regexes (rarely hit, not worth hand-rolling)
static RUBOCOP_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^\s*-\s*(rubocop[\w-]*)").unwrap());
static YAML_REQUIRE_BLOCK_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:require|plugins):\s*\n((?:\s*-\s*.+\n)*)").unwrap());
static YAML_ITEM_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^\s*-\s*(.+)").unwrap());
static INHERIT_GEM_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"inherit_gem:\s*\n((?:\s+\S+:\s*\n(?:\s+-.+\n)*)*)").unwrap());
static INHERIT_GEM_KEY_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^\s+([\w-]+):").unwrap());

const SCANNABLE_EXTENSIONS: &[&str] = &[
    ".rb", ".rake", ".gemspec", ".ru", ".erb", ".haml", ".slim", ".jbuilder", ".builder", ".rabl",
    ".thor", ".yml", ".yaml",
];

const SCANNABLE_FILENAMES: &[&str] = &[
    "Rakefile",
    "Thorfile",
    "Guardfile",
    "Capfile",
    "Vagrantfile",
    "Berksfile",
    "Procfile",
];

const DEFAULT_SKIP_DIRS: &[&str] = &[
    "vendor",
    ".bundle",
    "node_modules",
    ".git",
    "tmp",
    "log",
    "coverage",
    "pkg",
    "doc",
];

const RUBY_EXTS: &[&str] = &[
    ".rb", ".rake", ".gemspec", ".ru", ".erb", ".haml", ".slim", ".jbuilder", ".builder", ".rabl",
    ".thor",
];

// ── Hand-rolled byte scanners (memchr + manual parsing) ──────────────────────

#[inline]
fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[inline]
fn skip_whitespace(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    i
}

/// Extract a quoted string starting at position `i` (which should point at the quote char).
/// Returns (extracted_string, position_after_closing_quote).
#[inline]
fn extract_quoted(bytes: &[u8], i: usize) -> Option<(&[u8], usize)> {
    if i >= bytes.len() {
        return None;
    }
    let quote = bytes[i];
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let start = i + 1;
    let mut end = start;
    while end < bytes.len() && bytes[end] != quote && bytes[end] != b'\n' {
        end += 1;
    }
    if end >= bytes.len() || bytes[end] != quote {
        return None;
    }
    if end == start {
        return None; // empty string
    }
    Some((&bytes[start..end], end + 1))
}

/// Scan for `require 'name'`, `require('name')`, `require_dependency 'name'`, etc.
/// Matches regex: `(?:require|require_dependency)\s*[\(]?\s*['"]([^'"]+)['"]\s*[\)]?`
fn scan_requires(bytes: &[u8], results: &mut Vec<String>) {
    let finder = memmem::Finder::new(b"require");
    let mut pos = 0;
    while let Some(idx) = finder.find(&bytes[pos..]) {
        let abs = pos + idx;
        // Check word boundary before: must not be preceded by a word char
        if abs > 0 && is_word_char(bytes[abs - 1]) {
            pos = abs + 1;
            continue;
        }
        let mut i = abs + 7; // skip "require"
        // Optionally skip "_dependency" (11 chars)
        if i + 11 <= bytes.len() && &bytes[i..i + 11] == b"_dependency" {
            i += 11;
        }
        // Must not be followed by more word chars (e.g. "required", "require_relative")
        if i < bytes.len() && is_word_char(bytes[i]) {
            pos = abs + 1;
            continue;
        }
        // Skip whitespace
        i = skip_whitespace(bytes, i);
        // Optional opening paren
        let has_paren = i < bytes.len() && bytes[i] == b'(';
        if has_paren {
            i += 1;
        }
        // Skip whitespace after paren
        i = skip_whitespace(bytes, i);
        // Extract quoted string
        if let Some((s, _end)) = extract_quoted(bytes, i) {
            if let Ok(name) = std::str::from_utf8(s) {
                results.push(name.to_string());
            }
        }
        pos = abs + 1;
    }
}

/// Scan for `autoload :Symbol, 'name'`
/// Matches regex: `autoload\s*[\(]?\s*:\w+\s*,\s*['"]([^'"]+)['"]`
fn scan_autoloads(bytes: &[u8], results: &mut Vec<String>) {
    let finder = memmem::Finder::new(b"autoload");
    let mut pos = 0;
    while let Some(idx) = finder.find(&bytes[pos..]) {
        let abs = pos + idx;
        if abs > 0 && is_word_char(bytes[abs - 1]) {
            pos = abs + 1;
            continue;
        }
        let mut i = abs + 8; // skip "autoload"
        if i < bytes.len() && is_word_char(bytes[i]) {
            pos = abs + 1;
            continue;
        }
        i = skip_whitespace(bytes, i);
        // Optional paren
        if i < bytes.len() && bytes[i] == b'(' {
            i += 1;
        }
        i = skip_whitespace(bytes, i);
        // Expect :Symbol (must have at least one word char)
        if i >= bytes.len() || bytes[i] != b':' {
            pos = abs + 1;
            continue;
        }
        i += 1;
        let sym_start = i;
        while i < bytes.len() && is_word_char(bytes[i]) {
            i += 1;
        }
        if i == sym_start {
            // Empty symbol — e.g. `autoload :, 'foo'`
            pos = abs + 1;
            continue;
        }
        i = skip_whitespace(bytes, i);
        // Expect comma
        if i >= bytes.len() || bytes[i] != b',' {
            pos = abs + 1;
            continue;
        }
        i += 1;
        i = skip_whitespace(bytes, i);
        // Extract quoted string
        if let Some((s, _end)) = extract_quoted(bytes, i) {
            if let Ok(name) = std::str::from_utf8(s) {
                results.push(name.to_string());
            }
        }
        pos = abs + 1;
    }
}

/// Scan for `Gem.bin_path('name')` or `Gem.activate_bin_path('name')`
/// Matches regex: `Gem\.(?:activate_)?bin_path\s*\(\s*['"]([^'"]+)['"]`
fn scan_gem_bin(bytes: &[u8], results: &mut Vec<String>) {
    let finder = memmem::Finder::new(b"Gem.");
    let mut pos = 0;
    while let Some(idx) = finder.find(&bytes[pos..]) {
        let abs = pos + idx;
        let mut i = abs + 4; // skip "Gem."
        // "activate_bin_path" or "bin_path"
        if i + 17 <= bytes.len() && &bytes[i..i + 17] == b"activate_bin_path" {
            i += 17;
        } else if i + 8 <= bytes.len() && &bytes[i..i + 8] == b"bin_path" {
            i += 8;
        } else {
            pos = abs + 1;
            continue;
        }
        i = skip_whitespace(bytes, i);
        // Expect (
        if i >= bytes.len() || bytes[i] != b'(' {
            pos = abs + 1;
            continue;
        }
        i += 1;
        i = skip_whitespace(bytes, i);
        if let Some((s, _end)) = extract_quoted(bytes, i) {
            if let Ok(name) = std::str::from_utf8(s) {
                results.push(name.to_string());
            }
        }
        pos = abs + 1;
    }
}

/// Scan for uppercase module/class references: `Rails`, `ActiveRecord::Base`, etc.
/// Matches regex: `\b([A-Z][a-zA-Z0-9]+(?:::[A-Z][a-zA-Z0-9]+)*)\b`
/// Key semantics: must start at word boundary, must have 2+ alnum chars in first segment,
/// each `::` must be followed by an uppercase letter + 1+ alnum chars.
/// Uses backtracking: if `Foo::Bar_Baz` fails at `_`, emits `Foo::Bar` (like regex).
fn scan_modules(bytes: &[u8], results: &mut Vec<String>) {
    let mut i = 0;
    while i < bytes.len() {
        // Look for uppercase letter at a word boundary
        if bytes[i].is_ascii_uppercase() && (i == 0 || !is_word_char(bytes[i - 1])) {
            let start = i;
            i += 1;
            // Must have at least one more alnum char: [a-zA-Z0-9]+
            let seg_start_after_upper = i;
            while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
                i += 1;
            }
            if i == seg_start_after_upper {
                // Single uppercase letter like 'A' — doesn't match [A-Z][a-zA-Z0-9]+
                continue;
            }
            // last_good_end tracks the end of the last complete segment
            // so we can backtrack if a :: continuation fails the word boundary
            let mut last_good_end = i;
            // Check for ::UpperCase continuations
            while i + 1 < bytes.len() && bytes[i] == b':' && bytes[i + 1] == b':' {
                if i + 2 >= bytes.len() || !bytes[i + 2].is_ascii_uppercase() {
                    break;
                }
                let seg_start = i;
                i += 3; // skip :: and the uppercase letter
                let seg2_start = i;
                while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
                    i += 1;
                }
                if i == seg2_start {
                    // "::" + single uppercase letter — doesn't match the + quantifier
                    i = seg_start;
                    break;
                }
                // Check if this segment ends at a word boundary
                if i < bytes.len() && is_word_char(bytes[i]) {
                    // This segment's trailing word char means we can't include it.
                    // Backtrack: emit up to last_good_end, then skip forward.
                    i = seg_start;
                    break;
                }
                last_good_end = i;
            }
            // Use last_good_end for the match (handles backtracking)
            let match_end = if i < bytes.len() && is_word_char(bytes[i]) {
                // Current position has a word char — use backtracked position
                last_good_end
            } else {
                i
            };
            // Verify the match_end is at a word boundary
            if match_end < bytes.len() && is_word_char(bytes[match_end]) {
                // Even the backtracked position isn't at a word boundary — skip
                while i < bytes.len() && is_word_char(bytes[i]) {
                    i += 1;
                }
                continue;
            }
            if match_end > start {
                if let Ok(name) = std::str::from_utf8(&bytes[start..match_end]) {
                    results.push(name.to_string());
                }
            }
            // Continue scanning from after the match
            if i <= match_end {
                i = match_end;
            }
        } else {
            i += 1;
        }
    }
}

/// Scan for `Bundler.require` and extract the rest of the line.
/// Matches regex: `Bundler\.require\b(.*)`
/// Returns (rest_of_line, ...) for each match.
fn scan_bundler_require(
    bytes: &[u8],
    bundler_groups: &mut Vec<String>,
    bundler_require_all: &mut bool,
) {
    static GROUP_SYM_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r":(\w+)").unwrap());

    let finder = memmem::Finder::new(b"Bundler.require");
    let mut pos = 0;
    while let Some(idx) = finder.find(&bytes[pos..]) {
        let abs = pos + idx;
        let after = abs + 15; // skip "Bundler.require"
        // Word boundary: must not be followed by a word char
        if after < bytes.len() && is_word_char(bytes[after]) {
            pos = abs + 1;
            continue;
        }
        // Extract rest of line
        let mut end = after;
        while end < bytes.len() && bytes[end] != b'\n' {
            end += 1;
        }
        let rest = std::str::from_utf8(&bytes[after..end])
            .unwrap_or("")
            .trim();
        if rest.is_empty() || rest.starts_with('#') {
            *bundler_require_all = true;
        } else if rest.contains("Rails.groups") || rest.contains("Rails.env") {
            *bundler_require_all = true;
        } else {
            for g in GROUP_SYM_RE.captures_iter(rest) {
                bundler_groups.push(g[1].to_string());
            }
        }
        pos = abs + 1;
    }
}

pub fn scan_source_tree(dir: &Path, skip_dirs: &[&str]) -> ScanResults {
    let skip_set: HashSet<String> = skip_dirs.iter().map(|s| s.to_string()).collect();
    let dir_str = dir.to_string_lossy().to_string();

    let mut builder = ignore::WalkBuilder::new(dir);
    builder
        .standard_filters(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .follow_links(false)
        .hidden(false)
        .filter_entry(move |entry| {
            if entry.file_type().map_or(false, |ft| ft.is_dir()) {
                let name = entry.file_name().to_string_lossy();
                let depth = entry.depth();
                if skip_set.contains(name.as_ref()) && depth < 3 {
                    return false;
                }
            }
            true
        });

    // Fuse walk + read + scan: each walker thread reads and scans files inline
    let results = Arc::new(Mutex::new(Vec::<FileResult>::new()));
    let results_clone = Arc::clone(&results);
    let dir_str_shared = Arc::new(dir_str);

    builder.build_parallel().run(move || {
        let shared = Arc::clone(&results_clone);
        let dir_ref = Arc::clone(&dir_str_shared);
        struct FlushBuf {
            buf: Vec<FileResult>,
            shared: Arc<Mutex<Vec<FileResult>>>,
        }
        impl Drop for FlushBuf {
            fn drop(&mut self) {
                if !self.buf.is_empty() {
                    self.shared.lock().unwrap().append(&mut self.buf);
                }
            }
        }
        let mut flush_buf = FlushBuf {
            buf: Vec::new(),
            shared: Arc::clone(&shared),
        };
        Box::new(move |result| {
            if let Ok(entry) = result {
                if entry.file_type().map_or(false, |ft| ft.is_file()) {
                    let path_str = entry.path().to_string_lossy().to_string();
                    if is_scannable(&path_str, entry.path()) {
                        if let Some(file_result) = scan_file(&path_str, &dir_ref) {
                            flush_buf.buf.push(file_result);
                            if flush_buf.buf.len() >= 64 {
                                shared.lock().unwrap().append(&mut flush_buf.buf);
                            }
                        }
                    }
                }
            }
            ignore::WalkState::Continue
        })
    });

    let all_results = Arc::try_unwrap(results)
        .map(|m| m.into_inner().unwrap_or_default())
        .unwrap_or_default();

    let mut requires: HashMap<String, HashSet<String>> = HashMap::new();
    let mut modules: HashMap<String, HashSet<String>> = HashMap::new();
    let mut bundler_groups: HashSet<String> = HashSet::new();
    let mut bundler_require_all = false;

    for r in all_results {
        for req in &r.requires {
            requires
                .entry(req.clone())
                .or_default()
                .insert(r.rel_path.clone());
        }
        for m in &r.modules {
            modules
                .entry(m.clone())
                .or_default()
                .insert(r.rel_path.clone());
        }
        for g in &r.bundler_groups {
            bundler_groups.insert(g.clone());
        }
        if r.bundler_require_all {
            bundler_require_all = true;
        }
    }

    if bundler_require_all {
        bundler_groups.insert("default".to_string());
    }

    ScanResults {
        requires,
        modules,
        bundler_groups,
        bundler_require_all,
    }
}

pub fn default_skip_dirs() -> Vec<&'static str> {
    DEFAULT_SKIP_DIRS.to_vec()
}

fn is_scannable(path: &str, full_path: &Path) -> bool {
    let ext = Path::new(path)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    let basename = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    if SCANNABLE_EXTENSIONS.contains(&ext.as_str()) {
        return true;
    }
    if SCANNABLE_FILENAMES.contains(&basename.as_str()) {
        return true;
    }

    // Extensionless files with Ruby shebang
    if ext.is_empty() && full_path.is_file() {
        if let Ok(mut f) = std::fs::File::open(full_path) {
            use std::io::Read;
            let mut buf = [0u8; 64];
            if let Ok(n) = f.read(&mut buf) {
                let first_line = String::from_utf8_lossy(&buf[..n]);
                if let Some(line) = first_line.lines().next() {
                    if line.starts_with("#!") && line.contains("ruby") {
                        return true;
                    }
                }
            }
        }
    }

    false
}

fn scan_file(path: &str, dir: &str) -> Option<FileResult> {
    // Memory-map the file to avoid kernel→userspace copy
    let file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if metadata.len() == 0 {
        return None;
    }

    // For small files (<4KB), regular read is faster than mmap overhead
    let content = if metadata.len() < 4096 {
        let bytes = std::fs::read(path).ok()?;
        match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).to_string(),
        }
    } else {
        // SAFETY: file is opened read-only, we don't modify the mapping
        let mmap = unsafe { memmap2::Mmap::map(&file) }.ok()?;
        match std::str::from_utf8(&mmap) {
            Ok(s) => s.to_string(),
            Err(_) => String::from_utf8_lossy(&mmap).to_string(),
        }
    };

    let rel_path = Path::new(path)
        .strip_prefix(dir)
        .unwrap_or(Path::new(path))
        .display()
        .to_string();

    let ext = Path::new(path)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    let basename = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let is_ruby = RUBY_EXTS.contains(&ext.as_str()) || SCANNABLE_FILENAMES.contains(&basename.as_str());

    // Strip comments for Ruby files, avoid cloning for non-Ruby
    let scannable_content: Cow<str> = if is_ruby {
        let mut in_block_comment = false;
        Cow::Owned(
            content
                .lines()
                .filter(|line| {
                    let trimmed = line.trim_start();
                    if in_block_comment {
                        if trimmed == "=end" {
                            in_block_comment = false;
                        }
                        return false;
                    }
                    if trimmed == "=begin" {
                        in_block_comment = true;
                        return false;
                    }
                    !trimmed.starts_with('#')
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    } else {
        Cow::Borrowed(&content)
    };

    let mut requires = Vec::new();
    let mut mod_refs = Vec::new();
    let mut bundler_groups = Vec::new();
    let mut bundler_require_all = false;

    // Hand-rolled byte scanners (memchr + manual parsing)
    let scan_bytes = scannable_content.as_bytes();
    scan_requires(scan_bytes, &mut requires);
    scan_autoloads(scan_bytes, &mut requires);
    scan_gem_bin(scan_bytes, &mut requires);
    scan_modules(scan_bytes, &mut mod_refs);

    // YAML config patterns (keep regex — rarely hit, complex multiline patterns)
    if ext == ".yml" || ext == ".yaml" {
        for cap in RUBOCOP_RE.captures_iter(&content) {
            requires.push(cap[1].to_string());
        }
        for cap in YAML_REQUIRE_BLOCK_RE.captures_iter(&content) {
            for item_cap in YAML_ITEM_RE.captures_iter(&cap[1]) {
                requires.push(item_cap[1].trim().to_string());
            }
        }
        for cap in INHERIT_GEM_RE.captures_iter(&content) {
            for key_cap in INHERIT_GEM_KEY_RE.captures_iter(&cap[1]) {
                requires.push(key_cap[1].to_string());
            }
        }
    }

    // Bundler.require detection (hand-rolled)
    scan_bundler_require(scan_bytes, &mut bundler_groups, &mut bundler_require_all);

    Some(FileResult {
        rel_path,
        requires,
        modules: mod_refs,
        bundler_groups,
        bundler_require_all,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Comment stripping (tested via scan_file behavior) ──

    fn scan_content(content: &str) -> Vec<String> {
        let mut requires = Vec::new();
        // Simulate comment stripping for Ruby
        let mut in_block_comment = false;
        let stripped: String = content
            .lines()
            .filter(|line| {
                let trimmed = line.trim_start();
                if in_block_comment {
                    if trimmed == "=end" { in_block_comment = false; }
                    return false;
                }
                if trimmed == "=begin" { in_block_comment = true; return false; }
                !trimmed.starts_with('#')
            })
            .collect::<Vec<_>>()
            .join("\n");
        scan_requires(stripped.as_bytes(), &mut requires);
        requires
    }

    #[test]
    fn comment_stripping_hash() {
        let r = scan_content("# require 'foo'\nrequire 'bar'\n");
        assert_eq!(r, vec!["bar"]);
    }

    #[test]
    fn comment_stripping_begin_end() {
        let r = scan_content("=begin\nrequire 'foo'\n=end\nrequire 'bar'\n");
        assert_eq!(r, vec!["bar"]);
    }

    #[test]
    fn comment_stripping_indented_hash() {
        let r = scan_content("  # require 'foo'\nrequire 'bar'\n");
        assert_eq!(r, vec!["bar"]);
    }

    #[test]
    fn comment_stripping_no_comments() {
        let r = scan_content("require 'foo'\nrequire 'bar'\n");
        assert_eq!(r, vec!["foo", "bar"]);
    }

    // ── is_scannable ──

    #[test]
    fn scannable_ruby_extensions() {
        for ext in &[".rb", ".rake", ".gemspec", ".ru", ".erb", ".haml", ".slim", ".yml", ".yaml"] {
            let path = format!("/tmp/test{}", ext);
            let p = Path::new(&path);
            // Create a temp file to test
            assert!(
                SCANNABLE_EXTENSIONS.contains(ext),
                "{} should be scannable", ext
            );
        }
    }

    #[test]
    fn scannable_special_filenames() {
        for name in &["Rakefile", "Guardfile", "Capfile", "Procfile"] {
            assert!(SCANNABLE_FILENAMES.contains(name), "{} should be scannable", name);
        }
    }
}
