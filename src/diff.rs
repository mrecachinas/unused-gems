use std::collections::HashMap;

#[derive(Debug, serde::Deserialize)]
struct BaselineGem {
    name: String,
    status: String,
}

pub fn print_diff(
    results: &HashMap<String, crate::types::GemResult>,
    baseline_path: &str,
) -> usize {
    let content = match std::fs::read_to_string(baseline_path) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("Error: Baseline file not found: {}", baseline_path);
            std::process::exit(1);
        }
    };

    print_diff_from_content(results, &content)
}

fn print_diff_from_content(
    results: &HashMap<String, crate::types::GemResult>,
    content: &str,
) -> usize {
    let baseline: Vec<BaselineGem> = match serde_json::from_str(content) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Error: Could not parse baseline: {}", e);
            std::process::exit(1);
        }
    };

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
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    all_gems.sort();

    let mut newly_unused: Vec<(&str, &str)> = Vec::new();
    let mut newly_used: Vec<(&str, &str)> = Vec::new();
    let mut status_changes: Vec<(&str, &str, &str)> = Vec::new();
    let mut added: Vec<(&str, &str)> = Vec::new();
    let mut removed: Vec<(&str, &str)> = Vec::new();

    for name in &all_gems {
        let old_s = old_statuses.get(name).copied();
        let new_s = new_statuses.get(name).map(|s| s.as_str());

        match (old_s, new_s) {
            (None, Some(ns)) => added.push((name, ns)),
            (Some(os), None) => removed.push((name, os)),
            (Some(os), Some(ns)) if os != ns => {
                if ns == "unused" && os != "unused" {
                    newly_unused.push((name, os));
                } else if os == "unused" && ns != "unused" {
                    newly_used.push((name, ns));
                } else {
                    status_changes.push((name, os, ns));
                }
            }
            _ => {}
        }
    }

    let has_changes = !newly_unused.is_empty()
        || !newly_used.is_empty()
        || !status_changes.is_empty()
        || !added.is_empty()
        || !removed.is_empty();

    if !has_changes {
        println!("No changes since baseline.");
        return 0;
    }

    if !newly_unused.is_empty() {
        println!("🆕 Newly unused ({})", newly_unused.len());
        println!();
        for (name, from) in &newly_unused {
            println!("  {}  (was: {})", name, from);
        }
        println!();
    }

    if !newly_used.is_empty() {
        println!("✅ No longer unused ({})", newly_used.len());
        println!();
        for (name, to) in &newly_used {
            println!("  {}  (now: {})", name, to);
        }
        println!();
    }

    if !status_changes.is_empty() {
        println!("🔄 Status changed ({})", status_changes.len());
        println!();
        for (name, from, to) in &status_changes {
            println!("  {}  {} → {}", name, from, to);
        }
        println!();
    }

    if !added.is_empty() {
        println!("➕ Added to Gemfile ({})", added.len());
        println!();
        for (name, status) in &added {
            println!("  {}  ({})", name, status);
        }
        println!();
    }

    if !removed.is_empty() {
        println!("➖ Removed from Gemfile ({})", removed.len());
        println!();
        for (name, status) in &removed {
            println!("  {}  (was: {})", name, status);
        }
        println!();
    }

    newly_unused.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{GemEntry, GemResult, GemStatus};

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
}
