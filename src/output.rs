use std::collections::HashMap;
use std::path::Path;

use crate::lockfile::count_transitive_deps;
use crate::types::{Evidence, GemResult, GemStatus};

struct Categories<'a> {
    unused: Vec<(&'a str, &'a GemResult)>,
    bundler_required: Vec<(&'a str, &'a GemResult)>,
    used: Vec<(&'a str, &'a GemResult)>,
    ignored: Vec<(&'a str, &'a GemResult)>,
    platform_excluded: Vec<(&'a str, &'a GemResult)>,
}

fn categorize(results: &HashMap<String, GemResult>) -> Categories<'_> {
    let mut cats = Categories {
        unused: Vec::new(),
        bundler_required: Vec::new(),
        used: Vec::new(),
        ignored: Vec::new(),
        platform_excluded: Vec::new(),
    };

    for (name, info) in results {
        match info.status {
            GemStatus::Unused => cats.unused.push((name.as_str(), info)),
            GemStatus::BundlerRequired => cats.bundler_required.push((name.as_str(), info)),
            GemStatus::Used => cats.used.push((name.as_str(), info)),
            GemStatus::Ignored => cats.ignored.push((name.as_str(), info)),
            GemStatus::PlatformExcluded => cats.platform_excluded.push((name.as_str(), info)),
        }
    }

    cats.unused.sort_by_key(|(n, _)| *n);
    cats.bundler_required.sort_by_key(|(n, _)| *n);
    cats.used.sort_by_key(|(n, _)| *n);
    cats.ignored.sort_by_key(|(n, _)| *n);
    cats.platform_excluded.sort_by_key(|(n, _)| *n);

    cats
}

pub fn print_results_text(
    results: &HashMap<String, GemResult>,
    verbose: bool,
    lock_deps: &HashMap<String, Vec<String>>,
) -> usize {
    let cats = categorize(results);
    let total = results.len();

    if !cats.unused.is_empty() {
        println!("❌ Unused ({})", cats.unused.len());
        println!();
        for (name, info) in &cats.unused {
            let transitive = if !lock_deps.is_empty() {
                format!(
                    " ({} transitive deps)",
                    count_transitive_deps(name, lock_deps)
                )
            } else {
                String::new()
            };
            println!("  {}  ({}){}", name, info.gem.groups.join(", "), transitive);
            if verbose {
                println!("    require paths: {}", info.gem.require_paths.join(", "));
                println!();
            }
        }
        println!();
    }

    if !cats.bundler_required.is_empty() {
        println!("⚠️  Bundler.require only ({})", cats.bundler_required.len());
        println!();
        for (name, info) in &cats.bundler_required {
            println!("  {}  ({})", name, info.gem.groups.join(", "));
            if verbose {
                println!("    require paths: {}", info.gem.require_paths.join(", "));
                println!();
            }
        }
        println!();
    }

    if !cats.platform_excluded.is_empty() {
        println!("ℹ️  Platform-excluded ({})", cats.platform_excluded.len());
        println!();
        let names: Vec<&str> = cats.platform_excluded.iter().map(|(n, _)| *n).collect();
        println!("  {}", names.join(", "));
        println!();
    }

    if !cats.ignored.is_empty() {
        println!("🔇 Ignored via ignore file ({})", cats.ignored.len());
        println!();
        let names: Vec<&str> = cats.ignored.iter().map(|(n, _)| *n).collect();
        println!("  {}", names.join(", "));
        println!();
    }

    if !cats.used.is_empty() {
        println!("✅ Used ({})", cats.used.len());
        println!();
        if verbose {
            for (name, info) in &cats.used {
                println!("  {}  ({})", name, info.gem.groups.join(", "));
                for ev in &info.evidence {
                    match ev {
                        Evidence::Require { path, file } => {
                            println!("    → require '{}' in {}", path, file)
                        }
                        Evidence::Module {
                            name: mod_name,
                            file,
                        } => println!("    → {} in {}", mod_name, file),
                        Evidence::Script { file } => println!("    → script ref in {}", file),
                        Evidence::BundlerRequire => println!("    → Bundler.require"),
                        Evidence::DatabaseAdapter {
                            adapter,
                            kind,
                            source,
                        } => println!("    → {}: {} in {}", adapter_label(kind), adapter, source),
                    }
                }
                println!();
            }
        } else {
            let names: Vec<&str> = cats.used.iter().map(|(n, _)| *n).collect();
            for row in format_name_grid(&names, terminal_width()) {
                println!("{}", row);
            }
            println!();
        }
    }

    let extra = {
        let mut parts = Vec::new();
        if !cats.ignored.is_empty() {
            parts.push(format!("{} ignored", cats.ignored.len()));
        }
        if !cats.platform_excluded.is_empty() {
            parts.push(format!(
                "{} platform-excluded",
                cats.platform_excluded.len()
            ));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(", {}", parts.join(", "))
        }
    };
    println!(
        "{} gems: {} used, {} unused, {} bundler.require{}",
        total,
        cats.used.len(),
        cats.unused.len(),
        cats.bundler_required.len(),
        extra,
    );

    cats.unused.len()
}

#[derive(serde::Serialize)]
struct JsonGemRecord {
    name: String,
    status: String,
    groups: Vec<String>,
    require_paths: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    files: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    evidence: Vec<Evidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transitive_dep_count: Option<usize>,
}

pub fn print_results_json(
    results: &HashMap<String, GemResult>,
    lock_deps: &HashMap<String, Vec<String>>,
) -> usize {
    let mut records: Vec<JsonGemRecord> = results
        .iter()
        .map(|(name, info)| {
            let transitive = if !lock_deps.is_empty()
                && (info.status == GemStatus::Unused || info.status == GemStatus::BundlerRequired)
            {
                Some(count_transitive_deps(name, lock_deps))
            } else {
                None
            };

            JsonGemRecord {
                name: name.clone(),
                status: info.status.to_string(),
                groups: info.gem.groups.clone(),
                require_paths: info.gem.require_paths.clone(),
                files: info.files.clone(),
                evidence: info.evidence.clone(),
                transitive_dep_count: transitive,
            }
        })
        .collect();

    records.sort_by(|a, b| a.name.cmp(&b.name));

    let envelope = serde_json::json!({
        "schema_version": "1",
        "results": records,
    });
    println!("{}", serde_json::to_string_pretty(&envelope).unwrap());

    results
        .values()
        .filter(|v| v.status == GemStatus::Unused)
        .count()
}

pub fn print_results_sarif(
    results: &HashMap<String, GemResult>,
    _dir: &Path,
    _lock_deps: &HashMap<String, Vec<String>>,
) -> usize {
    let mut sarif_results = Vec::new();
    let mut sorted: Vec<(&String, &GemResult)> = results.iter().collect();
    sorted.sort_by_key(|(n, _)| n.as_str());

    for (name, info) in &sorted {
        if info.status != GemStatus::Unused {
            continue;
        }
        sarif_results.push(serde_json::json!({
            "ruleId": "unused-gem",
            "level": "warning",
            "message": { "text": format!("Gem '{}' appears unused", name) },
            "locations": [{
                "physicalLocation": {
                    "artifactLocation": { "uri": "Gemfile" }
                }
            }]
        }));
    }

    let sarif = serde_json::json!({
        "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "unused-gems",
                    "version": env!("CARGO_PKG_VERSION"),
                    "informationUri": "https://github.com/mrecachinas/unused-gems",
                    "rules": [{
                        "id": "unused-gem",
                        "shortDescription": { "text": "Gem declared in Gemfile but not referenced in source" },
                        "defaultConfiguration": { "level": "warning" }
                    }]
                }
            },
            "results": sarif_results
        }]
    });

    println!("{}", serde_json::to_string_pretty(&sarif).unwrap());

    sorted
        .iter()
        .filter(|(_, info)| info.status == GemStatus::Unused)
        .count()
}

pub fn print_results_github(
    results: &HashMap<String, GemResult>,
    dir: &Path,
    lock_deps: &HashMap<String, Vec<String>>,
) -> usize {
    let line_nums = gemfile_line_numbers(dir);
    let mut unused_count = 0;

    let mut entries: Vec<(&str, &GemResult)> =
        results.iter().map(|(n, r)| (n.as_str(), r)).collect();
    entries.sort_by_key(|(n, _)| *n);

    for (name, info) in entries {
        if info.status != GemStatus::Unused
            && info.status != GemStatus::BundlerRequired
            && info.status != GemStatus::PlatformExcluded
        {
            continue;
        }

        let location = match line_nums.get(name) {
            Some(line) => format!("file={},line={}", escape_property("Gemfile"), line),
            None => format!("file={}", escape_property("Gemfile")),
        };

        let level = if info.status == GemStatus::Unused {
            "warning"
        } else {
            "notice"
        };

        let transitive = if !lock_deps.is_empty() {
            format!(
                " (pulls in {} transitive deps)",
                count_transitive_deps(name, lock_deps)
            )
        } else {
            String::new()
        };

        let msg = match info.status {
            GemStatus::Unused => format!("Gem '{}' appears unused{}", name, transitive),
            GemStatus::PlatformExcluded => {
                format!(
                    "Gem '{}' excluded (platform {:?} does not match current)",
                    name, info.gem.platforms
                )
            }
            GemStatus::BundlerRequired => {
                let evidence_summary = summarize_evidence(&info.evidence);
                if evidence_summary.is_empty() {
                    format!(
                        "Gem '{}' is only loaded via Bundler.require — no direct reference found",
                        name
                    )
                } else {
                    format!(
                        "Gem '{}' is only loaded via Bundler.require ({})",
                        name, evidence_summary
                    )
                }
            }
            _ => continue,
        };

        println!("::{} {}::{}", level, location, escape_data(&msg));
        if info.status == GemStatus::Unused {
            unused_count += 1;
        }
    }

    unused_count
}

fn adapter_label(kind: &str) -> &'static str {
    match kind {
        "database" => "database adapter",
        "cable" => "cable adapter",
        "storage" => "storage service",
        "cache" => "cache store",
        _ => "adapter",
    }
}

fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|columns| columns.trim().parse::<usize>().ok())
        .filter(|&columns| columns > 0)
        .unwrap_or(100)
}

/// Lay names out in columns sized to the longest name (+2 spaces), fitting
/// as many columns as `width` allows (at least one), indented by two spaces.
fn format_name_grid(names: &[&str], width: usize) -> Vec<String> {
    const INDENT: usize = 2;
    let longest = names.iter().map(|n| n.chars().count()).max().unwrap_or(0);
    let column_width = longest + 2;
    let columns = (width.saturating_sub(INDENT) / column_width).max(1);
    names
        .chunks(columns)
        .map(|row| {
            let mut line = " ".repeat(INDENT);
            for name in row {
                line.push_str(&format!("{:<column_width$}", name));
            }
            line.trim_end().to_string()
        })
        .collect()
}

/// Escape a GitHub Actions workflow command message.
pub(crate) fn escape_data(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Escape a GitHub Actions workflow command property value.
pub(crate) fn escape_property(value: &str) -> String {
    escape_data(value).replace(':', "%3A").replace(',', "%2C")
}

fn summarize_evidence(evidence: &[Evidence]) -> String {
    let mut parts = Vec::new();
    let mut require_count = 0;
    let mut module_count = 0;
    let mut script_count = 0;
    for ev in evidence {
        match ev {
            Evidence::Require { .. } => require_count += 1,
            Evidence::Module { .. } => module_count += 1,
            Evidence::Script { .. } => script_count += 1,
            Evidence::BundlerRequire => parts.push("bundler_require".to_string()),
            Evidence::DatabaseAdapter { adapter, .. } => parts.push(format!("adapter:{}", adapter)),
        }
    }
    if require_count > 0 {
        parts.push(format!("{} require(s)", require_count));
    }
    if module_count > 0 {
        parts.push(format!("{} module ref(s)", module_count));
    }
    if script_count > 0 {
        parts.push(format!("{} script ref(s)", script_count));
    }
    parts.join(", ")
}

fn gemfile_line_numbers(dir: &Path) -> HashMap<String, usize> {
    let gemfile_path = dir.join("Gemfile");
    let content = match std::fs::read_to_string(gemfile_path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    let re = regex::Regex::new(r#"^\s*gem\s+['"]([^'"]+)['"]"#).unwrap();
    let mut lines = HashMap::new();

    for (idx, line) in content.lines().enumerate() {
        if let Some(cap) = re.captures(line) {
            lines.insert(cap[1].to_string(), idx + 1);
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::GemEntry;

    fn make_gem_entry(name: &str, groups: &[&str]) -> GemEntry {
        GemEntry {
            name: name.to_string(),
            groups: groups.iter().map(|s| s.to_string()).collect(),
            platforms: Vec::new(),
            require_paths: vec![name.to_string()],
            module_names: Vec::new(),
            require_false: false,
            source: None,
        }
    }

    fn make_result(name: &str, status: GemStatus, groups: &[&str]) -> (String, GemResult) {
        (
            name.to_string(),
            GemResult {
                status,
                files: if status == GemStatus::Used {
                    vec!["app.rb".to_string()]
                } else {
                    Vec::new()
                },
                evidence: if status == GemStatus::Used {
                    vec![Evidence::Require {
                        path: name.to_string(),
                        file: "app.rb".to_string(),
                    }]
                } else {
                    Vec::new()
                },
                gem: make_gem_entry(name, groups),
            },
        )
    }

    fn build_results() -> HashMap<String, GemResult> {
        [
            make_result("rails", GemStatus::Used, &["default"]),
            make_result("unused-gem", GemStatus::Unused, &["default"]),
            make_result("dotenv", GemStatus::BundlerRequired, &["default"]),
            make_result("debug", GemStatus::Ignored, &["development"]),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn text_output_returns_unused_count() {
        let results = build_results();
        let lock_deps = HashMap::new();
        let count = print_results_text(&results, false, &lock_deps);
        assert_eq!(count, 1);
    }

    #[test]
    fn text_output_verbose() {
        let results = build_results();
        let lock_deps = HashMap::new();
        let count = print_results_text(&results, true, &lock_deps);
        assert_eq!(count, 1);
    }

    #[test]
    fn text_output_with_lock_deps() {
        let results = build_results();
        let mut lock_deps = HashMap::new();
        lock_deps.insert("unused-gem".to_string(), vec!["dep1".to_string()]);
        lock_deps.insert("dep1".to_string(), vec![]);
        let count = print_results_text(&results, false, &lock_deps);
        assert_eq!(count, 1);
    }

    #[test]
    fn json_output_returns_unused_count() {
        let results = build_results();
        let lock_deps = HashMap::new();
        let count = print_results_json(&results, &lock_deps);
        assert_eq!(count, 1);
    }

    #[test]
    fn json_output_with_lock_deps() {
        let results = build_results();
        let mut lock_deps = HashMap::new();
        lock_deps.insert("unused-gem".to_string(), vec!["dep1".to_string()]);
        lock_deps.insert("dep1".to_string(), vec![]);
        let count = print_results_json(&results, &lock_deps);
        assert_eq!(count, 1);
    }

    #[test]
    fn github_output_returns_unused_count() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "gem 'rails'\ngem 'unused-gem'\ngem 'dotenv'\n",
        )
        .unwrap();
        let results = build_results();
        let lock_deps = HashMap::new();
        let count = print_results_github(&results, dir.path(), &lock_deps);
        assert_eq!(count, 1);
    }

    #[test]
    fn github_output_with_lock_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gem 'unused-gem'\n").unwrap();
        let results: HashMap<String, GemResult> =
            [make_result("unused-gem", GemStatus::Unused, &["default"])]
                .into_iter()
                .collect();
        let mut lock_deps = HashMap::new();
        lock_deps.insert("unused-gem".to_string(), vec!["dep1".to_string()]);
        let count = print_results_github(&results, dir.path(), &lock_deps);
        assert_eq!(count, 1);
    }

    #[test]
    fn categorize_sorts_alphabetically() {
        let results = build_results();
        let cats = categorize(&results);
        assert_eq!(cats.unused.len(), 1);
        assert_eq!(cats.used.len(), 1);
        assert_eq!(cats.bundler_required.len(), 1);
        assert_eq!(cats.ignored.len(), 1);
    }

    #[test]
    fn summarize_evidence_mixed() {
        let evidence = vec![
            Evidence::Require {
                path: "foo".into(),
                file: "a.rb".into(),
            },
            Evidence::Require {
                path: "foo".into(),
                file: "b.rb".into(),
            },
            Evidence::Module {
                name: "Foo".into(),
                file: "c.rb".into(),
            },
            Evidence::Script {
                file: "ci.yml".into(),
            },
            Evidence::BundlerRequire,
            Evidence::DatabaseAdapter {
                adapter: "pg".into(),
                kind: "database".into(),
                source: "config/database.yml".into(),
            },
        ];
        let s = summarize_evidence(&evidence);
        assert!(s.contains("2 require(s)"));
        assert!(s.contains("1 module ref(s)"));
        assert!(s.contains("1 script ref(s)"));
        assert!(s.contains("bundler_require"));
        assert!(s.contains("adapter:pg"));
    }

    #[test]
    fn summarize_evidence_empty() {
        assert_eq!(summarize_evidence(&[]), "");
    }

    #[test]
    fn gemfile_line_numbers_basic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "source 'https://rubygems.org'\ngem 'rails'\ngem 'puma'\n",
        )
        .unwrap();
        let lines = gemfile_line_numbers(dir.path());
        assert_eq!(lines["rails"], 2);
        assert_eq!(lines["puma"], 3);
    }

    #[test]
    fn gemfile_line_numbers_missing() {
        let dir = tempfile::tempdir().unwrap();
        let lines = gemfile_line_numbers(dir.path());
        assert!(lines.is_empty());
    }

    #[test]
    fn gem_status_display() {
        assert_eq!(GemStatus::Used.to_string(), "used");
        assert_eq!(GemStatus::Unused.to_string(), "unused");
        assert_eq!(GemStatus::BundlerRequired.to_string(), "bundler_required");
        assert_eq!(GemStatus::Ignored.to_string(), "ignored");
    }

    #[test]
    fn github_output_bundler_required_with_evidence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gem 'dotenv'\n").unwrap();
        let results: HashMap<String, GemResult> = [(
            "dotenv".to_string(),
            GemResult {
                status: GemStatus::BundlerRequired,
                files: vec!["app.rb".to_string()],
                evidence: vec![Evidence::Script {
                    file: "ci.yml".into(),
                }],
                gem: make_gem_entry("dotenv", &["default"]),
            },
        )]
        .into_iter()
        .collect();
        let lock_deps = HashMap::new();
        let count = print_results_github(&results, dir.path(), &lock_deps);
        assert_eq!(count, 0); // BundlerRequired doesn't count as unused
    }

    #[test]
    fn github_output_bundler_required_no_evidence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gem 'dotenv'\n").unwrap();
        let results: HashMap<String, GemResult> = [(
            "dotenv".to_string(),
            GemResult {
                status: GemStatus::BundlerRequired,
                files: Vec::new(),
                evidence: Vec::new(),
                gem: make_gem_entry("dotenv", &["default"]),
            },
        )]
        .into_iter()
        .collect();
        let lock_deps = HashMap::new();
        let count = print_results_github(&results, dir.path(), &lock_deps);
        assert_eq!(count, 0);
    }

    #[test]
    fn github_output_gem_not_in_gemfile() {
        let dir = tempfile::tempdir().unwrap();
        // Gemfile doesn't mention the unused gem
        std::fs::write(dir.path().join("Gemfile"), "gem 'rails'\n").unwrap();
        let results: HashMap<String, GemResult> =
            [make_result("orphan-gem", GemStatus::Unused, &["default"])]
                .into_iter()
                .collect();
        let lock_deps = HashMap::new();
        let count = print_results_github(&results, dir.path(), &lock_deps);
        assert_eq!(count, 1);
    }

    #[test]
    fn text_output_only_unused() {
        let results: HashMap<String, GemResult> = [
            make_result("unused1", GemStatus::Unused, &["default"]),
            make_result("unused2", GemStatus::Unused, &["test"]),
        ]
        .into_iter()
        .collect();
        let lock_deps = HashMap::new();
        let count = print_results_text(&results, false, &lock_deps);
        assert_eq!(count, 2);
    }

    #[test]
    fn text_output_only_bundler_required() {
        let results: HashMap<String, GemResult> = [make_result(
            "dotenv",
            GemStatus::BundlerRequired,
            &["default"],
        )]
        .into_iter()
        .collect();
        let lock_deps = HashMap::new();
        let count = print_results_text(&results, true, &lock_deps);
        assert_eq!(count, 0);
    }

    #[test]
    fn text_output_only_ignored() {
        let results: HashMap<String, GemResult> =
            [make_result("debug", GemStatus::Ignored, &["development"])]
                .into_iter()
                .collect();
        let lock_deps = HashMap::new();
        let count = print_results_text(&results, false, &lock_deps);
        assert_eq!(count, 0);
    }

    #[test]
    fn text_output_only_used() {
        let results: HashMap<String, GemResult> =
            [make_result("rails", GemStatus::Used, &["default"])]
                .into_iter()
                .collect();
        let lock_deps = HashMap::new();
        let count = print_results_text(&results, false, &lock_deps);
        assert_eq!(count, 0);
    }

    #[test]
    fn text_output_used_verbose_with_evidence() {
        let results: HashMap<String, GemResult> = [(
            "rails".to_string(),
            GemResult {
                status: GemStatus::Used,
                files: vec!["app.rb".to_string(), "config.yml".to_string()],
                evidence: vec![
                    Evidence::Require {
                        path: "rails".into(),
                        file: "app.rb".into(),
                    },
                    Evidence::Module {
                        name: "Rails".into(),
                        file: "app.rb".into(),
                    },
                    Evidence::Script {
                        file: "ci.yml".into(),
                    },
                    Evidence::BundlerRequire,
                    Evidence::DatabaseAdapter {
                        adapter: "pg".into(),
                        kind: "database".into(),
                        source: "config/database.yml".into(),
                    },
                ],
                gem: make_gem_entry("rails", &["default"]),
            },
        )]
        .into_iter()
        .collect();
        let lock_deps = HashMap::new();
        let count = print_results_text(&results, true, &lock_deps);
        assert_eq!(count, 0);
    }

    #[test]
    fn text_output_many_used_non_verbose() {
        // With 5+ used gems, exercises the chunked column display
        let results: HashMap<String, GemResult> = [
            make_result("activerecord", GemStatus::Used, &["default"]),
            make_result("activejob", GemStatus::Used, &["default"]),
            make_result("actionmailer", GemStatus::Used, &["default"]),
            make_result("actioncable", GemStatus::Used, &["default"]),
            make_result("activestorage", GemStatus::Used, &["default"]),
        ]
        .into_iter()
        .collect();
        let lock_deps = HashMap::new();
        let count = print_results_text(&results, false, &lock_deps);
        assert_eq!(count, 0);
    }

    #[test]
    fn name_grid_separates_long_names() {
        let names = [
            "opentelemetry-instrumentation-graphql",
            "opentelemetry-instrumentation-mysql2",
            "pg",
        ];
        let rows = format_name_grid(&names, 100);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains("opentelemetry-instrumentation-graphql  opentelemetry"));
        assert_eq!(rows[1], "  pg");
        for row in &rows {
            assert!(!row.ends_with(' '));
        }
    }

    #[test]
    fn name_grid_uses_at_least_one_column() {
        let names = ["a-very-long-gem-name-that-exceeds-the-width"];
        let rows = format_name_grid(&names, 10);
        assert_eq!(rows, vec!["  a-very-long-gem-name-that-exceeds-the-width"]);
        let short = ["a", "b", "c"];
        assert_eq!(format_name_grid(&short, 100), vec!["  a  b  c"]);
    }

    #[test]
    fn adapter_evidence_serializes_kind_and_source() {
        let evidence = Evidence::DatabaseAdapter {
            adapter: "redis".into(),
            kind: "cable".into(),
            source: "config/cable.yml".into(),
        };
        let json = serde_json::to_value(&evidence).unwrap();
        assert_eq!(json["type"], "database_adapter");
        assert_eq!(json["adapter"], "redis");
        assert_eq!(json["kind"], "cable");
        assert_eq!(json["source"], "config/cable.yml");
        assert_eq!(adapter_label("cable"), "cable adapter");
        assert_eq!(adapter_label("cache"), "cache store");
    }

    #[test]
    fn github_annotation_escaping() {
        assert_eq!(escape_data("a%b\r\nc: d, e"), "a%25b%0D%0Ac: d, e");
        assert_eq!(escape_property("a:b,c%\n"), "a%3Ab%2Cc%25%0A");
    }
}
