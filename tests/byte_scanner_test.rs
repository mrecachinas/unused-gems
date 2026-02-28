//! Correctness audit: hand-rolled byte scanners vs. original regex patterns.
//!
//! For each scanner function we run both the original regex and the byte scanner
//! on crafted inputs and compare results.  Mismatches are categorised as:
//!   - FALSE NEGATIVE: regex matches, byte scanner misses
//!   - FALSE POSITIVE: byte scanner matches, regex wouldn't
//!   - EDGE CASE: same match set but different captured text

use memchr::memmem;
use regex::Regex;

// ── Copied helpers (identical to scanner.rs) ─────────────────────────────────

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
        return None;
    }
    Some((&bytes[start..end], end + 1))
}

// ── Copied scanner functions (identical to scanner.rs) ───────────────────────

fn scan_requires(bytes: &[u8], results: &mut Vec<String>) {
    let finder = memmem::Finder::new(b"require");
    let mut pos = 0;
    while let Some(idx) = finder.find(&bytes[pos..]) {
        let abs = pos + idx;
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
        let has_paren = i < bytes.len() && bytes[i] == b'(';
        if has_paren {
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

fn scan_autoloads(bytes: &[u8], results: &mut Vec<String>) {
    let finder = memmem::Finder::new(b"autoload");
    let mut pos = 0;
    while let Some(idx) = finder.find(&bytes[pos..]) {
        let abs = pos + idx;
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

fn scan_gem_bin(bytes: &[u8], results: &mut Vec<String>) {
    let finder = memmem::Finder::new(b"Gem.");
    let mut pos = 0;
    while let Some(idx) = finder.find(&bytes[pos..]) {
        let abs = pos + idx;
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

fn scan_modules(bytes: &[u8], results: &mut Vec<String>) {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_uppercase() && (i == 0 || !is_word_char(bytes[i - 1])) {
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
                if let Ok(name) = std::str::from_utf8(&bytes[start..match_end]) {
                    results.push(name.to_string());
                }
            }
            if i <= match_end {
                i = match_end;
            }
        } else {
            i += 1;
        }
    }
}

fn scan_bundler_require(
    bytes: &[u8],
    bundler_groups: &mut Vec<String>,
    bundler_require_all: &mut bool,
) {
    let group_sym_re = Regex::new(r":(\w+)").unwrap();
    let finder = memmem::Finder::new(b"Bundler.require");
    let mut pos = 0;
    while let Some(idx) = finder.find(&bytes[pos..]) {
        let abs = pos + idx;
        let after = abs + 15;
        if after < bytes.len() && is_word_char(bytes[after]) {
            pos = abs + 1;
            continue;
        }
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
            for g in group_sym_re.captures_iter(rest) {
                bundler_groups.push(g[1].to_string());
            }
        }
        pos = abs + 1;
    }
}

// ── Regex helpers ────────────────────────────────────────────────────────────

fn regex_requires(input: &str) -> Vec<String> {
    let re = Regex::new(r"(?:require|require_dependency)\s*[\(]?\s*['\x22]([^'\x22]+)['\x22]\s*[\)]?").unwrap();
    re.captures_iter(input).map(|c| c[1].to_string()).collect()
}

fn regex_autoloads(input: &str) -> Vec<String> {
    let re = Regex::new(r"autoload\s*[\(]?\s*:\w+\s*,\s*['\x22]([^'\x22]+)['\x22]").unwrap();
    re.captures_iter(input).map(|c| c[1].to_string()).collect()
}

fn regex_gem_bin(input: &str) -> Vec<String> {
    let re = Regex::new(r"Gem\.(?:activate_)?bin_path\s*\(\s*['\x22]([^'\x22]+)['\x22]").unwrap();
    re.captures_iter(input).map(|c| c[1].to_string()).collect()
}

fn regex_modules(input: &str) -> Vec<String> {
    let re = Regex::new(r"\b([A-Z][a-zA-Z0-9]+(?:::[A-Z][a-zA-Z0-9]+)*)\b").unwrap();
    re.captures_iter(input).map(|c| c[1].to_string()).collect()
}

fn regex_bundler(input: &str) -> (Vec<String>, bool) {
    let re = Regex::new(r"Bundler\.require\b(.*)").unwrap();
    let group_sym_re = Regex::new(r":(\w+)").unwrap();
    let mut groups = Vec::new();
    let mut require_all = false;
    for cap in re.captures_iter(input) {
        let rest = cap[1].trim();
        if rest.is_empty() || rest.starts_with('#') {
            require_all = true;
        } else if rest.contains("Rails.groups") || rest.contains("Rails.env") {
            require_all = true;
        } else {
            for g in group_sym_re.captures_iter(rest) {
                groups.push(g[1].to_string());
            }
        }
    }
    (groups, require_all)
}

// ── Comparison helpers ───────────────────────────────────────────────────────

fn byte_requires(input: &str) -> Vec<String> {
    let mut v = Vec::new();
    scan_requires(input.as_bytes(), &mut v);
    v
}

fn byte_autoloads(input: &str) -> Vec<String> {
    let mut v = Vec::new();
    scan_autoloads(input.as_bytes(), &mut v);
    v
}

fn byte_gem_bin(input: &str) -> Vec<String> {
    let mut v = Vec::new();
    scan_gem_bin(input.as_bytes(), &mut v);
    v
}

fn byte_modules(input: &str) -> Vec<String> {
    let mut v = Vec::new();
    scan_modules(input.as_bytes(), &mut v);
    v
}

fn byte_bundler(input: &str) -> (Vec<String>, bool) {
    let mut groups = Vec::new();
    let mut require_all = false;
    scan_bundler_require(input.as_bytes(), &mut groups, &mut require_all);
    (groups, require_all)
}

/// Assert both produce the same results.
fn assert_same_requires(input: &str) {
    let r = regex_requires(input);
    let b = byte_requires(input);
    assert_eq!(r, b, "MISMATCH on input: {:?}\n  regex={:?}\n  bytes={:?}", input, r, b);
}

fn assert_same_autoloads(input: &str) {
    let r = regex_autoloads(input);
    let b = byte_autoloads(input);
    assert_eq!(r, b, "MISMATCH on input: {:?}\n  regex={:?}\n  bytes={:?}", input, r, b);
}

fn assert_same_gem_bin(input: &str) {
    let r = regex_gem_bin(input);
    let b = byte_gem_bin(input);
    assert_eq!(r, b, "MISMATCH on input: {:?}\n  regex={:?}\n  bytes={:?}", input, r, b);
}

fn assert_same_modules(input: &str) {
    let r = regex_modules(input);
    let b = byte_modules(input);
    assert_eq!(r, b, "MISMATCH on input: {:?}\n  regex={:?}\n  bytes={:?}", input, r, b);
}

fn assert_same_bundler(input: &str) {
    let r = regex_bundler(input);
    let b = byte_bundler(input);
    assert_eq!(r, b, "MISMATCH on input: {:?}\n  regex={:?}\n  bytes={:?}", input, r, b);
}

// ═════════════════════════════════════════════════════════════════════════════
//  TESTS
// ═════════════════════════════════════════════════════════════════════════════

// ── scan_requires: happy-path (should agree) ─────────────────────────────────

#[test]
fn requires_basic_single_quote() {
    assert_same_requires("require 'json'");
}

#[test]
fn requires_basic_double_quote() {
    assert_same_requires("require \"json\"");
}

#[test]
fn requires_with_parens() {
    assert_same_requires("require('json')");
}

#[test]
fn requires_dependency() {
    assert_same_requires("require_dependency 'foo/bar'");
}

#[test]
fn requires_leading_whitespace() {
    assert_same_requires("  require 'json'");
}

#[test]
fn requires_tab_before_quote() {
    assert_same_requires("require\t'json'");
}

#[test]
fn requires_rejects_require_relative() {
    assert_same_requires("require_relative 'foo'");
}

#[test]
fn requires_rejects_require_all() {
    assert_same_requires("require_all 'foo'");
}

#[test]
fn requires_rejects_required() {
    assert_same_requires("required 'foo'");
}

#[test]
fn requires_rejects_gem_require() {
    // The regex has NO \b anchor, so it matches "require" inside "gem_require".
    // The byte scanner correctly checks word boundary before the keyword.
    // This is a REGEX FALSE POSITIVE that the byte scanner avoids.
    let input = "gem_require 'foo'";
    let r = regex_requires(input);
    let b = byte_requires(input);
    assert_eq!(r, vec!["foo"], "regex lacks word boundary — matches inside gem_require");
    assert!(b.is_empty(), "byte scanner correctly rejects (word boundary check)");
}

#[test]
fn requires_paren_with_spaces() {
    assert_same_requires("require ( 'json' )");
}

#[test]
fn requires_empty_string_rejected() {
    // Both regex [^'"]+  and extract_quoted reject empty strings
    assert_same_requires("require ''");
}

// ── scan_requires: FALSE NEGATIVE — newlines in whitespace ───────────────────
// The regex \s* matches newlines; skip_whitespace does not.

#[test]
fn requires_false_neg_newline_before_quote() {
    let input = "require\n'json'";
    let r = regex_requires(input);
    let b = byte_requires(input);
    assert_eq!(r, vec!["json"], "regex should match");
    assert!(b.is_empty(), "FALSE NEGATIVE: byte scanner misses newline-separated require");
}

#[test]
fn requires_false_neg_newline_after_paren() {
    let input = "require(\n'json')";
    let r = regex_requires(input);
    let b = byte_requires(input);
    assert_eq!(r, vec!["json"], "regex should match");
    assert!(b.is_empty(), "FALSE NEGATIVE: byte scanner misses newline after paren");
}

// ── scan_requires: EDGE CASE — extract_quoted captures opposite quote ────────
// Regex [^'"]+ stops at EITHER quote type.  extract_quoted only stops at the
// SAME quote type used to open.  This produces different captured text.

#[test]
fn requires_edge_opposite_quote_in_string() {
    let input = r#"require 'foo"bar'"#;
    let r = regex_requires(input);
    let b = byte_requires(input);
    // Regex opens with ', [^'"]+ matches "foo" (stops at "), closing quote = "
    assert_eq!(r, vec!["foo"], "regex stops at the embedded double-quote");
    // Byte scanner opens with ', reads until ' → captures foo"bar
    assert_eq!(b, vec!["foo\"bar"], "byte scanner reads until matching quote");
    assert_ne!(r, b, "EDGE CASE: different captured text for mixed quotes");
}

#[test]
fn requires_edge_double_quote_contains_single() {
    let input = "require \"foo'bar\"";
    let r = regex_requires(input);
    let b = byte_requires(input);
    assert_eq!(r, vec!["foo"], "regex stops at embedded single-quote");
    assert_eq!(b, vec!["foo'bar"], "byte scanner reads until matching double-quote");
    assert_ne!(r, b);
}

// ── scan_requires: EDGE CASE — newline inside quoted string ──────────────────
// Regex [^'"]+ matches newlines.  extract_quoted stops at \n.

#[test]
fn requires_edge_newline_in_string() {
    let input = "require \"foo\nbar\"";
    let r = regex_requires(input);
    let b = byte_requires(input);
    // Regex [^'"]+ matches across newline
    assert_eq!(r, vec!["foo\nbar"], "regex matches across newline in string");
    // Byte scanner stops at \n → no closing quote found before newline
    assert!(b.is_empty(), "FALSE NEGATIVE: byte scanner rejects newline inside string");
}

// ── scan_autoloads: happy-path ───────────────────────────────────────────────

#[test]
fn autoloads_basic() {
    assert_same_autoloads("autoload :Foo, 'foo'");
}

#[test]
fn autoloads_with_parens() {
    assert_same_autoloads("autoload(:Foo, 'foo')");
}

#[test]
fn autoloads_double_quote() {
    assert_same_autoloads("autoload :Foo, \"foo/bar\"");
}

#[test]
fn autoloads_rejects_my_autoload() {
    // The regex has NO \b anchor, so it matches "autoload" inside "my_autoload".
    // The byte scanner correctly checks word boundary before the keyword.
    // This is a REGEX FALSE POSITIVE that the byte scanner avoids.
    let input = "my_autoload :Foo, 'foo'";
    let r = regex_autoloads(input);
    let b = byte_autoloads(input);
    assert_eq!(r, vec!["foo"], "regex lacks word boundary — matches inside my_autoload");
    assert!(b.is_empty(), "byte scanner correctly rejects (word boundary check)");
}

// ── scan_autoloads: Empty symbol correctly rejected ──────────────────────────
// Both regex and byte scanner require at least one word char after `:`.

#[test]
fn autoloads_rejects_empty_symbol() {
    let input = "autoload :, 'foo'";
    let r = regex_autoloads(input);
    let b = byte_autoloads(input);
    assert!(r.is_empty(), "regex requires at least one word char after :");
    assert!(b.is_empty(), "byte scanner now also rejects empty symbol");
}

// ── scan_autoloads: FALSE NEGATIVE — newlines ────────────────────────────────

#[test]
fn autoloads_false_neg_newline_before_symbol() {
    let input = "autoload\n:Foo, 'foo'";
    let r = regex_autoloads(input);
    let b = byte_autoloads(input);
    assert_eq!(r, vec!["foo"], "regex \\s* matches newline");
    assert!(b.is_empty(), "FALSE NEGATIVE: byte scanner misses newline");
}

// ── scan_autoloads: EDGE CASE — mixed quotes ────────────────────────────────

#[test]
fn autoloads_edge_opposite_quote() {
    let input = "autoload :Foo, 'bar\"baz'";
    let r = regex_autoloads(input);
    let b = byte_autoloads(input);
    assert_eq!(r, vec!["bar"], "regex stops at embedded double-quote");
    assert_eq!(b, vec!["bar\"baz"], "byte scanner reads until matching single-quote");
    assert_ne!(r, b);
}

// ── scan_gem_bin: happy-path ─────────────────────────────────────────────────

#[test]
fn gem_bin_basic() {
    assert_same_gem_bin("Gem.bin_path('rake')");
}

#[test]
fn gem_bin_activate() {
    assert_same_gem_bin("Gem.activate_bin_path('rake')");
}

#[test]
fn gem_bin_spaces() {
    assert_same_gem_bin("Gem.bin_path( 'rake' )");
}

// ── scan_gem_bin: FALSE NEGATIVE — newline before paren ──────────────────────

#[test]
fn gem_bin_false_neg_newline() {
    let input = "Gem.bin_path\n('rake')";
    let r = regex_gem_bin(input);
    let b = byte_gem_bin(input);
    assert_eq!(r, vec!["rake"], "regex \\s* matches newline");
    assert!(b.is_empty(), "FALSE NEGATIVE: byte scanner misses newline before paren");
}

// ── scan_gem_bin: EDGE CASE — mixed quotes ──────────────────────────────────

#[test]
fn gem_bin_edge_opposite_quote() {
    let input = "Gem.bin_path('foo\"bar')";
    let r = regex_gem_bin(input);
    let b = byte_gem_bin(input);
    assert_eq!(r, vec!["foo"], "regex stops at embedded double-quote");
    assert_eq!(b, vec!["foo\"bar"], "byte scanner reads until matching quote");
    assert_ne!(r, b);
}

// ═════════════════════════════════════════════════════════════════════════════
//  scan_modules — the most complex scanner
// ═════════════════════════════════════════════════════════════════════════════

// ── Happy-path ───────────────────────────────────────────────────────────────

#[test]
fn modules_simple() {
    assert_same_modules("Rails");
}

#[test]
fn modules_namespaced() {
    assert_same_modules("ActiveRecord::Base");
}

#[test]
fn modules_triple_namespace() {
    assert_same_modules("Foo::Bar::Baz");
}

#[test]
fn modules_digits() {
    assert_same_modules("Foo2Bar");
}

#[test]
fn modules_single_letter_rejected() {
    // [A-Z][a-zA-Z0-9]+ requires 2+ chars
    assert_same_modules("A");
}

#[test]
fn modules_two_chars() {
    assert_same_modules("Fo");
}

#[test]
fn modules_in_sentence() {
    assert_same_modules("use Rails and ActiveSupport::Concern here");
}

// ── Word boundary: underscore IS a word char for \b ──────────────────────────

#[test]
fn modules_underscore_prefix_no_match() {
    // _Foo: no word boundary between _ and F (both are word chars)
    assert_same_modules("_Foo");
}

#[test]
fn modules_underscore_suffix_no_match() {
    // Foo_: \b between o and _ does not hold (both word chars), so [a-zA-Z0-9]+
    // stops at _ but \b fails → no match for "Foo"
    // Byte scanner: reads "Foo", sees _, word char → reject
    assert_same_modules("Foo_");
}

#[test]
fn modules_underscore_mid_no_match() {
    // Foo_bar: regex [a-zA-Z0-9]+ matches "oo", \b at _ fails
    // Byte scanner: reads "Foo", sees _, word char → reject
    assert_same_modules("Foo_bar");
}

#[test]
fn modules_underscore_between_caps_no_match() {
    // Foo_Bar: no match for either part (no word boundary at _)
    assert_same_modules("Foo_Bar");
}

// ── scan_modules: Backtracking — greedy :: consumption with fallback ──────────
// The byte scanner now backtracks when a ::Segment continuation fails the word boundary.
// It emits the last valid prefix, matching the regex behavior.

#[test]
fn modules_backtrack_double_colon_then_underscore() {
    let input = "Foo::Bar_Baz";
    let r = regex_modules(input);
    let b = byte_modules(input);
    // Regex backtracks: Foo::Bar fails \b at _, so it drops ::Bar → matches "Foo"
    assert_eq!(r, vec!["Foo"], "regex backtracks to match 'Foo'");
    // Byte scanner now also backtracks
    assert_eq!(b, vec!["Foo"], "byte scanner backtracks to match 'Foo'");
}

#[test]
fn modules_backtrack_triple_then_underscore() {
    let input = "Foo::Bar::Baz_Qux";
    let r = regex_modules(input);
    let b = byte_modules(input);
    // Regex backtracks to Foo::Bar (boundary at : after r)
    assert_eq!(r, vec!["Foo::Bar"], "regex backtracks to Foo::Bar");
    assert_eq!(b, vec!["Foo::Bar"], "byte scanner backtracks to Foo::Bar");
}

#[test]
fn modules_backtrack_double_colon_then_digit_word() {
    // Foo::Bar3_ — byte scanner reads Foo::Bar3, then _ → backtrack to Foo
    // Regex backtracks to Foo
    let input = "Foo::Bar3_";
    let r = regex_modules(input);
    let b = byte_modules(input);
    assert_eq!(r, vec!["Foo"], "regex backtracks to Foo");
    assert_eq!(b, vec!["Foo"], "byte scanner backtracks to Foo");
}

// ── scan_modules: FALSE POSITIVE — Unicode word boundary ─────────────────────
// Rust's regex \b is Unicode-aware: accented letters are word characters.
// The byte scanner only checks ASCII, so it sees the first byte of a multi-byte
// char as a non-word char and accepts a word boundary that doesn't exist in Unicode.

#[test]
fn modules_false_pos_unicode_suffix() {
    let input = "Foo\u{00e9}"; // "Fooé" — é is a Unicode word char
    let r = regex_modules(input);
    let b = byte_modules(input);
    // Regex: \b between 'o' and 'é' does not hold (both Unicode word chars) → no match
    assert!(r.is_empty(), "regex sees no word boundary before Unicode letter");
    // Byte scanner: 'é' is 0xC3 0xA9 in UTF-8, first byte not ASCII alnum/_ → boundary
    assert_eq!(b, vec!["Foo"], "FALSE POSITIVE: byte scanner ignores Unicode word chars");
}

#[test]
fn modules_false_pos_unicode_prefix() {
    let input = "\u{00e9}Foo"; // "éFoo" — é precedes F
    let r = regex_modules(input);
    let b = byte_modules(input);
    // Regex: \b between 'é' (word) and 'F' (word) → no boundary → no match
    assert!(r.is_empty(), "regex sees no word boundary after Unicode letter");
    // Byte scanner: byte before F is 0xA9 (last byte of é), not ASCII word char → matches
    assert_eq!(b, vec!["Foo"], "FALSE POSITIVE: byte scanner treats UTF-8 continuation byte as non-word");
}

#[test]
fn modules_false_pos_unicode_between_namespaces() {
    // "FooéBar" — regex sees one continuous word (é is a word char), no inner boundary
    let input = "Foo\u{00e9}Bar";
    let r = regex_modules(input);
    let b = byte_modules(input);
    // Regex: Foo matches [A-Z][a-zA-Z0-9]+ = F + oo. Then \b at é → no boundary. No match.
    assert!(r.is_empty(), "regex fails due to Unicode word boundary");
    // Byte scanner: Foo matches, then 0xC3 is not word char → matches "Foo"
    // Then at position of "Bar" (after é's 2 bytes): B is uppercase, preceded by 0xA9 (not word char)
    // → matches "Bar"
    assert_eq!(b, vec!["Foo", "Bar"], "FALSE POSITIVE: byte scanner matches both parts");
}

// ── scan_modules: single uppercase + single :: segment ───────────────────────

#[test]
fn modules_single_uppercase_after_colons_no_match() {
    // Foo::B — [A-Z][a-zA-Z0-9]+ requires 2+ chars after ::
    // Both regex and byte scanner reject the ::B part, match only Foo
    assert_same_modules("Foo::B");
}

#[test]
fn modules_double_colon_lowercase_after() {
    // Foo::bar — :: not followed by uppercase, match only Foo
    assert_same_modules("Foo::bar");
}

#[test]
fn modules_trailing_double_colon() {
    // Foo:: — :: at end of string
    assert_same_modules("Foo::");
}

#[test]
fn modules_preceded_by_dot() {
    // .Foo — dot is not a word char
    assert_same_modules(".Foo");
}

#[test]
fn modules_preceded_by_colon() {
    // :Foo — colon is not a word char (Ruby symbol syntax)
    assert_same_modules(":Foo");
}

#[test]
fn modules_multiple_on_line() {
    assert_same_modules("Foo::Bar and Baz::Qux");
}

#[test]
fn modules_at_end_of_string() {
    assert_same_modules("use ActiveRecord");
}

// ═════════════════════════════════════════════════════════════════════════════
//  scan_bundler_require
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn bundler_basic_no_args() {
    assert_same_bundler("Bundler.require");
}

#[test]
fn bundler_with_groups() {
    assert_same_bundler("Bundler.require(:default, :development)");
}

#[test]
fn bundler_rails_groups() {
    assert_same_bundler("Bundler.require(*Rails.groups)");
}

#[test]
fn bundler_rails_env() {
    assert_same_bundler("Bundler.require(:default, Rails.env)");
}

#[test]
fn bundler_with_comment() {
    assert_same_bundler("Bundler.require # load all");
}

#[test]
fn bundler_rejects_bundler_required() {
    // "Bundler.required" — word char after "require" → rejected
    let input = "Bundler.required";
    let r = regex_bundler(input);
    let b = byte_bundler(input);
    assert_eq!(r, (vec![], false));
    assert_eq!(b, (vec![], false));
}

#[test]
fn bundler_no_leading_word_boundary() {
    // "xBundler.require" — neither regex nor byte scanner check leading boundary
    let input = "xBundler.require";
    assert_same_bundler(input);
}

// ═════════════════════════════════════════════════════════════════════════════
//  extract_quoted — escape sequences
// ═════════════════════════════════════════════════════════════════════════════
// Neither regex [^'"]+ nor extract_quoted handles backslash escapes.
// Both treat \' and \" as the literal characters \ and ' / ".
// This means they agree — but both are "wrong" for escaped quotes.

#[test]
fn requires_escaped_quote_both_stop_early() {
    // require 'foo\'bar' — both stop at the backslash-quote
    let input = r"require 'foo\'bar'";
    let r = regex_requires(input);
    let b = byte_requires(input);
    // Regex: [^'"]+ matches "foo\" (stops at '), closing quote matches '
    // → capture is "foo\"
    // Byte scanner: extract_quoted reads until ' → "foo\"
    assert_eq!(r, vec![r"foo\"]);
    assert_eq!(b, vec![r"foo\"]);
}

// ═════════════════════════════════════════════════════════════════════════════
//  Summary diagnostic — run all edge cases and print a report
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn print_audit_summary() {
    let mut false_negatives = Vec::new();
    let mut false_positives = Vec::new();
    let mut regex_false_positives = Vec::new();
    let mut edge_cases = Vec::new();

    // -- scan_requires --
    let cases_req: Vec<(&str, &str)> = vec![
        ("require\n'json'", "newline before quote"),
        ("require(\n'json')", "newline after paren"),
        ("require \"foo\nbar\"", "newline inside string"),
    ];
    for (input, desc) in &cases_req {
        let r = regex_requires(input);
        let b = byte_requires(input);
        if !r.is_empty() && b.is_empty() {
            false_negatives.push(format!("scan_requires: {desc} — regex={r:?}, bytes={b:?}"));
        }
    }

    let cases_req_edge: Vec<(&str, &str)> = vec![
        (r#"require 'foo"bar'"#, "opposite quote in single-quoted string"),
        (r#"require "foo'bar""#, "opposite quote in double-quoted string"),
    ];
    for (input, desc) in &cases_req_edge {
        let r = regex_requires(input);
        let b = byte_requires(input);
        if r != b {
            edge_cases.push(format!("scan_requires: {desc} — regex={r:?}, bytes={b:?}"));
        }
    }

    // -- scan_requires: regex false positives (byte scanner is stricter) --
    {
        let input = "gem_require 'foo'";
        let r = regex_requires(input);
        let b = byte_requires(input);
        if !r.is_empty() && b.is_empty() {
            regex_false_positives.push(format!("scan_requires: no \\b in regex, matches inside gem_require — regex={r:?}, bytes={b:?}"));
        }
    }

    // -- scan_autoloads: regex false positives --
    {
        let input = "my_autoload :Foo, 'foo'";
        let r = regex_autoloads(input);
        let b = byte_autoloads(input);
        if !r.is_empty() && b.is_empty() {
            regex_false_positives.push(format!("scan_autoloads: no \\b in regex, matches inside my_autoload — regex={r:?}, bytes={b:?}"));
        }
    }

    // -- scan_autoloads --
    {
        let input = "autoload :, 'foo'";
        let r = regex_autoloads(input);
        let b = byte_autoloads(input);
        if r.is_empty() && !b.is_empty() {
            false_positives.push(format!("scan_autoloads: empty symbol — regex={r:?}, bytes={b:?}"));
        }
    }
    {
        let input = "autoload\n:Foo, 'foo'";
        let r = regex_autoloads(input);
        let b = byte_autoloads(input);
        if !r.is_empty() && b.is_empty() {
            false_negatives.push(format!("scan_autoloads: newline before symbol — regex={r:?}, bytes={b:?}"));
        }
    }

    // -- scan_gem_bin --
    {
        let input = "Gem.bin_path\n('rake')";
        let r = regex_gem_bin(input);
        let b = byte_gem_bin(input);
        if !r.is_empty() && b.is_empty() {
            false_negatives.push(format!("scan_gem_bin: newline before paren — regex={r:?}, bytes={b:?}"));
        }
    }

    // -- scan_modules --
    let mod_fn_cases: Vec<(&str, &str)> = vec![
        ("Foo::Bar_Baz", "greedy :: then underscore"),
        ("Foo::Bar::Baz_Qux", "greedy triple :: then underscore"),
        ("Foo::Bar3_", "greedy :: + digits then underscore"),
    ];
    for (input, desc) in &mod_fn_cases {
        let r = regex_modules(input);
        let b = byte_modules(input);
        if !r.is_empty() && b.is_empty() {
            false_negatives.push(format!("scan_modules: {desc} — regex={r:?}, bytes={b:?}"));
        } else if r != b && !r.is_empty() {
            false_negatives.push(format!("scan_modules: {desc} — regex={r:?}, bytes={b:?}"));
        }
    }

    let mod_fp_cases: Vec<(&str, &str)> = vec![
        ("Foo\u{00e9}", "Unicode suffix (é)"),
        ("\u{00e9}Foo", "Unicode prefix (é)"),
        ("Foo\u{00e9}Bar", "Unicode between segments"),
    ];
    for (input, desc) in &mod_fp_cases {
        let r = regex_modules(input);
        let b = byte_modules(input);
        if r.is_empty() && !b.is_empty() {
            false_positives.push(format!("scan_modules: {desc} — regex={r:?}, bytes={b:?}"));
        } else if r != b {
            false_positives.push(format!("scan_modules: {desc} — regex={r:?}, bytes={b:?}"));
        }
    }

    // -- Print report --
    eprintln!("\n{}", "=".repeat(72));
    eprintln!("  BYTE SCANNER CORRECTNESS AUDIT SUMMARY");
    eprintln!("{}\n", "=".repeat(72));

    eprintln!("FALSE NEGATIVES (regex matches, byte scanner misses): {}", false_negatives.len());
    for fn_ in &false_negatives {
        eprintln!("  ✗ {fn_}");
    }

    eprintln!("\nREGEX FALSE POSITIVES (regex matches inside words, byte scanner correctly rejects): {}", regex_false_positives.len());
    for rfp in &regex_false_positives {
        eprintln!("  ✓ {rfp}");
    }

    eprintln!("\nFALSE POSITIVES (byte scanner matches, regex wouldn't): {}", false_positives.len());
    for fp in &false_positives {
        eprintln!("  ✗ {fp}");
    }

    eprintln!("\nEDGE CASES (different captured text): {}", edge_cases.len());
    for ec in &edge_cases {
        eprintln!("  ⚠ {ec}");
    }

    let total = false_negatives.len() + false_positives.len() + regex_false_positives.len() + edge_cases.len();
    eprintln!("\nTotal discrepancies: {total}");
    eprintln!("{}\n", "=".repeat(72));

    // This test intentionally does NOT assert failure — it's a diagnostic.
    // The individual tests above assert each specific case.
}
