use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::types::GemResult;

#[derive(Debug, serde::Deserialize)]
pub struct BaselineGem {
    name: String,
    status: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum Baseline {
    Legacy(Vec<BaselineGem>),
    Envelope { results: Vec<BaselineGem> },
}

/// Read and parse a baseline produced by `-o json` (envelope or legacy array).
pub fn load_baseline(baseline_path: &str) -> Result<Vec<BaselineGem>, String> {
    let content = std::fs::read_to_string(baseline_path)
        .map_err(|e| format!("Could not read baseline {}: {}", baseline_path, e))?;
    parse_baseline(&content)
        .map_err(|e| format!("Could not parse baseline {}: {}", baseline_path, e))
}

fn parse_baseline(content: &str) -> Result<Vec<BaselineGem>, serde_json::Error> {
    serde_json::from_str::<Baseline>(content)
        .map(|(Baseline::Legacy(results) | Baseline::Envelope { results })| results)
}

#[derive(Debug, Default, serde::Serialize)]
pub struct DiffReport {
    newly_unused: Vec<Change>,
    no_longer_unused: Vec<Change>,
    status_changed: Vec<Change>,
    added: Vec<Change>,
    removed: Vec<Change>,
}

#[derive(Debug, serde::Serialize)]
struct Change {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
}

impl DiffReport {
    fn is_empty(&self) -> bool {
        self.newly_unused.is_empty()
            && self.no_longer_unused.is_empty()
            && self.status_changed.is_empty()
            && self.added.is_empty()
            && self.removed.is_empty()
    }

    /// Gems that became unused plus newly added gems that are unused.
    fn new_unused(&self) -> impl Iterator<Item = &Change> {
        self.newly_unused.iter().chain(
            self.added
                .iter()
                .filter(|c| c.status.as_deref() == Some("unused")),
        )
    }

    pub fn new_unused_count(&self) -> usize {
        self.new_unused().count()
    }
}

pub fn compute_diff(results: &HashMap<String, GemResult>, baseline: &[BaselineGem]) -> DiffReport {
    let old_statuses: HashMap<&str, &str> = baseline
        .iter()
        .map(|g| (g.name.as_str(), g.status.as_str()))
        .collect();
    let new_statuses: HashMap<&str, String> = results
        .iter()
        .map(|(name, info)| (name.as_str(), info.status.to_string()))
        .collect();

    let mut all_gems: Vec<&str> = old_statuses
        .keys()
        .chain(new_statuses.keys())
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    all_gems.sort();

    let change = |name: &str, old: Option<&str>, new: Option<&str>| Change {
        name: name.to_string(),
        previous_status: old.map(str::to_string),
        status: new.map(str::to_string),
    };
    let mut report = DiffReport::default();
    for name in all_gems {
        let old_s = old_statuses.get(name).copied();
        let new_s = new_statuses.get(name).map(|s| s.as_str());
        match (old_s, new_s) {
            (None, Some(_)) => report.added.push(change(name, None, new_s)),
            (Some(_), None) => report.removed.push(change(name, old_s, None)),
            (Some(os), Some(ns)) if os != ns => {
                let entry = change(name, old_s, new_s);
                if ns == "unused" {
                    report.newly_unused.push(entry);
                } else if os == "unused" {
                    report.no_longer_unused.push(entry);
                } else {
                    report.status_changed.push(entry);
                }
            }
            _ => {}
        }
    }
    report
}

fn status(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("")
}

pub fn print_diff_text(report: &DiffReport) {
    if report.is_empty() {
        println!("No changes since baseline.");
        return;
    }
    let sections: [(&str, &Vec<Change>); 5] = [
        ("🆕 Newly unused", &report.newly_unused),
        ("✅ No longer unused", &report.no_longer_unused),
        ("🔄 Status changed", &report.status_changed),
        ("➕ Added to Gemfile", &report.added),
        ("➖ Removed from Gemfile", &report.removed),
    ];
    for (index, (title, changes)) in sections.iter().enumerate() {
        if changes.is_empty() {
            continue;
        }
        println!("{} ({})", title, changes.len());
        println!();
        for c in changes.iter() {
            match index {
                0 | 4 => println!("  {}  (was: {})", c.name, status(&c.previous_status)),
                1 => println!("  {}  (now: {})", c.name, status(&c.status)),
                2 => println!(
                    "  {}  {} → {}",
                    c.name,
                    status(&c.previous_status),
                    status(&c.status)
                ),
                _ => println!("  {}  ({})", c.name, status(&c.status)),
            }
        }
        println!();
    }
}

pub fn print_diff_json(report: &DiffReport) {
    let envelope = serde_json::json!({
        "schema_version": "1",
        "diff": report,
        "new_unused_count": report.new_unused_count(),
    });
    println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
}

/// Emit a GitHub warning annotation for each gem that is newly unused.
pub fn print_diff_github(report: &DiffReport, dir: &Path) {
    let line_nums = crate::output::gemfile_line_numbers(dir);
    for c in report.new_unused() {
        let location = match line_nums.get(&c.name) {
            Some(line) => format!(
                "file={},line={}",
                crate::output::escape_property("Gemfile"),
                line
            ),
            None => format!("file={}", crate::output::escape_property("Gemfile")),
        };
        let msg = match &c.previous_status {
            Some(prev) => format!("Gem '{}' became unused (was: {})", c.name, prev),
            None => format!("Newly added gem '{}' appears unused", c.name),
        };
        println!(
            "::warning {}::{}",
            location,
            crate::output::escape_data(&msg)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{GemEntry, GemResult, GemStatus};

    fn print_diff_from_content(results: &HashMap<String, GemResult>, content: &str) -> usize {
        let report = compute_diff(results, &parse_baseline(content).unwrap());
        print_diff_text(&report);
        report.new_unused_count()
    }

    fn make_result(name: &str, status: GemStatus) -> (String, GemResult) {
        (
            name.to_string(),
            GemResult {
                status,
                files: Vec::new(),
                evidence: Vec::new(),
                gem: GemEntry {
                    name: name.to_string(),
                    groups: vec!["default".to_string()],
                    platforms: Vec::new(),
                    require_paths: Vec::new(),
                    module_names: Vec::new(),
                    require_false: false,
                    source: None,
                    declarations: Vec::new(),
                },
            },
        )
    }

    #[test]
    fn no_changes() {
        let results: HashMap<String, GemResult> = [make_result("rails", GemStatus::Used)]
            .into_iter()
            .collect();
        let baseline = r#"[{"name":"rails","status":"used"}]"#;
        assert_eq!(print_diff_from_content(&results, baseline), 0);
    }

    #[test]
    fn accepts_json_output_envelope() {
        let results: HashMap<String, GemResult> = [make_result("rails", GemStatus::Used)]
            .into_iter()
            .collect();
        let baseline = r#"{"schema_version":"1","results":[{"name":"rails","status":"used"}]}"#;
        assert_eq!(print_diff_from_content(&results, baseline), 0);
    }

    #[test]
    fn newly_unused_gem() {
        let results: HashMap<String, GemResult> = [make_result("rails", GemStatus::Unused)]
            .into_iter()
            .collect();
        let baseline = r#"[{"name":"rails","status":"used"}]"#;
        assert_eq!(print_diff_from_content(&results, baseline), 1);
    }

    #[test]
    fn newly_used_gem() {
        let results: HashMap<String, GemResult> = [make_result("rails", GemStatus::Used)]
            .into_iter()
            .collect();
        let baseline = r#"[{"name":"rails","status":"unused"}]"#;
        assert_eq!(print_diff_from_content(&results, baseline), 0);
    }

    #[test]
    fn added_gem() {
        let results: HashMap<String, GemResult> = [
            make_result("rails", GemStatus::Used),
            make_result("puma", GemStatus::Used),
        ]
        .into_iter()
        .collect();
        let baseline = r#"[{"name":"rails","status":"used"}]"#;
        assert_eq!(print_diff_from_content(&results, baseline), 0);
    }

    #[test]
    fn added_unused_gem_is_newly_unused() {
        let results: HashMap<String, GemResult> = [
            make_result("rails", GemStatus::Used),
            make_result("puma", GemStatus::Unused),
        ]
        .into_iter()
        .collect();
        let baseline = r#"[{"name":"rails","status":"used"}]"#;
        assert_eq!(print_diff_from_content(&results, baseline), 1);
    }

    #[test]
    fn removed_gem() {
        let results: HashMap<String, GemResult> = [make_result("rails", GemStatus::Used)]
            .into_iter()
            .collect();
        let baseline = r#"[{"name":"rails","status":"used"},{"name":"puma","status":"used"}]"#;
        assert_eq!(print_diff_from_content(&results, baseline), 0);
    }

    #[test]
    fn status_change_non_unused() {
        let results: HashMap<String, GemResult> =
            [make_result("rails", GemStatus::BundlerRequired)]
                .into_iter()
                .collect();
        let baseline = r#"[{"name":"rails","status":"used"}]"#;
        assert_eq!(print_diff_from_content(&results, baseline), 0);
    }

    #[test]
    fn invalid_or_missing_baseline_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.json");
        assert!(load_baseline(missing.to_str().unwrap()).is_err());
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{not json").unwrap();
        assert!(load_baseline(bad.to_str().unwrap()).is_err());
    }

    #[test]
    fn diff_report_serializes_to_json() {
        let results: HashMap<String, GemResult> = [
            make_result("rails", GemStatus::Unused),
            make_result("puma", GemStatus::Unused),
        ]
        .into_iter()
        .collect();
        let baseline = parse_baseline(r#"[{"name":"rails","status":"used"}]"#).unwrap();
        let report = compute_diff(&results, &baseline);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["newly_unused"][0]["name"], "rails");
        assert_eq!(json["newly_unused"][0]["previous_status"], "used");
        assert_eq!(json["newly_unused"][0]["status"], "unused");
        assert_eq!(json["added"][0]["name"], "puma");
        assert!(json["added"][0].get("previous_status").is_none());
        assert_eq!(report.new_unused_count(), 2);
        print_diff_json(&report);
        let dir = tempfile::tempdir().unwrap();
        print_diff_github(&report, dir.path());
    }
}
