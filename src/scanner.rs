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
#[cfg(test)]
static HEREDOC_FINDER: Lazy<memmem::Finder<'static>> = Lazy::new(|| memmem::Finder::new(b"<<"));
static REQUIRE_FINDER: Lazy<memmem::Finder<'static>> =
    Lazy::new(|| memmem::Finder::new(b"require"));
static AUTOLOAD_FINDER: Lazy<memmem::Finder<'static>> =
    Lazy::new(|| memmem::Finder::new(b"autoload"));
static GEM_DOT_FINDER: Lazy<memmem::Finder<'static>> = Lazy::new(|| memmem::Finder::new(b"Gem."));
static ATTACHED_FINDER: Lazy<memmem::Finder<'static>> =
    Lazy::new(|| memmem::Finder::new(b"_attached"));
static BUNDLER_REQUIRE_FINDER: Lazy<memmem::Finder<'static>> =
    Lazy::new(|| memmem::Finder::new(b"Bundler.require"));
const RAILS_GROUP_CONDITION_PREFIX: &str = "__rails_group_condition__:";

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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum TemplateKind {
    None,
    Erb,
    Haml,
    Slim,
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

const SKIP_DIRS_AT_ANY_DEPTH: &[&str] = &[".bundle", "node_modules", ".git"];

// ── Hand-rolled byte scanners (memchr + manual parsing) ──────────────────────

#[inline]
fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[inline]
fn skip_whitespace(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
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
#[cfg(test)]
fn build_block_comment_ranges(bytes: &[u8]) -> Vec<(usize, usize)> {
    // This helper is retained for focused range tests. The production mask is
    // built by the lexical pass below so quoted/commented openers are ignored.
    if memmem::find(bytes, b"=begin").is_none() && memmem::find(bytes, b"<<").is_none() {
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
    while let Some(idx) = memmem::find(&bytes[pos..], b"\n=begin") {
        let abs = pos + idx + 1;
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

#[inline]
fn set_mask(mask: &mut [u8], start: usize, end: usize, value: bool) {
    let end = end.min(mask.len());
    mask[start..end].fill(u8::from(value));
}

type PercentEndCache = HashMap<(usize, bool), Option<usize>>;
type InterpolationEndCache = HashMap<usize, Option<usize>>;
/// (heredoc delimiter, search end) → earliest body start known to have no
/// terminator line before the search end.
type HeredocMissCache = HashMap<(Vec<u8>, usize), usize>;

#[cfg(test)]
type LexicalScanSteps = usize;
#[cfg(not(test))]
type LexicalScanSteps = ();

/// Memoization shared by one lexical masking pass so malformed input stays
/// linear instead of re-scanning to EOF for every unterminated opener.
#[derive(Default)]
struct LexCache {
    percent_ends: PercentEndCache,
    interpolation_ends: InterpolationEndCache,
    heredoc_misses: HeredocMissCache,
    steps: LexicalScanSteps,
}

#[inline]
fn count_lexical_scan_step(steps: &mut LexicalScanSteps) {
    #[cfg(test)]
    {
        *steps += 1;
    }
    #[cfg(not(test))]
    {
        let _ = steps;
    }
}

#[inline]
fn is_percent_literal_type(byte: u8) -> bool {
    matches!(
        byte,
        b'q' | b'Q' | b'r' | b'w' | b'W' | b'i' | b'I' | b's' | b'x'
    )
}

#[inline]
fn percent_literal_regex_semantics(bytes: &[u8], open_pos: usize) -> Option<bool> {
    (open_pos >= 2
        && bytes[open_pos - 2] == b'%'
        && is_percent_literal_type(bytes[open_pos - 1])
        && (open_pos == 2 || !is_word_char(bytes[open_pos - 3])))
    .then(|| bytes[open_pos - 1] == b'r')
}

fn find_interpolation_end_cached(
    bytes: &[u8],
    content_start: usize,
    end: usize,
    interpolation_ends: &mut InterpolationEndCache,
    lexical_steps: &mut LexicalScanSteps,
) -> Option<usize> {
    if let Some(cached) = interpolation_ends.get(&content_start) {
        return *cached;
    }

    let mut stack = Vec::with_capacity(8);
    stack.push(Some(content_start));
    let mut i = content_start;
    while i < end {
        count_lexical_scan_step(lexical_steps);
        match bytes[i] {
            b'\\' => i = (i + 2).min(end),
            b'\'' | b'"' => {
                let quote = bytes[i];
                i += 1;
                while i < end {
                    count_lexical_scan_step(lexical_steps);
                    if bytes[i] == b'\\' {
                        i = (i + 2).min(end);
                    } else if bytes[i] == quote {
                        i += 1;
                        break;
                    } else {
                        i += 1;
                    }
                }
            }
            b'#' if i + 1 < end && bytes[i + 1] == b'{' => {
                stack.push(Some(i + 2));
                i += 2;
            }
            b'{' => {
                stack.push(None);
                i += 1;
            }
            b'}' => {
                if let Some(interpolation_start) = stack.pop().flatten() {
                    if interpolation_start != content_start {
                        interpolation_ends.insert(interpolation_start, Some(i));
                    }
                }
                if stack.is_empty() {
                    interpolation_ends.insert(content_start, Some(i));
                    return Some(i);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }

    for interpolation_start in stack.into_iter().flatten() {
        interpolation_ends.insert(interpolation_start, None);
    }
    None
}

fn find_interpolation_end(bytes: &[u8], content_start: usize, end: usize) -> Option<usize> {
    let mut interpolation_ends = InterpolationEndCache::new();
    #[cfg(test)]
    let mut lexical_steps = 0;
    #[cfg(not(test))]
    let mut lexical_steps = ();
    find_interpolation_end_cached(
        bytes,
        content_start,
        end,
        &mut interpolation_ends,
        &mut lexical_steps,
    )
}

fn expose_interpolations(
    bytes: &[u8],
    mask: &mut [u8],
    start: usize,
    end: usize,
    cache: &mut LexCache,
) {
    let mut i = start;
    while i + 1 < end {
        if bytes[i] == b'\\' {
            i = (i + 2).min(end);
            continue;
        }
        if bytes[i] == b'#' && bytes[i + 1] == b'{' {
            if let Some(close) = find_interpolation_end_cached(
                bytes,
                i + 2,
                end,
                &mut cache.interpolation_ends,
                &mut cache.steps,
            ) {
                set_mask(mask, i + 2, close, false);
                mask_ruby_range_cached(bytes, mask, i + 2, close, cache);
                i = close + 1;
                continue;
            }
        }
        i += 1;
    }
}

fn apply_template_mask(bytes: &[u8], mask: &mut [u8], kind: TemplateKind) {
    match kind {
        TemplateKind::None => {}
        TemplateKind::Erb => {
            mask.fill(1);
            let mut pos = 0;
            while let Some(idx) = memmem::find(&bytes[pos..], b"<%") {
                let open = pos + idx;
                if bytes.get(open + 2) == Some(&b'%') {
                    pos = open + 3;
                    continue;
                }
                let Some(close_rel) = memmem::find(&bytes[open + 2..], b"%>") else {
                    break;
                };
                let close = open + 2 + close_rel;
                let mut code_start = open + 2;
                if bytes.get(code_start) == Some(&b'#') {
                    pos = close + 2;
                    continue;
                }
                if matches!(bytes.get(code_start), Some(b'=') | Some(b'-')) {
                    code_start += 1;
                }
                set_mask(mask, code_start, close, false);
                pos = close + 2;
            }
        }
        TemplateKind::Haml | TemplateKind::Slim => apply_indented_template_mask(bytes, mask, kind),
    }
}

/// An indented block owned by a previous template line.
#[derive(Copy, Clone, PartialEq, Eq)]
enum TemplateBlock {
    /// `:ruby` / `ruby:` filter body.
    Ruby,
    /// Other filters and Slim `|` text blocks: text with `#{}` interpolation.
    Text,
    /// `-#` (Haml) / `/` (Slim) comment bodies.
    Comment,
}

/// Slim embedded engines whose bodies are not Slim markup.
const SLIM_EMBEDDED_ENGINES: &[&[u8]] = &[
    b"javascript",
    b"css",
    b"sass",
    b"scss",
    b"less",
    b"coffee",
    b"markdown",
    b"textile",
    b"rdoc",
    b"creole",
    b"wiki",
    b"mediawiki",
    b"org",
    b"erb",
];

/// Haml and Slim: expose Ruby code (`=`/`-` lines, tag attributes, tag output
/// after `=`, `:ruby` filters, multi-line continuations) and `#{}`
/// interpolation in text; everything else stays masked.
fn apply_indented_template_mask(bytes: &[u8], mask: &mut [u8], kind: TemplateKind) {
    mask.fill(1);
    let len = bytes.len();
    let mut line_start = 0;
    let mut block: Option<(usize, TemplateBlock)> = None;
    let mut ruby_continues = false;
    while line_start < len {
        let eol = memchr::memchr(b'\n', &bytes[line_start..])
            .map(|n| line_start + n)
            .unwrap_or(len);
        let mut ws = line_start;
        while ws < eol && matches!(bytes[ws], b' ' | b'\t') {
            ws += 1;
        }
        let indent = ws - line_start;
        let content_end = if eol > ws && bytes[eol - 1] == b'\r' {
            eol - 1
        } else {
            eol
        };
        let next_line = |pos: usize| {
            memchr::memchr(b'\n', &bytes[pos..])
                .map(|n| pos + n + 1)
                .unwrap_or(len)
        };

        if let Some((block_indent, block_kind)) = block {
            if ws == content_end || indent > block_indent {
                match block_kind {
                    TemplateBlock::Ruby => set_mask(mask, ws, content_end, false),
                    TemplateBlock::Text => expose_text_interpolations(bytes, mask, ws, content_end),
                    TemplateBlock::Comment => {}
                }
                line_start = next_line(line_start);
                continue;
            }
            block = None;
        }
        if ruby_continues {
            // Include the preceding newline so literals spanning lines stay whole.
            set_mask(mask, line_start.saturating_sub(1), content_end, false);
            ruby_continues = ruby_line_continues(bytes, ws, content_end, kind);
            line_start = next_line(line_start);
            continue;
        }
        let content = &bytes[ws..content_end];
        if content.is_empty() {
            line_start = next_line(line_start);
            continue;
        }

        let mut resume = line_start;
        match kind {
            TemplateKind::Haml => {
                if content.starts_with(b"-#") {
                    block = Some((indent, TemplateBlock::Comment));
                } else if let Some(filter) = content.strip_prefix(b":") {
                    let kind = if filter == b"ruby" {
                        TemplateBlock::Ruby
                    } else {
                        TemplateBlock::Text
                    };
                    block = Some((indent, kind));
                } else if let Some(code) = haml_script_start(bytes, ws, content_end) {
                    set_mask(mask, code, content_end, false);
                    ruby_continues = ruby_line_continues(bytes, code, content_end, kind);
                } else if is_haml_tag_start(content) {
                    let (end, continues) = parse_haml_tag(bytes, mask, ws);
                    resume = end;
                    ruby_continues = continues;
                }
            }
            TemplateKind::Slim => {
                if content[0] == b'/' {
                    block = Some((indent, TemplateBlock::Comment));
                } else if content[0] == b'|' || content[0] == b'\'' {
                    block = Some((indent, TemplateBlock::Text));
                } else if content == b"ruby:" {
                    block = Some((indent, TemplateBlock::Ruby));
                } else if content
                    .strip_suffix(b":")
                    .is_some_and(|engine| SLIM_EMBEDDED_ENGINES.contains(&engine))
                {
                    block = Some((indent, TemplateBlock::Text));
                } else if matches!(content[0], b'=' | b'-') {
                    let mut code = ws + 1;
                    while code < content_end && matches!(bytes[code], b'=' | b'<' | b'>' | b'\'') {
                        code += 1;
                    }
                    set_mask(mask, code, content_end, false);
                    ruby_continues = ruby_line_continues(bytes, code, content_end, kind);
                } else if is_slim_tag_start(content) {
                    let (end, continues) = parse_slim_tag(bytes, mask, ws);
                    resume = end;
                    ruby_continues = continues;
                }
            }
            _ => {}
        }
        if block != Some((indent, TemplateBlock::Comment)) {
            expose_text_interpolations(bytes, mask, line_start, content_end);
        }
        line_start = next_line(resume.max(line_start));
    }
}

fn expose_text_interpolations(bytes: &[u8], mask: &mut [u8], start: usize, end: usize) {
    let mut i = start;
    while i + 1 < end {
        if bytes[i] == b'\\' {
            i = (i + 2).min(end);
            continue;
        }
        if bytes[i] == b'#' && bytes[i + 1] == b'{' {
            if let Some(close) = find_interpolation_end(bytes, i + 2, end) {
                set_mask(mask, i + 2, close, false);
                i = close + 1;
                continue;
            }
        }
        i += 1;
    }
}

/// Whether Ruby code ending at `end` continues on the next line: Haml and Slim
/// both continue after a trailing `,`; Slim also after a trailing `\`.
fn ruby_line_continues(bytes: &[u8], start: usize, end: usize, kind: TemplateKind) -> bool {
    let code = bytes[start..end].trim_ascii_end();
    match code.last() {
        Some(b',') => true,
        Some(b'\\') => kind == TemplateKind::Slim,
        _ => false,
    }
}

/// Start of the Ruby code on a Haml script line (`= x`, `- x`, `~ x`,
/// `!= x`, `&= x`); `==`, `!==` and `&==` are interpolated text.
fn haml_script_start(bytes: &[u8], ws: usize, end: usize) -> Option<usize> {
    let rest = &bytes[ws..end];
    let indicator = match rest {
        [b'!' | b'&', b'=', b'=', ..] | [b'=', b'=', ..] => return None,
        [b'!' | b'&', b'=', ..] => 2,
        [b'=' | b'-' | b'~', ..] => 1,
        _ => return None,
    };
    Some(ws + indicator)
}

#[inline]
fn is_template_name_char(b: u8) -> bool {
    is_word_char(b) || b == b'-'
}

fn is_haml_tag_start(content: &[u8]) -> bool {
    match content {
        [b'%', next, ..] => is_template_name_char(*next),
        [b'.' | b'#', next, ..] => is_template_name_char(*next),
        _ => false,
    }
}

fn is_slim_tag_start(content: &[u8]) -> bool {
    match content {
        [first, ..] if first.is_ascii_alphabetic() => true,
        [b'.' | b'#', next, ..] => is_template_name_char(*next),
        _ => false,
    }
}

/// Skip `.class` / `#id` shortcuts (and a tag name when `tag_prefix` is set).
fn skip_tag_and_shortcuts(bytes: &[u8], mut i: usize, end: usize, tag_prefix: Option<u8>) -> usize {
    if tag_prefix.is_none_or(|prefix| bytes.get(i) == Some(&prefix)) {
        if tag_prefix.is_some() {
            i += 1;
        }
        // Namespaced names (`svg:rect`) keep inner colons; a trailing `:` is
        // Slim's inline nesting.
        while i < end
            && (is_template_name_char(bytes[i])
                || (bytes[i] == b':' && bytes.get(i + 1).is_some_and(|&b| is_word_char(b))))
        {
            i += 1;
        }
    }
    while i + 1 < end && matches!(bytes[i], b'.' | b'#') && is_template_name_char(bytes[i + 1]) {
        i += 1;
        while i < end && is_template_name_char(bytes[i]) {
            i += 1;
        }
    }
    i
}

fn line_content_end(bytes: &[u8], pos: usize) -> usize {
    let eol = memchr::memchr(b'\n', &bytes[pos..])
        .map(|n| pos + n)
        .unwrap_or(bytes.len());
    if eol > pos && bytes[eol - 1] == b'\r' {
        eol - 1
    } else {
        eol
    }
}

/// Closing bracket for a template attribute group opened at `open_pos`,
/// skipping quoted strings; attribute groups may span lines.
fn find_template_group_end(bytes: &[u8], open_pos: usize) -> Option<usize> {
    const MAX_LINES: usize = 64;
    let open = bytes[open_pos];
    let close = match open {
        b'(' => b')',
        b'[' => b']',
        _ => b'}',
    };
    let mut depth = 0usize;
    let mut lines = 0;
    let mut i = open_pos;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 2;
                continue;
            }
            quote @ (b'\'' | b'"') => {
                i += 1;
                while i < bytes.len() && bytes[i] != quote && bytes[i] != b'\n' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                if bytes.get(i) != Some(&quote) {
                    continue;
                }
            }
            b'\n' => {
                lines += 1;
                if lines > MAX_LINES {
                    return None;
                }
            }
            b if b == open => depth += 1,
            b if b == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Expose an attribute group at `open_pos`; returns the position after it.
fn expose_attribute_group(bytes: &[u8], mask: &mut [u8], open_pos: usize) -> usize {
    match find_template_group_end(bytes, open_pos) {
        Some(close) => {
            set_mask(mask, open_pos + 1, close, false);
            close + 1
        }
        None => {
            let end = line_content_end(bytes, open_pos);
            set_mask(mask, open_pos + 1, end, false);
            end
        }
    }
}

/// Parse a Haml tag line (`%tag.cls#id{...}(...)[...]= code`) starting at
/// `ws`. Returns the last consumed position and whether Ruby continues on
/// the next line. Text after the tag stays masked.
fn parse_haml_tag(bytes: &[u8], mask: &mut [u8], ws: usize) -> (usize, bool) {
    let mut end = line_content_end(bytes, ws);
    let mut i = skip_tag_and_shortcuts(bytes, ws, end, Some(b'%'));
    while i < end && matches!(bytes[i], b'{' | b'(' | b'[') {
        i = expose_attribute_group(bytes, mask, i);
        end = line_content_end(bytes, i);
    }
    while i < end && matches!(bytes[i], b'<' | b'>') {
        i += 1;
    }
    if let Some(code) = haml_script_start(bytes, i, end) {
        set_mask(mask, code, end, false);
        return (
            end,
            ruby_line_continues(bytes, code, end, TemplateKind::Haml),
        );
    }
    (end, false)
}

/// Parse a Slim tag line following Slim's own grammar: tag and shortcuts,
/// then either a wrapped attribute group or `name=value` attributes (code
/// values are Ruby, quoted values are text), then `:` inline nesting, `=`
/// output, or text.
fn parse_slim_tag(bytes: &[u8], mask: &mut [u8], ws: usize) -> (usize, bool) {
    let mut end = line_content_end(bytes, ws);
    let mut i = ws;
    loop {
        i = if bytes[i].is_ascii_alphabetic() {
            skip_tag_and_shortcuts(bytes, i, end, None)
        } else {
            skip_tag_and_shortcuts(bytes, i, end, Some(b'*'))
        };
        while i < end && matches!(bytes[i], b'<' | b'>' | b'\'') {
            i += 1;
        }
        let group = skip_whitespace_inline(bytes, i);
        if group < end && matches!(bytes[group], b'(' | b'[' | b'{') {
            i = expose_attribute_group(bytes, mask, group);
            end = line_content_end(bytes, i);
        } else {
            i = parse_slim_plain_attributes(bytes, mask, i, end);
        }
        let next = skip_whitespace_inline(bytes, i);
        if next < end && bytes[next] == b':' {
            let child = skip_whitespace_inline(bytes, next + 1);
            if child < end && is_slim_tag_start(&bytes[child..end]) {
                i = child;
                continue;
            }
            return (end, false);
        }
        if next < end && bytes[next] == b'=' {
            let mut code = next + 1;
            while code < end && matches!(bytes[code], b'=' | b'<' | b'>' | b'\'') {
                code += 1;
            }
            set_mask(mask, code, end, false);
            return (
                end,
                ruby_line_continues(bytes, code, end, TemplateKind::Slim),
            );
        }
        return (end, false);
    }
}

/// Unwrapped Slim attributes: `name = value` pairs until something that is
/// not an attribute. Code values end at whitespace outside brackets.
fn parse_slim_plain_attributes(bytes: &[u8], mask: &mut [u8], mut i: usize, end: usize) -> usize {
    loop {
        let name_start = skip_whitespace_inline(bytes, i);
        let mut name_end = name_start;
        while name_end < end
            && !bytes[name_end].is_ascii_whitespace()
            && !matches!(
                bytes[name_end],
                b'"' | b'\'' | b'<' | b'>' | b'/' | b'=' | b'(' | b')' | b'[' | b']' | b'{' | b'}'
            )
        {
            name_end += 1;
        }
        if name_end == name_start || bytes[name_start] == b':' {
            return i;
        }
        let eq = skip_whitespace_inline(bytes, name_end);
        if eq >= end || bytes[eq] != b'=' {
            return i;
        }
        let mut value = eq + 1;
        if value < end && bytes[value] == b'=' {
            value += 1;
        }
        value = skip_whitespace_inline(bytes, value);
        if value >= end {
            return i;
        }
        if matches!(bytes[value], b'"' | b'\'') {
            let quote = bytes[value];
            let mut close = value + 1;
            while close < end && bytes[close] != quote {
                close += if bytes[close] == b'\\' { 2 } else { 1 };
            }
            i = (close + 1).min(end);
            continue;
        }
        let mut code_end = value;
        let mut depth = 0usize;
        let mut delims = (0u8, 0u8);
        while code_end < end {
            let b = bytes[code_end];
            if depth == 0 && b.is_ascii_whitespace() {
                break;
            }
            if matches!(b, b'"' | b'\'') {
                code_end += 1;
                while code_end < end && bytes[code_end] != b {
                    code_end += if bytes[code_end] == b'\\' { 2 } else { 1 };
                }
            } else if depth == 0 && matches!(b, b'(' | b'[' | b'{') {
                delims = (
                    b,
                    match b {
                        b'(' => b')',
                        b'[' => b']',
                        _ => b'}',
                    },
                );
                depth = 1;
            } else if depth > 0 && b == delims.0 {
                depth += 1;
            } else if depth > 0 && b == delims.1 {
                depth -= 1;
            }
            code_end += 1;
        }
        let code_end = code_end.min(end);
        set_mask(mask, value, code_end, false);
        i = code_end;
    }
}

fn previous_significant(bytes: &[u8], mask: &[u8], start: usize, mut i: usize) -> Option<usize> {
    while i > start {
        i -= 1;
        if mask[i] != 0 {
            return None;
        }
        if bytes[i] == b'\n' {
            return None;
        }
        if !bytes[i].is_ascii_whitespace() {
            return Some(i);
        }
    }
    None
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum LiteralStart {
    /// Division / modulo operator.
    Operator,
    /// Unambiguous value position (after an operator, keyword or `(`).
    Value,
    /// `ident /x` or `ident %(x)`: probably a command argument, but it may be a
    /// division by a local variable, so such literals must close on one line.
    CommandArgument,
}

fn is_regex_start(bytes: &[u8], mask: &[u8], start: usize, i: usize) -> bool {
    literal_start(bytes, mask, start, i) != LiteralStart::Operator
}

fn literal_start(bytes: &[u8], mask: &[u8], start: usize, i: usize) -> LiteralStart {
    use LiteralStart::*;
    let Some(prev) = previous_significant(bytes, mask, start, i) else {
        return Value;
    };
    // `:/` (and `:%`) directly after a colon is an operator symbol, e.g.
    // `reduce(:/)`, not the start of a literal.
    if bytes[prev] == b':' && prev + 1 == i && (prev == start || bytes[prev - 1] != b':') {
        return Operator;
    }
    if matches!(
        bytes[prev],
        b'=' | b'('
            | b'['
            | b'{'
            | b','
            | b';'
            | b':'
            | b'!'
            | b'?'
            | b'~'
            | b'&'
            | b'|'
            | b'+'
            | b'-'
            | b'*'
            | b'%'
            | b'^'
            | b'<'
            | b'>'
    ) {
        return Value;
    }
    if !is_word_char(bytes[prev]) {
        return Operator;
    }
    let mut word_start = prev;
    while word_start > start && is_word_char(bytes[word_start - 1]) {
        word_start -= 1;
    }
    // `def /(other)` / `def %(other)` define operator methods.
    if &bytes[word_start..=prev] == b"def" {
        return Operator;
    }
    if matches!(
        &bytes[word_start..=prev],
        b"if"
            | b"unless"
            | b"while"
            | b"until"
            | b"when"
            | b"return"
            | b"yield"
            | b"and"
            | b"or"
            | b"not"
            | b"then"
            | b"do"
            | b"case"
    ) {
        return Value;
    }
    if prev + 1 < i
        && i + 1 < bytes.len()
        && bytes[prev].is_ascii_lowercase()
        && !bytes[i + 1].is_ascii_whitespace()
    {
        CommandArgument
    } else {
        Operator
    }
}

fn find_delimited_end(
    bytes: &[u8],
    mut i: usize,
    end: usize,
    open: u8,
    close: u8,
    regex: bool,
) -> Option<usize> {
    let mut depth = 1usize;
    let mut in_class = false;
    while i < end {
        if bytes[i] == b'\\' {
            i = (i + 2).min(end);
            continue;
        }
        if i + 1 < end && bytes[i] == b'#' && bytes[i + 1] == b'{' {
            if let Some(interpolation_end) = find_interpolation_end(bytes, i + 2, end) {
                i = interpolation_end + 1;
                continue;
            }
        }
        if regex {
            if bytes[i] == b'[' {
                in_class = true;
            } else if bytes[i] == b']' {
                in_class = false;
            }
            if in_class {
                i += 1;
                continue;
            }
        }
        if open != close && bytes[i] == open {
            depth += 1;
        } else if bytes[i] == close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn find_percent_literal_end(
    bytes: &[u8],
    open_pos: usize,
    content_start: usize,
    end: usize,
    open: u8,
    close: u8,
    regex: bool,
    cache: &mut LexCache,
) -> Option<usize> {
    let LexCache {
        percent_ends,
        interpolation_ends,
        steps: lexical_steps,
        ..
    } = cache;
    let cache_key = (open_pos, regex);
    if let Some(cached) = percent_ends.get(&cache_key) {
        return *cached;
    }

    if open == close {
        let mut i = content_start;
        let mut in_class = false;
        while i < end {
            count_lexical_scan_step(lexical_steps);
            if bytes[i] == b'\\' {
                i = (i + 2).min(end);
                continue;
            }
            if i + 1 < end && bytes[i] == b'#' && bytes[i + 1] == b'{' {
                if let Some(interpolation_end) = find_interpolation_end_cached(
                    bytes,
                    i + 2,
                    end,
                    interpolation_ends,
                    lexical_steps,
                ) {
                    i = interpolation_end + 1;
                    continue;
                }
            }
            if regex {
                if bytes[i] == b'[' {
                    in_class = true;
                } else if bytes[i] == b']' {
                    in_class = false;
                }
                if in_class {
                    i += 1;
                    continue;
                }
            }
            if bytes[i] == close {
                return Some(i);
            }
            i += 1;
        }
        percent_ends.insert(cache_key, None);
        return None;
    }

    let mut stack = Vec::with_capacity(8);
    stack.push((open_pos, true));
    let mut i = content_start;
    let mut in_class = false;
    while i < end {
        count_lexical_scan_step(lexical_steps);
        if bytes[i] == b'\\' {
            i = (i + 2).min(end);
            continue;
        }
        if i + 1 < end && bytes[i] == b'#' && bytes[i + 1] == b'{' {
            if let Some(interpolation_end) =
                find_interpolation_end_cached(bytes, i + 2, end, interpolation_ends, lexical_steps)
            {
                i = interpolation_end + 1;
                continue;
            }
        }
        if regex {
            if bytes[i] == b'[' {
                in_class = true;
            } else if bytes[i] == b']' {
                in_class = false;
            }
            if in_class {
                i += 1;
                continue;
            }
        }
        if bytes[i] == open {
            let same_semantics = percent_literal_regex_semantics(bytes, i) == Some(regex);
            stack.push((i, same_semantics));
        } else if bytes[i] == close {
            if let Some((matched_open, is_percent_open)) = stack.pop() {
                if is_percent_open && matched_open != open_pos {
                    percent_ends.insert((matched_open, regex), Some(i));
                }
                if stack.is_empty() {
                    return Some(i);
                }
            }
        }
        i += 1;
    }

    for (unmatched_open, is_percent_open) in stack {
        if is_percent_open {
            percent_ends.insert((unmatched_open, regex), None);
        }
    }
    None
}

/// A heredoc opener parsed by `parse_heredoc`.
struct Heredoc {
    delim_end: usize,
    body_start: usize,
    /// End of the terminator line's content (exclusive).
    body_end: usize,
    interpolated: bool,
}

/// Parse a heredoc opener at `abs` (pointing at `<<`). `body_start` overrides
/// where the body begins, for the second of several heredocs opened on one
/// line (`foo(<<~A, <<~B)`).
fn parse_heredoc(
    bytes: &[u8],
    abs: usize,
    end: usize,
    body_start: Option<usize>,
    cache: &mut LexCache,
) -> Option<Heredoc> {
    if abs > 0
        && !matches!(
            bytes[abs - 1],
            b' ' | b'\t' | b'\n' | b'\r' | b'=' | b'(' | b',' | b'[' | b'{' | b';'
        )
    {
        return None;
    }
    let mut i = abs + 2;
    if i < end && matches!(bytes[i], b'~' | b'-') {
        i += 1;
    }
    let quote = if i < end && matches!(bytes[i], b'\'' | b'"') {
        let quote = bytes[i];
        i += 1;
        Some(quote)
    } else {
        None
    };
    let delim_start = i;
    while i < end && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    if i == delim_start {
        return None;
    }
    let delim_end = i;
    if let Some(quote) = quote {
        if bytes.get(i) != Some(&quote) {
            return None;
        }
        i += 1;
    }
    let line_end = memchr::memchr(b'\n', &bytes[i..end]).map(|n| i + n)?;
    let body_start = body_start.unwrap_or(line_end + 1);
    let delim = &bytes[delim_start..delim_end];
    // Terminator lines found at or after P are also found from any later
    // start, so a failed search from P rules out every later body start.
    let miss_key = (delim.to_vec(), end);
    if cache
        .heredoc_misses
        .get(&miss_key)
        .is_some_and(|&no_terminator_from| body_start >= no_terminator_from)
    {
        return None;
    }
    let mut line_start = body_start;
    while line_start < end {
        count_lexical_scan_step(&mut cache.steps);
        let eol = memchr::memchr(b'\n', &bytes[line_start..end])
            .map(|n| line_start + n)
            .unwrap_or(end);
        let content_end = if eol > line_start && bytes[eol - 1] == b'\r' {
            eol - 1
        } else {
            eol
        };
        let mut content = line_start;
        while content < content_end && matches!(bytes[content], b' ' | b'\t') {
            content += 1;
        }
        if content + delim.len() == content_end && &bytes[content..content_end] == delim {
            return Some(Heredoc {
                delim_end,
                body_start,
                body_end: content_end,
                interpolated: quote != Some(b'\''),
            });
        }
        line_start = if eol < end { eol + 1 } else { eol };
    }
    cache
        .heredoc_misses
        .entry(miss_key)
        .and_modify(|no_terminator_from| {
            *no_terminator_from = (*no_terminator_from).min(body_start)
        })
        .or_insert(body_start);
    None
}

fn mask_ruby_range(bytes: &[u8], mask: &mut [u8], start: usize, end: usize) {
    mask_ruby_range_cached(bytes, mask, start, end, &mut LexCache::default());
}

fn mask_ruby_range_cached(
    bytes: &[u8],
    mask: &mut [u8],
    start: usize,
    end: usize,
    cache: &mut LexCache,
) {
    // (opener line end, where the next heredoc body on that line starts)
    let mut pending_heredoc_body: Option<(usize, usize)> = None;
    let mut i = start;
    while i < end {
        // Heredoc bodies were already lexed (with their interpolations) when
        // the opener was parsed; resume after the last terminator.
        if let Some((opener_line_end, next_body)) = pending_heredoc_body {
            if i > opener_line_end {
                pending_heredoc_body = None;
                if i < next_body {
                    i = next_body;
                    continue;
                }
            }
        }
        if mask[i] != 0 {
            i += 1;
            continue;
        }
        let line_start = i == 0 || bytes[i - 1] == b'\n';
        if line_start && is_embedded_doc_marker(bytes, i, end, b"=begin") {
            let block_end = find_embedded_doc_end(bytes, i + 6, end);
            set_mask(mask, i, block_end, true);
            i = block_end;
            continue;
        }
        if line_start && is_data_marker(bytes, i, end) {
            // Everything after `__END__` is DATA, not code.
            set_mask(mask, i, end, true);
            break;
        }
        match bytes[i] {
            b'?' if i + 1 < end && !bytes[i + 1].is_ascii_whitespace() => {
                i += char_literal_len(bytes, mask, start, i, end);
            }
            // Punctuation globals such as `$"` (loaded features), `$'`, `$/`.
            b'$' if i + 1 < end && is_special_global_char(bytes[i + 1]) => i += 2,
            b'#' => {
                let eol = memchr::memchr(b'\n', &bytes[i..end])
                    .map(|n| i + n)
                    .unwrap_or(end);
                set_mask(mask, i, eol, true);
                i = eol;
            }
            b'\'' | b'"' => {
                let quote = bytes[i];
                let content_start = i + 1;
                let mut close = content_start;
                let mut found_close = false;
                // A literal never extends into bytes already masked (e.g. template
                // text between ERB tags, or heredoc bodies).
                let mut stop = end;
                while close < end {
                    if mask[close] != 0 {
                        stop = close;
                        break;
                    }
                    if bytes[close] == b'\\' {
                        close = (close + 2).min(end);
                    } else if quote == b'"'
                        && close + 1 < end
                        && bytes[close] == b'#'
                        && bytes[close + 1] == b'{'
                    {
                        close = find_interpolation_end_cached(
                            bytes,
                            close + 2,
                            end,
                            &mut cache.interpolation_ends,
                            &mut cache.steps,
                        )
                        .map(|interpolation_end| interpolation_end + 1)
                        .unwrap_or(end);
                    } else if bytes[close] == quote {
                        found_close = true;
                        break;
                    } else {
                        close += 1;
                    }
                }
                let literal_end = if found_close {
                    close
                } else {
                    memchr::memchr(b'\n', &bytes[content_start..stop])
                        .map(|offset| content_start + offset)
                        .unwrap_or(stop)
                };
                set_mask(mask, content_start, literal_end, true);
                if quote == b'"' {
                    expose_interpolations(bytes, mask, content_start, literal_end, cache);
                }
                i = if found_close { close + 1 } else { literal_end };
            }
            // Bare `%(...)` literals behave like `%Q(...)`; only in value
            // position, so `x % (y)` / `a %[b]` style modulo is left alone.
            b'%' if i + 1 < end
                && matches!(bytes[i + 1], b'(' | b'[' | b'{' | b'<' | b'|')
                && is_regex_start(bytes, mask, start, i) =>
            {
                let open = bytes[i + 1];
                let close = match open {
                    b'(' => b')',
                    b'[' => b']',
                    b'{' => b'}',
                    b'<' => b'>',
                    _ => open,
                };
                let content_start = i + 2;
                let line_end = memchr::memchr(b'\n', &bytes[content_start..end])
                    .map(|offset| content_start + offset)
                    .unwrap_or(end);
                // `ident %(x)` may be a modulo by a local variable: bound it.
                let limit = if literal_start(bytes, mask, start, i) == LiteralStart::Value {
                    end
                } else {
                    line_end
                };
                if let Some(literal_end) = find_percent_literal_end(
                    bytes,
                    i + 1,
                    content_start,
                    limit,
                    open,
                    close,
                    false,
                    cache,
                ) {
                    set_mask(mask, content_start, literal_end, true);
                    expose_interpolations(bytes, mask, content_start, literal_end, cache);
                    i = literal_end + 1;
                } else if limit == end {
                    set_mask(mask, content_start, line_end, true);
                    i = line_end;
                } else {
                    i += 1;
                }
            }
            b'%' if i + 2 < end
                && is_percent_literal_type(bytes[i + 1])
                && (i == start || mask[i - 1] != 0 || !is_word_char(bytes[i - 1])) =>
            {
                let (open, close) = match bytes[i + 2] {
                    b'{' => (b'{', b'}'),
                    b'(' => (b'(', b')'),
                    b'[' => (b'[', b']'),
                    b'<' => (b'<', b'>'),
                    delim if !delim.is_ascii_alphanumeric() && !delim.is_ascii_whitespace() => {
                        (delim, delim)
                    }
                    _ => {
                        i += 1;
                        continue;
                    }
                };
                let content_start = i + 3;
                if let Some(literal_end) = find_percent_literal_end(
                    bytes,
                    i + 2,
                    content_start,
                    end,
                    open,
                    close,
                    bytes[i + 1] == b'r',
                    cache,
                ) {
                    set_mask(mask, content_start, literal_end, true);
                    if matches!(bytes[i + 1], b'Q' | b'r' | b'W' | b'I' | b'x') {
                        expose_interpolations(bytes, mask, content_start, literal_end, cache);
                    }
                    i = literal_end + 1;
                } else {
                    let literal_end = memchr::memchr(b'\n', &bytes[content_start..end])
                        .map(|offset| content_start + offset)
                        .unwrap_or(end);
                    set_mask(mask, content_start, literal_end, true);
                    i = literal_end;
                }
            }
            b'/' if is_regex_start(bytes, mask, start, i) => {
                let regex_end = if literal_start(bytes, mask, start, i) == LiteralStart::Value {
                    find_delimited_end(bytes, i + 1, end, b'/', b'/', true)
                } else {
                    find_regex_literal_end(bytes, i, end)
                };
                if let Some(regex_end) = regex_end {
                    set_mask(mask, i + 1, regex_end, true);
                    expose_interpolations(bytes, mask, i + 1, regex_end, cache);
                    i = regex_end + 1;
                } else {
                    i += 1;
                }
            }
            b'<' if i + 1 < end && bytes[i + 1] == b'<' => {
                let line_end = memchr::memchr(b'\n', &bytes[i..end])
                    .map(|n| i + n)
                    .unwrap_or(end);
                let body_start = pending_heredoc_body
                    .filter(|&(opener_line_end, _)| opener_line_end == line_end)
                    .map(|(_, next_body)| next_body);
                if let Some(heredoc) = parse_heredoc(bytes, i, end, body_start, cache) {
                    set_mask(mask, i + 2, heredoc.delim_end, true);
                    set_mask(mask, heredoc.body_start, heredoc.body_end, true);
                    if heredoc.interpolated {
                        expose_interpolations(
                            bytes,
                            mask,
                            heredoc.body_start,
                            heredoc.body_end,
                            cache,
                        );
                    }
                    let next_body = memchr::memchr(b'\n', &bytes[heredoc.body_end..end])
                        .map(|n| heredoc.body_end + n + 1)
                        .unwrap_or(end);
                    pending_heredoc_body = Some((line_end, next_body));
                }
                i += 2;
            }
            _ => {
                i += 1;
                // Bytes that cannot start a token are skipped in bulk; stop at a
                // newline so line-start markers are still checked.
                if bytes[i - 1] != b'\n' {
                    while i < end && !LEX_TOKEN_START[bytes[i] as usize] {
                        i += 1;
                    }
                }
            }
        }
    }
}

/// Bytes handled by a `mask_ruby_range_cached` match arm (plus `\n`).
static LEX_TOKEN_START: [bool; 256] = {
    let mut table = [false; 256];
    let tokens = b"?$#'\"%/<\n";
    let mut i = 0;
    while i < tokens.len() {
        table[tokens[i] as usize] = true;
        i += 1;
    }
    table
};

/// `=begin` / `=end` markers must start a line and be followed by whitespace
/// or EOL (`=ending` is not `=end`).
fn is_embedded_doc_marker(bytes: &[u8], i: usize, end: usize, marker: &[u8]) -> bool {
    bytes[i..end].starts_with(marker)
        && bytes
            .get(i + marker.len())
            .is_none_or(|b| i + marker.len() >= end || b.is_ascii_whitespace())
}

/// End (exclusive, at the newline) of an `=begin` block whose body starts at
/// `from`, or `end` when the block is unterminated.
fn find_embedded_doc_end(bytes: &[u8], mut from: usize, end: usize) -> usize {
    while let Some(n) = memmem::find(&bytes[from..end], b"\n=end") {
        let marker = from + n + 1;
        if is_embedded_doc_marker(bytes, marker, end, b"=end") {
            return memchr::memchr(b'\n', &bytes[marker..end])
                .map(|tail| marker + tail)
                .unwrap_or(end);
        }
        from = marker;
    }
    end
}

/// A line consisting exactly of `__END__` (optionally followed by `\r`).
fn is_data_marker(bytes: &[u8], i: usize, end: usize) -> bool {
    const MARKER: &[u8] = b"__END__";
    if !bytes[i..end].starts_with(MARKER) {
        return false;
    }
    let rest = &bytes[i + MARKER.len()..end];
    rest.is_empty() || rest.starts_with(b"\n") || rest.starts_with(b"\r\n") || rest == b"\r"
}

/// Punctuation that forms a special global variable after `$`.
#[inline]
fn is_special_global_char(b: u8) -> bool {
    matches!(
        b,
        b'\''
            | b'"'
            | b'`'
            | b'!'
            | b'@'
            | b'&'
            | b'+'
            | b'~'
            | b'='
            | b'/'
            | b'\\'
            | b','
            | b';'
            | b'.'
            | b'<'
            | b'>'
            | b'*'
            | b'$'
            | b'?'
            | b':'
    )
}

/// Number of bytes to advance at a `?` (never 0). `?a`, `?'` and `?\n` are
/// character literals; `empty?`, `c ?'a':'b'` and `x ?y:z` are method-name
/// suffixes or ternaries, where the `?` alone is consumed so a following
/// quote still opens a string.
fn char_literal_len(bytes: &[u8], mask: &[u8], start: usize, i: usize, end: usize) -> usize {
    let after_word = i > start && mask[i - 1] == 0 && is_word_char(bytes[i - 1]);
    if after_word {
        return 1;
    }
    if bytes[i + 1] == b'\\' {
        return 3.min(end - i);
    }
    if i + 2 < end && is_word_char(bytes[i + 2]) {
        return 1;
    }
    2
}

/// Closing `/` of a regex literal in command-argument position (`x /2`). The
/// literal must close on the same line unless it is an extended (`/x`) regex,
/// so a division by a local variable never masks the following lines.
fn find_regex_literal_end(bytes: &[u8], open: usize, end: usize) -> Option<usize> {
    let line_end = memchr::memchr(b'\n', &bytes[open..end])
        .map(|n| open + n)
        .unwrap_or(end);
    if let Some(close) = find_delimited_end(bytes, open + 1, line_end, b'/', b'/', true) {
        return Some(close);
    }
    if line_end == end {
        return None;
    }
    let close = find_delimited_end(bytes, open + 1, end, b'/', b'/', true)?;
    let flags = bytes[close + 1..end]
        .iter()
        .take_while(|b| b.is_ascii_alphabetic())
        .copied()
        .collect::<Vec<u8>>();
    flags.contains(&b'x').then_some(close)
}

fn mask_non_ruby(bytes: &[u8], mask: &mut [u8], line_kind: LineCommentKind) {
    let mut line_start = 0;
    while line_start < bytes.len() {
        let eol = memchr::memchr(b'\n', &bytes[line_start..])
            .map(|n| line_start + n)
            .unwrap_or(bytes.len());
        let mut ws = line_start;
        while ws < eol && matches!(bytes[ws], b' ' | b'\t') {
            ws += 1;
        }
        let comment = match line_kind {
            LineCommentKind::Hash => ws < eol && bytes[ws] == b'#',
            LineCommentKind::Slash => ws < eol && bytes[ws] == b'/',
            LineCommentKind::DashHash => ws + 1 < eol && bytes[ws] == b'-' && bytes[ws + 1] == b'#',
        };
        if comment {
            set_mask(mask, ws, eol, true);
        } else if line_kind == LineCommentKind::Hash {
            let mut i = line_start;
            while i < eol {
                if matches!(bytes[i], b'\'' | b'"') {
                    let quote = bytes[i];
                    let start = i + 1;
                    i = start;
                    while i < eol && bytes[i] != quote {
                        i += if bytes[i] == b'\\' { 2 } else { 1 };
                    }
                    set_mask(mask, start, i.min(eol), true);
                } else if bytes[i] == b'#' {
                    set_mask(mask, i, eol, true);
                    break;
                }
                i += 1;
            }
        }
        line_start = if eol < bytes.len() { eol + 1 } else { eol };
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

/// Build a unified per-byte skip mask for tests and non-template callers.
///
/// `mask[i] != 0` means position `i` should be ignored by all token-level
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
#[cfg(test)]
fn build_skip_mask(bytes: &[u8], is_ruby: bool, line_kind: LineCommentKind) -> Vec<u8> {
    build_skip_mask_for_template(bytes, is_ruby, line_kind, TemplateKind::None)
}

#[cfg(test)]
fn build_ruby_skip_mask_with_lexical_steps(bytes: &[u8]) -> (Vec<u8>, usize) {
    let mut mask = vec![0; bytes.len()];
    let mut cache = LexCache::default();
    mask_ruby_range_cached(bytes, &mut mask, 0, bytes.len(), &mut cache);
    (mask, cache.steps)
}

fn build_skip_mask_for_template(
    bytes: &[u8],
    is_ruby: bool,
    line_kind: LineCommentKind,
    template_kind: TemplateKind,
) -> Vec<u8> {
    let mut mask = vec![0; bytes.len()];
    apply_template_mask(bytes, &mut mask, template_kind);
    if is_ruby {
        mask_ruby_range(bytes, &mut mask, 0, bytes.len());
    } else {
        mask_non_ruby(bytes, &mut mask, line_kind);
    }
    mask
}

/// O(1) replacement for `is_commented` using a precomputed mask.
#[inline]
fn is_skipped(mask: &[u8], pos: usize) -> bool {
    mask.get(pos).copied().unwrap_or(0) != 0
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

/// Scan for `Gem.bin_path('name')` or `Gem.activate_bin_path 'name'`
/// Matches regex: `Gem\.(?:activate_)?bin_path\s*\(?\s*['"]([^'"]+)['"]`
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
        // Optional `(`: `Gem.activate_bin_path "x", "y"` is common in binstubs.
        i = skip_whitespace(bytes, i);
        if i < bytes.len() && bytes[i] == b'(' {
            i += 1;
            i = skip_whitespace(bytes, i);
        }
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

#[cfg(test)]
fn scan_bundler_require(
    bytes: &[u8],
    bundler_groups: &mut Vec<String>,
    bundler_require_all: &mut bool,
) {
    let mask = build_skip_mask(bytes, true, LineCommentKind::Hash);
    scan_bundler_require_filtered(bytes, &mask, bundler_groups, bundler_require_all);
}

/// Filtered wrappers that skip matches in commented/string/heredoc regions
/// using a precomputed `mask`. For non-Ruby files the caller still passes a
/// mask built with appropriate `LineCommentKind` so YAML `#` comments etc.
/// continue to be filtered.
fn scan_requires_filtered(bytes: &[u8], mask: &[u8], results: &mut Vec<String>) {
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

fn scan_autoloads_filtered(bytes: &[u8], mask: &[u8], results: &mut Vec<String>) {
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

fn scan_gem_bin_filtered(bytes: &[u8], mask: &[u8], results: &mut Vec<String>) {
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
        // Optional `(`: `Gem.activate_bin_path "x", "y"` is common in binstubs.
        i = skip_whitespace(bytes, i);
        if i < bytes.len() && bytes[i] == b'(' {
            i += 1;
            i = skip_whitespace(bytes, i);
        }
        if let Some((s, _end)) = extract_quoted(bytes, i) {
            if let Ok(name) = std::str::from_utf8(s) {
                results.push(name.to_string());
            }
        }
        pos = abs + 1;
    }
}

/// Small, fast non-cryptographic hasher (FxHash) for short per-file keys.
/// Inputs are local source files, so HashDoS resistance is not needed.
#[derive(Default, Clone, Copy)]
struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl std::hash::Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for chunk in chunks {
            self.add(u64::from_le_bytes(*chunk));
        }
        if !rest.is_empty() {
            let mut word = [0u8; 8];
            word[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(word));
        }
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.add(value as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

/// Per-file set of unique constant names, borrowed from the file bytes so
/// repeated occurrences never allocate.
type FileModuleSet<'a> = HashSet<&'a [u8], std::hash::BuildHasherDefault<FxHasher>>;

/// Merge a file's unique constant names into a module map. When `keep_list` is
/// set the owned names are also returned (reused for the file's cache entry).
fn merge_file_modules(
    names: FileModuleSet<'_>,
    modules: &mut HashMap<String, HashSet<Arc<str>>>,
    rel_path: &Arc<str>,
    keep_list: bool,
) -> Vec<String> {
    let mut owned = Vec::with_capacity(if keep_list { names.len() } else { 0 });
    for name in names {
        // Constant names are ASCII by construction; middleware strings may not be.
        let Ok(name) = std::str::from_utf8(name) else {
            continue;
        };
        if let Some(files) = modules.get_mut(name) {
            files.insert(Arc::clone(rel_path));
        } else {
            let mut files = HashSet::with_capacity(1);
            files.insert(Arc::clone(rel_path));
            modules.insert(name.to_string(), files);
        }
        if keep_list {
            owned.push(name.to_string());
        }
    }
    owned
}

#[cfg(test)]
fn scan_modules_filtered_into(
    bytes: &[u8],
    mask: &[u8],
    modules: &mut HashMap<String, HashSet<Arc<str>>>,
    rel_path: &Arc<str>,
) {
    let mut names = FileModuleSet::default();
    collect_module_names(bytes, mask, &mut names);
    merge_file_modules(names, modules, rel_path, false);
}

/// An uppercase word at `i` is not a constant reference when it names an
/// instance/class/global variable (`@Beta`, `@@Beta`, `$Alpha`) or a symbol
/// (`:Beta`; `::Beta` is a top-level constant).
#[inline]
fn can_start_constant(bytes: &[u8], i: usize) -> bool {
    match i.checked_sub(1).map(|prev| bytes[prev]) {
        Some(b'@' | b'$') => false,
        Some(b':') => i >= 2 && bytes[i - 2] == b':',
        _ => true,
    }
}

fn collect_module_names<'a>(bytes: &'a [u8], mask: &[u8], names: &mut FileModuleSet<'a>) {
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
            if is_skipped(mask, i) || !can_start_constant(bytes, i) {
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
            // `Alpha: 1` is a hash key / keyword label, not a constant.
            let is_label =
                bytes.get(match_end) == Some(&b':') && bytes.get(match_end + 1) != Some(&b':');
            if match_end > start && !is_label {
                names.insert(&bytes[start..match_end]);
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
    mask: &[u8],
    bundler_groups: &mut Vec<String>,
    bundler_require_all: &mut bool,
) {
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
        let mut start = skip_whitespace_inline(bytes, after);
        let parenthesized = bytes.get(start) == Some(&b'(');
        if parenthesized {
            start += 1;
        }
        let mut end = start;
        let mut depth = 0usize;
        while end < bytes.len() {
            if !is_skipped(mask, end) {
                match bytes[end] {
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' if depth == 0 => break,
                    b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                    b'\n' | b';' if !parenthesized && depth == 0 => break,
                    _ => {}
                }
            }
            end += 1;
        }
        apply_bundler_require_args(
            &bytes[start..end],
            &mask[start..end],
            bytes,
            mask,
            bundler_groups,
            bundler_require_all,
        );
        pos = abs + 1;
    }
}

/// Interpret the bounded arguments of `Bundler.require`.
/// Explicit group literals are always recorded, including
/// alongside `Rails.env` (`Bundler.require(:default, :assets, Rails.env)`);
/// `Rails.groups(...)` conditions are recorded separately and are not treated
/// as unconditional groups.
fn apply_bundler_require_args(
    args: &[u8],
    args_mask: &[u8],
    bytes: &[u8],
    mask: &[u8],
    bundler_groups: &mut Vec<String>,
    bundler_require_all: &mut bool,
) {
    let mut start = 0;
    let mut depth = 0usize;
    let mut has_args = false;
    let mut explicit = Vec::new();
    for end in 0..=args.len() {
        if end < args.len() && !is_skipped(args_mask, end) {
            match args[end] {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        if end != args.len() && (args[end] != b',' || depth != 0 || is_skipped(args_mask, end)) {
            continue;
        }
        let mut arg_start = start;
        let mut arg_end = end;
        start = end + 1;
        while arg_start < arg_end
            && (args[arg_start].is_ascii_whitespace() || is_skipped(args_mask, arg_start))
        {
            arg_start += 1;
        }
        while arg_end > arg_start
            && (args[arg_end - 1].is_ascii_whitespace() || is_skipped(args_mask, arg_end - 1))
        {
            arg_end -= 1;
        }
        let Ok(arg) = std::str::from_utf8(&args[arg_start..arg_end]) else {
            continue;
        };
        if arg.is_empty() {
            continue;
        }
        has_args = true;
        let value = arg.strip_prefix('*').map(str::trim_start).unwrap_or(arg);
        if value.starts_with("Rails.groups") {
            push_rails_group_conditions(arg, bundler_groups);
            *bundler_require_all = true;
        } else if value == "Rails.env" {
            *bundler_require_all = true;
        } else if arg.starts_with('*') {
            explicit.push("default".to_string());
            if let Some(var) = splat_var_name(arg) {
                if push_rails_groups_for_splat_var(bytes, mask, var, bundler_groups) {
                    *bundler_require_all = true;
                }
            }
        } else {
            let literal = arg.strip_prefix(':').unwrap_or(arg);
            if literal.starts_with(['\'', '"']) {
                // Prism decodes escapes and rejects interpolated/dynamic names.
                let parsed = ruby_prism::parse(arg.as_bytes());
                if parsed.errors().next().is_some() {
                    continue;
                }
                let node = parsed.node();
                if let Some(program) = node.as_program_node() {
                    let statements = program.statements();
                    let mut body = statements.body().iter();
                    if let Some(value) = body.next().filter(|_| body.next().is_none()) {
                        let name = value
                            .as_string_node()
                            .map(|s| String::from_utf8_lossy(s.unescaped()).into_owned())
                            .or_else(|| {
                                value
                                    .as_symbol_node()
                                    .map(|s| String::from_utf8_lossy(s.unescaped()).into_owned())
                            });
                        if let Some(name) = name {
                            explicit.push(name);
                        }
                    }
                }
            } else {
                let name = arg
                    .strip_prefix(':')
                    .filter(|name| !name.is_empty() && name.bytes().all(is_word_char))
                    .map(str::to_string);
                if let Some(name) = name {
                    explicit.push(name);
                }
            }
        }
    }
    if !has_args {
        explicit.push("default".to_string());
    }
    bundler_groups.extend(explicit);
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
    mask: &[u8],
    var: &str,
    bundler_groups: &mut Vec<String>,
) -> bool {
    let mut aliases_rails_groups = false;
    let mut line_start = 0;
    for line in bytes.split(|&b| b == b'\n') {
        let offset = line_start;
        line_start += line.len() + 1;
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        if !line.contains(var) {
            continue;
        }
        // Skip commented-out or quoted assignments such as
        // `# groups = Rails.groups(assets: %w(development))`.
        let Some(call) = line.find("Rails.groups") else {
            continue;
        };
        if is_skipped(mask, offset + call) {
            continue;
        }
        aliases_rails_groups = true;
        push_rails_group_conditions(line, bundler_groups);
    }
    aliases_rails_groups
}

fn push_rails_group_conditions(source: &str, bundler_groups: &mut Vec<String>) {
    static CONDITION_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r#"(?x)
            :?(\w+)\s*(?::|=>)\s*
            (?:
                %w\[([^\]]*)\] |
                %w\(([^)]*)\) |
                %w\{([^}]*)\} |
                %w<([^>]*)> |
                \[([^\]]*)\]
            )
            "#,
        )
        .unwrap()
    });
    static ARRAY_ITEM_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#":(\w+)|['"]([^'"]+)['"]"#).unwrap());
    for cap in CONDITION_RE.captures_iter(source) {
        let value = (2..=6).find_map(|index| cap.get(index)).map(|m| m.as_str());
        let Some(value) = value else {
            continue;
        };
        let environments = if cap.get(6).is_some() {
            ARRAY_ITEM_RE
                .captures_iter(value)
                .filter_map(|item| item.get(1).or_else(|| item.get(2)))
                .map(|item| item.as_str())
                .collect::<Vec<_>>()
                .join(",")
        } else {
            value
                .split_whitespace()
                .filter(|env| !env.is_empty())
                .collect::<Vec<_>>()
                .join(",")
        };
        bundler_groups.push(format!(
            "{RAILS_GROUP_CONDITION_PREFIX}{}:{environments}",
            &cap[1]
        ));
    }
}

/// Scan for middleware string-literal references (e.g. `config.middleware.use "Bullet::Rack"`)
/// and insert each quoted module name into `modules`.
#[cfg(test)]
fn scan_middleware_refs(
    bytes: &[u8],
    mask: &[u8],
    modules: &mut HashMap<String, HashSet<Arc<str>>>,
    rel_path: &Arc<str>,
) {
    let mut names = FileModuleSet::default();
    collect_middleware_refs(bytes, mask, &mut names);
    merge_file_modules(names, modules, rel_path, false);
}

fn collect_middleware_refs<'a>(bytes: &'a [u8], mask: &[u8], names: &mut FileModuleSet<'a>) {
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

        collect_middleware_call_strings(bytes, mask, line_start + candidate, eol, names);
        line_start = if eol < bytes.len() { eol + 1 } else { eol };
    }
}

/// Collect string-literal middleware names passed to middleware-stack calls
/// between `from` and `eol`. Only the argument slots that name a middleware
/// are considered (`use X, *args` → first; `insert_before pos, X` / `swap`
/// → first two), and the walk stops at a comment, so strings in trailing
/// conditions, constructor arguments or comments are ignored.
fn collect_middleware_call_strings<'a>(
    bytes: &'a [u8],
    mask: &[u8],
    from: usize,
    eol: usize,
    names: &mut FileModuleSet<'a>,
) {
    // (max middleware-naming args, current arg index, paren depth of the call)
    let mut call: Option<(usize, usize, i32)> = None;
    let mut at_arg_start = false;
    let mut depth = 0i32;
    let mut i = from;
    while i < eol {
        let b = bytes[i];
        if is_skipped(mask, i) {
            // A comment (string interiors are consumed with their quotes).
            return;
        }
        if b == b'"' || b == b'\'' {
            let start = i + 1;
            let mut end = start;
            while end < eol && bytes[end] != b {
                end += if bytes[end] == b'\\' { 2 } else { 1 };
            }
            if end >= eol {
                return;
            }
            if let Some((max_args, arg, _)) = call {
                if at_arg_start && arg < max_args {
                    let name = bytes[start..end].trim_ascii();
                    if name.first().is_some_and(u8::is_ascii_uppercase) {
                        names.insert(name);
                    }
                }
            }
            at_arg_start = false;
            i = end + 1;
            continue;
        }
        if is_word_char(b) && (i == 0 || !is_word_char(bytes[i - 1])) {
            let mut word_end = i;
            while word_end < eol && is_word_char(bytes[word_end]) {
                word_end += 1;
            }
            let max_args = match &bytes[i..word_end] {
                b"use" | b"unshift" | b"push" | b"append" | b"prepend" => Some(1),
                b"insert" | b"insert_before" | b"insert_after" | b"swap" | b"move_before"
                | b"move_after" => Some(2),
                _ => None,
            };
            if let Some(max_args) = max_args.filter(|_| bytes.get(word_end) != Some(&b':')) {
                call = Some((max_args, 0, depth));
                at_arg_start = true;
            } else {
                at_arg_start = false;
            }
            i = word_end;
            continue;
        }
        match b {
            b'<' if bytes.get(i + 1) == Some(&b'<') => {
                call = Some((1, 0, depth));
                at_arg_start = true;
                i += 2;
                continue;
            }
            b' ' | b'\t' | b'\r' => {}
            b'(' if at_arg_start && call.is_some_and(|(_, arg, d)| arg == 0 && d == depth) => {
                depth += 1;
                if let Some(call) = call.as_mut() {
                    call.2 = depth;
                }
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                at_arg_start = false;
            }
            b')' | b']' | b'}' => {
                if call.is_some_and(|(_, _, d)| d == depth) {
                    call = None;
                }
                depth -= 1;
                at_arg_start = false;
            }
            b',' => {
                if let Some(call) = call.as_mut().filter(|call| call.2 == depth) {
                    call.1 += 1;
                    at_arg_start = true;
                } else {
                    at_arg_start = false;
                }
            }
            _ => at_arg_start = false,
        }
        i += 1;
    }
}

/// Collect Active Storage service names from attachment declarations such as
/// `has_one_attached :avatar, service: :amazon` (including multi-line calls and
/// lambdas like `service: ->(u) { u.eu? ? :europe : :usa }`). Only unmasked
/// code is considered, so comments and string contents are ignored.
fn collect_attachment_services(bytes: &[u8], mask: &[u8], services: &mut Vec<String>) {
    const MAX_STATEMENT_LINES: usize = 20;
    let finder = &*ATTACHED_FINDER;
    let mut pos = 0;
    while let Some(idx) = finder.find(&bytes[pos..]) {
        let hit = pos + idx;
        pos = hit + 1;
        let start = if hit >= 7 && &bytes[hit - 7..hit] == b"has_one" {
            hit - 7
        } else if hit >= 8 && &bytes[hit - 8..hit] == b"has_many" {
            hit - 8
        } else {
            continue;
        };
        let after = hit + b"_attached".len();
        if is_skipped(mask, start)
            || (start > 0 && (is_word_char(bytes[start - 1]) || bytes[start - 1] == b':'))
            || bytes.get(after).is_some_and(|&b| is_word_char(b))
        {
            continue;
        }

        // Walk the call's argument list until the statement ends.
        let mut i = after;
        let mut depth = 0i32;
        let mut lines = 0;
        let mut last_significant = 0u8;
        let mut service_depth: Option<i32> = None;
        while i < bytes.len() {
            let b = bytes[i];
            if is_skipped(mask, i) {
                i += 1;
                continue;
            }
            match b {
                b'\n' => {
                    if depth <= 0
                        && !matches!(last_significant, b',' | b'\\' | b'(' | b'{' | b'[' | b':')
                    {
                        break;
                    }
                    lines += 1;
                    if lines > MAX_STATEMENT_LINES {
                        break;
                    }
                }
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth < 0 {
                        break;
                    }
                    if service_depth.is_some_and(|d| depth < d) {
                        service_depth = None;
                    }
                }
                b',' if service_depth == Some(depth) => service_depth = None,
                b';' if depth == 0 => break,
                b'd' if depth == 0
                    && bytes[i..].starts_with(b"do")
                    && !is_word_char(bytes[i - 1])
                    && !bytes.get(i + 2).is_some_and(|&c| is_word_char(c)) =>
                {
                    break;
                }
                b's' if service_depth.is_none()
                    && bytes[i..].starts_with(b"service")
                    && !is_word_char(bytes[i - 1]) =>
                {
                    let key_end = i + b"service".len();
                    let label =
                        bytes.get(key_end) == Some(&b':') && bytes.get(key_end + 1) != Some(&b':');
                    let rocket = bytes[i - 1] == b':' && {
                        let arrow = skip_whitespace_inline(bytes, key_end);
                        bytes[arrow..].starts_with(b"=>")
                    };
                    if label || rocket {
                        service_depth = Some(depth);
                        last_significant = b':';
                        i = if label {
                            key_end + 1
                        } else {
                            skip_whitespace_inline(bytes, key_end) + 2
                        };
                        let value = skip_whitespace_inline(bytes, i);
                        if let Some((name, _)) = extract_quoted(bytes, value) {
                            if let Ok(name) = std::str::from_utf8(name) {
                                services.push(name.to_string());
                            }
                        }
                        continue;
                    }
                }
                b':' if service_depth.is_some()
                    && bytes.get(i + 1).is_some_and(|&c| is_word_char(c))
                    && !is_word_char(bytes[i - 1])
                    && bytes[i - 1] != b':' =>
                {
                    let sym_start = i + 1;
                    let mut sym_end = sym_start;
                    while sym_end < bytes.len() && is_word_char(bytes[sym_end]) {
                        sym_end += 1;
                    }
                    if let Ok(name) = std::str::from_utf8(&bytes[sym_start..sym_end]) {
                        services.push(name.to_string());
                    }
                    last_significant = b'_';
                    i = sym_end;
                    continue;
                }
                _ => {}
            }
            if !b.is_ascii_whitespace() {
                last_significant = b;
            }
            i += 1;
        }
    }
}

fn collect_storage_config_services(bytes: &[u8], mask: &[u8], services: &mut Vec<String>) {
    for start in memmem::find_iter(bytes, b"config") {
        if is_skipped(mask, start)
            || (start > 0
                && (is_word_char(bytes[start - 1])
                    || matches!(bytes[start - 1], b'.' | b':' | b'@' | b'$')))
        {
            continue;
        }
        let mut i = start;
        let mut matched = true;
        for token in [
            b"config".as_slice(),
            b".",
            b"active_storage",
            b".",
            b"service",
            b"=",
        ] {
            i = skip_whitespace(bytes, i);
            if !bytes[i..].starts_with(token)
                || (i..i + token.len()).any(|pos| is_skipped(mask, pos))
            {
                matched = false;
                break;
            }
            i += token.len();
        }
        if !matched {
            continue;
        }
        i = skip_whitespace(bytes, i);
        if i >= bytes.len() || is_skipped(mask, i) {
            continue;
        }
        if bytes[i] == b':' {
            i += 1;
            let start = i;
            while i < bytes.len() && is_word_char(bytes[i]) && !is_skipped(mask, i) {
                i += 1;
            }
            if i > start {
                services.push(String::from_utf8_lossy(&bytes[start..i]).into_owned());
                continue;
            }
        }
        if let Some((name, _)) = extract_quoted(bytes, i) {
            if !name.contains(&b'\\') && !name.windows(2).any(|pair| pair == b"#{") {
                services.push(String::from_utf8_lossy(name).into_owned());
            }
        }
    }
}

#[inline]
fn skip_whitespace_inline(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    i
}

/// Type alias for cache update entries collected during the walk
pub type CacheUpdate = crate::cache::CacheUpdate;

type ScriptFileList = Vec<(String, Vec<u8>)>;
type ModuleMap = Vec<HashMap<String, HashSet<Arc<str>>>>;

/// Default walker thread count. Conservative cap of 4 chosen because raising
/// it globally regresses warm-cache and large generated-source workloads even
/// though it helps large cold scans (see issue #13).
pub fn default_walker_threads() -> usize {
    rayon::current_num_threads().clamp(1, 4)
}

/// Public entry point using the default (capped) walker thread count.
#[allow(dead_code)]
pub fn scan_source_tree(
    dir: &Path,
    skip_dirs: &[&str],
    cache: Arc<crate::cache::FileCache>,
) -> (ScanResults, Vec<CacheUpdate>) {
    scan_source_tree_with_threads(dir, skip_dirs, cache, None)
}

/// Scan with an explicit walker thread count. `None` uses the conservative
/// default cap; `Some(n)` overrides it (clamped to at least 1). Intended for
/// CLI `--jobs`, tests, and thread-sweep benchmarks so callers can vary the
/// walker fan-out without mutating a global thread pool.
pub fn scan_source_tree_with_threads(
    dir: &Path,
    skip_dirs: &[&str],
    cache: Arc<crate::cache::FileCache>,
    threads: Option<usize>,
) -> (ScanResults, Vec<CacheUpdate>) {
    scan_source_tree_with_threads_and_explicit_skips(dir, skip_dirs, &[], cache, threads)
}

pub fn scan_source_tree_with_threads_and_explicit_skips(
    dir: &Path,
    skip_dirs: &[&str],
    explicit_skip_dirs: &[&str],
    cache: Arc<crate::cache::FileCache>,
    threads: Option<usize>,
) -> (ScanResults, Vec<CacheUpdate>) {
    let skip_set: HashSet<String> = skip_dirs.iter().map(|s| s.to_string()).collect();
    let explicit_skip_set: HashSet<String> =
        explicit_skip_dirs.iter().map(|s| s.to_string()).collect();
    // Entries with a path separator (`app/legacy`) match the directory's path
    // relative to the scan root rather than its name.
    let path_skips: Vec<std::path::PathBuf> = skip_dirs
        .iter()
        .chain(explicit_skip_dirs)
        .filter(|skip| skip.contains('/') || skip.contains('\\'))
        .map(|skip| {
            let skip = skip.trim_end_matches(['/', '\\']);
            let skip = skip.strip_prefix("./").unwrap_or(skip);
            std::path::PathBuf::from(skip)
        })
        .collect();
    let walk_root = dir.to_path_buf();
    let dir_str = dir.to_string_lossy().to_string();
    let use_cache = cache.is_enabled();

    let walker_threads = threads
        .map(|n| n.max(1))
        .unwrap_or_else(default_walker_threads);

    let mut builder = ignore::WalkBuilder::new(dir);
    builder
        .threads(walker_threads)
        .standard_filters(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        // Symlinked files and directories are scanned like regular ones; the
        // walker reports symlink loops as errors, which are skipped.
        .follow_links(true)
        .hidden(false)
        .filter_entry(move |entry| {
            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                if !path_skips.is_empty() {
                    let rel = entry
                        .path()
                        .strip_prefix(&walk_root)
                        .unwrap_or(entry.path());
                    if path_skips.iter().any(|skip| skip.as_path() == rel) {
                        return false;
                    }
                }
                let name = entry.file_name().to_string_lossy();
                let is_vendor_bundle = name == "bundle"
                    && entry
                        .path()
                        .parent()
                        .and_then(Path::file_name)
                        .is_some_and(|parent| parent == "vendor");
                if is_vendor_bundle {
                    return false;
                }
                let is_builtin = DEFAULT_SKIP_DIRS.contains(&name.as_ref());
                let skip_at_any_depth = explicit_skip_set.contains(name.as_ref())
                    || !is_builtin
                    || SKIP_DIRS_AT_ANY_DEPTH.contains(&name.as_ref());
                if skip_set.contains(name.as_ref()) && (entry.depth() == 1 || skip_at_any_depth) {
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

                    let is_source = is_scannable(&path_str, path);
                    let is_script = crate::scripts::is_script_file(&rel_path_cow, &file_name);

                    // Check if it's a scannable source file
                    if is_source {
                        // Use metadata from DirEntry (avoids redundant stat syscall)
                        let metadata = entry.metadata().ok();
                        let cache_timestamp = metadata
                            .as_ref()
                            .map(crate::cache::cache_timestamp)
                            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                        // Fix #9: take() consumes the entry on a validated hit and we
                        // immediately reinsert it back into the cache map (without marking
                        // it dirty). This avoids cloning the entry's Vec<String> fields on
                        // the hot path while preserving the "no spurious updates" contract.
                        let cache_hit = if use_cache {
                            if let Some(ref md) = metadata {
                                cache_shared.take(&rel_path_cow, cache_timestamp, md.len(), path)
                            } else {
                                None
                            }
                        } else {
                            None
                        };

                        if let Some(entry) = cache_hit {
                            // Cache hit: reconstruct FileResult + modules without rescanning.
                            let rel: Arc<str> = Arc::from(rel_path_cow.as_ref());
                            for m in &entry.modules {
                                if let Some(files) = flush_buf.local_modules.get_mut(m.as_str()) {
                                    files.insert(Arc::clone(&rel));
                                } else {
                                    flush_buf
                                        .local_modules
                                        .insert(m.clone(), HashSet::from([Arc::clone(&rel)]));
                                }
                            }
                            // Build FileResult by cloning small Vec<String>s; reinsert
                            // owned entry back into cache so durability is preserved.
                            let file_result = FileResult {
                                rel_path: rel.to_string(),
                                requires: entry.requires.clone(),
                                modules: Vec::new(),
                                bundler_groups: entry.bundler_groups.clone(),
                                bundler_require_all: entry.bundler_require_all,
                                storage_services: entry.storage_services.clone(),
                            };
                            cache_shared.reinsert(rel.to_string(), entry);
                            flush_buf.buf.push(file_result);
                        } else {
                            // Cache miss: scan file

                            if let Some(mut file_result) = scan_file_with(
                                &path_str,
                                &dir_ref,
                                &mut flush_buf.local_modules,
                                use_cache,
                            ) {
                                // The file's own unique module list doubles as its
                                // cache entry (no scan of the thread-wide map).
                                let file_modules = std::mem::take(&mut file_result.modules);
                                if use_cache {
                                    if let Some(ref md) = metadata {
                                        flush_buf.cache_buf.push(CacheUpdate {
                                            rel_path: file_result.rel_path.clone(),
                                            mtime: cache_timestamp,
                                            size: md.len(),
                                            requires: file_result.requires.clone(),
                                            modules: file_modules,
                                            bundler_groups: file_result.bundler_groups.clone(),
                                            bundler_require_all: file_result.bundler_require_all,
                                            storage_services: file_result.storage_services.clone(),
                                        });
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
                    if is_script {
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
    let mut storage_services: HashSet<String> = HashSet::new();

    for r in all_results {
        storage_services.extend(r.storage_services);
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
            storage_services,
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
        // Extensionless files with Ruby shebang. The walker only passes regular
        // files, so no extra stat is needed; names that are never executable
        // Ruby scripts are rejected before opening the file.
        if full_path.extension().is_none() && may_be_ruby_script_name(name_bytes) {
            if let Ok(mut f) = std::fs::File::open(full_path) {
                use std::io::Read;
                let mut buf = [0u8; 64];
                if let Ok(n) = f.read(&mut buf) {
                    return has_ruby_shebang(&buf[..n]);
                }
            }
        }
    }

    false
}

fn has_ruby_shebang(bytes: &[u8]) -> bool {
    let prefix = &bytes[..bytes.len().min(64)];
    let end = memchr::memchr(b'\n', prefix).unwrap_or(prefix.len());
    prefix.starts_with(b"#!") && prefix[..end].windows(4).any(|w| w == b"ruby")
}

/// Extensionless names that are never Ruby scripts even though they cannot be
/// ruled out by extension (placeholders, docs, and DSL files without shebangs).
const NON_SCRIPT_FILENAMES: &[&str] = &[
    "Gemfile",
    "Makefile",
    "Dockerfile",
    "Brewfile",
    "Podfile",
    "Fastfile",
    "Appfile",
    "Dangerfile",
    "Steepfile",
    "Containerfile",
    "Justfile",
];

/// Cheap name filter applied before opening an extensionless file to look for
/// a Ruby shebang: skips dotfiles (`.keep`, `.gitkeep`), all-caps docs
/// (`LICENSE`, `README`, `VERSION`) and well-known non-script names.
fn may_be_ruby_script_name(name: &[u8]) -> bool {
    if name.is_empty() || name[0] == b'.' {
        return false;
    }
    let all_caps = name
        .iter()
        .all(|&b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if all_caps {
        return false;
    }
    !NON_SCRIPT_FILENAMES.iter().any(|f| f.as_bytes() == name)
}

// (FileContent enum removed — plain read() is faster for single sequential reads)

#[cfg(test)]
fn scan_file(
    path: &str,
    dir: &str,
    thread_modules: &mut HashMap<String, HashSet<Arc<str>>>,
) -> Option<FileResult> {
    scan_file_with(path, dir, thread_modules, true)
}

/// `keep_module_list` fills `FileResult::modules` with the file's unique
/// constant names (needed only when the result is written to the cache).
fn scan_file_with(
    path: &str,
    dir: &str,
    thread_modules: &mut HashMap<String, HashSet<Arc<str>>>,
    keep_module_list: bool,
) -> Option<FileResult> {
    // Plain read() is faster than mmap for single sequential reads (no VMA/TLB overhead)
    let bytes = std::fs::read(path).ok()?;
    scan_file_bytes_with(path, dir, &bytes, thread_modules, keep_module_list)
}

#[allow(dead_code)]
fn scan_file_bytes(
    path: &str,
    dir: &str,
    bytes: &[u8],
    thread_modules: &mut HashMap<String, HashSet<Arc<str>>>,
) -> Option<FileResult> {
    scan_file_bytes_with(path, dir, bytes, thread_modules, true)
}

fn scan_file_bytes_with(
    path: &str,
    dir: &str,
    bytes: &[u8],
    thread_modules: &mut HashMap<String, HashSet<Arc<str>>>,
    keep_module_list: bool,
) -> Option<FileResult> {
    if bytes.is_empty() {
        return None;
    }

    let rel_path = Path::new(path)
        .strip_prefix(dir)
        .unwrap_or(Path::new(path))
        .display()
        .to_string();
    // Fix #10: intern rel_path as Arc<str> so each per-module insertion is a
    // cheap Arc::clone instead of a fresh String allocation.
    let rel_path_arc: Arc<str> = Arc::from(rel_path.as_str());

    let full_path = Path::new(path);
    let (is_ruby, is_yaml, template_kind) = classify_file(full_path, bytes);
    let is_slim = template_kind == TemplateKind::Slim;
    let is_haml = template_kind == TemplateKind::Haml;

    // Fix #4: Slim line comments use `/`, Haml line comments use `-#`.
    // Other supported file types (Ruby, YAML, ERB, shell etc.) use `#`.
    let line_kind = if is_slim {
        LineCommentKind::Slash
    } else if is_haml {
        LineCommentKind::DashHash
    } else {
        LineCommentKind::Hash
    };

    let has_ruby_scan_tokens = REQUIRE_FINDER.find(bytes).is_some()
        || AUTOLOAD_FINDER.find(bytes).is_some()
        || GEM_DOT_FINDER.find(bytes).is_some()
        || BUNDLER_REQUIRE_FINDER.find(bytes).is_some();

    // YAML does not use module scanning, so files without Ruby scan tokens can
    // skip the full-size comment/string mask and all four Ruby token scanners.
    let mask = if !is_yaml || has_ruby_scan_tokens {
        build_skip_mask_for_template(
            bytes,
            is_ruby || template_kind != TemplateKind::None,
            line_kind,
            template_kind,
        )
    } else {
        Vec::new()
    };

    let mut requires = Vec::new();
    let mut bundler_groups = Vec::new();
    let mut bundler_require_all = false;
    let mut storage_services = Vec::new();

    if is_ruby && ATTACHED_FINDER.find(bytes).is_some() {
        collect_attachment_services(bytes, &mask, &mut storage_services);
    }
    let rel = Path::new(&rel_path);
    if is_ruby
        && (rel == Path::new("config/application.rb")
            || (rel.parent() == Some(Path::new("config/environments"))
                && rel.extension().is_some_and(|ext| ext == "rb")))
    {
        collect_storage_config_services(bytes, &mask, &mut storage_services);
    }

    if has_ruby_scan_tokens {
        scan_requires_filtered(bytes, &mask, &mut requires);
        scan_autoloads_filtered(bytes, &mask, &mut requires);
        scan_gem_bin_filtered(bytes, &mask, &mut requires);
        scan_bundler_require_filtered(bytes, &mask, &mut bundler_groups, &mut bundler_require_all);
    }
    // Fix #5: do NOT run the generic uppercase-token module scanner on YAML
    // files — YAML keys/values like `UserNotFound:` would otherwise be
    // extracted as module references. The explicit YAML regex extractors
    // below still pick up rubocop/plugins/inherit_gem/require: entries.
    let mut file_modules = Vec::new();
    if !is_yaml {
        let mut names = FileModuleSet::default();
        collect_module_names(bytes, &mask, &mut names);
        collect_middleware_refs(bytes, &mask, &mut names);
        file_modules = merge_file_modules(names, thread_modules, &rel_path_arc, keep_module_list);
    }

    // YAML config patterns (keep regex — rarely hit, complex multiline patterns).
    // Fix #8: use regex::bytes::Regex to avoid the String::from_utf8_lossy
    // allocation; patterns are byte-compatible.
    if is_yaml && is_rubocop_config(full_path) {
        rubocop_config_gems(bytes, &mut requires);
    } else if is_yaml
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

    Some(FileResult {
        rel_path,
        requires,
        modules: file_modules,
        bundler_groups,
        bundler_require_all,
        storage_services,
    })
}

/// `.rubocop.yml`, `.rubocop_todo.yml`, `rubocop-*.yml`, `.standard.yml`, ...
fn is_rubocop_config(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    name.contains("rubocop") || name.starts_with(".standard")
}

/// Extract gem names referenced by a RuboCop/Standard config: `require:` and
/// `plugins:` (a string, a list of strings, or a list of single-key maps) and
/// the keys of `inherit_gem:`. Invalid YAML (e.g. ERB) falls back to a
/// tolerant line-based reader.
fn rubocop_config_gems(bytes: &[u8], gems: &mut Vec<String>) {
    use serde_yaml::Value;

    fn push_names(value: &Value, gems: &mut Vec<String>) {
        match value {
            Value::String(name) => gems.push(name.clone()),
            Value::Sequence(items) => {
                for item in items {
                    match item {
                        Value::String(name) => gems.push(name.clone()),
                        Value::Mapping(map) => push_keys(map, gems),
                        _ => {}
                    }
                }
            }
            Value::Mapping(map) => push_keys(map, gems),
            _ => {}
        }
    }

    fn push_keys(map: &serde_yaml::Mapping, gems: &mut Vec<String>) {
        gems.extend(map.keys().filter_map(Value::as_str).map(str::to_string));
    }

    match serde_yaml::from_slice::<Value>(bytes) {
        Ok(Value::Mapping(config)) => {
            for key in ["require", "plugins"] {
                if let Some(value) = config.get(key) {
                    push_names(value, gems);
                }
            }
            if let Some(Value::Mapping(inherited)) = config.get("inherit_gem") {
                push_keys(inherited, gems);
            }
        }
        Ok(_) => {}
        Err(_) => rubocop_config_gems_fallback(bytes, gems),
    }
}

/// Line-based reader for RuboCop configs that are not valid YAML.
fn rubocop_config_gems_fallback(bytes: &[u8], gems: &mut Vec<String>) {
    fn strip_comment(value: &str) -> &str {
        let value = value.trim();
        if value.starts_with('#') {
            return "";
        }
        value.split(" #").next().unwrap_or("").trim()
    }
    fn unquote(value: &str) -> &str {
        value.trim().trim_matches(|c| c == '\'' || c == '"').trim()
    }
    fn push_flow(value: &str, keys_only: bool, gems: &mut Vec<String>) {
        let inner = value
            .trim_start_matches(['[', '{'])
            .trim_end_matches([']', '}']);
        for item in inner.split(',') {
            let item = if keys_only || item.contains(": ") || item.trim_end().ends_with(':') {
                item.split(':').next().unwrap_or("")
            } else {
                item
            };
            let item = unquote(item);
            if !item.is_empty() {
                gems.push(item.to_string());
            }
        }
    }

    let text = String::from_utf8_lossy(bytes);
    let mut section: Option<&str> = None;
    let mut child_indent: Option<usize> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - trimmed.len();
        if indent == 0 {
            child_indent = None;
            section = ["require", "plugins", "inherit_gem"]
                .into_iter()
                .find(|key| {
                    trimmed
                        .strip_prefix(key)
                        .is_some_and(|rest| rest.starts_with(':'))
                });
            let Some(key) = section else {
                continue;
            };
            let value = strip_comment(&trimmed[key.len() + 1..]);
            if value.starts_with('[') || value.starts_with('{') {
                push_flow(value, key == "inherit_gem", gems);
                section = None;
            } else if !value.is_empty() {
                if key != "inherit_gem" {
                    gems.push(unquote(value).to_string());
                }
                section = None;
            }
            continue;
        }
        let Some(key) = section else {
            continue;
        };
        let child_indent = *child_indent.get_or_insert(indent);
        if indent != child_indent {
            continue;
        }
        let entry = if key == "inherit_gem" {
            trimmed.split(':').next().unwrap_or("")
        } else if let Some(item) = trimmed.strip_prefix('-') {
            let item = strip_comment(item);
            if item.ends_with(':') || item.contains(": ") {
                item.split(':').next().unwrap_or("")
            } else {
                item
            }
        } else {
            continue;
        };
        let entry = unquote(entry);
        if !entry.is_empty() {
            gems.push(entry.to_string());
        }
    }
    for cap in RUBOCOP_RE.captures_iter(bytes) {
        if let Ok(name) = std::str::from_utf8(&cap[1]) {
            if !gems.iter().any(|gem| gem == name) {
                gems.push(name.to_string());
            }
        }
    }
}

/// Classify Ruby, YAML, and template dialects using the path and already-read bytes.
fn classify_file(full_path: &Path, bytes: &[u8]) -> (bool, bool, TemplateKind) {
    if let Some(ext) = full_path.extension() {
        let ext_bytes = ext.as_encoded_bytes();
        match ext_bytes.len() {
            2 => {
                let l = [ext_bytes[0] | 0x20, ext_bytes[1] | 0x20];
                let is_ruby = matches!(&l, b"rb" | b"ru");
                return (is_ruby, false, TemplateKind::None);
            }
            3 => {
                let l = [
                    ext_bytes[0] | 0x20,
                    ext_bytes[1] | 0x20,
                    ext_bytes[2] | 0x20,
                ];
                let is_yaml = &l == b"yml";
                let template = if &l == b"erb" {
                    TemplateKind::Erb
                } else {
                    TemplateKind::None
                };
                return (false, is_yaml, template);
            }
            4 => {
                let l = [
                    ext_bytes[0] | 0x20,
                    ext_bytes[1] | 0x20,
                    ext_bytes[2] | 0x20,
                    ext_bytes[3] | 0x20,
                ];
                let is_yaml = &l == b"yaml";
                let is_ruby = matches!(&l, b"rake" | b"rabl" | b"thor");
                let template = if &l == b"slim" {
                    TemplateKind::Slim
                } else if &l == b"haml" {
                    TemplateKind::Haml
                } else {
                    TemplateKind::None
                };
                return (is_ruby, is_yaml, template);
            }
            7 => {
                // gemspec | builder
                let mut l = [0u8; 7];
                for (i, b) in ext_bytes.iter().enumerate() {
                    l[i] = b | 0x20;
                }
                let is_ruby = &l == b"gemspec" || &l == b"builder";
                return (is_ruby, false, TemplateKind::None);
            }
            8 => {
                // jbuilder
                let mut l = [0u8; 8];
                for (i, b) in ext_bytes.iter().enumerate() {
                    l[i] = b | 0x20;
                }
                let is_ruby = &l == b"jbuilder";
                return (is_ruby, false, TemplateKind::None);
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
        .any(|f| f.as_bytes() == name_bytes)
        || (full_path.extension().is_none() && has_ruby_shebang(bytes));
    (is_ruby, false, TemplateKind::None)
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
    fn percent_q_after_word_char_is_modulo_expression() {
        let input = b"x%q(require 'real_gem')\n";
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

    fn ruby_findings(input: &[u8]) -> (Vec<String>, HashSet<String>) {
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut requires = Vec::new();
        scan_requires_filtered(input, &mask, &mut requires);
        let mut modules = HashMap::new();
        let path: Arc<str> = Arc::from("test.rb");
        scan_modules_filtered_into(input, &mask, &mut modules, &path);
        (requires, modules.into_keys().collect())
    }

    fn assert_alpha_and_beta_visible(input: &[u8]) {
        let (requires, modules) = ruby_findings(input);
        let text = String::from_utf8_lossy(input);
        assert_eq!(requires, vec!["alpha"], "{text}");
        assert!(modules.contains("Beta"), "{text}: {modules:?}");
    }

    #[test]
    fn bare_percent_literals_are_strings() {
        assert_alpha_and_beta_visible(
            b"x = %(don't panic)\nrequire \"alpha\"\nBeta.call\ny = 'z'\n",
        );
        assert_alpha_and_beta_visible(b"x = %{it's}\nrequire \"alpha\"\nBeta.call\ny = 'z'\n");
        assert_alpha_and_beta_visible(
            b"x = %[it's]\ny = %<it's>\nz = %|it's|\nrequire \"alpha\"\nBeta.call\nw = 'z'\n",
        );
        let (_, modules) = ruby_findings(b"x = %(Gamma #{Beta})\n");
        assert!(!modules.contains("Gamma"));
        assert!(modules.contains("Beta"));
        // Modulo stays an operator.
        let (_, modules) = ruby_findings(b"x = 10 %(3)\ny = n % (Beta)\n");
        assert!(modules.contains("Beta"));
    }

    #[test]
    fn question_mark_before_quote_in_ternary_is_not_char_literal() {
        assert_alpha_and_beta_visible(
            b"c = true\nx = c ?'a':'b'\nrequire \"alpha\"\nBeta.call\ny = 'q'\n",
        );
        assert_alpha_and_beta_visible(b"ok = valid?'x'\nrequire \"alpha\"\nBeta.call\ny = 'q'\n");
        // Real character literals still mask the quote character.
        assert_alpha_and_beta_visible(b"q = ?'\nd = ?\"\nrequire \"alpha\"\nBeta.call\n");
    }

    #[test]
    fn punctuation_globals_do_not_open_strings() {
        assert_alpha_and_beta_visible(
            b"$\".include?(1)\nrequire \"alpha\"\nBeta.call\nz = \"q\"\n",
        );
        assert_alpha_and_beta_visible(b"m = $'\nrequire \"alpha\"\nBeta.call\nz = 'q'\n");
    }

    #[test]
    fn slash_operators_do_not_start_regexes() {
        assert_alpha_and_beta_visible(
            b"x = [4,2].reduce(:/)\nrequire \"alpha\"\nBeta.call\ny = 4 / 2\n",
        );
        assert_alpha_and_beta_visible(
            b"class V\n  def /(o)\n    self\n  end\nend\nrequire \"alpha\"\nBeta.call\nz = 4 / 2\n",
        );
        assert_alpha_and_beta_visible(
            b"x = 10\ny = x /2\nrequire \"alpha\"\nBeta.call\nz = 4/ 2\n",
        );
        // Real regexes (including multi-line extended ones) are still masked.
        let (_, modules) =
            ruby_findings(b"a = x.match /Static/\nb = /\n  Extended\n/x\nBeta.call\n");
        assert!(!modules.contains("Static"));
        assert!(!modules.contains("Extended"));
        assert!(modules.contains("Beta"));
    }

    #[test]
    fn data_section_after_end_marker_is_ignored() {
        let (requires, modules) = ruby_findings(b"puts 1\n__END__\nrequire \"alpha\"\nBeta.call\n");
        assert!(requires.is_empty());
        assert!(!modules.contains("Beta"));
        let (requires, _) = ruby_findings(b"puts 1\r\n__END__\r\nrequire \"alpha\"\r\n");
        assert!(requires.is_empty());
        // Not a marker when anything else is on the line.
        assert_alpha_and_beta_visible(b"x = __END__x\nrequire \"alpha\"\nBeta.call\n");
    }

    #[test]
    fn embedded_doc_end_marker_requires_word_boundary() {
        let (_, modules) =
            ruby_findings(b"x = 1\n=begin\nfoo\n=ending stuff\nBeta.call\n=end\nAfter.call\n");
        assert!(!modules.contains("Beta"));
        assert!(modules.contains("After"));
        let (_, modules) = ruby_findings(b"=beginning\nBeta.call\n");
        assert!(modules.contains("Beta"));
    }

    #[test]
    fn erb_string_does_not_extend_into_template_text() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.erb");
        std::fs::write(
            &file,
            "<p>it's</p>\n<%= %(don't) %>\n<%= Beta.x %>\n<p>isn't</p>\n<%= x ? 'a : 'b' %>\n<%= Alpha.y %>\n",
        )
        .unwrap();
        let mut modules = HashMap::new();
        scan_file(
            file.to_str().unwrap(),
            dir.path().to_str().unwrap(),
            &mut modules,
        )
        .unwrap();
        assert!(modules.contains_key("Beta"));
        assert!(modules.contains_key("Alpha"));
    }

    #[test]
    fn multiline_strings_mask_static_text_but_keep_interpolation_code() {
        let input = b"single = 'StaticSingle\nrequire \"single_fake\"'\ndouble = \"StaticDouble\n#{require \"double_dynamic\"; InterpolatedModule}\"\nrequire 'real'\n";
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut requires = Vec::new();
        scan_requires_filtered(input, &mask, &mut requires);
        assert_eq!(requires, vec!["double_dynamic", "real"]);

        let mut modules = HashMap::new();
        let path: Arc<str> = Arc::from("test.rb");
        scan_modules_filtered_into(input, &mask, &mut modules, &path);
        assert!(!modules.contains_key("StaticSingle"));
        assert!(!modules.contains_key("StaticDouble"));
        assert!(modules.contains_key("InterpolatedModule"));
    }

    #[test]
    fn malformed_quotes_and_character_literals_do_not_mask_later_code() {
        let input =
            b"message = \"unterminated\napostrophe = ?'\nrequire 'after_malformed'\nAfterMalformed.call\n";
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut requires = Vec::new();
        scan_requires_filtered(input, &mask, &mut requires);
        assert_eq!(requires, vec!["after_malformed"]);

        let mut modules = HashMap::new();
        let path: Arc<str> = Arc::from("test.rb");
        scan_modules_filtered_into(input, &mask, &mut modules, &path);
        assert!(modules.contains_key("AfterMalformed"));
    }

    #[test]
    fn percent_literal_families_mask_static_text_and_bound_failures() {
        let input = br#"
words = %w[StaticWords can't require 'words_fake']
symbols = %i[StaticSymbols developer's]
expanded_words = %W[StaticExpanded #{ExpandedWordsInterpolation}]
expanded_symbols = %I[StaticSymbolsExpanded #{ExpandedSymbolsInterpolation}]
symbol = %s[StaticSymbol]
command = %x[StaticCommand require 'command_fake' #{CommandInterpolation}]
broken = %w[unterminated apostrophe's
require 'after_percent_literals'
AfterPercentLiterals.call
"#;
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut requires = Vec::new();
        scan_requires_filtered(input, &mask, &mut requires);
        assert_eq!(requires, vec!["after_percent_literals"]);

        let mut modules = HashMap::new();
        let path: Arc<str> = Arc::from("test.rb");
        scan_modules_filtered_into(input, &mask, &mut modules, &path);
        for hidden in [
            "StaticWords",
            "StaticSymbols",
            "StaticExpanded",
            "StaticSymbolsExpanded",
            "StaticSymbol",
            "StaticCommand",
        ] {
            assert!(!modules.contains_key(hidden), "{hidden}");
        }
        for visible in [
            "ExpandedWordsInterpolation",
            "ExpandedSymbolsInterpolation",
            "CommandInterpolation",
            "AfterPercentLiterals",
        ] {
            assert!(modules.contains_key(visible), "{visible}");
        }
    }

    #[test]
    fn unterminated_heredoc_openers_scan_linearly() {
        fn steps_for(lines: usize) -> usize {
            let mut input = b"items <<item\n".repeat(lines);
            input.extend_from_slice(b"require 'after_shovels'\n");
            let (mask, steps) = build_ruby_skip_mask_with_lexical_steps(&input);
            let mut requires = Vec::new();
            scan_requires_filtered(&input, &mask, &mut requires);
            assert_eq!(requires, vec!["after_shovels"]);
            steps
        }
        let small = steps_for(1_000);
        let large = steps_for(2_000);
        assert!(
            large <= small * 3,
            "doubling input must remain linear: {small} -> {large}"
        );
    }

    #[test]
    fn two_heredocs_on_one_line_have_sequential_bodies() {
        let input = b"foo(<<~A, <<~'B')\n  #{Beta.x}\n  StaticA\nA\n  raw #{NotInterpolated}\nB\nAfter.call\n";
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut modules = HashMap::new();
        let path: Arc<str> = Arc::from("test.rb");
        scan_modules_filtered_into(input, &mask, &mut modules, &path);
        assert!(modules.contains_key("Beta"), "{modules:?}");
        assert!(modules.contains_key("After"));
        assert!(!modules.contains_key("StaticA"));
        assert!(!modules.contains_key("NotInterpolated"));
    }

    #[test]
    fn repeated_malformed_percent_literals_scan_linearly() {
        fn build_case(lines: usize) -> (Vec<u8>, usize) {
            let mut input = Vec::with_capacity(lines * 12 + 128);
            for _ in 0..lines {
                input.extend_from_slice(b"x = %w[a b\n");
            }
            input.extend_from_slice(
                b"valid = %W[\nStaticValidPercent\n#{ValidPercentInterpolation}\n]\nrequire 'after_adversarial_percent'\n",
            );
            let (mask, steps) = build_ruby_skip_mask_with_lexical_steps(&input);

            let mut requires = Vec::new();
            scan_requires_filtered(&input, &mask, &mut requires);
            assert_eq!(requires, vec!["after_adversarial_percent"]);

            let mut modules = HashMap::new();
            let path: Arc<str> = Arc::from("test.rb");
            scan_modules_filtered_into(&input, &mask, &mut modules, &path);
            assert!(!modules.contains_key("StaticValidPercent"));
            assert!(modules.contains_key("ValidPercentInterpolation"));
            (input, steps)
        }

        let (small, small_steps) = build_case(512);
        let (large, large_steps) = build_case(1_024);
        assert!(
            large_steps <= small_steps * 3,
            "doubling input must remain linear: {small_steps} -> {large_steps}"
        );
        assert!(
            large_steps <= large.len() * 2,
            "delimiter scan steps must stay proportional to bytes: {} steps for {} bytes",
            large_steps,
            large.len()
        );
        assert!(small_steps <= small.len() * 2);
    }

    #[test]
    fn percent_cache_does_not_share_regex_failure_with_words_literal() {
        let input = br#"broken = %r{
words = %w{[
FakeWordsModule
require 'fake_words'
}
require 'real_words'
"#;
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut requires = Vec::new();
        scan_requires_filtered(input, &mask, &mut requires);
        assert_eq!(requires, vec!["real_words"]);

        let mut modules = HashMap::new();
        let path: Arc<str> = Arc::from("test.rb");
        scan_modules_filtered_into(input, &mask, &mut modules, &path);
        assert!(!modules.contains_key("FakeWordsModule"));
    }

    #[test]
    fn percent_cache_does_not_share_plain_match_with_regex_literal() {
        let input = br#"broken = %q{{
pattern = %r{[}] FakeRegexModule require 'fake_regex' }
require 'real_regex'
"#;
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut requires = Vec::new();
        scan_requires_filtered(input, &mask, &mut requires);
        assert_eq!(requires, vec!["real_regex"]);

        let mut modules = HashMap::new();
        let path: Arc<str> = Arc::from("test.rb");
        scan_modules_filtered_into(input, &mask, &mut modules, &path);
        assert!(!modules.contains_key("FakeRegexModule"));
    }

    #[test]
    fn repeated_malformed_percent_interpolations_scan_linearly() {
        fn build_case(lines: usize) -> (Vec<u8>, usize) {
            let mut input = Vec::with_capacity(lines * 16 + 128);
            for _ in 0..lines {
                input.extend_from_slice(b"x = %W[a#{b\n");
            }
            input.extend_from_slice(
                b"valid = %W[\nStaticInterpolationText\n#{ValidLaterInterpolation}\n]\nrequire 'after_malformed_interpolation'\n",
            );
            let (mask, steps) = build_ruby_skip_mask_with_lexical_steps(&input);

            let mut requires = Vec::new();
            scan_requires_filtered(&input, &mask, &mut requires);
            assert_eq!(requires, vec!["after_malformed_interpolation"]);

            let mut modules = HashMap::new();
            let path: Arc<str> = Arc::from("test.rb");
            scan_modules_filtered_into(&input, &mask, &mut modules, &path);
            assert!(!modules.contains_key("StaticInterpolationText"));
            assert!(modules.contains_key("ValidLaterInterpolation"));
            (input, steps)
        }

        let (small, small_steps) = build_case(512);
        let (large, large_steps) = build_case(1_024);
        assert!(
            large_steps <= small_steps * 3,
            "doubling malformed interpolations must remain linear: {small_steps} -> {large_steps}"
        );
        assert!(
            large_steps <= large.len() * 4,
            "all delimiter and interpolation work must stay proportional: {} steps for {} bytes",
            large_steps,
            large.len()
        );
        assert!(small_steps <= small.len() * 4);
    }

    #[test]
    fn regex_literals_mask_static_text_but_keep_interpolation_code() {
        let input = b"slash = /StaticSlash\nrequire 'slash_fake'\n#{SlashInterpolation}/\npercent = %r{StaticPercent\nrequire 'percent_fake'\n#{PercentInterpolation}}\nrequire 'real'\n";
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut requires = Vec::new();
        scan_requires_filtered(input, &mask, &mut requires);
        assert_eq!(requires, vec!["real"]);

        let mut modules = HashMap::new();
        let path: Arc<str> = Arc::from("test.rb");
        scan_modules_filtered_into(input, &mask, &mut modules, &path);
        assert!(!modules.contains_key("StaticSlash"));
        assert!(!modules.contains_key("StaticPercent"));
        assert!(modules.contains_key("SlashInterpolation"));
        assert!(modules.contains_key("PercentInterpolation"));
    }

    #[test]
    fn heredoc_accepts_one_char_delimiter_and_keeps_interpolation_code() {
        let input = b"doc = <<A\nStaticHeredoc require 'fake'\n#{require 'dynamic'; HeredocInterpolation}\nA\nrequire 'real'\n";
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut requires = Vec::new();
        scan_requires_filtered(input, &mask, &mut requires);
        assert_eq!(requires, vec!["dynamic", "real"]);

        let mut modules = HashMap::new();
        let path: Arc<str> = Arc::from("test.rb");
        scan_modules_filtered_into(input, &mask, &mut modules, &path);
        assert!(!modules.contains_key("StaticHeredoc"));
        assert!(modules.contains_key("HeredocInterpolation"));
    }

    #[test]
    fn heredoc_accepts_crlf_terminator() {
        let input = b"doc = <<~TEXT\r\nrequire 'hidden_crlf'\r\nTEXT\r\nrequire 'after_crlf'\r\n";
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut requires = Vec::new();
        scan_requires_filtered(input, &mask, &mut requires);
        assert_eq!(requires, vec!["after_crlf"]);
    }

    #[test]
    fn quoted_heredoc_does_not_expose_interpolation() {
        let input = b"doc = <<'A'\n#{require 'fake'; HiddenInterpolation}\nA\nrequire 'real'\n";
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut requires = Vec::new();
        scan_requires_filtered(input, &mask, &mut requires);
        assert_eq!(requires, vec!["real"]);

        let mut modules = HashMap::new();
        let path: Arc<str> = Arc::from("test.rb");
        scan_modules_filtered_into(input, &mask, &mut modules, &path);
        assert!(!modules.contains_key("HiddenInterpolation"));
    }

    #[test]
    fn commented_and_string_heredoc_openers_are_inactive() {
        let input = b"# ignored = <<COMMENTED\nrequire 'after_comment'\nCOMMENTED\ntext = \"prefix <<STRING\"\nrequire 'after_string'\nSTRING\nrequire 'real'\n";
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut requires = Vec::new();
        scan_requires_filtered(input, &mask, &mut requires);
        assert_eq!(requires, vec!["after_comment", "after_string", "real"]);
    }

    #[test]
    fn division_operator_does_not_start_regex_masking() {
        let input = b"ratio = Dividend / Divisor\nrequire 'real'\n";
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut modules = HashMap::new();
        let path: Arc<str> = Arc::from("test.rb");
        scan_modules_filtered_into(input, &mask, &mut modules, &path);
        assert!(modules.contains_key("Dividend"));
        assert!(modules.contains_key("Divisor"));
    }

    #[test]
    fn command_argument_regex_is_masked() {
        let input = b"puts /CommandRegexModule/\nRealModule.call\n";
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut modules = HashMap::new();
        let path: Arc<str> = Arc::from("test.rb");
        scan_modules_filtered_into(input, &mask, &mut modules, &path);
        assert!(!modules.contains_key("CommandRegexModule"));
        assert!(modules.contains_key("RealModule"));
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
        // (`MyNewModule:` would be a hash-key label, which is not a reference.)
        let input = b"# MyOldModule was removed\nMyNewModule.configure\n  key: value\n";
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
    fn rubocop_config_reference_forms() {
        let cases: &[(&str, &[&str])] = &[
            (
                "inherit_gem:\n  rubocop-shopify: rubocop.yml\n",
                &["rubocop-shopify"],
            ),
            (
                "inherit_gem: { rubocop-rails-omakase: rubocop.yml }\n",
                &["rubocop-rails-omakase"],
            ),
            (
                "inherit_gem:\n  rubocop-github:\n    - config/default.yml\n",
                &["rubocop-github"],
            ),
            (
                "plugins: [standard-performance]\n",
                &["standard-performance"],
            ),
            ("require: standard\n", &["standard"]),
            ("plugins:\n  - \"rubocop-rails\"\n", &["rubocop-rails"]),
            ("require:\n  - standard # comment\n", &["standard"]),
            ("require:\n  - standard", &["standard"]),
            (
                "plugins:\n  - rubocop-rails:\n      foo: bar\n  - rubocop-rspec\n",
                &["rubocop-rails", "rubocop-rspec"],
            ),
        ];
        for (config, expected) in cases {
            let mut gems = Vec::new();
            rubocop_config_gems(config.as_bytes(), &mut gems);
            assert_eq!(&gems, expected, "{config:?}");
        }
    }

    #[test]
    fn rubocop_config_invalid_yaml_uses_fallback() {
        let config = b"<% if ENV['CI'] %>\ninherit_gem: { rubocop-rails-omakase: rubocop.yml }\n<% end %>\nplugins: [standard-performance, 'standard-rails']\nrequire:\n  - \"rubocop-rspec\" # comment\n  - rubocop-rake:\n      x: 1\nAllCops:\n  - rubocop-minitest\n";
        assert!(serde_yaml::from_slice::<serde_yaml::Value>(config).is_err());
        let mut gems = Vec::new();
        rubocop_config_gems(config, &mut gems);
        gems.sort();
        assert_eq!(
            gems,
            vec![
                "rubocop-minitest",
                "rubocop-rails-omakase",
                "rubocop-rake",
                "rubocop-rspec",
                "standard-performance",
                "standard-rails",
            ]
        );
    }

    #[test]
    fn scan_file_dot_rubocop_flow_inherit_gem() {
        let dir = tempfile::tempdir().unwrap();
        let yml = dir.path().join(".rubocop.yml");
        std::fs::write(
            &yml,
            "inherit_gem: { rubocop-rails-omakase: rubocop.yml }\nplugins: [standard-performance]\n",
        )
        .unwrap();
        let mut modules = HashMap::new();
        let result = scan_file(
            yml.to_str().unwrap(),
            dir.path().to_str().unwrap(),
            &mut modules,
        )
        .unwrap();
        assert_eq!(
            result.requires,
            vec!["standard-performance", "rubocop-rails-omakase"]
        );
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
        assert!(r.bundler_require_all);
        assert!(r.bundler_groups.contains(&"default".to_string()));
        assert!(r
            .bundler_groups
            .contains(&"__rails_group_condition__:assets:development,test,profile".to_string()));
        assert!(!r.bundler_groups.contains(&"migrations".to_string()));
    }

    #[test]
    fn rails_group_conditions_support_hash_rockets() {
        let mut groups = Vec::new();
        push_rails_group_conditions(
            "Bundler.require(*Rails.groups(:assets => %w(development test)))",
            &mut groups,
        );
        assert_eq!(
            groups,
            vec!["__rails_group_condition__:assets:development,test"]
        );
    }

    #[test]
    fn bundler_require_rails_env_keeps_explicit_groups() {
        let input = b"Bundler.require(:default, :assets, Rails.env)\n";
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut groups = Vec::new();
        let mut require_all = false;
        scan_bundler_require_filtered(input, &mask, &mut groups, &mut require_all);
        assert!(require_all);
        assert_eq!(groups, vec!["default", "assets"]);
    }

    #[test]
    fn bundler_require_bounded_literal_arguments() {
        let cases: &[(&[u8], &[&str], bool)] = &[
            (b"Bundler.require(\n :default,\n \"assets\",\n 'test'\n)\n", &["default", "assets", "test"], false),
            (b"Bundler.require(:default); foo(:test, 'assets')\n", &["default"], false),
            (b"Bundler.require(:default) if Rails.env.test?\n", &["default"], false),
            (b"Bundler.require :default, \"assets\"; foo(:test)\n", &["default", "assets"], false),
            (b"Bundler.require(\n # :test, \"ignored\"\n :default, # ) :test\n \"assets\" # :test\n)\n", &["default", "assets"], false),
            (b"Bundler.require(\"assets)prod\", 'hash#group', ':test')\n", &["assets)prod", "hash#group", ":test"], false),
            (b"Bundler.require(:'assets', \"as\\x73ets\", \"#{dynamic}\")\n", &["assets", "assets"], false),
            (b"Bundler.require(:default, helper(:test), \"assets\")\n", &["default", "assets"], false),
            (b"Bundler.require(\n \"default\",\n 'assets',\n Rails.env\n); foo(:test)\n", &["default", "assets"], true),
            (b"Bundler.require(\n :default,\n *Rails.groups(\n :assets => %w(development test)\n )\n); foo(:test)\n", &["__rails_group_condition__:assets:development,test", "default"], true),
            (b"Bundler.require(\n # no arguments\n); foo(:test)\n", &["default"], false),
            (b"Bundler.require(:default); Bundler.require(\"assets\")\n", &["default", "assets"], false),
        ];
        for &(input, expected, expected_all) in cases {
            let mask = build_skip_mask(input, true, LineCommentKind::Hash);
            let mut groups = Vec::new();
            let mut require_all = false;
            scan_bundler_require_filtered(input, &mask, &mut groups, &mut require_all);
            assert_eq!(groups, expected, "{}", String::from_utf8_lossy(input));
            assert_eq!(
                require_all,
                expected_all,
                "{}",
                String::from_utf8_lossy(input)
            );
        }
    }

    #[test]
    fn bundler_require_rails_groups_hash_keys_are_not_plain_groups() {
        let input = b"Bundler.require(:default, *Rails.groups(:assets => %w(development test)))\n";
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut groups = Vec::new();
        let mut require_all = false;
        scan_bundler_require_filtered(input, &mask, &mut groups, &mut require_all);
        assert!(require_all);
        assert_eq!(
            groups,
            vec![
                "__rails_group_condition__:assets:development,test",
                "default"
            ]
        );
    }

    #[test]
    fn bundler_splat_var_ignores_commented_rails_groups_assignment() {
        let input = b"groups = [:default]\n# groups = Rails.groups(assets: %w(development))\nBundler.require(*groups)\n";
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut groups = Vec::new();
        let mut require_all = false;
        scan_bundler_require_filtered(input, &mask, &mut groups, &mut require_all);
        assert!(!require_all);
        assert_eq!(groups, vec!["default"]);
    }

    #[test]
    fn rails_group_conditions_support_symbol_arrays_and_percent_delimiters() {
        let mut groups = Vec::new();
        push_rails_group_conditions(
            "Rails.groups assets: [:development, :test], metrics: %w{production staging}",
            &mut groups,
        );
        assert_eq!(
            groups,
            vec![
                "__rails_group_condition__:assets:development,test",
                "__rails_group_condition__:metrics:production,staging"
            ]
        );
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
    fn is_scannable_skips_placeholder_and_doc_names_without_opening() {
        let dir = tempfile::tempdir().unwrap();
        for name in [".keep", ".gitkeep", "LICENSE", "README", "Makefile"] {
            let path = dir.path().join(name);
            std::fs::write(&path, "#!/usr/bin/env ruby\nrequire 'x'\n").unwrap();
            assert!(!is_scannable(path.to_str().unwrap(), &path), "{name}");
        }
        let script = dir.path().join("setup");
        std::fs::write(&script, "#!/usr/bin/env ruby\nrequire 'x'\n").unwrap();
        assert!(is_scannable(script.to_str().unwrap(), &script));
        assert!(may_be_ruby_script_name(b"console"));
        assert!(!may_be_ruby_script_name(b".keep"));
        assert!(!may_be_ruby_script_name(b"CHANGELOG"));
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
    fn scan_source_tree_ruby_shebang_filters_heredoc_requires() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("bin")).unwrap();
        for (name, shebang) in [
            ("tool", "#!/usr/bin/env ruby"),
            ("direct", "#!/usr/bin/ruby"),
        ] {
            std::fs::write(
                dir.path().join("bin").join(name),
                format!(
                    "{shebang}\ntext = <<~TEXT\nrequire 'heredoc_only'\nTEXT\nrequire 'real'\n"
                ),
            )
            .unwrap();
        }
        let cache = std::sync::Arc::new(crate::cache::FileCache::disabled());
        let (results, _) = scan_source_tree(dir.path(), &[], cache);
        assert!(!results.requires.contains_key("heredoc_only"));
        assert_eq!(
            results.requires["real"]
                .iter()
                .map(Path::new)
                .collect::<HashSet<_>>(),
            HashSet::from([Path::new("bin/direct"), Path::new("bin/tool")])
        );

        let cache = Arc::new(crate::cache::FileCache::load(dir.path()));
        let (cold, updates) = scan_source_tree(dir.path(), &[], cache);
        assert_eq!(cold.requires, results.requires);
        assert_eq!(updates.len(), 2);
        let mut cache = crate::cache::FileCache::load(dir.path());
        for update in updates {
            cache.put(update);
        }
        cache.save();
        let cache = Arc::new(crate::cache::FileCache::load(dir.path()));
        let (warm, updates) = scan_source_tree(dir.path(), &[], cache);
        assert!(updates.is_empty());
        assert_eq!(warm.requires, results.requires);
    }

    #[test]
    fn scan_file_bytes_ruby_shebang_uses_full_lexical_filtering() {
        let bytes = br#"#!/usr/bin/env ruby
=begin
require 'block_comment'
=end
text = "require 'string_only'"
text = %q{require 'percent_only'}
pattern = /require 'regex_only'/
x = 1 # require 'inline_comment'
require 'real'
__END__
require 'data_only'
"#;
        let mut modules = HashMap::new();
        let result = scan_file_bytes("bin/tool", "", bytes, &mut modules).unwrap();
        assert_eq!(result.requires, vec!["real"]);
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
    fn scan_source_tree_scans_gitignored_ruby_sources() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("generated")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "generated/\n").unwrap();
        std::fs::write(
            dir.path().join("generated/schema.rb"),
            "require 'generated_source'\nGeneratedModel\n",
        )
        .unwrap();
        let cache = std::sync::Arc::new(crate::cache::FileCache::disabled());
        let (results, _) = scan_source_tree(dir.path(), &default_skip_dirs(), cache);
        assert!(results.requires.contains_key("generated_source"));
        assert!(results.modules.contains_key("GeneratedModel"));
    }

    #[test]
    fn scan_source_tree_scans_nested_builtin_names() {
        let dir = tempfile::tempdir().unwrap();
        for (name, require_path) in [
            ("vendor", "nested_vendor"),
            ("tmp", "nested_tmp"),
            ("coverage", "nested_coverage"),
            ("pkg", "nested_pkg"),
        ] {
            let nested = dir.path().join("app/services").join(name);
            std::fs::create_dir_all(&nested).unwrap();
            std::fs::write(
                nested.join("source.rb"),
                format!("require '{require_path}'\n"),
            )
            .unwrap();
        }
        let cache = std::sync::Arc::new(crate::cache::FileCache::disabled());
        let (results, _) = scan_source_tree(dir.path(), &default_skip_dirs(), cache);
        for visible in [
            "nested_vendor",
            "nested_tmp",
            "nested_coverage",
            "nested_pkg",
        ] {
            assert!(results.requires.contains_key(visible), "{visible}");
        }
    }

    #[test]
    fn scan_source_tree_skips_nested_vendor_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("engines/foo/vendor/bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("gem.rb"), "require 'hidden'\n").unwrap();
        std::fs::write(dir.path().join("app.rb"), "require 'visible'\n").unwrap();
        let cache = std::sync::Arc::new(crate::cache::FileCache::disabled());
        let (results, _) = scan_source_tree(dir.path(), &default_skip_dirs(), cache);
        assert!(results.requires.contains_key("visible"));
        assert!(!results.requires.contains_key("hidden"));
    }

    #[test]
    fn explicit_skip_still_applies_to_nested_builtin_name() {
        let dir = tempfile::tempdir().unwrap();
        let vendor = dir.path().join("engines/foo/vendor");
        std::fs::create_dir_all(&vendor).unwrap();
        std::fs::write(vendor.join("gem.rb"), "require 'hidden'\n").unwrap();
        let cache = std::sync::Arc::new(crate::cache::FileCache::disabled());
        let (results, _) = scan_source_tree_with_threads_and_explicit_skips(
            dir.path(),
            &default_skip_dirs(),
            &["vendor"],
            cache,
            None,
        );
        assert!(!results.requires.contains_key("hidden"));
    }

    #[test]
    fn explicit_skip_dir_paths_match_relative_to_root() {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["app/legacy", "app/legacy_v2", "lib/app/legacy"] {
            let path = dir.path().join(sub);
            std::fs::create_dir_all(&path).unwrap();
            let gem = sub.replace('/', "_");
            std::fs::write(path.join("a.rb"), format!("require '{gem}'\n")).unwrap();
        }
        let cache = Arc::new(crate::cache::FileCache::disabled());
        for skip in ["app/legacy", "./app/legacy/"] {
            let (results, _) = scan_source_tree_with_threads_and_explicit_skips(
                dir.path(),
                &[skip],
                &[skip],
                cache.clone(),
                None,
            );
            assert!(!results.requires.contains_key("app_legacy"), "{skip}");
            assert!(results.requires.contains_key("app_legacy_v2"), "{skip}");
            assert!(results.requires.contains_key("lib_app_legacy"), "{skip}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn scan_source_tree_follows_symlinks() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("project");
        let shared = outer.path().join("shared");
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::create_dir_all(root.join("lib")).unwrap();
        std::fs::create_dir_all(shared.join("lib")).unwrap();
        std::fs::write(shared.join("a.rb"), "require 'alpha'\n").unwrap();
        std::fs::write(shared.join("lib").join("b.rb"), "Beta.x\n").unwrap();
        std::fs::write(root.join("lib").join("g_real.rb"), "Gamma.x\n").unwrap();
        std::os::unix::fs::symlink(shared.join("a.rb"), root.join("app").join("a.rb")).unwrap();
        std::os::unix::fs::symlink(shared.join("lib"), root.join("app").join("shared_lib"))
            .unwrap();
        std::os::unix::fs::symlink(
            root.join("lib").join("g_real.rb"),
            root.join("app").join("g_link.rb"),
        )
        .unwrap();
        // A directory loop must not hang or crash the walk.
        std::os::unix::fs::symlink(&root, root.join("lib").join("loop")).unwrap();

        let cache = Arc::new(crate::cache::FileCache::disabled());
        let (results, _) = scan_source_tree(&root, &default_skip_dirs(), cache);
        assert!(results.requires.contains_key("alpha"));
        assert!(results.modules.contains_key("Beta"));
        assert!(results.modules["Gamma"].len() >= 2);
    }

    #[test]
    fn filtered_scanners_accept_multiline_calls() {
        let input = b"require(\n  'json'\n)\nautoload(\n  :Thing,\n  'thing'\n)\nGem.bin_path(\n  'rake'\n)\n";
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut requires = Vec::new();
        scan_requires_filtered(input, &mask, &mut requires);
        scan_autoloads_filtered(input, &mask, &mut requires);
        scan_gem_bin_filtered(input, &mask, &mut requires);
        assert_eq!(requires, vec!["json", "thing", "rake"]);
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
        for update in updates {
            cache_mut.put(update);
        }
        cache_mut.save();

        // Second scan: should use cache
        let cache2 = std::sync::Arc::new(crate::cache::FileCache::load(dir.path()));
        let (results2, updates2) = scan_source_tree(dir.path(), &[], cache2);
        assert!(results2.requires.contains_key("cached_gem"));
        assert!(updates2.is_empty()); // all cache hits
    }

    #[test]
    fn scan_source_tree_cache_rejects_same_metadata_different_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.rb");
        std::fs::write(&path, "require 'alpha'\n").unwrap();

        let cache = std::sync::Arc::new(crate::cache::FileCache::load(dir.path()));
        let (_, updates) = scan_source_tree(dir.path(), &[], cache);
        let mut cache = crate::cache::FileCache::load(dir.path());
        for update in updates {
            cache.put(update);
        }
        cache.save();

        let original_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, "require 'bravo'\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(original_mtime))
            .unwrap();

        let cache = std::sync::Arc::new(crate::cache::FileCache::load(dir.path()));
        let (results, updates) = scan_source_tree(dir.path(), &[], cache);
        assert!(results.requires.contains_key("bravo"));
        assert!(!results.requires.contains_key("alpha"));
        assert_eq!(updates.len(), 1);
    }

    #[test]
    fn scan_source_tree_with_threads_matches_default_across_counts() {
        // Same inputs across thread counts must produce identical scan results.
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        for i in 0..25 {
            std::fs::write(
                app.join(format!("f{i}.rb")),
                format!("require 'g{i}'\nmodule M{i}\nend\n"),
            )
            .unwrap();
        }

        fn sorted<T: Ord + Clone>(v: impl IntoIterator<Item = T>) -> Vec<T> {
            let mut v: Vec<T> = v.into_iter().collect();
            v.sort();
            v
        }
        let cache = std::sync::Arc::new(crate::cache::FileCache::disabled());
        let (baseline, _) = scan_source_tree_with_threads(dir.path(), &[], cache.clone(), None);
        let baseline_reqs = sorted(baseline.requires.keys().cloned());
        let baseline_mods = sorted(baseline.modules.keys().cloned());

        for threads in [1usize, 2, 4, 8, 12] {
            let cache = std::sync::Arc::new(crate::cache::FileCache::disabled());
            let (r, _) = scan_source_tree_with_threads(dir.path(), &[], cache, Some(threads));
            assert_eq!(sorted(r.requires.keys().cloned()), baseline_reqs);
            assert_eq!(sorted(r.modules.keys().cloned()), baseline_mods);
        }
    }

    #[test]
    fn scan_source_tree_with_threads_zero_is_treated_as_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rb"), "require 'x'\n").unwrap();
        let cache = std::sync::Arc::new(crate::cache::FileCache::disabled());
        // 0 must not panic; walker floor is 1.
        let (r, _) = scan_source_tree_with_threads(dir.path(), &[], cache, Some(0));
        assert!(r.requires.contains_key("x"));
    }

    #[test]
    fn default_walker_threads_is_capped_at_four() {
        assert!(default_walker_threads() >= 1);
        assert!(default_walker_threads() <= 4);
    }

    #[test]
    fn attachment_services_are_collected_from_code_only() {
        let input = b"class User\n  has_one_attached :avatar, service: :amazon, dependent: :purge\n  has_many_attached :docs,\n    service: ->(u) { u.eu? ? :europe : :usa }\n  has_one_attached :legacy, :service => :old_disk\n  has_one_attached :named, service: \"quoted\"\n  # has_one_attached :x, service: :commented\n  s = \"has_one_attached :y, service: :in_string\"\n  has_one_attached :plain\n  validates :name, service: :not_attachment\nend\n";
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut services = Vec::new();
        collect_attachment_services(input, &mask, &mut services);
        services.sort();
        assert_eq!(
            services,
            vec!["amazon", "europe", "old_disk", "quoted", "usa"]
        );
    }

    #[test]
    fn attachment_services_survive_cache_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("user.rb"),
            "has_one_attached :avatar, service: :amazon\n",
        )
        .unwrap();
        let cache = Arc::new(crate::cache::FileCache::load(dir.path()));
        let (first, updates) = scan_source_tree(dir.path(), &[], cache);
        assert!(first.storage_services.contains("amazon"));
        let mut cache = crate::cache::FileCache::load(dir.path());
        for update in updates {
            cache.put(update);
        }
        cache.save();

        let cache = Arc::new(crate::cache::FileCache::load(dir.path()));
        let (second, updates) = scan_source_tree(dir.path(), &[], cache);
        assert!(
            updates.is_empty(),
            "second scan should be served from cache"
        );
        assert!(second.storage_services.contains("amazon"));
    }

    #[test]
    fn storage_config_services_are_collected_from_code_only() {
        let input = br##"config.active_storage.service = :amazon
config . active_storage . service =
  "google"
config.active_storage.service = :'quoted'
config.active_storage.service = "dynamic_#{region}"
# config.active_storage.service = :comment
text = "config.active_storage.service = :string"
text = <<~DOC
config.active_storage.service = :heredoc
DOC
other_config.active_storage.service = :other
object.config.active_storage.service = :receiver
config.active_storage.service_name = :suffix
config.active_storage.service == :comparison
config.active_storage.service = # :comment_value
  :after_comment
text = "#{config.active_storage.service = :interpolation}"
"##;
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut services = Vec::new();
        collect_storage_config_services(input, &mask, &mut services);
        assert_eq!(services, ["amazon", "google", "quoted", "interpolation"]);
    }

    #[test]
    fn storage_config_services_survive_cache_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config/environments");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("production.rb"),
            "config.active_storage.service = :amazon\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("unrelated.rb"),
            "config.active_storage.service = :unrelated\n",
        )
        .unwrap();
        let cache = Arc::new(crate::cache::FileCache::load(dir.path()));
        let (first, updates) = scan_source_tree(dir.path(), &[], cache);
        assert_eq!(
            first.storage_services,
            HashSet::from(["amazon".to_string()])
        );
        let mut cache = crate::cache::FileCache::load(dir.path());
        for update in updates {
            cache.put(update);
        }
        cache.save();
        let cache = Arc::new(crate::cache::FileCache::load(dir.path()));
        let (second, updates) = scan_source_tree(dir.path(), &[], cache);
        assert!(updates.is_empty(), "expected cache hits");
        assert_eq!(second.storage_services, first.storage_services);
    }

    #[test]
    fn scan_file_reports_unique_modules_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let rb = dir.path().join("a.rb");
        std::fs::write(&rb, "Foo.a\nFoo.b\nBar::Baz.c\nFoo.d\n").unwrap();
        let mut modules = HashMap::new();
        let result = scan_file(
            rb.to_str().unwrap(),
            dir.path().to_str().unwrap(),
            &mut modules,
        )
        .unwrap();
        let mut own = result.modules.clone();
        own.sort();
        assert_eq!(own, vec!["Bar::Baz", "Foo"]);
        assert_eq!(modules["Foo"].len(), 1);
    }

    #[test]
    fn cold_cache_scan_is_not_quadratic_in_module_count() {
        // Regression: on a cache miss the walker used to scan the entire
        // per-thread module map to find the current file's modules, making a
        // cold cached scan quadratic in the number of files.
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        for i in 0..1500 {
            let body: String = (0..60).map(|j| format!("Const{i}x{j}.call\n")).collect();
            std::fs::write(app.join(format!("f{i}.rb")), body).unwrap();
        }
        let started = std::time::Instant::now();
        let disabled = Arc::new(crate::cache::FileCache::disabled());
        let (uncached, _) = scan_source_tree_with_threads(dir.path(), &[], disabled, Some(1));
        let uncached_time = started.elapsed();

        let started = std::time::Instant::now();
        let cache = Arc::new(crate::cache::FileCache::load(dir.path()));
        let (cached, updates) = scan_source_tree_with_threads(dir.path(), &[], cache, Some(1));
        let cached_time = started.elapsed();

        assert_eq!(uncached.modules.len(), 90_000);
        assert_eq!(cached.modules.len(), 90_000);
        assert_eq!(updates.len(), 1500);
        assert!(updates.iter().all(|update| update.modules.len() == 60));
        assert!(
            cached_time <= uncached_time * 4 + std::time::Duration::from_millis(500),
            "cold cached scan {cached_time:?} vs uncached {uncached_time:?}"
        );
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
    fn gem_bin_without_paren() {
        let mut results = Vec::new();
        scan_gem_bin(b"Gem.bin_path 'foo'\n", &mut results);
        assert_eq!(results, vec!["foo"]);

        let input = b"Gem.activate_bin_path \"beta\", \"b\"\nGem.bin_path(\"alpha\", \"a\")\n";
        let mask = build_skip_mask(input, true, LineCommentKind::Hash);
        let mut results = Vec::new();
        scan_gem_bin_filtered(input, &mask, &mut results);
        assert_eq!(results, vec!["beta", "alpha"]);
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
    fn templates_only_scan_executable_ruby_and_interpolations() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (
                "view.erb",
                "StaticErb require 'static_erb'\n<% require 'erb_exec' %>\n<%= ErbExecModule %>\n<%# require 'erb_comment' ErbCommentModule %>\n",
                "erb_exec",
                "ErbExecModule",
                "StaticErb",
            ),
            (
                "view.haml",
                "%p StaticHaml require 'static_haml'\n= require 'haml_exec'\n%p hello #{HamlInterpolation}\n- HamlLine.call\n",
                "haml_exec",
                "HamlInterpolation",
                "StaticHaml",
            ),
            (
                "view.slim",
                "p StaticSlim require 'static_slim'\n= require 'slim_exec'\np hello #{SlimInterpolation}\n- SlimLine.call\n",
                "slim_exec",
                "SlimInterpolation",
                "StaticSlim",
            ),
        ];

        for (name, content, expected_require, expected_module, static_module) in cases {
            let file = dir.path().join(name);
            std::fs::write(&file, content).unwrap();
            let mut modules = HashMap::new();
            let result = scan_file(
                file.to_str().unwrap(),
                dir.path().to_str().unwrap(),
                &mut modules,
            )
            .unwrap();
            assert_eq!(result.requires, vec![expected_require], "{name}");
            assert!(modules.contains_key(expected_module), "{name}");
            assert!(!modules.contains_key(static_module), "{name}");
        }
    }

    fn template_modules(name: &str, content: &str) -> HashSet<String> {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(name);
        std::fs::write(&file, content).unwrap();
        let mut modules = HashMap::new();
        scan_file(
            file.to_str().unwrap(),
            dir.path().to_str().unwrap(),
            &mut modules,
        );
        modules.into_keys().collect()
    }

    #[test]
    fn haml_and_slim_text_is_not_exposed_as_ruby() {
        for (name, content) in [
            (
                "a.haml",
                "%p Hello (it's me)\n= Beta.render\n%p= link_to 'x', y\n",
            ),
            (
                "a.slim",
                "p Hello (it's me)\n= Beta.render\np = link_to 'x', y\n",
            ),
        ] {
            let modules = template_modules(name, content);
            assert!(modules.contains("Beta"), "{name}: {modules:?}");
        }
        let modules = template_modules("b.haml", "%p See (Gamma docs) here {Delta}\n");
        assert!(!modules.contains("Gamma"), "{modules:?}");
        assert!(!modules.contains("Delta"), "{modules:?}");
    }

    #[test]
    fn haml_and_slim_attributes_and_output_after_tags() {
        let modules = template_modules(
            "a.haml",
            "%p{class: \"a\"}= Beta.x\n#main= Gamma.x\n%a.btn#go(href=Delta.path)[Epsilon] text\n%img/\n",
        );
        for expected in ["Beta", "Gamma", "Delta", "Epsilon"] {
            assert!(modules.contains(expected), "{expected}: {modules:?}");
        }
        let modules = template_modules(
            "a.slim",
            "div class=Beta.cls\na(href=Gamma.path title=\"it's\") Text\nli: a href=Delta.path Link\n.box#id data-x=Epsilon.x Plain Text\n",
        );
        for expected in ["Beta", "Gamma", "Delta", "Epsilon"] {
            assert!(modules.contains(expected), "{expected}: {modules:?}");
        }
        assert!(!modules.contains("Link"));
        assert!(!modules.contains("Plain"));
    }

    #[test]
    fn haml_and_slim_multiline_ruby() {
        for (name, content) in [
            ("a.haml", "= link_to \"x\",\n  Beta.path\n%p Static\n"),
            ("a.slim", "= link_to \"x\",\n  Beta.path\np Static\n"),
            ("b.slim", "= link_to \"x\", \\\n  Beta.path\np Static\n"),
            ("b.haml", "%div{ a: 1,\n  b: Beta.x }\n%p Static\n"),
            ("c.haml", "%p= link_to \"x\",\n  Beta.path\n%p Static\n"),
            ("c.slim", "div(a=1\n  b=Beta.x)\np Static\n"),
        ] {
            let modules = template_modules(name, content);
            assert!(modules.contains("Beta"), "{name}: {modules:?}");
            assert!(!modules.contains("Static"), "{name}: {modules:?}");
        }
    }

    #[test]
    fn haml_and_slim_filters_and_comment_blocks_stay_masked() {
        let modules = template_modules(
            "a.haml",
            ":javascript\n  var Beta = 1;\n  .Gamma { x }\n-#\n  %p= Delta.x\n= Epsilon.x\n",
        );
        assert!(!modules.contains("Beta"));
        assert!(!modules.contains("Gamma"));
        assert!(!modules.contains("Delta"));
        assert!(modules.contains("Epsilon"));
        let modules = template_modules(
            "a.slim",
            "javascript:\n  Beta = foo.bar\n/\n  p = Delta.x\n| Some text\n  Gamma = x\n= Epsilon.x\n",
        );
        assert!(!modules.contains("Beta"));
        assert!(!modules.contains("Delta"));
        assert!(!modules.contains("Gamma"));
        assert!(modules.contains("Epsilon"));
    }

    #[test]
    fn haml_and_slim_scan_inline_attributes_and_ruby_filters() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (
                "forms.haml",
                "%p= HamlInlineOutput\n%div{ class: HamlHashAttribute }\n%span(title=HamlParenAttribute)\n:ruby\n  require 'haml_filter'\n  HamlFilterModule.call\n%p StaticHamlAfter\n",
                [
                    "HamlInlineOutput",
                    "HamlHashAttribute",
                    "HamlParenAttribute",
                    "HamlFilterModule",
                ],
                "haml_filter",
                "StaticHamlAfter",
            ),
            (
                "forms.slim",
                "p = SlimInlineOutput\np class=SlimAttribute data-id=SlimDataAttribute\nruby:\n  require 'slim_filter'\n  SlimFilterModule.call\np StaticSlimAfter\n",
                [
                    "SlimInlineOutput",
                    "SlimAttribute",
                    "SlimDataAttribute",
                    "SlimFilterModule",
                ],
                "slim_filter",
                "StaticSlimAfter",
            ),
        ];

        for (name, content, expected_modules, expected_require, static_module) in cases {
            let file = dir.path().join(name);
            std::fs::write(&file, content).unwrap();
            let mut modules = HashMap::new();
            let result = scan_file(
                file.to_str().unwrap(),
                dir.path().to_str().unwrap(),
                &mut modules,
            )
            .unwrap();
            assert!(
                result.requires.contains(&expected_require.to_string()),
                "{name}"
            );
            for expected in expected_modules {
                assert!(modules.contains_key(expected), "{name}: {expected}");
            }
            assert!(!modules.contains_key(static_module), "{name}");
        }
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
        assert!(!ranges.is_empty());
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
    fn middleware_only_takes_middleware_argument_slots() {
        for (input, expected, unexpected) in [
            (
                &b"config.middleware.use Foo # was \"Beta::Rack\"\n"[..],
                None,
                "Beta::Rack",
            ),
            (
                &b"config.middleware.use \"Beta::Rack\" if x == \"Alpha\"\n"[..],
                Some("Beta::Rack"),
                "Alpha",
            ),
            (
                &b"config.middleware.use \"Beta::Rack\", \"Option\"\n"[..],
                Some("Beta::Rack"),
                "Option",
            ),
            (
                &b"config.middleware.insert_before(\"Rack::Runtime\", \"Beta::Rack\", \"Arg\")\n"[..],
                Some("Beta::Rack"),
                "Arg",
            ),
        ] {
            let mask = build_skip_mask(input, true, LineCommentKind::Hash);
            let mut modules: HashMap<String, HashSet<Arc<str>>> = HashMap::new();
            let rel: Arc<str> = Arc::from("test.rb");
            scan_middleware_refs(input, &mask, &mut modules, &rel);
            let text = String::from_utf8_lossy(input);
            if let Some(expected) = expected {
                assert!(modules.contains_key(expected), "{text}");
            }
            assert!(!modules.contains_key(unexpected), "{text}");
        }
    }

    #[test]
    fn symbols_labels_and_variables_are_not_constants() {
        let (_, modules) = ruby_findings(
            b"x = :Beta
h = { Alpha: 1 }
@Gamma = 1
@@Delta = 2
$Epsilon = 3
foo(Zeta: 1)
y = ::Top::Level
z = Foo::Bar
w = cond ? Eta : Theta
",
        );
        for excluded in ["Beta", "Alpha", "Gamma", "Delta", "Epsilon", "Zeta"] {
            assert!(!modules.contains(excluded), "{excluded}: {modules:?}");
        }
        for included in ["Top::Level", "Foo::Bar", "Eta", "Theta"] {
            assert!(modules.contains(included), "{included}: {modules:?}");
        }
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
