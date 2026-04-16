use std::collections::{HashMap, HashSet};
use std::path::Path;

use memchr::memmem;
use rayon::prelude::*;
use aho_corasick::AhoCorasick;
use regex::Regex;

/// Check if a path matches our script file patterns.
pub(crate) fn is_script_file(rel_path: &str, file_name: &str) -> bool {
    // Normalize path separators for cross-platform support
    let rel_path = &rel_path.replace('\\', "/");
    // Exact top-level filenames
    match file_name {
        "Makefile" | "makefile" | "GNUmakefile" | "Dockerfile" | "Procfile" => return true,
        _ => {}
    }
    // Dockerfile.* / Procfile.*
    if file_name.starts_with("Dockerfile.") || file_name.starts_with("Procfile.") {
        return true;
    }
    // docker-compose.yml/yaml
    if file_name == "docker-compose.yml" || file_name == "docker-compose.yaml" {
        return true;
    }
    // .github/workflows/*.yml/*.yaml
    if (rel_path.starts_with(".github/workflows/") || rel_path.starts_with(".github/actions/"))
        && (file_name.ends_with(".yml") || file_name.ends_with(".yaml"))
    {
        return true;
    }
    // .circleci/config.yml, .gitlab-ci.yml
    if rel_path == ".circleci/config.yml" || rel_path == ".gitlab-ci.yml" {
        return true;
    }
    // script/**/* , scripts/**/* , bin/**/*
    if rel_path.starts_with("script/")
        || rel_path.starts_with("scripts/")
        || rel_path.starts_with("bin/")
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

/// Strip comment lines from script file content.
/// Handles `#` comments for YAML/shell/Ruby, and Makefile-style comments.
/// Preserves non-comment lines so byte offsets for bundle exec/bin scanners stay valid.
fn strip_comment_lines(content: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(content.len());
    let mut i = 0;
    while i < content.len() {
        // Find end of line
        let line_start = i;
        while i < content.len() && content[i] != b'\n' {
            i += 1;
        }
        let line_end = i;
        if i < content.len() {
            i += 1; // skip \n
        }

        // Check if line is a comment (starts with # after whitespace)
        let mut j = line_start;
        while j < line_end && (content[j] == b' ' || content[j] == b'\t') {
            j += 1;
        }
        if j < line_end && content[j] == b'#' {
            // Replace with blank line to preserve structure
            result.push(b'\n');
        } else {
            result.extend_from_slice(&content[line_start..line_end]);
            if line_end < content.len() {
                result.push(b'\n');
            }
        }
    }
    result
}

/// Scan non-Ruby files (Makefile, Dockerfile, CI configs, scripts)
/// for `bundle exec <gem>` invocations and bare gem CLI references.
/// Uses pre-collected script files from the source tree walk (single-walk mode).
pub fn scan_scripts_for_gem_usage(
    _dir: &Path,
    gem_names: &[String],
    gem_index: &HashMap<String, std::path::PathBuf>,
    script_files: &[(String, Vec<u8>)],
) -> HashMap<String, HashSet<String>> {
    let gem_name_set: HashSet<&str> = gem_names.iter().map(|s| s.as_str()).collect();

    // Build executable→gem name mapping from installed gemspecs (parallel)
    let exe_to_gem = build_exe_to_gem_map(gem_names, gem_index);

    // Build AhoCorasick automaton for multi-pattern byte matching
    let ac = AhoCorasick::new(gem_names).unwrap();

    // Process pre-collected script files in parallel
    let per_file_refs: Vec<Vec<(String, String)>> = script_files
        .par_iter()
        .filter_map(|(rel_path, content)| {
            let mut refs = Vec::new();

            // Strip comment lines to avoid matching gem names in comments
            let stripped = strip_comment_lines(content);

            // Byte scanner: bundle exec
            for cmd in scan_bundle_exec(&stripped) {
                if gem_name_set.contains(cmd.as_str()) {
                    refs.push((cmd, rel_path.clone()));
                } else if let Some(gem_name) = exe_to_gem.get(&cmd) {
                    refs.push((gem_name.clone(), rel_path.clone()));
                }
            }

            // Byte scanner: bin/ references
            for cmd in scan_bin_refs(&stripped) {
                if gem_name_set.contains(cmd.as_str()) {
                    refs.push((cmd, rel_path.clone()));
                } else if let Some(gem_name) = exe_to_gem.get(&cmd) {
                    refs.push((gem_name.clone(), rel_path.clone()));
                }
            }

            // AhoCorasick: gem name byte match with manual word boundaries
            let mut seen = HashSet::new();
            for mat in ac.find_overlapping_iter(&stripped) {
                let pid = mat.pattern().as_usize();
                if !seen.insert(pid) {
                    continue;
                }
                let start = mat.start();
                let end = mat.end();
                let before_ok = start == 0 || {
                    let b = stripped[start - 1];
                    !b.is_ascii_alphanumeric() && b != b'_'
                };
                let after_ok = end >= stripped.len() || {
                    let b = stripped[end];
                    !b.is_ascii_alphanumeric() && b != b'_'
                };
                if before_ok && after_ok {
                    refs.push((gem_names[pid].clone(), rel_path.clone()));
                }
            }

            if refs.is_empty() {
                None
            } else {
                Some(refs)
            }
        })
        .collect();

    let mut gem_refs: HashMap<String, HashSet<String>> = HashMap::new();
    for refs in per_file_refs {
        for (gem_name, file) in refs {
            gem_refs.entry(gem_name).or_default().insert(file);
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

    // Collect gem names that have a gem_index entry, then read in parallel
    let gems_with_dirs: Vec<(&str, &std::path::PathBuf)> = gem_names
        .iter()
        .filter_map(|n| gem_index.get(n).map(|d| (n.as_str(), d)))
        .collect();

    let entries: Vec<(String, String)> = gems_with_dirs
        .par_iter()
        .flat_map(|(gem_name, gem_dir)| {
            let mut exes = Vec::new();
            let pattern = gem_dir.join("*.gemspec").display().to_string();
            for spec_path in glob::glob(&pattern).into_iter().flatten().flatten() {
                if let Ok(content) = std::fs::read_to_string(&spec_path) {
                    if let Some(cap) = EXE_RE.captures(&content) {
                        for exe in cap[1].split(',') {
                            let exe = exe.trim().trim_matches(|c| c == '\'' || c == '"').trim();
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
        let script_files = vec![("Dockerfile".to_string(), b"RUN gem install puma\n".to_vec())];
        let gem_names = vec!["puma".to_string()];
        let gem_index = HashMap::new();
        let results =
            scan_scripts_for_gem_usage(Path::new("."), &gem_names, &gem_index, &script_files);
        assert!(results.contains_key("puma"));
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
}
