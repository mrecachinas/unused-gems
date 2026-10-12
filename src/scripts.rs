use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use memchr::memmem;
use rayon::prelude::*;
use regex::Regex;

/// Check if a path matches our script file patterns.
pub(crate) fn is_script_file(rel_path: &str, file_name: &str) -> bool {
    // Normalize path separators for cross-platform support
    let rel_path = &rel_path.replace('\\', "/");
    // Exact top-level filenames
    match file_name {
        "Makefile" | "makefile" | "GNUmakefile" | "Rakefile" | "Dockerfile" | "Procfile" => {
            return true
        }
        "Jenkinsfile" => return true,
        "Earthfile" => return true,
        "Justfile" | "justfile" | ".justfile" => return true,
        "Taskfile.yml" | "Taskfile.yaml" => return true,
        "mise.toml" | ".mise.toml" | ".tool-versions" => return true,
        _ => {}
    }
    // Rake task files (e.g. lib/tasks/*.rake) shell out just like the Rakefile.
    // They are also Ruby sources, so the walker scans them for requires too.
    if file_name.ends_with(".rake") {
        return true;
    }
    // Dockerfile.* / Procfile.* / Jenkinsfile.*
    if file_name.starts_with("Dockerfile.")
        || file_name.starts_with("Procfile.")
        || file_name.starts_with("Jenkinsfile.")
    {
        return true;
    }
    // docker-compose.yml/yaml
    if file_name == "docker-compose.yml" || file_name == "docker-compose.yaml" {
        return true;
    }
    // .github/workflows/*.yml/*.yaml , .github/actions/*.yml/*.yaml
    if (rel_path.starts_with(".github/workflows/") || rel_path.starts_with(".github/actions/"))
        && (file_name.ends_with(".yml") || file_name.ends_with(".yaml"))
    {
        return true;
    }
    // .circleci/config.yml, .gitlab-ci.yml
    if rel_path == ".circleci/config.yml" || rel_path == ".gitlab-ci.yml" {
        return true;
    }
    // .buildkite/**/*.yml/*.yaml
    if rel_path.starts_with(".buildkite/")
        && (file_name.ends_with(".yml") || file_name.ends_with(".yaml"))
    {
        return true;
    }
    // .drone.yml/.drone.yaml
    if rel_path == ".drone.yml" || rel_path == ".drone.yaml" {
        return true;
    }
    // bitbucket-pipelines.yml/yaml (top-level)
    if rel_path == "bitbucket-pipelines.yml" || rel_path == "bitbucket-pipelines.yaml" {
        return true;
    }
    // azure-pipelines.yml/yaml (top-level) and .azure-pipelines/**/*.yml|yaml
    if file_name == "azure-pipelines.yml" || file_name == "azure-pipelines.yaml" {
        return true;
    }
    if rel_path.starts_with(".azure-pipelines/")
        && (file_name.ends_with(".yml") || file_name.ends_with(".yaml"))
    {
        return true;
    }
    // script/**/* , scripts/**/* , bin/**/*
    if rel_path.starts_with("script/")
        || rel_path.starts_with("scripts/")
        || rel_path.starts_with("bin/")
    {
        return true;
    }
    // Top-level *.sh, *.bash, *.bats
    if !rel_path.contains('/')
        && (file_name.ends_with(".sh")
            || file_name.ends_with(".bash")
            || file_name.ends_with(".bats"))
    {
        return true;
    }
    false
}

/// Byte scanner for `bundle exec <cmd>` — returns extracted command names.
fn scan_bundle_exec(bytes: &[u8]) -> Vec<String> {
    let finder = memmem::Finder::new(b"bundle");
    let mut results = Vec::new();
    let mut pos = 0;
    while let Some(idx) = finder.find(&bytes[pos..]) {
        let abs = pos + idx;
        let mut i = abs + 6; // skip "bundle"
                             // require whitespace
        if i >= bytes.len() || (bytes[i] != b' ' && bytes[i] != b'\t') {
            pos = abs + 1;
            continue;
        }
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        // expect "exec"
        if i + 4 > bytes.len() || &bytes[i..i + 4] != b"exec" {
            pos = abs + 1;
            continue;
        }
        i += 4;
        // require whitespace after exec
        if i >= bytes.len() || (bytes[i] != b' ' && bytes[i] != b'\t') {
            pos = abs + 1;
            continue;
        }
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        // extract command name: [\w-]+
        let cmd_start = i;
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'-')
        {
            i += 1;
        }
        if i > cmd_start {
            if let Ok(cmd) = std::str::from_utf8(&bytes[cmd_start..i]) {
                results.push(cmd.to_string());
            }
        }
        pos = abs + 1;
    }
    results
}

/// Byte scanner for `bin/<cmd>` or `./bin/<cmd>` — returns extracted command names.
fn scan_bin_refs(bytes: &[u8]) -> Vec<String> {
    let finder = memmem::Finder::new(b"bin/");
    let mut results = Vec::new();
    let mut pos = 0;
    while let Some(idx) = finder.find(&bytes[pos..]) {
        let abs = pos + idx;
        // Check for optional "./" prefix
        let valid_prefix = abs == 0
            || (abs >= 2 && bytes[abs - 2] == b'.' && bytes[abs - 1] == b'/')
            || !bytes[abs - 1].is_ascii_alphanumeric();
        if !valid_prefix {
            pos = abs + 1;
            continue;
        }
        let mut i = abs + 4; // skip "bin/"
        let cmd_start = i;
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'-')
        {
            i += 1;
        }
        if i > cmd_start {
            if let Ok(cmd) = std::str::from_utf8(&bytes[cmd_start..i]) {
                results.push(cmd.to_string());
            }
        }
        pos = abs + 1;
    }
    results
}

/// Strip an inline `#` comment from a single line (no trailing `\n`).
/// Returns the slice up to (but not including) an unquoted `#` that's preceded
/// by whitespace or starts the line. Tracks single/double quote state so that
/// `#` inside string literals is preserved.
fn strip_inline_comment(line: &[u8]) -> &[u8] {
    let mut in_double = false;
    let mut in_single = false;
    let mut prev_ws = true; // start of line counts as a word boundary
    let mut i = 0;
    while i < line.len() {
        let b = line[i];
        if in_double {
            // Handle escapes inside double quotes
            if b == b'\\' && i + 1 < line.len() {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
            prev_ws = false;
        } else if in_single {
            if b == b'\'' {
                in_single = false;
            }
            prev_ws = false;
        } else if b == b'"' {
            in_double = true;
            prev_ws = false;
        } else if b == b'\'' {
            in_single = true;
            prev_ws = false;
        } else if b == b'#' && prev_ws {
            return &line[..i];
        } else {
            prev_ws = b == b' ' || b == b'\t';
        }
        i += 1;
    }
    line
}

/// Strip comment lines and inline `#` comments from script file content.
/// Handles `#` comments for YAML/shell/Ruby/Makefile-style files. Preserves
/// non-comment content so byte offsets for downstream scanners remain meaningful.
///
/// Returns a borrowed slice when no `#` is present anywhere in the input,
/// avoiding an allocation for the common case.
fn strip_comment_lines(content: &[u8]) -> Cow<'_, [u8]> {
    // Fast path: if there's no `#` at all, nothing to strip.
    if memchr::memchr(b'#', content).is_none() {
        return Cow::Borrowed(content);
    }

    let mut result = Vec::with_capacity(content.len());
    let mut i = 0;
    while i < content.len() {
        let line_start = i;
        while i < content.len() && content[i] != b'\n' {
            i += 1;
        }
        let line_end = i;
        if i < content.len() {
            i += 1; // consume \n
        }

        let line = &content[line_start..line_end];
        let kept = strip_inline_comment(line);
        result.extend_from_slice(kept);
        if line_end < content.len() {
            result.push(b'\n');
        }
    }
    Cow::Owned(result)
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Extract the string argument of a quoted literal starting at `bytes[0]`.
/// Returns the literal's content, or the rest of the line when unquoted.
fn literal_argument(bytes: &[u8]) -> &[u8] {
    let (open, close) = match bytes.first() {
        Some(b'"') => (1, b'"'),
        Some(b'\'') => (1, b'\''),
        Some(b'%') => match bytes.get(1) {
            Some(b'(') => (2, b')'),
            Some(b'[') => (2, b']'),
            Some(b'{') => (2, b'}'),
            Some(b'w' | b'W' | b'q' | b'Q') => match bytes.get(2) {
                Some(b'(') => (3, b')'),
                Some(b'[') => (3, b']'),
                Some(b'{') => (3, b'}'),
                _ => return bytes,
            },
            _ => return bytes,
        },
        _ => return bytes,
    };
    let rest = &bytes[open..];
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == b'\\' {
            i += 2;
            continue;
        }
        if rest[i] == close {
            return &rest[..i];
        }
        i += 1;
    }
    rest
}

/// Find shell-out calls in a line of Ruby/Groovy code: `sh "..."`, `system(...)`,
/// `exec ...` and backtick literals. Only matches the keyword as a standalone
/// identifier outside of string literals, so `desc "Flush redis cache"` or
/// `task :sh` are not mistaken for commands.
fn ruby_shell_calls<'a>(line: &'a [u8], keywords: &[&[u8]], backticks: bool) -> Vec<&'a [u8]> {
    let mut calls = Vec::new();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    while i < line.len() {
        let b = line[i];
        if let Some(q) = quote {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' => {
                quote = Some(b);
                i += 1;
                continue;
            }
            b'`' if backticks => {
                let rest = &line[i + 1..];
                let end = memchr::memchr(b'`', rest).unwrap_or(rest.len());
                calls.push(&rest[..end]);
                i += end + 2;
                continue;
            }
            _ => {}
        }
        let boundary = i == 0 || {
            let prev = line[i - 1];
            !is_ident_byte(prev) && !matches!(prev, b'.' | b':' | b'@' | b'$')
        };
        if boundary {
            if let Some(kw) = keywords.iter().find(|kw| line[i..].starts_with(kw)) {
                let after = i + kw.len();
                if matches!(line.get(after), Some(b' ' | b'\t' | b'(')) {
                    let mut j = after;
                    while j < line.len() && matches!(line[j], b' ' | b'\t' | b'(') {
                        j += 1;
                    }
                    calls.push(literal_argument(&line[j..]));
                    i = after;
                    continue;
                }
            }
        }
        i += 1;
    }
    calls
}

fn mise_commands(rel_path: &str, content: &[u8]) -> Vec<String> {
    let text = match std::str::from_utf8(content) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("Warning: cannot read mise tasks in {rel_path}: {error}");
            return Vec::new();
        }
    };
    let config = match text.parse::<toml::Value>() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("Warning: cannot parse mise tasks in {rel_path}: {error}");
            return Vec::new();
        }
    };
    let mut commands = Vec::new();
    if let Some(tasks) = config.get("tasks").and_then(toml::Value::as_table) {
        for task in tasks.values() {
            let run = if task.is_table() {
                task.get("run")
            } else {
                Some(task)
            };
            match run {
                Some(toml::Value::String(command)) => commands.push(command.clone()),
                Some(toml::Value::Array(entries)) => commands.extend(
                    entries
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .map(str::to_owned),
                ),
                _ => {}
            }
        }
    }
    commands
}

fn command_slices<'a>(rel_path: &str, content: &'a [u8]) -> Vec<&'a [u8]> {
    let normalized = rel_path.replace('\\', "/");
    let file_name = normalized.rsplit('/').next().unwrap_or(&normalized);
    let is_shell = normalized.starts_with("script/")
        || normalized.starts_with("scripts/")
        || normalized.starts_with("bin/")
        || (!normalized.contains('/')
            && (file_name.ends_with(".sh")
                || file_name.ends_with(".bash")
                || file_name.ends_with(".bats")));
    let is_make = matches!(file_name, "Makefile" | "makefile" | "GNUmakefile");
    let is_just = matches!(file_name, "Justfile" | "justfile" | ".justfile");
    let is_rake = file_name == "Rakefile" || file_name.ends_with(".rake");
    let is_jenkins = file_name == "Jenkinsfile" || file_name.starts_with("Jenkinsfile.");
    let is_procfile = file_name == "Procfile" || file_name.starts_with("Procfile.");
    let is_docker = file_name == "Dockerfile"
        || file_name.starts_with("Dockerfile.")
        || file_name == "Earthfile";
    let is_yaml = file_name.ends_with(".yml") || file_name.ends_with(".yaml");

    let mut slices = Vec::new();
    // (indent of the line that opened the block, column of a compact list)
    let mut yaml_block: Option<(usize, Option<usize>)> = None;
    for line in content.split(|b| *b == b'\n') {
        let indent = line
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .unwrap_or(line.len());
        let trimmed = &line[indent..];
        if trimmed.is_empty() {
            continue;
        }

        if is_yaml {
            if let Some((block_indent, compact_col)) = yaml_block {
                let compact_item = compact_col == Some(indent) && trimmed.starts_with(b"- ");
                if indent > block_indent || compact_item {
                    slices.push(trimmed.strip_prefix(b"- ").unwrap_or(trimmed));
                    continue;
                }
                yaml_block = None;
            }
        }

        if is_shell || (is_make && line.first() == Some(&b'\t')) || (is_just && indent > 0) {
            slices.push(trimmed);
            continue;
        }

        if is_rake {
            slices.extend(ruby_shell_calls(
                trimmed,
                &[b"sh", b"system", b"exec"],
                true,
            ));
            continue;
        }
        if is_jenkins {
            slices.extend(ruby_shell_calls(trimmed, &[b"sh"], false));
            continue;
        }

        let Ok(text) = std::str::from_utf8(trimmed) else {
            continue;
        };
        if is_docker {
            if let Some(command) = ["RUN ", "CMD ", "ENTRYPOINT "]
                .iter()
                .find_map(|prefix| text.strip_prefix(prefix))
            {
                slices.push(command.as_bytes());
            }
        } else if is_procfile {
            if let Some((_, command)) = text.split_once(':') {
                slices.push(command.trim_start().as_bytes());
            }
        } else if is_yaml {
            let (item, item_offset) = match text.strip_prefix("- ") {
                Some(item) => (item, 2),
                None => (text, 0),
            };
            if let Some(command) = [
                "run:",
                "script:",
                "cmds:",
                "commands:",
                "command:",
                "entrypoint:",
            ]
            .iter()
            .find_map(|key| item.strip_prefix(key))
            {
                let command = command.trim_start();
                let is_block = command
                    .as_bytes()
                    .split_first()
                    .is_some_and(|(first, rest)| {
                        (*first == b'|' || *first == b'>')
                            && rest
                                .iter()
                                .all(|b| b.is_ascii_digit() || *b == b'+' || *b == b'-')
                    });
                if is_block {
                    yaml_block = Some((indent, None));
                } else if command.is_empty() {
                    // `script:` followed by a list may put its `- item`s at the
                    // same column as the key (GitLab's compact style).
                    yaml_block = Some((indent, Some(indent + item_offset)));
                } else {
                    slices.push(command.as_bytes());
                }
            }
        }
    }
    slices
}

/// Wrappers that run the following word as the actual command.
const COMMAND_PREFIXES: &[&str] = &[
    "sudo", "time", "nohup", "exec", "env", "command", "nice", "then", "do", "else", "if", "elif",
    "while", "until", "!", "{", "xargs",
];

/// Extract the words in command position from a shell-like command line:
/// the first word and every word after `&&`, `||`, `;`, `|`, `(` or a
/// backtick, skipping env assignments (`RAILS_ENV=test`), wrapper commands
/// (`sudo`, `time`, `nohup`, `exec`, `bundle exec`, …) and their flags.
/// Paths are reduced to their basename so `bin/rails` yields `rails`.
fn command_words<'a>(command: &'a [u8]) -> Vec<&'a str> {
    let Ok(command) = std::str::from_utf8(command) else {
        return Vec::new();
    };
    let bytes = command.as_bytes();
    let mut words = Vec::new();
    let mut at_start = true;
    let mut after_prefix = false;
    let mut after_bundle = false;
    let mut quote: Option<u8> = None;
    let mut token_start: Option<usize> = None;

    let mut finish =
        |token: &'a str, at_start: &mut bool, after_prefix: &mut bool, after_bundle: &mut bool| {
            let token = token.trim_matches(|c| matches!(c, '"' | '\'' | '[' | ']' | ',' | '@'));
            if token.is_empty() {
                return;
            }
            if *after_bundle {
                *after_bundle = false;
                if token == "exec" {
                    *at_start = true;
                    *after_prefix = true;
                    return;
                }
            }
            if !*at_start {
                return;
            }
            if *after_prefix && token.starts_with('-') {
                return;
            }
            let is_assignment = token.split_once('=').is_some_and(|(name, _)| {
                !name.is_empty()
                    && name.bytes().all(is_ident_byte)
                    && !name.as_bytes()[0].is_ascii_digit()
            });
            if is_assignment {
                return;
            }
            let word = token.rsplit('/').next().unwrap_or(token);
            if COMMAND_PREFIXES.contains(&word) {
                *after_prefix = true;
                return;
            }
            *at_start = false;
            *after_prefix = false;
            if word == "bundle" {
                *after_bundle = true;
            }
            if !word.is_empty() {
                words.push(word);
            }
        };

    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == b'\\' && q == b'"' {
                i += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        let is_separator = matches!(b, b';' | b'|' | b'&' | b'(' | b')' | b'`' | b'\n');
        if b.is_ascii_whitespace() || is_separator {
            if let Some(start) = token_start.take() {
                finish(
                    &command[start..i],
                    &mut at_start,
                    &mut after_prefix,
                    &mut after_bundle,
                );
            }
            if is_separator {
                at_start = true;
                after_prefix = false;
                after_bundle = false;
            }
        } else {
            if b == b'"' || b == b'\'' {
                quote = Some(b);
            }
            if token_start.is_none() {
                token_start = Some(i);
            }
        }
        i += 1;
    }
    if let Some(start) = token_start {
        finish(
            &command[start..],
            &mut at_start,
            &mut after_prefix,
            &mut after_bundle,
        );
    }
    words
}

/// Curated executable → gem mapping used when the providing gem is not
/// installed locally (so its gemspec `executables` cannot be read). Every
/// declared candidate is credited.
static KNOWN_EXECUTABLES: &[(&str, &[&str])] = &[
    ("annotate", &["annotate"]),
    ("annotaterb", &["annotaterb"]),
    ("appsignal", &["appsignal"]),
    ("brakeman", &["brakeman"]),
    ("bundle", &["bundler"]),
    ("bundle-audit", &["bundler-audit"]),
    ("bundler-audit", &["bundler-audit"]),
    ("byebug", &["byebug"]),
    ("cap", &["capistrano"]),
    ("capify", &["capistrano"]),
    ("cucumber", &["cucumber"]),
    ("danger", &["danger"]),
    ("derailed", &["derailed_benchmarks"]),
    ("erb_lint", &["erb_lint"]),
    ("erblint", &["erb_lint"]),
    ("erd", &["rails-erd"]),
    ("fastlane", &["fastlane"]),
    ("foreman", &["foreman"]),
    ("good_job", &["good_job"]),
    ("guard", &["guard"]),
    ("haml-lint", &["haml_lint"]),
    ("htmlbeautifier", &["htmlbeautifier"]),
    ("htmlproofer", &["html-proofer"]),
    ("i18n-tasks", &["i18n-tasks"]),
    ("kamal", &["kamal"]),
    ("license_finder", &["license_finder"]),
    ("mailcatcher", &["mailcatcher"]),
    ("newrelic", &["newrelic_rpm"]),
    ("overcommit", &["overcommit"]),
    ("parallel_cucumber", &["parallel_tests"]),
    ("parallel_rspec", &["parallel_tests"]),
    ("parallel_test", &["parallel_tests"]),
    ("pod", &["cocoapods"]),
    ("pry", &["pry"]),
    ("puma", &["puma"]),
    ("pumactl", &["puma"]),
    ("rackup", &["rackup", "rack"]),
    ("rails", &["railties", "rails"]),
    ("rake", &["rake"]),
    ("rdbg", &["debug"]),
    ("rdoc", &["rdoc"]),
    ("reek", &["reek"]),
    ("ri", &["rdoc"]),
    ("rspec", &["rspec-core", "rspec", "rspec-rails"]),
    ("rubocop", &["rubocop"]),
    ("rubycritic", &["rubycritic"]),
    ("rufo", &["rufo"]),
    ("scss-lint", &["scss_lint"]),
    ("shoryuken", &["shoryuken"]),
    ("sidekiq", &["sidekiq"]),
    ("sidekiqmon", &["sidekiq"]),
    ("skylight", &["skylight"]),
    ("slim-lint", &["slim_lint"]),
    ("solargraph", &["solargraph"]),
    ("spring", &["spring"]),
    ("srb", &["sorbet"]),
    ("srb-rbi", &["sorbet"]),
    ("standardrb", &["standard"]),
    ("steep", &["steep"]),
    ("stree", &["syntax_tree"]),
    ("tailwindcss", &["tailwindcss-ruby", "tailwindcss-rails"]),
    ("tapioca", &["tapioca"]),
    ("thrust", &["thruster"]),
    ("unicorn", &["unicorn"]),
    ("unicorn_rails", &["unicorn"]),
    ("vite", &["vite_ruby"]),
    ("whenever", &["whenever"]),
    ("wheneverize", &["whenever"]),
    ("yard", &["yard"]),
    ("yardoc", &["yard"]),
    ("yri", &["yard"]),
];

fn normalize_name(name: &str) -> String {
    name.to_ascii_lowercase().replace('-', "_")
}

/// Resolves a command word to the declared gem(s) that provide it.
struct CommandResolver<'a> {
    gem_names: HashSet<&'a str>,
    normalized: HashMap<String, &'a str>,
    exe_to_gem: HashMap<String, String>,
}

impl<'a> CommandResolver<'a> {
    fn new(gem_names: &'a [String], exe_to_gem: HashMap<String, String>) -> Self {
        CommandResolver {
            gem_names: gem_names.iter().map(String::as_str).collect(),
            normalized: gem_names
                .iter()
                .map(|name| (normalize_name(name), name.as_str()))
                .collect(),
            exe_to_gem,
        }
    }

    fn resolve(&self, command: &str, out: &mut Vec<String>) {
        if self.gem_names.contains(command) {
            out.push(command.to_string());
        }
        if let Some(gem) = self.exe_to_gem.get(command) {
            out.push(gem.clone());
        }
        if let Some((_, gems)) = KNOWN_EXECUTABLES.iter().find(|(exe, _)| *exe == command) {
            out.extend(
                gems.iter()
                    .filter(|gem| self.gem_names.contains(**gem))
                    .map(|gem| gem.to_string()),
            );
        }
        if let Some(gem) = self.normalized.get(&normalize_name(command)) {
            out.push(gem.to_string());
        }
    }
}

/// Scan non-Ruby files (Makefile, Dockerfile, CI configs, scripts)
/// for `bundle exec <gem>` invocations and gem executables run as commands.
/// Uses pre-collected script files from the source tree walk (single-walk mode).
pub fn scan_scripts_for_gem_usage(
    _dir: &Path,
    gem_names: &[String],
    gem_index: &HashMap<String, std::path::PathBuf>,
    script_files: &[(String, Vec<u8>)],
) -> HashMap<String, HashSet<String>> {
    // Build executable→gem name mapping from installed gemspecs (parallel)
    let resolver = CommandResolver::new(gem_names, build_exe_to_gem_map(gem_names, gem_index));

    let per_file_refs: Vec<(String, Vec<String>)> = script_files
        .par_iter()
        .filter_map(|(rel_path, content)| {
            let mut gems = Vec::new();
            let normalized = rel_path.replace('\\', "/");
            let file_name = normalized.rsplit('/').next().unwrap_or(&normalized);
            let is_mise = matches!(file_name, "mise.toml" | ".mise.toml");
            let is_drone = matches!(normalized.as_str(), ".drone.yml" | ".drone.yaml");

            // Parse TOML before stripping shell comments so multiline strings
            // and escapes retain their TOML meaning.
            let mise = is_mise.then(|| mise_commands(rel_path, content));
            let stripped = if is_mise {
                Cow::Borrowed(content.as_slice())
            } else {
                strip_comment_lines(content)
            };
            let stripped: &[u8] = stripped.as_ref();

            // Structured configs must not credit invocations in metadata.
            if !is_mise && !is_drone {
                for cmd in scan_bundle_exec(stripped) {
                    resolver.resolve(&cmd, &mut gems);
                }
                for cmd in scan_bin_refs(stripped) {
                    resolver.resolve(&cmd, &mut gems);
                }
            }
            // Only words in command position count: arguments such as
            // `apt-get install -y git curl` must not credit `git`/`curl` gems.
            let commands = match &mise {
                Some(commands) => commands.iter().map(|command| command.as_bytes()).collect(),
                None => command_slices(rel_path, stripped),
            };
            for command in commands {
                let command = if is_drone && matches!(command.first(), Some(b'"' | b'\'')) {
                    match serde_yaml::from_slice::<String>(command) {
                        Ok(command) => Cow::Owned(command.into_bytes()),
                        Err(error) => {
                            eprintln!("Warning: cannot parse Drone command in {rel_path}: {error}");
                            continue;
                        }
                    }
                } else {
                    Cow::Borrowed(command)
                };
                let command = strip_comment_lines(&command);
                for word in command_words(&command) {
                    resolver.resolve(word, &mut gems);
                }
            }

            if gems.is_empty() {
                None
            } else {
                Some((rel_path.clone(), gems))
            }
        })
        .collect();

    let mut gem_refs: HashMap<String, HashSet<String>> = HashMap::new();
    for (file, gems) in per_file_refs {
        for gem_name in gems {
            gem_refs.entry(gem_name).or_default().insert(file.clone());
        }
    }

    gem_refs
}

/// Build a mapping from executable names to gem names by reading gemspec executables.
fn build_exe_to_gem_map(
    gem_names: &[String],
    gem_index: &HashMap<String, std::path::PathBuf>,
) -> HashMap<String, String> {
    static EXE_RE: once_cell::sync::Lazy<Regex> =
        once_cell::sync::Lazy::new(|| Regex::new(r#"executables\s*=\s*\[([^\]]*)\]"#).unwrap());
    static EXE_SINGLE_RE: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        Regex::new(r#"executables\s*(?:<<|=)\s*['"]([^'"]+)['"]"#).unwrap()
    });
    static QUOTED_RE: once_cell::sync::Lazy<Regex> =
        once_cell::sync::Lazy::new(|| Regex::new(r#"['"]([^'"]+)['"]"#).unwrap());

    // Collect gem names that have a gem_index entry, then read in parallel
    let gems_with_dirs: Vec<(&str, &std::path::PathBuf)> = gem_names
        .iter()
        .filter_map(|n| gem_index.get(n).map(|d| (n.as_str(), d)))
        .collect();

    let entries: Vec<(String, String)> = gems_with_dirs
        .par_iter()
        .flat_map(|(gem_name, gem_dir)| {
            let mut exes = Vec::new();
            let mut spec_paths = Vec::new();
            if let Ok(rd) = std::fs::read_dir(gem_dir) {
                spec_paths.extend(rd.flatten().map(|entry| entry.path()).filter(|path| {
                    path.extension().and_then(|ext| ext.to_str()) == Some("gemspec")
                }));
            }
            if gem_dir
                .parent()
                .and_then(|path| path.file_name())
                .is_some_and(|name| name == "gems")
            {
                if let (Some(root), Some(installed_name)) = (
                    gem_dir.parent().and_then(|path| path.parent()),
                    gem_dir.file_name(),
                ) {
                    let installed_spec = root
                        .join("specifications")
                        .join(format!("{}.gemspec", installed_name.to_string_lossy()));
                    if installed_spec.is_file() {
                        spec_paths.push(installed_spec);
                    }
                }
            }
            for spec_path in spec_paths {
                if let Ok(content) = std::fs::read_to_string(&spec_path) {
                    if let Some(cap) = EXE_RE.captures(&content) {
                        for exe_cap in QUOTED_RE.captures_iter(&cap[1]) {
                            let exe = exe_cap[1].trim();
                            if !exe.is_empty() && exe != *gem_name {
                                exes.push((exe.to_string(), gem_name.to_string()));
                            }
                        }
                    }
                    if let Some(cap) = EXE_SINGLE_RE.captures(&content) {
                        let exe = cap[1].trim();
                        if !exe.is_empty() && exe != *gem_name {
                            exes.push((exe.to_string(), gem_name.to_string()));
                        }
                    }
                }
            }
            exes
        })
        .collect();

    entries.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── scan_bundle_exec ──

    #[test]
    fn bundle_exec_basic() {
        assert_eq!(scan_bundle_exec(b"bundle exec rspec"), vec!["rspec"]);
    }

    #[test]
    fn bundle_exec_with_tabs() {
        assert_eq!(scan_bundle_exec(b"bundle\texec\trubocop"), vec!["rubocop"]);
    }

    #[test]
    fn bundle_exec_multiple() {
        let input = b"bundle exec rspec\nbundle exec rubocop";
        assert_eq!(scan_bundle_exec(input), vec!["rspec", "rubocop"]);
    }

    #[test]
    fn bundle_exec_hyphenated_cmd() {
        assert_eq!(scan_bundle_exec(b"bundle exec ruby-lsp"), vec!["ruby-lsp"]);
    }

    #[test]
    fn bundle_exec_extra_spaces() {
        assert_eq!(scan_bundle_exec(b"bundle   exec   rake"), vec!["rake"]);
    }

    #[test]
    fn bundle_exec_no_exec() {
        assert!(scan_bundle_exec(b"bundle install").is_empty());
    }

    #[test]
    fn bundle_exec_no_whitespace() {
        assert!(scan_bundle_exec(b"bundleexec rspec").is_empty());
    }

    #[test]
    fn bundle_exec_no_cmd() {
        assert!(scan_bundle_exec(b"bundle exec ").is_empty());
    }

    #[test]
    fn bundle_exec_in_makefile_context() {
        let input = b"test:\n\tbundle exec rspec spec/\n\nlint:\n\tbundle exec rubocop";
        assert_eq!(scan_bundle_exec(input), vec!["rspec", "rubocop"]);
    }

    // ── scan_bin_refs ──

    #[test]
    fn bin_ref_basic() {
        assert_eq!(scan_bin_refs(b"bin/rails"), vec!["rails"]);
    }

    #[test]
    fn bin_ref_dotslash() {
        assert_eq!(scan_bin_refs(b"./bin/rails"), vec!["rails"]);
    }

    #[test]
    fn bin_ref_hyphenated() {
        assert_eq!(scan_bin_refs(b"bin/ruby-lsp"), vec!["ruby-lsp"]);
    }

    #[test]
    fn bin_ref_after_space() {
        assert_eq!(scan_bin_refs(b"run bin/setup"), vec!["setup"]);
    }

    #[test]
    fn bin_ref_multiple() {
        let input = b"bin/rails server\n./bin/rake db:migrate";
        assert_eq!(scan_bin_refs(input), vec!["rails", "rake"]);
    }

    #[test]
    fn bin_ref_rejects_embedded() {
        // "cabin/foo" should not match — 'n' is alphanumeric before "bin/"
        assert!(scan_bin_refs(b"cabin/foo").is_empty());
    }

    #[test]
    fn bin_ref_after_newline() {
        assert_eq!(scan_bin_refs(b"\nbin/rspec"), vec!["rspec"]);
    }

    #[test]
    fn bin_ref_at_start() {
        assert_eq!(scan_bin_refs(b"bin/puma"), vec!["puma"]);
    }

    #[test]
    fn bin_ref_no_command() {
        assert!(scan_bin_refs(b"bin/").is_empty());
    }

    // ── is_script_file ──

    #[test]
    fn script_file_makefile() {
        assert!(is_script_file("Makefile", "Makefile"));
        assert!(is_script_file("GNUmakefile", "GNUmakefile"));
    }

    #[test]
    fn script_file_dockerfile() {
        assert!(is_script_file("Dockerfile", "Dockerfile"));
        assert!(is_script_file("Dockerfile.prod", "Dockerfile.prod"));
    }

    #[test]
    fn script_file_github_workflows() {
        assert!(is_script_file(".github/workflows/ci.yml", "ci.yml"));
        assert!(is_script_file(
            ".github/actions/deploy/action.yaml",
            "action.yaml"
        ));
    }

    #[test]
    fn script_file_script_dirs() {
        assert!(is_script_file("script/bootstrap", "bootstrap"));
        assert!(is_script_file("scripts/deploy.sh", "deploy.sh"));
        assert!(is_script_file("bin/setup", "setup"));
    }

    #[test]
    fn script_file_ci_configs() {
        assert!(is_script_file(".circleci/config.yml", "config.yml"));
        assert!(is_script_file(".gitlab-ci.yml", ".gitlab-ci.yml"));
    }

    #[test]
    fn script_file_rejects_ruby() {
        assert!(!is_script_file("app/models/user.rb", "user.rb"));
        assert!(!is_script_file("lib/foo.rb", "foo.rb"));
    }

    #[test]
    fn script_file_docker_compose() {
        assert!(is_script_file("docker-compose.yml", "docker-compose.yml"));
        assert!(is_script_file("docker-compose.yaml", "docker-compose.yaml"));
    }

    // ── strip_comment_lines ──

    #[test]
    fn strip_comments_removes_hash_lines() {
        let input = b"# This is a comment\nbundle exec rspec\n";
        let stripped = strip_comment_lines(input);
        let s = String::from_utf8_lossy(&stripped);
        assert!(!s.contains("This is a comment"));
        assert!(s.contains("bundle exec rspec"));
    }

    #[test]
    fn strip_comments_removes_indented_comments() {
        let input = b"  # indented comment\n  bundle exec rspec\n";
        let stripped = strip_comment_lines(input);
        let s = String::from_utf8_lossy(&stripped);
        assert!(!s.contains("indented comment"));
        assert!(s.contains("bundle exec rspec"));
    }

    #[test]
    fn strip_comments_preserves_non_comment_lines() {
        let input = b"steps:\n  - run: bundle exec rspec\n";
        let stripped = strip_comment_lines(input);
        let s = String::from_utf8_lossy(&stripped);
        assert!(s.contains("bundle exec rspec"));
    }

    #[test]
    fn strip_comments_yaml_ci_config() {
        // Gem name in comment should be stripped; gem in command should remain
        let input =
            b"# We removed rspec in favor of minitest\nsteps:\n  - run: bundle exec minitest\n";
        let stripped = strip_comment_lines(input);
        let s = String::from_utf8_lossy(&stripped);
        assert!(!s.contains("rspec"), "rspec in comment should be stripped");
        assert!(s.contains("minitest"), "minitest in command should remain");
    }

    // ── scan_scripts_for_gem_usage comment filtering ──

    #[test]
    fn script_scan_ignores_gem_in_comment() {
        let script_files = vec![(
            ".github/workflows/ci.yml".to_string(),
            b"# We removed rspec in favor of minitest\nsteps:\n  - run: bundle exec minitest\n"
                .to_vec(),
        )];
        let gem_names = vec!["rspec".to_string(), "minitest".to_string()];
        let gem_index = HashMap::new();
        let results =
            scan_scripts_for_gem_usage(Path::new("."), &gem_names, &gem_index, &script_files);
        assert!(
            !results.contains_key("rspec"),
            "rspec should not match from comment"
        );
        assert!(
            results.contains_key("minitest"),
            "minitest should match from command"
        );
    }

    #[test]
    fn script_scan_bundle_exec() {
        let script_files = vec![(
            "Makefile".to_string(),
            b"test:\n\tbundle exec rspec\n".to_vec(),
        )];
        let gem_names = vec!["rspec".to_string()];
        let gem_index = HashMap::new();
        let results =
            scan_scripts_for_gem_usage(Path::new("."), &gem_names, &gem_index, &script_files);
        assert!(results.contains_key("rspec"));
    }

    #[test]
    fn script_scan_bin_refs() {
        let script_files = vec![(
            "script/setup".to_string(),
            b"bin/rails db:migrate\n".to_vec(),
        )];
        let gem_names = vec!["rails".to_string()];
        let gem_index = HashMap::new();
        let results =
            scan_scripts_for_gem_usage(Path::new("."), &gem_names, &gem_index, &script_files);
        assert!(results.contains_key("rails"));
    }

    #[test]
    fn script_scan_regex_match() {
        // Installing a gem is an argument, not a use; running it is.
        let script_files = vec![(
            "Dockerfile".to_string(),
            b"RUN gem install puma foreman\nCMD puma -C config/puma.rb\n".to_vec(),
        )];
        let gem_names = vec!["puma".to_string(), "foreman".to_string()];
        let gem_index = HashMap::new();
        let results =
            scan_scripts_for_gem_usage(Path::new("."), &gem_names, &gem_index, &script_files);
        assert!(results.contains_key("puma"));
        assert!(!results.contains_key("foreman"));
    }

    #[test]
    fn script_scan_no_matches() {
        let script_files = vec![("Makefile".to_string(), b"all:\n\techo hello\n".to_vec())];
        let gem_names = vec!["rails".to_string()];
        let gem_index = HashMap::new();
        let results =
            scan_scripts_for_gem_usage(Path::new("."), &gem_names, &gem_index, &script_files);
        assert!(!results.contains_key("rails"));
    }

    #[test]
    fn script_scan_ignores_gem_name_in_yaml_prose() {
        let script_files = vec![(
            "docker-compose.yml".to_string(),
            b"services:\n  web:\n    labels:\n      description: puma migration notes\n    command: ruby app.rb\n"
                .to_vec(),
        )];
        let gem_names = vec!["puma".to_string()];
        let gem_index = HashMap::new();
        let results =
            scan_scripts_for_gem_usage(Path::new("."), &gem_names, &gem_index, &script_files);
        assert!(!results.contains_key("puma"));
    }

    #[test]
    fn script_scan_matches_bare_command_in_yaml_block() {
        let script_files = vec![(
            ".github/workflows/ci.yml".to_string(),
            b"steps:\n  - run: |\n      rubocop --parallel\n".to_vec(),
        )];
        let gem_names = vec!["rubocop".to_string()];
        let gem_index = HashMap::new();
        let results =
            scan_scripts_for_gem_usage(Path::new("."), &gem_names, &gem_index, &script_files);
        assert!(results.contains_key("rubocop"));
    }

    #[test]
    fn script_scan_matches_justfile_space_indented_recipe() {
        let script_files = vec![(
            "Justfile".to_string(),
            b"lint:\n  rubocop --parallel\n".to_vec(),
        )];
        let gem_names = vec!["rubocop".to_string()];
        let results =
            scan_scripts_for_gem_usage(Path::new("."), &gem_names, &HashMap::new(), &script_files);
        assert!(results.contains_key("rubocop"));
    }

    #[test]
    fn script_scan_matches_taskfile_cmds_block() {
        let script_files = vec![(
            "Taskfile.yml".to_string(),
            b"tasks:\n  lint:\n    cmds:\n      - rubocop --parallel\n".to_vec(),
        )];
        let gem_names = vec!["rubocop".to_string()];
        let results =
            scan_scripts_for_gem_usage(Path::new("."), &gem_names, &HashMap::new(), &script_files);
        assert!(results.contains_key("rubocop"));
    }

    #[test]
    fn script_scan_matches_yaml_block_chomping_indicator() {
        let script_files = vec![(
            ".github/workflows/ci.yml".to_string(),
            b"steps:\n  - run: |-\n      rubocop --parallel\n".to_vec(),
        )];
        let gem_names = vec!["rubocop".to_string()];
        let results =
            scan_scripts_for_gem_usage(Path::new("."), &gem_names, &HashMap::new(), &script_files);
        assert!(results.contains_key("rubocop"));
    }

    #[test]
    fn build_exe_to_gem_map_basic() {
        let dir = tempfile::tempdir().unwrap();
        let gem_dir = dir.path().join("gems").join("rake-13.0.0");
        std::fs::create_dir_all(&gem_dir).unwrap();
        std::fs::write(
            gem_dir.join("rake.gemspec"),
            "Gem::Specification.new do |s|\n  s.executables = ['rake']\nend\n",
        )
        .unwrap();

        let mut gem_index = HashMap::new();
        gem_index.insert("rake".to_string(), gem_dir);

        let gem_names = vec!["rake".to_string()];
        let _map = build_exe_to_gem_map(&gem_names, &gem_index);
        // rake executable matches gem name, so it's excluded from the map
        // (exe != gem_name check in the code)
    }

    #[test]
    fn build_exe_to_gem_map_with_alias() {
        let dir = tempfile::tempdir().unwrap();
        let gem_dir = dir.path().join("gems").join("webpacker-5.0.0");
        std::fs::create_dir_all(&gem_dir).unwrap();
        std::fs::write(
            gem_dir.join("webpacker.gemspec"),
            "Gem::Specification.new do |s|\n  s.executables = ['webpack', 'webpacker']\nend\n",
        )
        .unwrap();

        let mut gem_index = HashMap::new();
        gem_index.insert("webpacker".to_string(), gem_dir);

        let gem_names = vec!["webpacker".to_string()];
        let map = build_exe_to_gem_map(&gem_names, &gem_index);
        assert_eq!(map.get("webpack"), Some(&"webpacker".to_string()));
    }

    #[test]
    fn build_exe_to_gem_map_reads_installed_specifications() {
        let dir = tempfile::tempdir().unwrap();
        let gem_dir = dir.path().join("gems").join("reviewtool-core-1.0.0");
        let specs_dir = dir.path().join("specifications");
        std::fs::create_dir_all(&gem_dir).unwrap();
        std::fs::create_dir_all(&specs_dir).unwrap();
        std::fs::write(
            specs_dir.join("reviewtool-core-1.0.0.gemspec"),
            "s.executables = [\"reviewtool\".freeze]\n",
        )
        .unwrap();

        let gem_index = HashMap::from([("reviewtool-core".to_string(), gem_dir)]);
        let map = build_exe_to_gem_map(&["reviewtool-core".to_string()], &gem_index);
        assert_eq!(map.get("reviewtool"), Some(&"reviewtool-core".to_string()));
    }

    #[test]
    fn executable_map_uses_locked_installed_version() {
        let dir = tempfile::tempdir().unwrap();
        let gems_dir = dir.path().join("gems");
        for (version, executable) in [("1.0.0", "locked-runner"), ("2.0.0", "wrong-runner")] {
            let gem_dir = gems_dir.join(format!("runner-{version}"));
            std::fs::create_dir_all(&gem_dir).unwrap();
            std::fs::write(
                gem_dir.join("runner.gemspec"),
                format!("s.executables = ['{executable}']\n"),
            )
            .unwrap();
        }
        std::fs::write(
            dir.path().join("Gemfile.lock"),
            "GEM\n  specs:\n    runner (1.0.0)\n\nPLATFORMS\n  ruby\n",
        )
        .unwrap();
        let lockfile = crate::lockfile::parse_gemfile_lock(dir.path());
        let gem_index = crate::introspect::build_gem_index_for_lock(
            &[dir.path().to_path_buf()],
            &lockfile,
            Some("ruby"),
        );

        let map = build_exe_to_gem_map(&["runner".to_string()], &gem_index);
        assert_eq!(map.get("locked-runner"), Some(&"runner".to_string()));
        assert!(!map.contains_key("wrong-runner"));
    }

    #[test]
    fn strip_comments_empty_input() {
        let stripped = strip_comment_lines(b"");
        assert!(stripped.is_empty());
    }

    #[test]
    fn strip_comments_all_comments() {
        let input = b"# comment 1\n# comment 2\n# comment 3\n";
        let stripped = strip_comment_lines(input);
        let s = String::from_utf8_lossy(&stripped);
        assert!(!s.contains("comment 1"));
        assert!(!s.contains("comment 2"));
    }

    #[test]
    fn strip_comments_no_trailing_newline() {
        let input = b"actual content";
        let stripped = strip_comment_lines(input);
        assert_eq!(String::from_utf8_lossy(&stripped), "actual content");
    }

    #[test]
    fn build_exe_to_gem_map_single_exe_syntax() {
        // Tests the s.executables << 'exe' or s.executables = 'exe' pattern
        let dir = tempfile::tempdir().unwrap();
        let gem_dir = dir.path().join("gems").join("thor-1.0.0");
        std::fs::create_dir_all(&gem_dir).unwrap();
        std::fs::write(
            gem_dir.join("thor.gemspec"),
            "Gem::Specification.new do |s|\n  s.executables = 'thor_runner'\nend\n",
        )
        .unwrap();

        let mut gem_index = HashMap::new();
        gem_index.insert("thor".to_string(), gem_dir);

        let gem_names = vec!["thor".to_string()];
        let map = build_exe_to_gem_map(&gem_names, &gem_index);
        assert_eq!(map.get("thor_runner"), Some(&"thor".to_string()));
    }

    #[test]
    fn build_exe_to_gem_map_no_gemspec() {
        let dir = tempfile::tempdir().unwrap();
        let gem_dir = dir.path().join("gems").join("mygem-1.0.0");
        std::fs::create_dir_all(&gem_dir).unwrap();
        // No gemspec file at all

        let mut gem_index = HashMap::new();
        gem_index.insert("mygem".to_string(), gem_dir);

        let gem_names = vec!["mygem".to_string()];
        let map = build_exe_to_gem_map(&gem_names, &gem_index);
        assert!(map.is_empty());
    }

    #[test]
    fn scan_scripts_exe_to_gem_mapping() {
        // Tests the path where a script references an exe that maps to a gem via exe_to_gem
        let dir = tempfile::tempdir().unwrap();
        let gem_dir = dir.path().join("gems").join("webpacker-5.0.0");
        std::fs::create_dir_all(&gem_dir).unwrap();
        std::fs::write(
            gem_dir.join("webpacker.gemspec"),
            "Gem::Specification.new do |s|\n  s.executables = ['webpack']\nend\n",
        )
        .unwrap();

        let mut gem_index = HashMap::new();
        gem_index.insert("webpacker".to_string(), gem_dir);

        let script_files = vec![(
            "Makefile".to_string(),
            b"all:\n\tbundle exec webpack\n".to_vec(),
        )];
        let gem_names = vec!["webpacker".to_string()];
        let results =
            scan_scripts_for_gem_usage(Path::new("."), &gem_names, &gem_index, &script_files);
        assert!(
            results.contains_key("webpacker"),
            "should map webpack exe to webpacker gem"
        );
    }

    #[test]
    fn scan_scripts_bin_ref_with_exe_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let gem_dir = dir.path().join("gems").join("webpacker-5.0.0");
        std::fs::create_dir_all(&gem_dir).unwrap();
        std::fs::write(
            gem_dir.join("webpacker.gemspec"),
            "Gem::Specification.new do |s|\n  s.executables = ['webpack']\nend\n",
        )
        .unwrap();

        let mut gem_index = HashMap::new();
        gem_index.insert("webpacker".to_string(), gem_dir);

        let script_files = vec![("script/deploy".to_string(), b"bin/webpack\n".to_vec())];
        let gem_names = vec!["webpacker".to_string()];
        let results =
            scan_scripts_for_gem_usage(Path::new("."), &gem_names, &gem_index, &script_files);
        assert!(results.contains_key("webpacker"));
    }

    #[test]
    fn scan_bundle_exec_no_whitespace_after_exec() {
        // "bundle execute" should not match
        let results = scan_bundle_exec(b"bundle execute rspec\n");
        assert!(results.is_empty());
    }

    // ── inline comment stripping (fix #1) ──

    #[test]
    fn strip_comments_inline_yaml() {
        // Inline `#` after whitespace should be stripped
        let input = b"  - run: bundle exec minitest  # rspec was removed\n";
        let stripped = strip_comment_lines(input);
        let s = String::from_utf8_lossy(&stripped);
        assert!(s.contains("minitest"), "minitest must remain");
        assert!(
            !s.contains("rspec"),
            "inline comment with rspec must be stripped"
        );
    }

    #[test]
    fn strip_comments_inline_preserves_hash_in_double_quotes() {
        // `#` inside a double-quoted string is NOT a comment
        let input = b"  url: \"https://example.com/#anchor\"\n";
        let stripped = strip_comment_lines(input);
        let s = String::from_utf8_lossy(&stripped);
        assert!(
            s.contains("#anchor"),
            "fragment inside quotes must be preserved"
        );
    }

    #[test]
    fn strip_comments_inline_preserves_hash_in_single_quotes() {
        let input = b"  - run: echo 'foo#bar' # trailing\n";
        let stripped = strip_comment_lines(input);
        let s = String::from_utf8_lossy(&stripped);
        assert!(s.contains("foo#bar"), "hash inside single quotes preserved");
        assert!(!s.contains("trailing"), "trailing comment stripped");
    }

    #[test]
    fn strip_comments_inline_no_strip_without_whitespace() {
        // `#` not preceded by whitespace shouldn't be treated as a comment
        // (e.g., "id=#123" or anchor-like tokens)
        let input = b"label=foo#bar\n";
        let stripped = strip_comment_lines(input);
        let s = String::from_utf8_lossy(&stripped);
        assert!(
            s.contains("foo#bar"),
            "hash without leading ws is preserved"
        );
    }

    #[test]
    fn strip_comments_returns_borrowed_when_no_hash() {
        let input = b"steps:\n  - run: bundle exec rspec\n";
        let stripped = strip_comment_lines(input);
        // No `#` anywhere → must be Borrowed (zero-copy fast path)
        assert!(matches!(stripped, Cow::Borrowed(_)));
    }

    #[test]
    fn strip_comments_returns_owned_when_hash_present() {
        let input = b"# comment\nbundle exec rspec\n";
        let stripped = strip_comment_lines(input);
        assert!(matches!(stripped, Cow::Owned(_)));
    }

    #[test]
    fn script_scan_ignores_inline_gem_comment() {
        // End-to-end: gem appearing only in an inline comment must not be marked used
        let script_files = vec![(
            ".github/workflows/ci.yml".to_string(),
            b"steps:\n  - run: bundle exec minitest  # rspec was removed\n".to_vec(),
        )];
        let gem_names = vec!["rspec".to_string(), "minitest".to_string()];
        let gem_index = HashMap::new();
        let results =
            scan_scripts_for_gem_usage(Path::new("."), &gem_names, &gem_index, &script_files);
        assert!(
            !results.contains_key("rspec"),
            "rspec in inline comment must not be matched"
        );
        assert!(
            results.contains_key("minitest"),
            "minitest in command should be matched"
        );
    }

    // ── is_script_file expanded coverage (fix #2) ──

    #[test]
    fn script_file_buildkite() {
        assert!(is_script_file(".buildkite/pipeline.yml", "pipeline.yml"));
        assert!(is_script_file(".buildkite/steps/test.yaml", "test.yaml"));
    }

    #[test]
    fn script_file_drone() {
        assert!(is_script_file(".drone.yml", ".drone.yml"));
        assert!(is_script_file(".drone.yaml", ".drone.yaml"));
    }

    #[test]
    fn script_file_jenkinsfile() {
        assert!(is_script_file("Jenkinsfile", "Jenkinsfile"));
        assert!(is_script_file("ci/Jenkinsfile", "Jenkinsfile"));
        assert!(is_script_file("Jenkinsfile.release", "Jenkinsfile.release"));
    }

    #[test]
    fn script_file_bitbucket() {
        assert!(is_script_file(
            "bitbucket-pipelines.yml",
            "bitbucket-pipelines.yml"
        ));
        assert!(is_script_file(
            "bitbucket-pipelines.yaml",
            "bitbucket-pipelines.yaml"
        ));
    }

    #[test]
    fn script_file_azure_pipelines() {
        assert!(is_script_file("azure-pipelines.yml", "azure-pipelines.yml"));
        assert!(is_script_file(
            "azure-pipelines.yaml",
            "azure-pipelines.yaml"
        ));
        assert!(is_script_file(".azure-pipelines/build.yml", "build.yml"));
    }

    #[test]
    fn script_file_earthfile() {
        assert!(is_script_file("Earthfile", "Earthfile"));
    }

    #[test]
    fn script_file_justfile() {
        assert!(is_script_file("Justfile", "Justfile"));
        assert!(is_script_file("justfile", "justfile"));
        assert!(is_script_file(".justfile", ".justfile"));
    }

    #[test]
    fn script_file_taskfile() {
        assert!(is_script_file("Taskfile.yml", "Taskfile.yml"));
        assert!(is_script_file("Taskfile.yaml", "Taskfile.yaml"));
    }

    #[test]
    fn script_file_top_level_shell() {
        assert!(is_script_file("setup.sh", "setup.sh"));
        assert!(is_script_file("run.bash", "run.bash"));
        assert!(is_script_file("test.bats", "test.bats"));
    }

    #[test]
    fn script_file_nested_shell_not_top_level() {
        // Outside script/ scripts/ bin/ a nested shell file should not match
        assert!(!is_script_file("vendor/setup.sh", "setup.sh"));
        // But under script/ it does (existing rule)
        assert!(is_script_file("script/setup.sh", "setup.sh"));
    }

    #[test]
    fn script_file_mise_and_tool_versions() {
        assert!(is_script_file("mise.toml", "mise.toml"));
        assert!(is_script_file(".mise.toml", ".mise.toml"));
        assert!(is_script_file(".tool-versions", ".tool-versions"));
    }

    // ── AhoCorasick word boundary: hyphen is part of the gem token (fix #3) ──

    #[test]
    fn script_scan_rack_mini_profiler_does_not_match_rack() {
        let script_files = vec![(
            "Makefile".to_string(),
            b"profile:\n\tbundle exec rack-mini-profiler\n".to_vec(),
        )];
        let gem_names = vec!["rack".to_string(), "rack-mini-profiler".to_string()];
        let gem_index = HashMap::new();
        let results =
            scan_scripts_for_gem_usage(Path::new("."), &gem_names, &gem_index, &script_files);
        assert!(
            results.contains_key("rack-mini-profiler"),
            "rack-mini-profiler must be matched"
        );
        assert!(
            !results.contains_key("rack"),
            "rack must NOT be matched as a substring of rack-mini-profiler"
        );
    }

    #[test]
    fn script_scan_rack_alone_still_matches() {
        let script_files = vec![("script/server".to_string(), b"rack\n".to_vec())];
        let gem_names = vec!["rack".to_string()];
        let gem_index = HashMap::new();
        let results =
            scan_scripts_for_gem_usage(Path::new("."), &gem_names, &gem_index, &script_files);
        assert!(
            results.contains_key("rack"),
            "standalone rack reference must still match"
        );
    }

    fn scan(files: &[(&str, &str)], gems: &[&str]) -> HashMap<String, HashSet<String>> {
        let script_files: Vec<(String, Vec<u8>)> = files
            .iter()
            .map(|(path, content)| (path.to_string(), content.as_bytes().to_vec()))
            .collect();
        let gem_names: Vec<String> = gems.iter().map(|g| g.to_string()).collect();
        scan_scripts_for_gem_usage(Path::new("."), &gem_names, &HashMap::new(), &script_files)
    }

    // ── C8: *.rake task files ──

    #[test]
    fn script_file_rake_tasks() {
        assert!(is_script_file("lib/tasks/lint.rake", "lint.rake"));
        assert!(is_script_file("engines/a/lib/tasks/x.rake", "x.rake"));
    }

    #[test]
    fn script_scan_rake_task_shell_outs() {
        let results = scan(
            &[(
                "lib/tasks/lint.rake",
                "task :lint do\n  sh \"bundle exec rubocop\"\n  sh \"bundle exec haml-lint app/views\"\n  system(\"standardrb --fix\")\n  `erblint --lint-all`\nend\n",
            )],
            &["rubocop", "haml_lint", "standard", "erb_lint", "rails"],
        );
        for gem in ["rubocop", "haml_lint", "standard", "erb_lint"] {
            assert!(results.contains_key(gem), "{gem} should be credited");
        }
        assert!(!results.contains_key("rails"));
    }

    #[test]
    fn script_scan_rake_ignores_keywords_inside_strings() {
        let results = scan(
            &[(
                "Rakefile",
                "desc \"Flush redis cache\"\ntask :flush do\n  puts 'run system redis'\nend\ntask :sh\n",
            )],
            &["redis", "cache"],
        );
        assert!(results.is_empty(), "got {results:?}");
    }

    // ── C21: only command words count ──

    #[test]
    fn script_scan_ignores_command_arguments() {
        let results = scan(
            &[(
                "Dockerfile",
                "FROM ruby:3.3\nRUN apt-get update && apt-get install -y git curl sqlite3\n",
            )],
            &["git", "curl", "sqlite3"],
        );
        assert!(results.is_empty(), "got {results:?}");
    }

    #[test]
    fn command_words_skip_prefixes_and_assignments() {
        assert_eq!(
            command_words(b"RAILS_ENV=test bundle exec rake db:setup && sudo -E foreman start"),
            vec!["bundle", "rake", "foreman"]
        );
        assert_eq!(
            command_words(b"time nohup ./bin/sidekiq -q default | tee log; exec puma"),
            vec!["sidekiq", "tee", "puma"]
        );
        assert_eq!(
            command_words(b"echo \"a; rubocop\" || env FOO=1 brakeman -q"),
            vec!["echo", "brakeman"]
        );
        assert_eq!(
            command_words(br#"["./bin/thrust", "./bin/rails", "server"]"#),
            vec!["thrust"]
        );
    }

    // ── C22: curated executable fallback ──

    #[test]
    fn script_scan_known_executable_fallback() {
        let results = scan(
            &[
                (
                    ".github/workflows/ci.yml",
                    "jobs:\n  t:\n    steps:\n      - run: bundle exec cap production deploy\n      - run: rdbg -c -- bin/rails test\n",
                ),
                ("Dockerfile", "CMD [\"./bin/thrust\", \"./bin/rails\", \"server\"]\n"),
                ("Procfile", "web: bundle exec pumactl start\nlint: haml-lint\n"),
            ],
            &["capistrano", "debug", "thruster", "railties", "puma", "haml_lint"],
        );
        for gem in [
            "capistrano",
            "debug",
            "thruster",
            "railties",
            "puma",
            "haml_lint",
        ] {
            assert!(results.contains_key(gem), "{gem} should be credited");
        }
    }

    #[test]
    fn normalizes_hyphen_and_underscore() {
        let results = scan(&[("bin/lint", "i18n_tasks health\n")], &["i18n-tasks"]);
        assert!(results.contains_key("i18n-tasks"));
    }

    // ── C11: GitLab compact YAML lists ──

    #[test]
    fn script_scan_gitlab_compact_list() {
        let results = scan(
            &[(
                ".gitlab-ci.yml",
                "lint:\n  script:\n  - rubocop\n  - brakeman -q\n  tags:\n  - puma\n",
            )],
            &["rubocop", "brakeman", "puma"],
        );
        assert!(results.contains_key("rubocop"));
        assert!(results.contains_key("brakeman"));
        assert!(!results.contains_key("puma"), "tags list is not a script");
    }

    #[test]
    fn script_scan_block_scalar_does_not_capture_sibling_steps() {
        let results = scan(
            &[(
                ".github/workflows/ci.yml",
                "steps:\n  - run: |\n      echo hi\n  - name: puma\n",
            )],
            &["puma"],
        );
        assert!(!results.contains_key("puma"));
    }
}
