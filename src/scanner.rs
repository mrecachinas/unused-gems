use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use memchr::memmem;
use once_cell::sync::Lazy;
use regex::bytes::Regex as BytesRegex;
use regex::Regex;

use crate::types::{FileResult, ScanResults};

// YAML-specific regexes (rarely hit, not worth hand-rolling).
// Use bytes-mode regex to avoid String::from_utf8_lossy allocations.
static RUBOCOP_RE: Lazy<BytesRegex> =
    Lazy::new(|| BytesRegex::new(r"(?m)^\s*-\s*(rubocop[\w-]*)").unwrap());
static YAML_REQUIRE_BLOCK_RE: Lazy<BytesRegex> =
    Lazy::new(|| BytesRegex::new(r"(?:require|plugins):\s*\n((?:\s*-\s*.+\n)*)").unwrap());
static YAML_ITEM_RE: Lazy<BytesRegex> = Lazy::new(|| BytesRegex::new(r"(?m)^\s*-\s*(.+)").unwrap());
static INHERIT_GEM_RE: Lazy<BytesRegex> =
    Lazy::new(|| BytesRegex::new(r"inherit_gem:\s*\n((?:\s+\S+:\s*\n(?:\s+-.+\n)*)*)").unwrap());
static INHERIT_GEM_KEY_RE: Lazy<BytesRegex> =
    Lazy::new(|| BytesRegex::new(r"(?m)^\s+([\w-]+):").unwrap());

// Precompiled byte finders (avoid re-building per call)
static NEWLINE_BEGIN_FINDER: Lazy<memmem::Finder<'static>> =
    Lazy::new(|| memmem::Finder::new(b"\n=begin"));
static BEGIN_FINDER: Lazy<memmem::Finder<'static>> = Lazy::new(|| memmem::Finder::new(b"=begin"));
static HEREDOC_FINDER: Lazy<memmem::Finder<'static>> = Lazy::new(|| memmem::Finder::new(b"<<"));
static REQUIRE_FINDER: Lazy<memmem::Finder<'static>> =
    Lazy::new(|| memmem::Finder::new(b"require"));
static AUTOLOAD_FINDER: Lazy<memmem::Finder<'static>> =
    Lazy::new(|| memmem::Finder::new(b"autoload"));
static GEM_DOT_FINDER: Lazy<memmem::Finder<'static>> = Lazy::new(|| memmem::Finder::new(b"Gem."));
static BUNDLER_REQUIRE_FINDER: Lazy<memmem::Finder<'static>> =
    Lazy::new(|| memmem::Finder::new(b"Bundler.require"));

/// What style of single-line comment marker the file uses.
/// Drives `build_skip_mask` so that Slim/Haml comment lines are skipped
/// even though they don't start with `#`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum LineCommentKind {
    /// `#` — Ruby, YAML, ERB, shell, generic. Also tracks inline `#` comments.
    Hash,
    /// `/` — Slim line comments.
    Slash,
    /// `-#` — Haml line comments.
    DashHash,
}

#[cfg(test)]
const SCANNABLE_EXTENSIONS: &[&str] = &[
    ".rb",
    ".rake",
    ".gemspec",
    ".ru",
    ".erb",
    ".haml",
    ".slim",
    ".jbuilder",
    ".builder",
    ".rabl",
    ".thor",
    ".yml",
    ".yaml",
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

/// Check if position `pos` is on a line that starts with `#` (after whitespace),
/// OR if `pos` appears after an inline `#` comment on the same line.
/// Uses SIMD-accelerated memrchr to find line start.
#[inline]
#[allow(dead_code)]
fn is_line_comment(bytes: &[u8], pos: usize) -> bool {
    // SIMD-accelerated backward scan for newline
    let line_start = if pos == 0 {
        0
    } else {
        match memchr::memrchr(b'\n', &bytes[..pos]) {
            Some(nl) => nl + 1,
            None => 0,
        }
    };
    // Skip leading whitespace
    let mut i = line_start;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'#' {
        return true;
    }
    // Check for inline comment: scan from start of line to `pos` for unquoted `#`
    has_inline_comment_before(bytes, line_start, pos)
}

/// Check if there is an unquoted `#` between `from` and `pos` on the same line.
/// Uses a simple state machine to track single/double quoted strings.
#[allow(dead_code)]
#[inline]
fn has_inline_comment_before(bytes: &[u8], from: usize, pos: usize) -> bool {
    let mut i = from;
    while i < pos {
        match bytes[i] {
            b'\'' | b'"' => {
                let quote = bytes[i];
                i += 1;
                // Skip to matching close quote (handle escaped quotes)
                while i < pos {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'#' => return true,
            _ => i += 1,
        }
    }
    false
}

/// Precompute byte ranges covered by =begin/=end block comments and heredocs.
/// Returns sorted Vec of (start, end) byte offsets.
fn build_block_comment_ranges(bytes: &[u8]) -> Vec<(usize, usize)> {
    // Short-circuit: if neither =begin nor << appear, no block ranges possible
    if BEGIN_FINDER.find(bytes).is_none() && memmem::find(bytes, b"<<").is_none() {
        return Vec::new();
    }

    let mut ranges = Vec::new();

    // Pass 1: =begin/=end block comments
    let mut pos = 0;
    // Also check if file starts with =begin
    if bytes.starts_with(b"=begin") {
        if let Some(end) = memmem::find(&bytes[6..], b"\n=end") {
            let block_end = 6 + end + 5; // skip past "\n=end"
                                         // find next newline after =end
            let mut e = block_end;
            while e < bytes.len() && bytes[e] != b'\n' {
                e += 1;
            }
            ranges.push((0, e));
            pos = e;
        } else {
            // =begin with no =end — entire file is commented
            return vec![(0, bytes.len())];
        }
    }
    while let Some(idx) = NEWLINE_BEGIN_FINDER.find(&bytes[pos..]) {
        let abs = pos + idx + 1; // skip the \n, point at =begin
        let after_begin = abs + 6;
        // Check it's a full line (next char is whitespace or newline)
        if after_begin < bytes.len()
            && bytes[after_begin] != b'\n'
            && bytes[after_begin] != b' '
            && bytes[after_begin] != b'\t'
        {
            pos = after_begin;
            continue;
        }
        if let Some(end_idx) = memmem::find(&bytes[after_begin..], b"\n=end") {
            let block_end = after_begin + end_idx + 5;
            let mut e = block_end;
            while e < bytes.len() && bytes[e] != b'\n' {
                e += 1;
            }
            ranges.push((abs, e));
            pos = e;
        } else {
            ranges.push((abs, bytes.len()));
            break;
        }
    }

    // Pass 2: heredocs (<<~DELIM, <<-DELIM, <<DELIM, <<~'DELIM', <<~"DELIM")
    pos = 0;
    while let Some(idx) = HEREDOC_FINDER.find(&bytes[pos..]) {
        let abs = pos + idx;
        // Fix #6: For `<<IDENT` to start a heredoc, the byte preceding `<<`
        // must NOT be an identifier/word char. This prevents `arr<<MY_CONST`
        // (shovel operator) from being treated as a heredoc start.
        if abs > 0 {
            let prev = bytes[abs - 1];
            // Allowed leading contexts: whitespace, =, (, comma, [, {, ;, BOF.
            // Anything else (word char, digit, ', ", ), ], etc.) means it's a
            // shovel operator or expression continuation, not a heredoc.
            let ok = matches!(
                prev,
                b' ' | b'\t' | b'\n' | b'\r' | b'=' | b'(' | b',' | b'[' | b'{' | b';'
            );
            if !ok {
                pos = abs + 2;
                continue;
            }
        }
        let mut i = abs + 2;
        // Optional ~ or -
        if i < bytes.len() && (bytes[i] == b'~' || bytes[i] == b'-') {
            i += 1;
        }
        // Optional quote char
        let quote_char = if i < bytes.len() && (bytes[i] == b'\'' || bytes[i] == b'"') {
            let q = bytes[i];
            i += 1;
            Some(q)
        } else {
            None
        };
        // Extract delimiter (uppercase letters, underscores, digits)
        let delim_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        if i == delim_start {
            pos = abs + 1;
            continue;
        }
        let delim = &bytes[delim_start..i];
        // Closing quote if opened
        if let Some(q) = quote_char {
            if i < bytes.len() && bytes[i] == q {
                i += 1;
            } else {
                pos = abs + 1;
                continue;
            }
        }
        // Delimiter must be at least 2 chars to avoid false matches
        if delim.len() < 2 {
            pos = abs + 1;
            continue;
        }
        // Find the start of heredoc content (next line)
        while i < bytes.len() && bytes[i] != b'\n' {
            i += 1;
        }
        if i < bytes.len() {
            i += 1; // skip \n
        }
        let heredoc_content_start = i;
        // Find closing delimiter on its own line (with optional leading whitespace)
        let mut found_end = false;
        while i < bytes.len() {
            let line_start = i;
            // Skip leading whitespace
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            // Check if this line is the closing delimiter
            if i + delim.len() <= bytes.len() && &bytes[i..i + delim.len()] == delim {
                let after_delim = i + delim.len();
                if after_delim >= bytes.len()
                    || bytes[after_delim] == b'\n'
                    || bytes[after_delim] == b'\r'
                {
                    // Found closing delimiter
                    ranges.push((heredoc_content_start, after_delim));
                    pos = after_delim;
                    found_end = true;
                    break;
                }
            }
            // Skip to next line
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            if i == line_start {
                break; // safety: no progress
            }
        }
        if !found_end {
            pos = abs + 1;
        }
    }

    ranges.sort_by_key(|&(start, _)| start);
    ranges
}

fn mask_percent_q_strings(bytes: &[u8], mask: &mut [bool]) {
    let mut i = 0;
    while i + 2 < bytes.len() {
        if mask[i] || bytes[i] != b'%' || (bytes[i + 1] != b'q' && bytes[i + 1] != b'Q') {
            i += 1;
            continue;
        }

        let (open, close) = match bytes[i + 2] {
            b'{' => (b'{', b'}'),
            b'(' => (b'(', b')'),
            b'[' => (b'[', b']'),
            b'<' => (b'<', b'>'),
            _ => {
                i += 1;
                continue;
            }
        };

        let content_start = i + 3;
        let mut depth = 1usize;
        let mut k = content_start;
        while k < bytes.len() {
            if bytes[k] == b'\\' {
                k = (k + 2).min(bytes.len());
                continue;
            }
            if bytes[k] == open {
                depth += 1;
            } else if bytes[k] == close {
                depth -= 1;
                if depth == 0 {
                    for b in mask.iter_mut().take(k).skip(content_start) {
                        *b = true;
                    }
                    i = k + 1;
                    break;
                }
            }
            k += 1;
        }

        if depth != 0 {
            i += 1;
        }
    }
}

/// Check if position `pos` falls within a block comment range.
#[inline]
#[allow(dead_code)]
fn is_in_block_comment(pos: usize, ranges: &[(usize, usize)]) -> bool {
    ranges.iter().any(|&(start, end)| pos >= start && pos < end)
}

/// Combined check: is this position in a # comment line OR a =begin/=end block?
#[inline]
#[allow(dead_code)]
fn is_commented(bytes: &[u8], pos: usize, block_ranges: &[(usize, usize)]) -> bool {
    is_line_comment(bytes, pos) || is_in_block_comment(pos, block_ranges)
}

/// Build a unified per-byte "skip mask" in a single linear pass.
///
/// `mask[i] == true` means position `i` should be ignored by all token-level
/// scanners. The mask covers:
///   - `#` line comments (whole line, leading-whitespace aware)
///   - inline `#` comments after non-string code (Hash kind only)
///   - `=begin` / `=end` block comments (Ruby only)
///   - heredoc bodies (Ruby only)
///   - interiors of single- and double-quoted string literals (so a `require`
///     keyword that lives inside a string is not detected as a real require).
///
/// `line_kind` controls which marker starts a line comment:
///   - `Hash`     → `#`  (also enables inline `#` comment detection)
///   - `Slash`    → `/`  (Slim)
///   - `DashHash` → `-#` (Haml)
///
/// String literal interiors are masked regardless of `line_kind` so that fix
/// #3 (`raise "Please require 'oauth2'"` not detected as a require) holds for
/// both Ruby and the templating dialects.
fn build_skip_mask(bytes: &[u8], is_ruby: bool, line_kind: LineCommentKind) -> Vec<bool> {
    let mut mask = vec![false; bytes.len()];

    // Pass 1: heredoc + =begin/=end ranges (Ruby only).
    if is_ruby {
        let ranges = build_block_comment_ranges(bytes);
        for (s, e) in ranges {
            let e = e.min(bytes.len());
            for b in mask.iter_mut().take(e).skip(s) {
                *b = true;
            }
        }
    }

    // Pass 2: line-by-line walk for line comments, inline comments, strings.
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        let line_start = i;
        // Find end of line.
        let mut eol = i;
        while eol < len && bytes[eol] != b'\n' {
            eol += 1;
        }

        // Find first non-whitespace position on the line.
        let mut ws = line_start;
        while ws < eol && (bytes[ws] == b' ' || bytes[ws] == b'\t') {
            ws += 1;
        }

        // Whole-line comment detection based on line_kind.
        let line_is_comment = match line_kind {
            LineCommentKind::Hash => ws < eol && bytes[ws] == b'#',
            LineCommentKind::Slash => ws < eol && bytes[ws] == b'/',
            LineCommentKind::DashHash => ws + 1 < eol && bytes[ws] == b'-' && bytes[ws + 1] == b'#',
        };

        if line_is_comment {
            for b in mask.iter_mut().take(eol).skip(ws) {
                *b = true;
            }
        } else {
            // Walk for string state and (for Hash kind) inline `#` comments.
            // Skip positions already masked by heredoc/=begin pass.
            let mut j = line_start;
            while j < eol {
                if mask[j] {
                    j += 1;
                    continue;
                }
                let b = bytes[j];
                if b == b'\'' || b == b'"' {
                    let quote = b;
                    let str_content_start = j + 1;
                    let mut k = str_content_start;
                    while k < eol {
                        if bytes[k] == b'\\' && k + 1 < eol {
                            k += 2;
                            continue;
                        }
                        if bytes[k] == quote {
                            break;
                        }
                        k += 1;
                    }
                    // Mask string interior (NOT the surrounding quotes — extract_quoted
                    // and similar consumers expect the quote chars to remain visible).
                    let str_end = k.min(eol);
                    for b in mask.iter_mut().take(str_end).skip(str_content_start) {
                        *b = true;
                    }
                    j = if k < eol { k + 1 } else { eol };
                    continue;
                }
                // Inline `#` only counts as a comment for Hash kind.
                if matches!(line_kind, LineCommentKind::Hash) && b == b'#' {
                    for b in mask.iter_mut().take(eol).skip(j) {
                        *b = true;
                    }
                    break;
                }
                j += 1;
            }
        }

        i = if eol < len { eol + 1 } else { eol };
    }

    if is_ruby {
        mask_percent_q_strings(bytes, &mut mask);
    }

    mask
}

/// O(1) replacement for `is_commented` using a precomputed mask.
#[inline]
fn is_skipped(mask: &[bool], pos: usize) -> bool {
    mask.get(pos).copied().unwrap_or(false)
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
/// Matches regex: `(?:require|require_dependency}\s*[\(]?\s*['"]([^'"]+)['"]\s*[\)]?`
#[cfg(test)]
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
#[cfg(test)]
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
#[cfg(test)]
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
#[cfg(test)]
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
#[cfg(test)]
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
        let rest_owned =
            strip_inline_comment(std::str::from_utf8(&bytes[after..end]).unwrap_or(""));
        let rest = rest_owned.trim();
        let rest_inner = rest.strip_prefix('(').map(|s| s.trim()).unwrap_or(rest);
        let is_empty_call = rest.is_empty() || rest == "(" || rest == "()" || rest == "( )" || {
            let r = rest_inner.trim_start();
            let r = r.strip_prefix(')').map(|s| s.trim()).unwrap_or(r);
            rest.starts_with('(') && r.is_empty()
        };
        if rest.contains("Rails.groups") || rest.contains("Rails.env") {
            *bundler_require_all = true;
        } else {
            if is_empty_call || rest_inner.starts_with('*') {
                bundler_groups.push("default".to_string());
                if let Some(var) = splat_var_name(rest_inner) {
                    if push_rails_groups_for_splat_var(bytes, var, bundler_groups) {
                        *bundler_require_all = true;
                    }
                }
            }
            for g in GROUP_SYM_RE.captures_iter(rest) {
                bundler_groups.push(g[1].to_string());
            }
        }
        pos = abs + 1;
    }
}

/// Filtered wrappers that skip matches in commented/string/heredoc regions
/// using a precomputed `mask`. For non-Ruby files the caller still passes a
/// mask built with appropriate `LineCommentKind` so YAML `#` comments etc.
/// continue to be filtered.
fn scan_requires_filtered(bytes: &[u8], mask: &[bool], results: &mut Vec<String>) {
    let mut pos = 0;
    while let Some(idx) = REQUIRE_FINDER.find(&bytes[pos..]) {
        let abs = pos + idx;
        if is_skipped(mask, abs) {
            pos = abs + 1;
            continue;
        }
        if abs > 0 && is_word_char(bytes[abs - 1]) {
            pos = abs + 1;
            continue;
        }
        let mut i = abs + 7;
        if i + 11 <= bytes.len() && &bytes[i..i + 11] == b"_dependency" {
            i += 11;
        }
        if i < bytes.len() && is_word_char(bytes[i]) {
            pos = abs + 1;
            continue;
        }
        i = skip_whitespace(bytes, i);
        if i < bytes.len() && bytes[i] == b'(' {
            i += 1;
        }
        i = skip_whitespace(bytes, i);
        if let Some((s, _end)) = extract_quoted(bytes, i) {
            if let Ok(name) = std::str::from_utf8(s) {
                results.push(name.to_string());
            }
        }
        pos = abs + 1;
    }
}

fn scan_autoloads_filtered(bytes: &[u8], mask: &[bool], results: &mut Vec<String>) {
    let mut pos = 0;
    while let Some(idx) = AUTOLOAD_FINDER.find(&bytes[pos..]) {
        let abs = pos + idx;
        if is_skipped(mask, abs) {
            pos = abs + 1;
            continue;
        }
        if abs > 0 && is_word_char(bytes[abs - 1]) {
            pos = abs + 1;
            continue;
        }
        let mut i = abs + 8;
        if i < bytes.len() && is_word_char(bytes[i]) {
            pos = abs + 1;
            continue;
        }
        i = skip_whitespace(bytes, i);
        if i < bytes.len() && bytes[i] == b'(' {
            i += 1;
        }
        i = skip_whitespace(bytes, i);
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
            pos = abs + 1;
            continue;
        }
        i = skip_whitespace(bytes, i);
        if i >= bytes.len() || bytes[i] != b',' {
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

fn scan_gem_bin_filtered(bytes: &[u8], mask: &[bool], results: &mut Vec<String>) {
    let mut pos = 0;
    while let Some(idx) = GEM_DOT_FINDER.find(&bytes[pos..]) {
        let abs = pos + idx;
        if is_skipped(mask, abs) {
            pos = abs + 1;
            continue;
        }
        let mut i = abs + 4;
        if i + 17 <= bytes.len() && &bytes[i..i + 17] == b"activate_bin_path" {
            i += 17;
        } else if i + 8 <= bytes.len() && &bytes[i..i + 8] == b"bin_path" {
            i += 8;
        } else {
            pos = abs + 1;
            continue;
        }
        i = skip_whitespace(bytes, i);
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

fn scan_modules_filtered_into(
    bytes: &[u8],
    mask: &[bool],
    modules: &mut HashMap<String, HashSet<Arc<str>>>,
    rel_path: &Arc<str>,
) {
    let mut i = 0;
    let len = bytes.len();
    while i < len {
        // Fast skip non-uppercase bytes
        while i < len && !bytes[i].is_ascii_uppercase() {
            i += 1;
        }
        if i >= len {
            break;
        }
        if i == 0 || !is_word_char(bytes[i - 1]) {
            if is_skipped(mask, i) {
                // Skip rest of masked region (just advance by 1 — mask is per-byte)
                i += 1;
                continue;
            }
            let start = i;
            i += 1;
            let seg_start_after_upper = i;
            while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
                i += 1;
            }
            if i == seg_start_after_upper {
                continue;
            }
            let mut last_good_end = i;
            while i + 1 < bytes.len() && bytes[i] == b':' && bytes[i + 1] == b':' {
                if i + 2 >= bytes.len() || !bytes[i + 2].is_ascii_uppercase() {
                    break;
                }
                let seg_start = i;
                i += 3;
                let seg2_start = i;
                while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
                    i += 1;
                }
                if i == seg2_start {
                    i = seg_start;
                    break;
                }
                if i < bytes.len() && is_word_char(bytes[i]) {
                    i = seg_start;
                    break;
                }
                last_good_end = i;
            }
            let match_end = if i < bytes.len() && is_word_char(bytes[i]) {
                last_good_end
            } else {
                i
            };
            if match_end < bytes.len() && is_word_char(bytes[match_end]) {
                while i < bytes.len() && is_word_char(bytes[i]) {
                    i += 1;
                }
                continue;
            }
            if match_end > start {
                let slice = &bytes[start..match_end];
                if let Ok(name) = std::str::from_utf8(slice) {
                    // Use entry().or_default() to avoid double lookup;
                    // rel_path is an Arc<str> so the clone is a refcount bump.
                    modules
                        .entry(name.to_string())
                        .or_default()
                        .insert(Arc::clone(rel_path));
                }
            }
            if i <= match_end {
                i = match_end;
            }
        } else {
            // Uppercase but preceded by word char — skip past word
            i += 1;
        }
    }
}

fn scan_bundler_require_filtered(
    bytes: &[u8],
    mask: &[bool],
    bundler_groups: &mut Vec<String>,
    bundler_require_all: &mut bool,
) {
    static GROUP_SYM_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r":(\w+)").unwrap());
    let mut pos = 0;
    while let Some(idx) = BUNDLER_REQUIRE_FINDER.find(&bytes[pos..]) {
        let abs = pos + idx;
        if is_skipped(mask, abs) {
            pos = abs + 1;
            continue;
        }
        let after = abs + 15;
        if after < bytes.len() && is_word_char(bytes[after]) {
            pos = abs + 1;
            continue;
        }
        let mut end = after;
        while end < bytes.len() && bytes[end] != b'\n' {
            end += 1;
        }
        let raw = std::str::from_utf8(&bytes[after..end]).unwrap_or("").trim();
        // Fix #2: strip an inline `#` comment from the captured tail before
        // applying GROUP_SYM_RE so that
        //   Bundler.require(:default) # :test removed
        // does not capture `:test` from the comment. We must respect string
        // literals so a `#` inside `"..."` / `'...'` doesn't terminate.
        let rest_owned = strip_inline_comment(raw);
        let rest = rest_owned.trim();
        let rest_inner = rest.strip_prefix('(').map(|s| s.trim()).unwrap_or(rest);

        // Fix #1: `Bundler.require`, `Bundler.require()`, `Bundler.require( )`
        // and `Bundler.require(\n)` all mean "auto-require the :default group".
        // Detect the "no args" form before falling through to group extraction.
        let is_empty_call = rest.is_empty() || rest == "(" || rest == "()" || rest == "( )" || {
            let r = rest_inner.trim_start();
            let r = r.strip_prefix(')').map(|s| s.trim()).unwrap_or(r);
            rest.starts_with('(') && r.is_empty()
        };

        if rest.contains("Rails.groups") || rest.contains("Rails.env") {
            *bundler_require_all = true;
        } else {
            if is_empty_call || rest_inner.starts_with('*') {
                bundler_groups.push("default".to_string());
                if let Some(var) = splat_var_name(rest_inner) {
                    if push_rails_groups_for_splat_var(bytes, var, bundler_groups) {
                        *bundler_require_all = true;
                    }
                }
            }
            for g in GROUP_SYM_RE.captures_iter(rest) {
                bundler_groups.push(g[1].to_string());
            }
        }
        pos = abs + 1;
    }
}

fn splat_var_name(rest_inner: &str) -> Option<&str> {
    let rest = rest_inner.trim_start();
    let rest = rest.strip_prefix('*')?.trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    (end > 0).then_some(&rest[..end])
}

fn push_rails_groups_for_splat_var(
    bytes: &[u8],
    var: &str,
    bundler_groups: &mut Vec<String>,
) -> bool {
    static GROUP_SYM_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r":(\w+)").unwrap());
    static PERCENT_W_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"%w[\[\(]\s*([^\]\)]*)[\]\)]").unwrap());

    let mut aliases_unfiltered_rails_groups = false;
    for line in bytes.split(|&b| b == b'\n') {
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        if !line.contains(var) || !line.contains("Rails.groups") {
            continue;
        }
        let before = bundler_groups.len();
        for cap in GROUP_SYM_RE.captures_iter(line) {
            bundler_groups.push(cap[1].to_string());
        }
        for cap in PERCENT_W_RE.captures_iter(line) {
            for group in cap[1].split_whitespace() {
                if !group.is_empty() {
                    bundler_groups.push(group.to_string());
                }
            }
        }
        if bundler_groups.len() == before {
            aliases_unfiltered_rails_groups = true;
        }
    }
    aliases_unfiltered_rails_groups
}

/// Strip an inline `#` comment from `s`, respecting single- and double-quoted
/// string literals so a `#` inside a string is preserved.
fn strip_inline_comment(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' || b == b'"' {
            let q = b;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == q {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if b == b'#' {
            return std::str::from_utf8(&bytes[..i]).unwrap_or("").to_string();
        }
        i += 1;
    }
    s.to_string()
}

/// Scan for middleware string-literal references (e.g. `config.middleware.use "Bullet::Rack"`)
/// and insert each quoted module name into `modules`.
fn scan_middleware_refs(
    bytes: &[u8],
    mask: &[bool],
    modules: &mut HashMap<String, HashSet<Arc<str>>>,
    rel_path: &Arc<str>,
) {
    let has_candidate = memmem::find(bytes, b".middleware.").is_some()
        || memmem::find(bytes, b"middleware_stack").is_some()
        || memmem::find(bytes, b"Rack::Builder").is_some();
    if !has_candidate {
        return;
    }

    let mut line_start = 0;
    while line_start < bytes.len() {
        let mut eol = line_start;
        while eol < bytes.len() && bytes[eol] != b'\n' {
            eol += 1;
        }
        let line = &bytes[line_start..eol];
        let candidate = memmem::find(line, b".middleware.")
            .or_else(|| memmem::find(line, b"middleware_stack"))
            .or_else(|| memmem::find(line, b"Rack::Builder"));
        let Some(candidate) = candidate else {
            line_start = if eol < bytes.len() { eol + 1 } else { eol };
            continue;
        };
        if is_skipped(mask, line_start + candidate) {
            line_start = if eol < bytes.len() { eol + 1 } else { eol };
            continue;
        }

        // Extract all quoted string literals from the rest of the line.
        // NOTE: we walk raw bytes here (not the mask) because string interiors
        // are exactly what the middleware scanner cares about — masking them
        // would defeat the purpose.
        let mut i = line_start + candidate;
        while i < eol {
            if bytes[i] == b'"' || bytes[i] == b'\'' {
                let quote = bytes[i];
                let start = i + 1;
                let mut end = start;
                while end < eol && bytes[end] != quote {
                    end += 1;
                }
                if end < eol {
                    if let Ok(name) = std::str::from_utf8(&bytes[start..end]) {
                        let name = name.trim();
                        // Only insert if it looks like a module name (starts with uppercase)
                        if !name.is_empty() && name.as_bytes()[0].is_ascii_uppercase() {
                            modules
                                .entry(name.to_string())
                                .or_default()
                                .insert(Arc::clone(rel_path));
                        }
                    }
                    i = end + 1;
                } else {
                    break;
                }
            } else {
                i += 1;
            }
        }
        line_start = if eol < bytes.len() { eol + 1 } else { eol };
    }
}

/// Type alias for cache update entries collected during the walk
pub type CacheUpdate = (
    String,
    std::time::SystemTime,
    u64,
    Vec<String>,
    Vec<String>,
    Vec<String>,
    bool,
);

type ScriptFileList = Vec<(String, Vec<u8>)>;
type ModuleMap = Vec<HashMap<String, HashSet<Arc<str>>>>;

pub fn scan_source_tree(
    dir: &Path,
    skip_dirs: &[&str],
    cache: Arc<crate::cache::FileCache>,
) -> (ScanResults, Vec<CacheUpdate>) {
    let skip_set: HashSet<String> = skip_dirs.iter().map(|s| s.to_string()).collect();
    let dir_str = dir.to_string_lossy().to_string();
    let use_cache = cache.is_enabled();

    let mut builder = ignore::WalkBuilder::new(dir);
    builder
        .threads(rayon::current_num_threads().clamp(1, 4))
        .standard_filters(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .follow_links(false)
        .hidden(false)
        .filter_entry(move |entry| {
            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                let name = entry.file_name().to_string_lossy();
                if skip_set.contains(name.as_ref()) {
                    return false;
                }
            }
            true
        });

    // Fuse walk + read + scan: each walker thread reads and scans files inline
    // Also collects script files for later gem-name matching (single walk)
    let results = Arc::new(Mutex::new(Vec::<FileResult>::new()));
    let script_files = Arc::new(Mutex::new(Vec::<(String, Vec<u8>)>::new()));
    let thread_modules = Arc::new(Mutex::new(ModuleMap::new()));
    // Cache updates collected per-thread, merged after walk
    let cache_updates = Arc::new(Mutex::new(Vec::<CacheUpdate>::new()));
    let results_clone = Arc::clone(&results);
    let scripts_clone = Arc::clone(&script_files);
    let modules_clone = Arc::clone(&thread_modules);
    let cache_updates_clone = Arc::clone(&cache_updates);
    let cache_for_walk = Arc::clone(&cache);
    let dir_str_shared = Arc::new(dir_str);

    builder.build_parallel().run(move || {
        let shared = Arc::clone(&results_clone);
        let scripts = Arc::clone(&scripts_clone);
        let modules_shared = Arc::clone(&modules_clone);
        let cache_shared = Arc::clone(&cache_for_walk);
        let cache_puts = Arc::clone(&cache_updates_clone);
        let dir_ref = Arc::clone(&dir_str_shared);
        struct FlushBuf {
            buf: Vec<FileResult>,
            script_buf: Vec<(String, Vec<u8>)>,
            cache_buf: Vec<CacheUpdate>,
            shared: Arc<Mutex<Vec<FileResult>>>,
            scripts: Arc<Mutex<ScriptFileList>>,
            modules_shared: Arc<Mutex<ModuleMap>>,
            cache_puts: Arc<Mutex<Vec<CacheUpdate>>>,
            // Thread-local module accumulator: maps module_name → set of rel_paths.
            // rel_paths are interned as Arc<str> so each file allocates the path
            // once and per-module insertions are cheap refcount bumps (fix #10).
            local_modules: HashMap<String, HashSet<Arc<str>>>,
        }
        impl FlushBuf {
            fn flush_results(&mut self) {
                if !self.buf.is_empty() {
                    self.shared.lock().unwrap().append(&mut self.buf);
                }
                if !self.script_buf.is_empty() {
                    self.scripts.lock().unwrap().append(&mut self.script_buf);
                }
                if !self.cache_buf.is_empty() {
                    self.cache_puts.lock().unwrap().append(&mut self.cache_buf);
                }
            }
        }
        impl Drop for FlushBuf {
            fn drop(&mut self) {
                self.flush_results();
                if !self.local_modules.is_empty() {
                    let modules = std::mem::take(&mut self.local_modules);
                    self.modules_shared.lock().unwrap().push(modules);
                }
            }
        }
        let mut flush_buf = FlushBuf {
            buf: Vec::new(),
            script_buf: Vec::new(),
            cache_buf: Vec::new(),
            shared: Arc::clone(&shared),
            scripts: Arc::clone(&scripts),
            modules_shared: Arc::clone(&modules_shared),
            cache_puts: Arc::clone(&cache_puts),
            local_modules: HashMap::new(),
        };
        Box::new(move |result| {
            if let Ok(entry) = result {
                if entry.file_type().is_some_and(|ft| ft.is_file()) {
                    let path = entry.path();
                    let path_str = path.to_string_lossy().to_string();
                    let rel_path_cow = path
                        .strip_prefix(dir_ref.as_str())
                        .unwrap_or(path)
                        .to_string_lossy();
                    let file_name = path
                        .file_name()
                        .map(|n| n.to_string_lossy())
                        .unwrap_or_default();

                    // Check if it's a scannable source file
                    if is_scannable(&path_str, path) {
                        // Use metadata from DirEntry (avoids redundant stat syscall)
                        let metadata = entry.metadata().ok();
                        // Fix #9: take() consumes the entry on a validated hit and we
                        // immediately reinsert it back into the cache map (without marking
                        // it dirty). This avoids cloning the entry's Vec<String> fields on
                        // the hot path while preserving the "no spurious updates" contract.
                        let cache_hit = if use_cache {
                            if let Some(ref md) = metadata {
                                let mtime =
                                    md.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                                cache_shared.take(&rel_path_cow, mtime, md.len())
                            } else {
                                None
                            }
                        } else {
                            None
                        };

                        if let Some(entry) = cache_hit {
                            // Cache hit: reconstruct FileResult + modules without reading file.
                            let rel: Arc<str> = Arc::from(rel_path_cow.as_ref());
                            // Clone modules so we can both (a) insert into local_modules and
                            // (b) reinsert the original entry back into the cache map.
                            let entry_modules = entry.modules.clone();
                            for m in entry_modules {
                                flush_buf
                                    .local_modules
                                    .entry(m)
                                    .or_default()
                                    .insert(Arc::clone(&rel));
                            }
                            // Build FileResult by cloning small Vec<String>s; reinsert
                            // owned entry back into cache so durability is preserved.
                            let file_result = FileResult {
                                rel_path: rel.to_string(),
                                requires: entry.requires.clone(),
                                bundler_groups: entry.bundler_groups.clone(),
                                bundler_require_all: entry.bundler_require_all,
                            };
                            cache_shared.reinsert(rel.to_string(), entry);
                            flush_buf.buf.push(file_result);
                        } else {
                            // Cache miss: scan file

                            if let Some(file_result) =
                                scan_file(&path_str, &dir_ref, &mut flush_buf.local_modules)
                            {
                                // Collect new modules added by this file for cache
                                if use_cache {
                                    if let Some(ref md) = metadata {
                                        let mtime = md
                                            .modified()
                                            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                                        let rel_for_filter: Arc<str> =
                                            Arc::from(file_result.rel_path.as_str());
                                        let new_modules: Vec<String> = flush_buf
                                            .local_modules
                                            .iter()
                                            .filter(|(_, files)| files.contains(&rel_for_filter))
                                            .map(|(name, _)| name.clone())
                                            .collect();
                                        flush_buf.cache_buf.push((
                                            file_result.rel_path.clone(),
                                            mtime,
                                            md.len(),
                                            file_result.requires.clone(),
                                            new_modules,
                                            file_result.bundler_groups.clone(),
                                            file_result.bundler_require_all,
                                        ));
                                    }
                                }
                                flush_buf.buf.push(file_result);
                            }
                        }
                        if flush_buf.buf.len() >= 256 {
                            shared.lock().unwrap().append(&mut flush_buf.buf);
                        }
                    }

                    // Check if it's a script file (can be both — e.g. bin/setup is scannable + script)
                    if crate::scripts::is_script_file(&rel_path_cow, &file_name) {
                        if let Ok(content) = std::fs::read(path) {
                            flush_buf
                                .script_buf
                                .push((rel_path_cow.to_string(), content));
                            if flush_buf.script_buf.len() >= 128 {
                                scripts.lock().unwrap().append(&mut flush_buf.script_buf);
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

    // Merge thread-local module maps into global map
    let all_thread_modules = Arc::try_unwrap(thread_modules)
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
        for g in &r.bundler_groups {
            bundler_groups.insert(g.clone());
        }
        if r.bundler_require_all {
            bundler_require_all = true;
        }
    }

    // Merge per-thread module maps (each module name allocated once per thread, not per file).
    // Convert interned Arc<str> rel_paths back to String for the public API.
    for thread_map in all_thread_modules {
        for (module_name, files) in thread_map {
            modules
                .entry(module_name)
                .or_default()
                .extend(files.into_iter().map(|p| p.to_string()));
        }
    }

    if bundler_require_all {
        bundler_groups.insert("default".to_string());
    }

    // Collect cache updates (applied by caller)
    let all_cache_updates = Arc::try_unwrap(cache_updates)
        .map(|m| m.into_inner().unwrap_or_default())
        .unwrap_or_default();

    let all_script_files = Arc::try_unwrap(script_files)
        .map(|m| m.into_inner().unwrap_or_default())
        .unwrap_or_default();

    (
        ScanResults {
            requires,
            modules,
            bundler_groups,
            bundler_require_all,
            script_files: all_script_files,
        },
        all_cache_updates,
    )
}

pub fn default_skip_dirs() -> Vec<&'static str> {
    DEFAULT_SKIP_DIRS.to_vec()
}

fn is_scannable(_path: &str, full_path: &Path) -> bool {
    // Fast extension check using byte-level comparison (no allocations)
    if let Some(ext) = full_path.extension() {
        let ext_bytes = ext.as_encoded_bytes();
        let matches = match ext_bytes.len() {
            2 => {
                let lower: [u8; 2] = [ext_bytes[0] | 0x20, ext_bytes[1] | 0x20];
                matches!(&lower, b"rb" | b"ru")
            }
            3 => {
                let lower: [u8; 3] = [
                    ext_bytes[0] | 0x20,
                    ext_bytes[1] | 0x20,
                    ext_bytes[2] | 0x20,
                ];
                matches!(&lower, b"erb" | b"yml")
            }
            4 => {
                let lower: [u8; 4] = [
                    ext_bytes[0] | 0x20,
                    ext_bytes[1] | 0x20,
                    ext_bytes[2] | 0x20,
                    ext_bytes[3] | 0x20,
                ];
                matches!(
                    &lower,
                    b"rake" | b"haml" | b"slim" | b"rabl" | b"thor" | b"yaml"
                )
            }
            7 => {
                // Fix #11: stack array instead of heap Vec for the lowercased
                // 7-byte extension comparison.
                let mut lower = [0u8; 7];
                for (i, b) in ext_bytes.iter().enumerate() {
                    lower[i] = b | 0x20;
                }
                &lower == b"gemspec" || &lower == b"builder"
            }
            8 => {
                // Fix #11: stack array instead of heap Vec.
                let mut lower = [0u8; 8];
                for (i, b) in ext_bytes.iter().enumerate() {
                    lower[i] = b | 0x20;
                }
                &lower == b"jbuilder"
            }
            _ => false,
        };
        if matches {
            return true;
        }
    }

    if let Some(name) = full_path.file_name() {
        let name_bytes = name.as_encoded_bytes();
        if SCANNABLE_FILENAMES
            .iter()
            .any(|f| f.as_bytes() == name_bytes)
        {
            return true;
        }
        // Extensionless files with Ruby shebang
        if full_path.extension().is_none() && full_path.is_file() {
            if let Ok(mut f) = std::fs::File::open(full_path) {
                use std::io::Read;
                let mut buf = [0u8; 64];
                if let Ok(n) = f.read(&mut buf) {
                    if let Some(nl) = memchr::memchr(b'\n', &buf[..n]) {
                        if n >= 2 && buf[0] == b'#' && buf[1] == b'!' {
                            return buf[..nl].windows(4).any(|w| w == b"ruby");
                        }
                    } else if n >= 2 && buf[0] == b'#' && buf[1] == b'!' {
                        return buf[..n].windows(4).any(|w| w == b"ruby");
                    }
                }
            }
        }
    }

    false
}

// (FileContent enum removed — plain read() is faster for single sequential reads)

fn scan_file(
    path: &str,
    dir: &str,
    thread_modules: &mut HashMap<String, HashSet<Arc<str>>>,
) -> Option<FileResult> {
    // Plain read() is faster than mmap for single sequential reads (no VMA/TLB overhead)
    let bytes = std::fs::read(path).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let bytes = bytes.as_slice();

    let rel_path = Path::new(path)
        .strip_prefix(dir)
        .unwrap_or(Path::new(path))
        .display()
        .to_string();
    // Fix #10: intern rel_path as Arc<str> so each per-module insertion is a
    // cheap Arc::clone instead of a fresh String allocation.
    let rel_path_arc: Arc<str> = Arc::from(rel_path.as_str());

    let full_path = Path::new(path);
    let (is_ruby, is_yaml, is_slim, is_haml) = classify_file(full_path);

    // Fix #4: Slim line comments use `/`, Haml line comments use `-#`.
    // Other supported file types (Ruby, YAML, ERB, shell etc.) use `#`.
    let line_kind = if is_slim {
        LineCommentKind::Slash
    } else if is_haml {
        LineCommentKind::DashHash
    } else {
        LineCommentKind::Hash
    };

    // Fix #7: build a single unified skip mask covering line/inline/block
    // comments, heredoc bodies, and string-literal interiors. All filtered
    // scanners now do O(1) lookups against this mask, avoiding the previous
    // quadratic per-call memrchr/range-scan cost on heredoc-heavy files.
    let mask = build_skip_mask(bytes, is_ruby, line_kind);

    let mut requires = Vec::new();
    let mut bundler_groups = Vec::new();
    let mut bundler_require_all = false;

    // Hand-rolled byte scanners with mask-based skipping
    scan_requires_filtered(bytes, &mask, &mut requires);
    scan_autoloads_filtered(bytes, &mask, &mut requires);
    scan_gem_bin_filtered(bytes, &mask, &mut requires);
    // Fix #5: do NOT run the generic uppercase-token module scanner on YAML
    // files — YAML keys/values like `UserNotFound:` would otherwise be
    // extracted as module references. The explicit YAML regex extractors
    // below still pick up rubocop/plugins/inherit_gem/require: entries.
    if !is_yaml {
        scan_modules_filtered_into(bytes, &mask, thread_modules, &rel_path_arc);
        scan_middleware_refs(bytes, &mask, thread_modules, &rel_path_arc);
    }

    // YAML config patterns (keep regex — rarely hit, complex multiline patterns).
    // Fix #8: use regex::bytes::Regex to avoid the String::from_utf8_lossy
    // allocation; patterns are byte-compatible.
    if is_yaml
        && (memmem::find(bytes, b"rubocop").is_some()
            || memmem::find(bytes, b"require:").is_some()
            || memmem::find(bytes, b"plugins:").is_some()
            || memmem::find(bytes, b"inherit_gem:").is_some())
    {
        for cap in RUBOCOP_RE.captures_iter(bytes) {
            if let Some(m) = cap.get(1) {
                if let Ok(s) = std::str::from_utf8(m.as_bytes()) {
                    requires.push(s.to_string());
                }
            }
        }
        for cap in YAML_REQUIRE_BLOCK_RE.captures_iter(bytes) {
            if let Some(block) = cap.get(1) {
                for item_cap in YAML_ITEM_RE.captures_iter(block.as_bytes()) {
                    if let Some(item) = item_cap.get(1) {
                        if let Ok(s) = std::str::from_utf8(item.as_bytes()) {
                            requires.push(s.trim().to_string());
                        }
                    }
                }
            }
        }
        for cap in INHERIT_GEM_RE.captures_iter(bytes) {
            if let Some(block) = cap.get(1) {
                for key_cap in INHERIT_GEM_KEY_RE.captures_iter(block.as_bytes()) {
                    if let Some(key) = key_cap.get(1) {
                        if let Ok(s) = std::str::from_utf8(key.as_bytes()) {
                            requires.push(s.to_string());
                        }
                    }
                }
            }
        }
    }

    // Bundler.require detection
    scan_bundler_require_filtered(bytes, &mask, &mut bundler_groups, &mut bundler_require_all);

    Some(FileResult {
        rel_path,
        requires,
        bundler_groups,
        bundler_require_all,
    })
}

/// Classify a path into (is_ruby, is_yaml, is_slim, is_haml). Single pass.
fn classify_file(full_path: &Path) -> (bool, bool, bool, bool) {
    if let Some(ext) = full_path.extension() {
        let ext_bytes = ext.as_encoded_bytes();
        match ext_bytes.len() {
            2 => {
                let l = [ext_bytes[0] | 0x20, ext_bytes[1] | 0x20];
                let is_ruby = matches!(&l, b"rb" | b"ru");
                return (is_ruby, false, false, false);
            }
            3 => {
                let l = [
                    ext_bytes[0] | 0x20,
                    ext_bytes[1] | 0x20,
                    ext_bytes[2] | 0x20,
                ];
                let is_yaml = &l == b"yml";
                return (false, is_yaml, false, false);
            }
            4 => {
                let l = [
                    ext_bytes[0] | 0x20,
                    ext_bytes[1] | 0x20,
                    ext_bytes[2] | 0x20,
                    ext_bytes[3] | 0x20,
                ];
                let is_yaml = &l == b"yaml";
                let is_slim = &l == b"slim";
                let is_haml = &l == b"haml";
                let is_ruby = matches!(&l, b"rake" | b"rabl" | b"thor");
                return (is_ruby, is_yaml, is_slim, is_haml);
            }
            7 => {
                // gemspec | builder
                let mut l = [0u8; 7];
                for (i, b) in ext_bytes.iter().enumerate() {
                    l[i] = b | 0x20;
                }
                let is_ruby = &l == b"gemspec" || &l == b"builder";
                return (is_ruby, false, false, false);
            }
            8 => {
                // jbuilder
                let mut l = [0u8; 8];
                for (i, b) in ext_bytes.iter().enumerate() {
                    l[i] = b | 0x20;
                }
                let is_ruby = &l == b"jbuilder";
                return (is_ruby, false, false, false);
            }
            _ => {}
        }
    }

    let name_bytes = full_path
        .file_name()
        .map(|n| n.as_encoded_bytes())
        .unwrap_or(b"");
    let is_ruby = SCANNABLE_FILENAMES
        .iter()
        .any(|f| f.as_bytes() == name_bytes);
    (is_ruby, false, false, false)
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

    // ── Inline comment filtering ──

    #[test]
    fn inline_comment_require_is_filtered() {
        let input = b"x = 1 # require 'unused_gem'\nrequire 'real_gem'\n";
        let _block_ranges = build_block_comment_ranges(input);
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            scan_requires_filtered(input, &__mask, &mut results);
        }
        assert_eq!(results, vec!["real_gem"]);
    }

    #[test]
    fn inline_comment_after_string_not_filtered() {
        // The # inside a string shouldn't trigger comment detection
        let input = b"x = 'has#hash'\nrequire 'real_gem'\n";
        let _block_ranges = build_block_comment_ranges(input);
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            scan_requires_filtered(input, &__mask, &mut results);
        }
        assert_eq!(results, vec!["real_gem"]);
    }

    #[test]
    fn inline_comment_with_code_before() {
        let input = b"configure do |c|\n  c.adapter = :memory # require 'redis' for production\nend\nrequire 'active_support'\n";
        let _block_ranges = build_block_comment_ranges(input);
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            scan_requires_filtered(input, &__mask, &mut results);
        }
        assert_eq!(results, vec!["active_support"]);
    }

    // ── Heredoc filtering ──

    #[test]
    fn heredoc_require_is_filtered() {
        let input = b"doc = <<~HELP\n  require 'fake_gem'\nHELP\nrequire 'real_gem'\n";
        let _block_ranges = build_block_comment_ranges(input);
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            scan_requires_filtered(input, &__mask, &mut results);
        }
        assert_eq!(results, vec!["real_gem"]);
    }

    #[test]
    fn percent_q_require_is_filtered() {
        let input = b"message = %q{\n  require 'fake_gem'\n}\nrequire 'real_gem'\n";
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            scan_requires_filtered(input, &__mask, &mut results);
        }
        assert_eq!(results, vec!["real_gem"]);
    }

    #[test]
    fn percent_upper_q_require_is_filtered_with_nested_braces() {
        let input = b"message = %Q{before { require 'fake_gem' } after}\nrequire 'real_gem'\n";
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            scan_requires_filtered(input, &__mask, &mut results);
        }
        assert_eq!(results, vec!["real_gem"]);
    }

    #[test]
    fn percent_q_in_comment_does_not_mask_later_code() {
        let input = b"# %q(unclosed\nrequire 'real_gem'\n";
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut results = Vec::new();
        scan_requires_filtered(input, &mask, &mut results);
        assert_eq!(results, vec!["real_gem"]);
    }

    #[test]
    fn percent_q_in_heredoc_does_not_mask_later_code() {
        let input = b"doc = <<~HELP\n  %q(unclosed\nHELP\nrequire 'real_gem'\n";
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut results = Vec::new();
        scan_requires_filtered(input, &mask, &mut results);
        assert_eq!(results, vec!["real_gem"]);
    }

    #[test]
    fn heredoc_with_dash_syntax_is_filtered() {
        let input = b"doc = <<-HEREDOC\n  require 'fake_gem'\nHEREDOC\nrequire 'real_gem'\n";
        let _block_ranges = build_block_comment_ranges(input);
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            scan_requires_filtered(input, &__mask, &mut results);
        }
        assert_eq!(results, vec!["real_gem"]);
    }

    #[test]
    fn heredoc_with_quotes_is_filtered() {
        let input = b"doc = <<~'EOF'\n  require 'fake_gem'\nEOF\nrequire 'real_gem'\n";
        let _block_ranges = build_block_comment_ranges(input);
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            scan_requires_filtered(input, &__mask, &mut results);
        }
        assert_eq!(results, vec!["real_gem"]);
    }

    #[test]
    fn heredoc_modules_are_filtered() {
        let input = b"doc = <<~HELP\n  Use MyGem::Config to configure\nHELP\nMyRealModule.call\n";
        let _block_ranges = build_block_comment_ranges(input);
        let mut modules: HashMap<String, HashSet<Arc<str>>> = HashMap::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            {
                let __rp: Arc<str> = Arc::from("test.rb");
                scan_modules_filtered_into(input, &__mask, &mut modules, &__rp)
            };
        }
        assert!(
            !modules.contains_key("MyGem"),
            "MyGem inside heredoc should be filtered"
        );
        assert!(
            modules.contains_key("MyRealModule"),
            "MyRealModule outside heredoc should be found"
        );
    }

    // ── Non-Ruby comment filtering ──

    #[test]
    fn yaml_comment_require_is_filtered() {
        let input = b"# require 'old_gem'\nkey: value\n";
        let _block_ranges: Vec<(usize, usize)> = Vec::new();
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, false, LineCommentKind::Hash);
            scan_requires_filtered(input, &__mask, &mut results);
        }
        assert!(
            results.is_empty(),
            "require in YAML comment should be filtered"
        );
    }

    #[test]
    fn yaml_non_comment_require_is_kept() {
        let input = b"key: value\nrequire 'real_gem'\n";
        let _block_ranges: Vec<(usize, usize)> = Vec::new();
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, false, LineCommentKind::Hash);
            scan_requires_filtered(input, &__mask, &mut results);
        }
        assert_eq!(results, vec!["real_gem"]);
    }

    #[test]
    fn non_ruby_module_comment_is_filtered() {
        let input = b"# MyOldModule was removed\nMyNewModule:\n  key: value\n";
        let _block_ranges: Vec<(usize, usize)> = Vec::new();
        let mut modules: HashMap<String, HashSet<Arc<str>>> = HashMap::new();
        {
            let __mask = build_skip_mask(input, false, LineCommentKind::Hash);
            {
                let __rp: Arc<str> = Arc::from("test.yml");
                scan_modules_filtered_into(input, &__mask, &mut modules, &__rp)
            };
        }
        assert!(
            !modules.contains_key("MyOldModule"),
            "module in comment should be filtered"
        );
        assert!(
            modules.contains_key("MyNewModule"),
            "module outside comment should be found"
        );
    }

    // ── is_scannable ──

    #[test]
    fn scannable_ruby_extensions() {
        for ext in &[
            ".rb", ".rake", ".gemspec", ".ru", ".erb", ".haml", ".slim", ".yml", ".yaml",
        ] {
            let path = format!("/tmp/test{}", ext);
            let _p = Path::new(&path);
            // Create a temp file to test
            assert!(
                SCANNABLE_EXTENSIONS.contains(ext),
                "{} should be scannable",
                ext
            );
        }
    }

    #[test]
    fn scannable_special_filenames() {
        for name in &["Rakefile", "Guardfile", "Capfile", "Procfile"] {
            assert!(
                SCANNABLE_FILENAMES.contains(name),
                "{} should be scannable",
                name
            );
        }
    }

    // ── build_block_comment_ranges ──

    #[test]
    fn block_comment_at_file_start() {
        let input = b"=begin\nsome comment\n=end\nrequire 'real'\n";
        let ranges = build_block_comment_ranges(input);
        assert!(!ranges.is_empty());
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            scan_requires_filtered(input, &__mask, &mut results);
        }
        assert_eq!(results, vec!["real"]);
    }

    #[test]
    fn block_comment_begin_no_end() {
        let input = b"=begin\ncommented out forever\nrequire 'nope'\n";
        let ranges = build_block_comment_ranges(input);
        assert_eq!(ranges, vec![(0, input.len())]);
    }

    #[test]
    fn block_comment_mid_file() {
        let input = b"require 'before'\n=begin\nrequire 'hidden'\n=end\nrequire 'after'\n";
        let ranges = build_block_comment_ranges(input);
        assert!(!ranges.is_empty());
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            scan_requires_filtered(input, &__mask, &mut results);
        }
        assert_eq!(results, vec!["before", "after"]);
    }

    #[test]
    fn block_comment_begin_not_full_word() {
        // =beginning should not be treated as =begin
        let input = b"\n=beginning_of_doc\nrequire 'real'\n";
        let _ranges = build_block_comment_ranges(input);
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            scan_requires_filtered(input, &__mask, &mut results);
        }
        assert_eq!(results, vec!["real"]);
    }

    // ── has_inline_comment_before ──

    #[test]
    fn inline_comment_after_escaped_quote() {
        // The \' inside a string should not confuse quote tracking
        let input = b"x = 'it\\'s' # require 'fake'\nrequire 'real'\n";
        let _ranges = build_block_comment_ranges(input);
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            scan_requires_filtered(input, &__mask, &mut results);
        }
        assert_eq!(results, vec!["real"]);
    }

    #[test]
    fn inline_comment_hash_in_double_quoted_string() {
        // "has#hash" should not trigger comment detection
        let input = b"x = \"has#hash\"\nrequire 'real'\n";
        let _ranges = build_block_comment_ranges(input);
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            scan_requires_filtered(input, &__mask, &mut results);
        }
        assert_eq!(results, vec!["real"]);
    }

    // ── scan_file via temp files ──

    #[test]
    fn scan_file_ruby() {
        let dir = tempfile::tempdir().unwrap();
        let rb = dir.path().join("app.rb");
        std::fs::write(&rb, "require 'foo'\n# require 'bar'\n").unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();
        let mut modules = HashMap::new();
        let result = scan_file(rb.to_str().unwrap(), &dir_str, &mut modules);
        assert!(result.is_some());
        let r = result.unwrap();
        assert_eq!(r.requires, vec!["foo"]);
    }

    #[test]
    fn scan_file_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let yml = dir.path().join("rubocop.yml");
        std::fs::write(&yml, "require:\n  - rubocop-rails\n  - rubocop-rspec\n").unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();
        let mut modules = HashMap::new();
        let result = scan_file(yml.to_str().unwrap(), &dir_str, &mut modules);
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(r.requires.contains(&"rubocop-rails".to_string()));
        assert!(r.requires.contains(&"rubocop-rspec".to_string()));
    }

    #[test]
    fn scan_file_yaml_rubocop_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let yml = dir.path().join("rubocop.yml");
        std::fs::write(&yml, "plugins:\n  - rubocop-performance\n").unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();
        let mut modules = HashMap::new();
        let result = scan_file(yml.to_str().unwrap(), &dir_str, &mut modules);
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(r.requires.contains(&"rubocop-performance".to_string()));
    }

    #[test]
    fn scan_file_yaml_inherit_gem() {
        let dir = tempfile::tempdir().unwrap();
        let yml = dir.path().join("rubocop.yml");
        std::fs::write(
            &yml,
            "inherit_gem:\n  rubocop-github:\n    - config/default.yml\n",
        )
        .unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();
        let mut modules = HashMap::new();
        let result = scan_file(yml.to_str().unwrap(), &dir_str, &mut modules);
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(r.requires.contains(&"rubocop-github".to_string()));
    }

    #[test]
    fn scan_file_yaml_rubocop_extension() {
        let dir = tempfile::tempdir().unwrap();
        let yml = dir.path().join("rubocop.yml");
        std::fs::write(
            &yml,
            "AllCops:\n  EnabledByDefault: false\n\n- rubocop-minitest\n",
        )
        .unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();
        let mut modules = HashMap::new();
        let result = scan_file(yml.to_str().unwrap(), &dir_str, &mut modules);
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(r.requires.contains(&"rubocop-minitest".to_string()));
    }

    #[test]
    fn scan_file_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let rb = dir.path().join("empty.rb");
        std::fs::write(&rb, "").unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();
        let mut modules = HashMap::new();
        let result = scan_file(rb.to_str().unwrap(), &dir_str, &mut modules);
        assert!(result.is_none()); // empty files are skipped
    }

    #[test]
    fn scan_file_bundler_require() {
        let dir = tempfile::tempdir().unwrap();
        let rb = dir.path().join("config.rb");
        std::fs::write(&rb, "Bundler.require(*Rails.groups)\n").unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();
        let mut modules = HashMap::new();
        let result = scan_file(rb.to_str().unwrap(), &dir_str, &mut modules);
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(r.bundler_require_all);
    }

    #[test]
    fn scan_file_bundler_unknown_splat_only_loads_default() {
        let dir = tempfile::tempdir().unwrap();
        let rb = dir.path().join("config.rb");
        std::fs::write(
            &rb,
            "bundler_groups = [:default]\nBundler.require(*bundler_groups)\n",
        )
        .unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();
        let mut modules = HashMap::new();
        let result = scan_file(rb.to_str().unwrap(), &dir_str, &mut modules);
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(!r.bundler_require_all);
        assert_eq!(r.bundler_groups, vec!["default"]);
    }

    #[test]
    fn scan_file_bundler_splat_var_uses_explicit_rails_groups() {
        let dir = tempfile::tempdir().unwrap();
        let rb = dir.path().join("config.rb");
        std::fs::write(
            &rb,
            "bundler_groups = [:default]\n\
             bundler_groups = bundler_groups.concat(Rails.groups(assets: %w[development test profile]))\n\
             Bundler.require(*bundler_groups)\n",
        )
        .unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();
        let mut modules = HashMap::new();
        let result = scan_file(rb.to_str().unwrap(), &dir_str, &mut modules);
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(!r.bundler_require_all);
        assert!(r.bundler_groups.contains(&"default".to_string()));
        assert!(r.bundler_groups.contains(&"development".to_string()));
        assert!(r.bundler_groups.contains(&"test".to_string()));
        assert!(r.bundler_groups.contains(&"profile".to_string()));
        assert!(!r.bundler_groups.contains(&"migrations".to_string()));
    }

    #[test]
    fn scan_file_bundler_splat_var_aliasing_rails_groups_loads_all() {
        let dir = tempfile::tempdir().unwrap();
        let rb = dir.path().join("config.rb");
        std::fs::write(&rb, "groups = Rails.groups\nBundler.require(*groups)\n").unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();
        let mut modules = HashMap::new();
        let result = scan_file(rb.to_str().unwrap(), &dir_str, &mut modules);
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(r.bundler_require_all);
    }

    #[test]
    fn scan_file_empty_bundler_require_only_loads_default() {
        let dir = tempfile::tempdir().unwrap();
        let rb = dir.path().join("config.rb");
        std::fs::write(&rb, "Bundler.require\n").unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();
        let mut modules = HashMap::new();
        let result = scan_file(rb.to_str().unwrap(), &dir_str, &mut modules);
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(!r.bundler_require_all);
        assert_eq!(r.bundler_groups, vec!["default"]);
    }

    #[test]
    fn scan_file_modules_detected() {
        let dir = tempfile::tempdir().unwrap();
        let rb = dir.path().join("app.rb");
        std::fs::write(&rb, "class MyApp < Rails::Application\nend\n").unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();
        let mut modules = HashMap::new();
        let result = scan_file(rb.to_str().unwrap(), &dir_str, &mut modules);
        assert!(result.is_some());
        assert!(modules.contains_key("MyApp"));
        assert!(modules.contains_key("Rails::Application"));
    }

    #[test]
    fn scan_file_autoload_detected() {
        let dir = tempfile::tempdir().unwrap();
        let rb = dir.path().join("app.rb");
        std::fs::write(&rb, "autoload :Foo, 'foo/bar'\n").unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();
        let mut modules = HashMap::new();
        let result = scan_file(rb.to_str().unwrap(), &dir_str, &mut modules);
        assert!(result.is_some());
        assert!(result.unwrap().requires.contains(&"foo/bar".to_string()));
    }

    #[test]
    fn scan_file_gem_bin_detected() {
        let dir = tempfile::tempdir().unwrap();
        let rb = dir.path().join("binstub.rb");
        std::fs::write(&rb, "Gem.bin_path('rake', 'rake')\n").unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();
        let mut modules = HashMap::new();
        let result = scan_file(rb.to_str().unwrap(), &dir_str, &mut modules);
        assert!(result.is_some());
        assert!(result.unwrap().requires.contains(&"rake".to_string()));
    }

    #[test]
    fn is_scannable_rb() {
        let dir = tempfile::tempdir().unwrap();
        let rb = dir.path().join("test.rb");
        std::fs::write(&rb, "x = 1\n").unwrap();
        assert!(is_scannable(rb.to_str().unwrap(), &rb));
    }

    #[test]
    fn is_scannable_yml() {
        let dir = tempfile::tempdir().unwrap();
        let yml = dir.path().join("test.yml");
        std::fs::write(&yml, "key: val\n").unwrap();
        assert!(is_scannable(yml.to_str().unwrap(), &yml));
    }

    #[test]
    fn is_scannable_rakefile() {
        let dir = tempfile::tempdir().unwrap();
        let rf = dir.path().join("Rakefile");
        std::fs::write(&rf, "task :default\n").unwrap();
        assert!(is_scannable(rf.to_str().unwrap(), &rf));
    }

    #[test]
    fn is_scannable_ruby_shebang() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("my_script");
        std::fs::write(&script, "#!/usr/bin/env ruby\nputs 'hello'\n").unwrap();
        assert!(is_scannable(script.to_str().unwrap(), &script));
    }

    #[test]
    fn is_scannable_non_ruby_shebang() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("my_script");
        std::fs::write(&script, "#!/bin/bash\necho hello\n").unwrap();
        assert!(!is_scannable(script.to_str().unwrap(), &script));
    }

    #[test]
    fn is_scannable_rejects_txt() {
        let dir = tempfile::tempdir().unwrap();
        let txt = dir.path().join("test.txt");
        std::fs::write(&txt, "hello\n").unwrap();
        assert!(!is_scannable(txt.to_str().unwrap(), &txt));
    }

    // ── scan_source_tree integration ──

    #[test]
    fn scan_source_tree_basic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.rb"), "require 'foo'\nFoo.new\n").unwrap();
        std::fs::write(dir.path().join("test.txt"), "not scanned\n").unwrap();
        let cache = std::sync::Arc::new(crate::cache::FileCache::disabled());
        let (results, _updates) = scan_source_tree(dir.path(), &[], cache);
        assert!(results.requires.contains_key("foo"));
        assert!(results.modules.contains_key("Foo"));
    }

    #[test]
    fn scan_source_tree_skips_vendor() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("vendor")).unwrap();
        std::fs::write(dir.path().join("vendor/gem.rb"), "require 'hidden'\n").unwrap();
        std::fs::write(dir.path().join("app.rb"), "require 'visible'\n").unwrap();
        let cache = std::sync::Arc::new(crate::cache::FileCache::disabled());
        let (results, _) = scan_source_tree(dir.path(), &default_skip_dirs(), cache);
        assert!(results.requires.contains_key("visible"));
        assert!(!results.requires.contains_key("hidden"));
    }

    #[test]
    fn scan_source_tree_skips_deep_vendor() {
        let dir = tempfile::tempdir().unwrap();
        let vendor = dir.path().join("engines/foo/vendor");
        std::fs::create_dir_all(&vendor).unwrap();
        std::fs::write(vendor.join("gem.rb"), "require 'hidden'\n").unwrap();
        std::fs::write(dir.path().join("app.rb"), "require 'visible'\n").unwrap();
        let cache = std::sync::Arc::new(crate::cache::FileCache::disabled());
        let (results, _) = scan_source_tree(dir.path(), &default_skip_dirs(), cache);
        assert!(results.requires.contains_key("visible"));
        assert!(!results.requires.contains_key("hidden"));
    }

    #[test]
    fn scan_source_tree_collects_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir(&bin_dir).unwrap();
        std::fs::write(bin_dir.join("setup"), "#!/bin/bash\nbundle exec rails\n").unwrap();
        let cache = std::sync::Arc::new(crate::cache::FileCache::disabled());
        let (results, _) = scan_source_tree(dir.path(), &[], cache);
        assert!(!results.script_files.is_empty());
    }

    #[test]
    fn scan_source_tree_with_cache() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.rb"), "require 'cached_gem'\n").unwrap();

        // First scan: populates cache
        let cache = std::sync::Arc::new(crate::cache::FileCache::load(dir.path()));
        let (results, updates) = scan_source_tree(dir.path(), &[], cache.clone());
        assert!(results.requires.contains_key("cached_gem"));
        assert!(!updates.is_empty());

        // Apply cache updates
        let mut cache_mut = crate::cache::FileCache::load(dir.path());
        for (rp, mt, sz, reqs, mods, bg, ba) in updates {
            cache_mut.put(rp, mt, sz, &reqs, mods, &bg, ba);
        }
        cache_mut.save();

        // Second scan: should use cache
        let cache2 = std::sync::Arc::new(crate::cache::FileCache::load(dir.path()));
        let (results2, updates2) = scan_source_tree(dir.path(), &[], cache2);
        assert!(results2.requires.contains_key("cached_gem"));
        assert!(updates2.is_empty()); // all cache hits
    }

    // ── Filtered scanner edge cases ──

    #[test]
    fn non_ruby_autoload_comment_filtered() {
        let input = b"# autoload :Foo, 'foo'\nautoload :Bar, 'bar'\n";
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, false, LineCommentKind::Hash);
            scan_autoloads_filtered(input, &__mask, &mut results);
        }
        assert_eq!(results, vec!["bar"]);
    }

    #[test]
    fn non_ruby_gem_bin_comment_filtered() {
        let input = b"# Gem.bin_path('old', 'old')\nGem.bin_path('real', 'real')\n";
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, false, LineCommentKind::Hash);
            scan_gem_bin_filtered(input, &__mask, &mut results);
        }
        assert_eq!(results, vec!["real"]);
    }

    #[test]
    fn non_ruby_bundler_require_comment_filtered() {
        let input = b"# Bundler.require\nBundler.require(:default)\n";
        let mut groups = Vec::new();
        let mut require_all = false;
        {
            let __mask = build_skip_mask(input, false, LineCommentKind::Hash);
            scan_bundler_require_filtered(input, &__mask, &mut groups, &mut require_all);
        }
        assert!(!require_all);
        assert_eq!(groups, vec!["default"]);
    }

    // ── default_skip_dirs ──

    #[test]
    fn default_skip_dirs_contains_vendor() {
        let dirs = default_skip_dirs();
        assert!(dirs.contains(&"vendor"));
        assert!(dirs.contains(&".git"));
        assert!(dirs.contains(&"node_modules"));
    }

    // ── scan_autoloads (unfiltered) ──

    #[test]
    fn autoload_basic() {
        let mut results = Vec::new();
        scan_autoloads(b"autoload :Foo, 'foo/bar'\n", &mut results);
        assert_eq!(results, vec!["foo/bar"]);
    }

    #[test]
    fn autoload_with_parens() {
        let mut results = Vec::new();
        scan_autoloads(b"autoload(:Bar, \"baz\")\n", &mut results);
        assert_eq!(results, vec!["baz"]);
    }

    #[test]
    fn autoload_not_word_boundary() {
        let mut results = Vec::new();
        scan_autoloads(b"xautoload :Foo, 'bad'\n", &mut results);
        assert!(results.is_empty());
    }

    #[test]
    fn autoload_trailing_word_char() {
        let mut results = Vec::new();
        scan_autoloads(b"autoloader :Foo, 'bad'\n", &mut results);
        assert!(results.is_empty());
    }

    #[test]
    fn autoload_missing_colon() {
        let mut results = Vec::new();
        scan_autoloads(b"autoload Foo, 'bad'\n", &mut results);
        assert!(results.is_empty());
    }

    #[test]
    fn autoload_empty_symbol() {
        let mut results = Vec::new();
        scan_autoloads(b"autoload :, 'bad'\n", &mut results);
        assert!(results.is_empty());
    }

    #[test]
    fn autoload_missing_comma() {
        let mut results = Vec::new();
        scan_autoloads(b"autoload :Foo 'bad'\n", &mut results);
        assert!(results.is_empty());
    }

    // ── scan_gem_bin (unfiltered) ──

    #[test]
    fn gem_bin_path_basic() {
        let mut results = Vec::new();
        scan_gem_bin(b"Gem.bin_path('rails')\n", &mut results);
        assert_eq!(results, vec!["rails"]);
    }

    #[test]
    fn gem_activate_bin_path() {
        let mut results = Vec::new();
        scan_gem_bin(b"Gem.activate_bin_path('rake')\n", &mut results);
        assert_eq!(results, vec!["rake"]);
    }

    #[test]
    fn gem_bin_not_a_match() {
        let mut results = Vec::new();
        scan_gem_bin(b"Gem.other_method('foo')\n", &mut results);
        assert!(results.is_empty());
    }

    #[test]
    fn gem_bin_missing_paren() {
        let mut results = Vec::new();
        scan_gem_bin(b"Gem.bin_path 'foo'\n", &mut results);
        assert!(results.is_empty());
    }

    // ── scan_modules (unfiltered) ──

    #[test]
    fn modules_basic() {
        let mut results = Vec::new();
        scan_modules(b"class Rails\nend\n", &mut results);
        assert!(results.contains(&"Rails".to_string()));
    }

    #[test]
    fn modules_namespaced() {
        let mut results = Vec::new();
        scan_modules(b"module ActiveRecord::Base\n", &mut results);
        assert!(results.contains(&"ActiveRecord::Base".to_string()));
    }

    #[test]
    fn modules_single_uppercase_skipped() {
        let mut results = Vec::new();
        scan_modules(b"X = 1\n", &mut results);
        assert!(results.is_empty());
    }

    #[test]
    fn modules_word_boundary_left() {
        let mut results = Vec::new();
        scan_modules(b"xFoo\n", &mut results);
        assert!(results.is_empty());
    }

    #[test]
    fn modules_backtracking() {
        let mut results = Vec::new();
        scan_modules(b"Foo::Bar_baz\n", &mut results);
        // Bar_baz has word char after Bar — should backtrack to Foo
        assert!(results.contains(&"Foo".to_string()));
    }

    #[test]
    fn modules_double_colon_single_upper() {
        let mut results = Vec::new();
        scan_modules(b"Foo::B\n", &mut results);
        // B is single char — doesn't match [A-Z][a-zA-Z0-9]+, should backtrack to Foo
        assert!(results.contains(&"Foo".to_string()));
    }

    // ── scan_bundler_require (unfiltered) ──

    #[test]
    fn bundler_require_no_args() {
        let mut groups = Vec::new();
        let mut require_all = false;
        scan_bundler_require(b"Bundler.require\n", &mut groups, &mut require_all);
        assert!(!require_all);
        assert_eq!(groups, vec!["default"]);
    }

    #[test]
    fn bundler_require_with_groups() {
        let mut groups = Vec::new();
        let mut require_all = false;
        scan_bundler_require(
            b"Bundler.require(:default, :development)\n",
            &mut groups,
            &mut require_all,
        );
        assert!(!require_all);
        assert!(groups.contains(&"default".to_string()));
        assert!(groups.contains(&"development".to_string()));
    }

    #[test]
    fn bundler_require_rails_groups() {
        let mut groups = Vec::new();
        let mut require_all = false;
        scan_bundler_require(
            b"Bundler.require(*Rails.groups)\n",
            &mut groups,
            &mut require_all,
        );
        assert!(require_all);
    }

    #[test]
    fn bundler_require_rails_env() {
        let mut groups = Vec::new();
        let mut require_all = false;
        scan_bundler_require(
            b"Bundler.require(:default, Rails.env)\n",
            &mut groups,
            &mut require_all,
        );
        assert!(require_all);
    }

    #[test]
    fn bundler_require_with_comment() {
        let mut groups = Vec::new();
        let mut require_all = false;
        scan_bundler_require(
            b"Bundler.require # load all\n",
            &mut groups,
            &mut require_all,
        );
        assert!(!require_all);
        assert_eq!(groups, vec!["default"]);
    }

    #[test]
    fn bundler_require_word_boundary() {
        let mut groups = Vec::new();
        let mut require_all = false;
        scan_bundler_require(b"Bundler.require_all_gems\n", &mut groups, &mut require_all);
        assert!(!require_all);
        assert!(groups.is_empty());
    }

    // ── is_scannable ──

    #[test]
    fn is_scannable_various_extensions() {
        let dir = tempfile::tempdir().unwrap();

        for ext in &[
            "rb", "ru", "erb", "yml", "rake", "haml", "slim", "rabl", "thor", "yaml", "gemspec",
            "builder", "jbuilder",
        ] {
            let p = dir.path().join(format!("test.{}", ext));
            std::fs::write(&p, "# content").unwrap();
            assert!(
                is_scannable(p.to_str().unwrap(), &p),
                "Expected .{} to be scannable",
                ext
            );
        }

        // Non-scannable extensions
        let p = dir.path().join("test.txt");
        std::fs::write(&p, "# content").unwrap();
        assert!(!is_scannable(p.to_str().unwrap(), &p));

        let p = dir.path().join("test.js");
        std::fs::write(&p, "// content").unwrap();
        assert!(!is_scannable(p.to_str().unwrap(), &p));
    }

    #[test]
    fn is_scannable_known_filenames() {
        let dir = tempfile::tempdir().unwrap();
        for name in &[
            "Rakefile",
            "Thorfile",
            "Guardfile",
            "Capfile",
            "Vagrantfile",
            "Berksfile",
            "Procfile",
        ] {
            let p = dir.path().join(name);
            std::fs::write(&p, "# content").unwrap();
            assert!(
                is_scannable(p.to_str().unwrap(), &p),
                "Expected {} to be scannable",
                name
            );
        }
    }

    #[test]
    fn is_scannable_ruby_shebang_env() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("my_script");
        std::fs::write(&p, "#!/usr/bin/env ruby\nputs 'hello'\n").unwrap();
        assert!(is_scannable(p.to_str().unwrap(), &p));
    }

    #[test]
    fn is_scannable_non_ruby_shebang_python() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("my_script");
        std::fs::write(&p, "#!/usr/bin/env python\nprint('hello')\n").unwrap();
        assert!(!is_scannable(p.to_str().unwrap(), &p));
    }

    #[test]
    fn is_scannable_shebang_no_newline() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("short_script");
        std::fs::write(&p, "#!/usr/bin/ruby").unwrap();
        assert!(is_scannable(p.to_str().unwrap(), &p));
    }

    // ── scan_file for various extensions ──

    #[test]
    fn scan_file_rake() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("tasks.rake");
        std::fs::write(&f, "require 'rails'\n").unwrap();
        let mut modules = HashMap::new();
        let result = scan_file(
            f.to_str().unwrap(),
            dir.path().to_str().unwrap(),
            &mut modules,
        );
        assert!(result.is_some());
        assert!(result.unwrap().requires.contains(&"rails".to_string()));
    }

    #[test]
    fn scan_file_haml() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("view.haml");
        std::fs::write(&f, "= require 'haml_helper'\n").unwrap();
        let mut modules = HashMap::new();
        let result = scan_file(
            f.to_str().unwrap(),
            dir.path().to_str().unwrap(),
            &mut modules,
        );
        assert!(result.is_some());
    }

    #[test]
    fn scan_file_gemspec() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("test.gemspec");
        std::fs::write(&f, "require 'mygem/version'\n").unwrap();
        let mut modules = HashMap::new();
        let result = scan_file(
            f.to_str().unwrap(),
            dir.path().to_str().unwrap(),
            &mut modules,
        );
        assert!(result.is_some());
        assert!(result
            .unwrap()
            .requires
            .contains(&"mygem/version".to_string()));
    }

    #[test]
    fn scan_file_builder() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("test.builder");
        std::fs::write(&f, "require 'builder_lib'\n").unwrap();
        let mut modules = HashMap::new();
        let result = scan_file(
            f.to_str().unwrap(),
            dir.path().to_str().unwrap(),
            &mut modules,
        );
        assert!(result.is_some());
    }

    #[test]
    fn scan_file_jbuilder() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("show.json.jbuilder");
        std::fs::write(&f, "json.name @user.name\n").unwrap();
        let mut modules = HashMap::new();
        let result = scan_file(
            f.to_str().unwrap(),
            dir.path().to_str().unwrap(),
            &mut modules,
        );
        assert!(result.is_some());
    }

    #[test]
    fn scan_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("empty.rb");
        std::fs::write(&f, "").unwrap();
        let mut modules = HashMap::new();
        let result = scan_file(
            f.to_str().unwrap(),
            dir.path().to_str().unwrap(),
            &mut modules,
        );
        assert!(result.is_none());
    }

    #[test]
    fn scan_file_extensionless_gemfile() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("Rakefile");
        std::fs::write(&f, "require 'bundler'\n").unwrap();
        let mut modules = HashMap::new();
        let result = scan_file(
            f.to_str().unwrap(),
            dir.path().to_str().unwrap(),
            &mut modules,
        );
        assert!(result.is_some());
    }

    // ── heredoc edge cases ──

    #[test]
    fn heredoc_without_closing_delimiter() {
        let input = b"x = <<~HEREDOC\nsome content\nmore content\n";
        let ranges = build_block_comment_ranges(input);
        // Unclosed heredoc — skipped, no range added
        assert!(ranges.is_empty());
    }

    #[test]
    fn heredoc_with_short_delimiter() {
        let input = b"x = <<A\ncontent\nA\n";
        let ranges = build_block_comment_ranges(input);
        // Single-char delimiter is skipped (needs >=2 chars)
        assert!(ranges.is_empty());
    }

    #[test]
    fn heredoc_with_unmatched_quote() {
        let input = b"x = <<~'HEREDOC\ncontent\nHEREDOC\n";
        let ranges = build_block_comment_ranges(input);
        // Unmatched quote — should not produce a range
        assert!(ranges.is_empty());
    }

    #[test]
    fn heredoc_empty_delimiter() {
        let input = b"x = <<\ncontent\n";
        let ranges = build_block_comment_ranges(input);
        assert!(ranges.is_empty());
    }

    #[test]
    fn heredoc_double_less_than_operator() {
        // << used as operator, not heredoc — delimiter too short (single char)
        let input = b"y = x << 5\n";
        let ranges = build_block_comment_ranges(input);
        assert!(ranges.is_empty());
    }

    #[test]
    fn begin_end_mid_file_no_closing() {
        // Covers lines 159-160: =begin mid-file with no =end
        let input = b"require 'before'\n=begin\ncommented out forever\n";
        let ranges = build_block_comment_ranges(input);
        assert!(!ranges.is_empty());
        assert_eq!(ranges[0].1, input.len());
    }

    // ── filtered scanner edge cases ──

    #[test]
    fn non_ruby_autoload_comment_filtered_with_hash() {
        let input = b"# autoload :Foo, 'foo'\nautoload :Bar, 'bar'\n";
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, false, LineCommentKind::Hash);
            scan_autoloads_filtered(input, &__mask, &mut results);
        }
        assert_eq!(results, vec!["bar"]);
    }

    #[test]
    fn non_ruby_modules_filtered_comment_skipped() {
        let input = b"# MyModule\nRealModule\n";
        let mut modules = HashMap::new();
        {
            let __mask = build_skip_mask(input, false, LineCommentKind::Hash);
            {
                let __rp: Arc<str> = Arc::from("test.py");
                scan_modules_filtered_into(input, &__mask, &mut modules, &__rp)
            };
        }
        assert!(modules.contains_key("RealModule"));
        assert!(!modules.contains_key("MyModule"));
    }

    #[test]
    fn ruby_modules_filtered_block_comment_skipped() {
        let input = b"=begin\nSkippedModule\n=end\nRealModule\n";
        let _ranges = build_block_comment_ranges(input);
        let mut modules = HashMap::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            {
                let __rp: Arc<str> = Arc::from("test.rb");
                scan_modules_filtered_into(input, &__mask, &mut modules, &__rp)
            };
        }
        assert!(modules.contains_key("RealModule"));
        assert!(!modules.contains_key("SkippedModule"));
    }

    #[test]
    fn ruby_gem_bin_filtered_block_comment() {
        let input = b"=begin\nGem.bin_path('skip')\n=end\nGem.bin_path('real')\n";
        let _ranges = build_block_comment_ranges(input);
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            scan_gem_bin_filtered(input, &__mask, &mut results);
        }
        assert_eq!(results, vec!["real"]);
    }

    #[test]
    fn ruby_autoload_filtered_block_comment() {
        let input = b"=begin\nautoload :Foo, 'skip'\n=end\nautoload :Bar, 'real'\n";
        let _ranges = build_block_comment_ranges(input);
        let mut results = Vec::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            scan_autoloads_filtered(input, &__mask, &mut results);
        }
        assert_eq!(results, vec!["real"]);
    }

    #[test]
    fn ruby_bundler_require_filtered_block_comment() {
        let input = b"=begin\nBundler.require\n=end\nBundler.require(:test)\n";
        let _ranges = build_block_comment_ranges(input);
        let mut groups = Vec::new();
        let mut require_all = false;
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            scan_bundler_require_filtered(input, &__mask, &mut groups, &mut require_all);
        }
        assert!(!require_all);
        assert_eq!(groups, vec!["test"]);
    }

    // ── Bundler.require splat forms ──

    #[test]
    fn bundler_require_splat_local_var() {
        let input = b"Bundler.require(*groups)\n";
        let mut groups = Vec::new();
        let mut require_all = false;
        scan_bundler_require(input, &mut groups, &mut require_all);
        assert!(
            !require_all,
            "splat local var should not synthesize all declared groups"
        );
        assert_eq!(groups, vec!["default"]);
    }

    #[test]
    fn bundler_require_splat_unparen() {
        let input = b"Bundler.require *Rails.groups\n";
        let mut groups = Vec::new();
        let mut require_all = false;
        scan_bundler_require(input, &mut groups, &mut require_all);
        assert!(require_all, "unparen splat should set bundler_require_all");
    }

    #[test]
    fn bundler_require_groups_still_work() {
        let input = b"Bundler.require(:default, :production)\n";
        let mut groups = Vec::new();
        let mut require_all = false;
        scan_bundler_require(input, &mut groups, &mut require_all);
        assert!(
            !require_all,
            "symbol groups should not set bundler_require_all"
        );
        assert_eq!(groups, vec!["default", "production"]);
    }

    #[test]
    fn bundler_require_rails_groups_literal() {
        let input = b"Bundler.require(*Rails.groups)\n";
        let mut groups = Vec::new();
        let mut require_all = false;
        scan_bundler_require(input, &mut groups, &mut require_all);
        assert!(
            require_all,
            "Rails.groups splat should set bundler_require_all"
        );
    }

    // ── Middleware string references ──

    #[test]
    fn middleware_use_string() {
        let input = b"config.middleware.use \"Bullet::Rack\"\n";
        let mut modules: HashMap<String, HashSet<Arc<str>>> = HashMap::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            {
                let __rp: Arc<str> = Arc::from("test.rb");
                scan_middleware_refs(input, &__mask, &mut modules, &__rp)
            };
        }
        assert!(
            modules.contains_key("Bullet::Rack"),
            "should detect double-quoted middleware"
        );
    }

    #[test]
    fn middleware_insert_before_string() {
        let input = b"config.middleware.insert_before Rack::Runtime, \"BatchLoader::Middleware\"\n";
        let mut modules: HashMap<String, HashSet<Arc<str>>> = HashMap::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            {
                let __rp: Arc<str> = Arc::from("test.rb");
                scan_middleware_refs(input, &__mask, &mut modules, &__rp)
            };
        }
        assert!(
            modules.contains_key("BatchLoader::Middleware"),
            "should detect string arg in insert_before"
        );
    }

    #[test]
    fn middleware_symbol_ignored() {
        let input = b"config.middleware.use MyClass\n";
        let mut modules: HashMap<String, HashSet<Arc<str>>> = HashMap::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            {
                let __rp: Arc<str> = Arc::from("test.rb");
                scan_middleware_refs(input, &__mask, &mut modules, &__rp)
            };
        }
        assert!(
            !modules.contains_key("MyClass"),
            "bare constants should not be captured by middleware scanner"
        );
    }

    #[test]
    fn middleware_commented() {
        let input = b"# config.middleware.use \"Bullet::Rack\"\n";
        let mut modules: HashMap<String, HashSet<Arc<str>>> = HashMap::new();
        {
            let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
            {
                let __rp: Arc<str> = Arc::from("test.rb");
                scan_middleware_refs(input, &__mask, &mut modules, &__rp)
            };
        }
        assert!(
            !modules.contains_key("Bullet::Rack"),
            "commented middleware should be ignored"
        );
    }

    #[test]
    fn middleware_stack_shovel_string() {
        let input = b"config.middleware_stack << \"Bullet::Rack\"\n";
        let mut modules: HashMap<String, HashSet<Arc<str>>> = HashMap::new();
        let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let __rp: Arc<str> = Arc::from("test.rb");
        scan_middleware_refs(input, &__mask, &mut modules, &__rp);
        assert!(modules.contains_key("Bullet::Rack"));
    }

    #[test]
    fn rack_builder_use_string() {
        let input = b"Rack::Builder.new { use \"BatchLoader::Middleware\" }\n";
        let mut modules: HashMap<String, HashSet<Arc<str>>> = HashMap::new();
        let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let __rp: Arc<str> = Arc::from("test.rb");
        scan_middleware_refs(input, &__mask, &mut modules, &__rp);
        assert!(modules.contains_key("BatchLoader::Middleware"));
    }

    // ── New tests for fixes #1–#6 ──

    #[test]
    fn fix1_bundler_require_empty_parens_loads_default() {
        // `Bundler.require()` (no args) auto-requires only the default group.
        let input = b"Bundler.require()\n";
        let mut groups: Vec<String> = Vec::new();
        let mut require_all = false;
        let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
        scan_bundler_require_filtered(input, &__mask, &mut groups, &mut require_all);
        assert!(!require_all);
        assert_eq!(groups, vec!["default"]);
    }

    #[test]
    fn fix1_bundler_require_empty_parens_with_space() {
        let input = b"Bundler.require( )\n";
        let mut groups: Vec<String> = Vec::new();
        let mut require_all = false;
        let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
        scan_bundler_require_filtered(input, &__mask, &mut groups, &mut require_all);
        assert!(!require_all);
        assert_eq!(groups, vec!["default"]);
    }

    #[test]
    fn fix1_bundler_require_no_args_no_parens() {
        let input = b"Bundler.require\n";
        let mut groups: Vec<String> = Vec::new();
        let mut require_all = false;
        let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
        scan_bundler_require_filtered(input, &__mask, &mut groups, &mut require_all);
        assert!(!require_all);
        assert_eq!(groups, vec!["default"]);
    }

    #[test]
    fn fix2_bundler_require_strips_inline_comment() {
        // Fix #2: trailing `# :test` comment must not become a captured group.
        let input = b"Bundler.require(:default) # :test removed\n";
        let mut groups: Vec<String> = Vec::new();
        let mut require_all = false;
        let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
        scan_bundler_require_filtered(input, &__mask, &mut groups, &mut require_all);
        assert!(groups.iter().any(|g| g == "default"));
        assert!(
            !groups.iter().any(|g| g == "test"),
            "inline-comment :test must not be captured"
        );
    }

    #[test]
    fn fix3_string_literal_interior_not_scanned_for_requires() {
        // Fix #3: gem names mentioned inside string literals must not be extracted.
        let input = b"raise \"Please require 'oauth2'\"\n";
        let mut results = Vec::new();
        let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
        scan_requires_filtered(input, &__mask, &mut results);
        assert!(
            !results.iter().any(|s| s == "oauth2"),
            "require keyword inside a string literal must not be extracted, got {:?}",
            results
        );
    }

    #[test]
    fn fix4_slim_slash_comment_is_filtered() {
        // Fix #4: Slim comment lines start with `/`.
        let input = b"/ require 'foo'\nrequire 'bar'\n";
        let mut results = Vec::new();
        let __mask = build_skip_mask(input, false, LineCommentKind::Slash);
        scan_requires_filtered(input, &__mask, &mut results);
        assert_eq!(results, vec!["bar"], "slim `/` line must be skipped");
    }

    #[test]
    fn fix4_haml_dash_hash_comment_is_filtered() {
        // Fix #4: Haml comment lines start with `-#`.
        let input = b"-# require 'foo'\nrequire 'bar'\n";
        let mut results = Vec::new();
        let __mask = build_skip_mask(input, false, LineCommentKind::DashHash);
        scan_requires_filtered(input, &__mask, &mut results);
        assert_eq!(results, vec!["bar"], "haml `-#` line must be skipped");
    }

    #[test]
    fn fix5_yaml_files_not_scanned_for_modules() {
        // Fix #5: scan_file must not extract Ruby module references from a YAML file.
        let dir = tempfile::tempdir().unwrap();
        let yml = dir.path().join("config.yml");
        // PascalCase tokens in YAML values must not be reported as module references.
        std::fs::write(
            &yml,
            "default: &default\n  adapter: postgresql\n  pool: SomeModule\n",
        )
        .unwrap();
        let cache = std::sync::Arc::new(crate::cache::FileCache::disabled());
        let (results, _) = scan_source_tree(dir.path(), &[], cache);
        assert!(
            !results.modules.contains_key("SomeModule"),
            "YAML file must not be scanned for module references"
        );
    }

    #[test]
    fn fix6_left_shift_operator_after_word_char_is_not_heredoc() {
        // Fix #6: `arr<<MY_CONST` (no space) is the `<<` operator on a Ruby Array,
        // not a heredoc. `MY_CONST` must NOT be misclassified as a heredoc body.
        let input = b"arr<<MY_CONST\nrequire 'after'\n";
        let mut results = Vec::new();
        let __mask = build_skip_mask(input, true, LineCommentKind::Hash);
        scan_requires_filtered(input, &__mask, &mut results);
        // The require after the supposed heredoc must still be picked up.
        assert_eq!(results, vec!["after"]);
    }
}
