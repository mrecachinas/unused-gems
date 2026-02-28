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

    let baseline: Vec<BaselineGem> = match serde_json::from_str(&content) {
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
