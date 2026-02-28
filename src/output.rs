use std::collections::HashMap;
use std::path::Path;

use crate::lockfile::count_transitive_deps;
use crate::types::{Evidence, GemResult, GemStatus};

struct Categories<'a> {
    unused: Vec<(&'a str, &'a GemResult)>,
    bundler_required: Vec<(&'a str, &'a GemResult)>,
    require_false: Vec<(&'a str, &'a GemResult)>,
    used: Vec<(&'a str, &'a GemResult)>,
    ignored: Vec<(&'a str, &'a GemResult)>,
}

fn categorize(results: &HashMap<String, GemResult>) -> Categories<'_> {
    let mut cats = Categories {
        unused: Vec::new(),
        bundler_required: Vec::new(),
        require_false: Vec::new(),
        used: Vec::new(),
        ignored: Vec::new(),
    };

    for (name, info) in results {
        match info.status {
            GemStatus::Unused => cats.unused.push((name.as_str(), info)),
            GemStatus::BundlerRequired => cats.bundler_required.push((name.as_str(), info)),
            GemStatus::RequireFalse => cats.require_false.push((name.as_str(), info)),
            GemStatus::Used => cats.used.push((name.as_str(), info)),
            GemStatus::Ignored => cats.ignored.push((name.as_str(), info)),
        }
    }

    cats.unused.sort_by_key(|(n, _)| *n);
    cats.bundler_required.sort_by_key(|(n, _)| *n);
    cats.require_false.sort_by_key(|(n, _)| *n);
    cats.used.sort_by_key(|(n, _)| *n);
    cats.ignored.sort_by_key(|(n, _)| *n);

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
                format!(" ({} transitive deps)", count_transitive_deps(name, lock_deps))
            } else {
                String::new()
            };
            println!(
                "  {}  ({}){}",
                name,
                info.gem.groups.join(", "),
                transitive
            );
            if verbose {
                println!("    require paths: {}", info.gem.require_paths.join(", "));
                println!();
            }
        }
        println!();
    }

    if !cats.bundler_required.is_empty() {
        println!(
            "⚠️  Bundler.require only ({})",
            cats.bundler_required.len()
        );
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

    if !cats.require_false.is_empty() {
        println!(
            "💤 Lazy-loaded / require: false ({})",
            cats.require_false.len()
        );
        println!();
        let names: Vec<&str> = cats.require_false.iter().map(|(n, _)| *n).collect();
        println!("  {}", names.join(", "));
        println!();
    }

    if !cats.ignored.is_empty() {
        println!(
            "🔇 Ignored via .unused_gems.yml ({})",
            cats.ignored.len()
        );
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
                        Evidence::Require { path, file } => println!("    → require '{}' in {}", path, file),
                        Evidence::Module { name: mod_name, file } => println!("    → {} in {}", mod_name, file),
                        Evidence::Script { file } => println!("    → script ref in {}", file),
                        Evidence::BundlerRequire => println!("    → Bundler.require"),
                        Evidence::DatabaseAdapter { adapter } => println!("    → adapter: {} in config/database.yml", adapter),
                    }
                }
                println!();
            }
        } else {
            let names: Vec<&str> = cats.used.iter().map(|(n, _)| *n).collect();
            for chunk in names.chunks(4) {
                let row: Vec<String> = chunk.iter().map(|n| format!("{:<28}", n)).collect();
                println!("  {}", row.join(""));
            }
            println!();
        }
    }

    println!(
        "{} gems: {} used, {} unused, {} bundler.require, {} lazy{}",
        total,
        cats.used.len(),
        cats.unused.len(),
        cats.bundler_required.len(),
        cats.require_false.len(),
        if !cats.ignored.is_empty() {
            format!(", {} ignored", cats.ignored.len())
        } else {
            String::new()
        }
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
    println!("{}", serde_json::to_string_pretty(&records).unwrap());

    results
        .values()
        .filter(|v| v.status == GemStatus::Unused)
        .count()
}

pub fn print_results_github(
    results: &HashMap<String, GemResult>,
    dir: &Path,
    lock_deps: &HashMap<String, Vec<String>>,
) -> usize {
    let line_nums = gemfile_line_numbers(dir);
    let mut unused_count = 0;

    let mut entries: Vec<(&str, &GemResult)> = results
        .iter()
        .map(|(n, r)| (n.as_str(), r))
        .collect();
    entries.sort_by_key(|(n, _)| *n);

    for (name, info) in entries {
        if info.status != GemStatus::Unused && info.status != GemStatus::BundlerRequired {
            continue;
        }

        let location = match line_nums.get(name) {
            Some(line) => format!("file=Gemfile,line={}", line),
            None => "file=Gemfile".to_string(),
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

        println!("::{} {}::{}", level, location, msg);
        if info.status == GemStatus::Unused {
            unused_count += 1;
        }
    }

    unused_count
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
            Evidence::DatabaseAdapter { adapter } => parts.push(format!("adapter:{}", adapter)),
        }
    }
    if require_count > 0 { parts.push(format!("{} require(s)", require_count)); }
    if module_count > 0 { parts.push(format!("{} module ref(s)", module_count)); }
    if script_count > 0 { parts.push(format!("{} script ref(s)", script_count)); }
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
