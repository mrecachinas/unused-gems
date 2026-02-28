use std::collections::{HashMap, HashSet};

use crate::types::{Evidence, GemEntry, GemResult, GemStatus, ScanResults};

pub fn match_gem_usage(
    gems: &[GemEntry],
    scan_results: &ScanResults,
) -> HashMap<String, GemResult> {
    let requires = &scan_results.requires;
    let modules = &scan_results.modules;
    let bundler_groups = &scan_results.bundler_groups;
    let bundler_require_all = scan_results.bundler_require_all;

    // Pre-build prefix map for O(1) sub-require lookups
    let mut prefix_map: HashMap<&str, Vec<(&str, &HashSet<String>)>> = HashMap::new();
    for (req_name, files) in requires {
        let top_segment = req_name.split('/').next().unwrap_or(req_name);
        prefix_map
            .entry(top_segment)
            .or_default()
            .push((req_name.as_str(), files));
    }

    // Pre-lowercase all source module names once (avoids repeated to_lowercase per gem)
    let modules_lower: Vec<(String, &HashSet<String>)> = modules
        .iter()
        .map(|(name, files)| (name.to_lowercase(), files))
        .collect();

    // Keep original module names for evidence reporting
    let modules_original: HashMap<String, &str> = modules
        .iter()
        .map(|(name, _)| (name.to_lowercase(), name.as_str()))
        .collect();

    let mut results = HashMap::new();

    for gem_entry in gems {
        let mut found_files: HashSet<String> = HashSet::new();
        let mut evidence: Vec<Evidence> = Vec::new();

        // Check each require path for this gem
        for req_path in &gem_entry.require_paths {
            if let Some(files) = requires.get(req_path) {
                for f in files {
                    evidence.push(Evidence::Require {
                        path: req_path.clone(),
                        file: f.clone(),
                    });
                }
                found_files.extend(files.iter().cloned());
            }

            // Check sub-requires via prefix map
            let top = req_path.split('/').next().unwrap_or(req_path);
            if let Some(entries) = prefix_map.get(top) {
                let prefix = format!("{}/", req_path);
                for (req_name, files) in entries {
                    if req_name.starts_with(&prefix) {
                        for f in *files {
                            evidence.push(Evidence::Require {
                                path: req_name.to_string(),
                                file: f.clone(),
                            });
                        }
                        found_files.extend(files.iter().cloned());
                    }
                }
            }
        }

        // Check module/class name references against pre-lowercased source modules
        for mod_pattern in &gem_entry.module_names {
            let mod_down = mod_pattern.to_lowercase();
            // Skip very short module names that cause false positives (e.g., "Cli", "Api")
            // UNLESS they match the gem name exactly (e.g. "aws", "jwt", "csv")
            if mod_down.len() < 4 {
                let gem_simple = gem_entry.name.replace('-', "").replace('_', "").to_lowercase();
                if mod_down != gem_simple && mod_down != gem_entry.name.to_lowercase() {
                    continue;
                }
            }
            let prefix = format!("{}::", mod_down);

            for (name_down, files) in &modules_lower {
                if *name_down == mod_down || name_down.starts_with(&prefix) {
                    let original_name = match modules_original.get(name_down.as_str()) {
                        Some(n) => *n,
                        None => name_down.as_str(),
                    };
                    for f in *files {
                        evidence.push(Evidence::Module {
                            name: original_name.to_string(),
                            file: f.clone(),
                        });
                    }
                    found_files.extend(files.iter().cloned());
                }
            }
        }

        // Determine status
        let status = if !found_files.is_empty() {
            GemStatus::Used
        } else if gem_entry.require_false {
            GemStatus::RequireFalse
        } else if bundler_require_all
            || gem_entry
                .groups
                .iter()
                .any(|g| bundler_groups.contains(g))
        {
            evidence.push(Evidence::BundlerRequire);
            GemStatus::BundlerRequired
        } else {
            GemStatus::Unused
        };

        let mut files: Vec<String> = found_files.into_iter().collect();
        files.sort();

        // Deduplicate evidence
        evidence.sort_by(|a, b| format!("{:?}", a).cmp(&format!("{:?}", b)));
        evidence.dedup_by(|a, b| format!("{:?}", a) == format!("{:?}", b));

        results.insert(
            gem_entry.name.clone(),
            GemResult {
                status,
                files,
                evidence,
                gem: gem_entry.clone(),
            },
        );
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gem(name: &str, require_paths: &[&str], module_names: &[&str]) -> GemEntry {
        GemEntry {
            name: name.to_string(),
            groups: vec!["default".to_string()],
            require_paths: require_paths.iter().map(|s| s.to_string()).collect(),
            module_names: module_names.iter().map(|s| s.to_string()).collect(),
            require_false: false,
        }
    }

    fn scan(requires: &[(&str, &str)], modules: &[(&str, &str)]) -> ScanResults {
        let mut req_map: HashMap<String, HashSet<String>> = HashMap::new();
        for (name, file) in requires {
            req_map.entry(name.to_string()).or_default().insert(file.to_string());
        }
        let mut mod_map: HashMap<String, HashSet<String>> = HashMap::new();
        for (name, file) in modules {
            mod_map.entry(name.to_string()).or_default().insert(file.to_string());
        }
        ScanResults {
            requires: req_map,
            modules: mod_map,
            bundler_groups: HashSet::new(),
            bundler_require_all: false,
        }
    }

    #[test]
    fn match_by_require_path() {
        let gems = vec![gem("failbot", &["failbot"], &[])];
        let results = match_gem_usage(&gems, &scan(&[("failbot", "app.rb")], &[]));
        assert_eq!(results["failbot"].status, GemStatus::Used);
        assert!(results["failbot"].evidence.iter().any(|e| matches!(e, Evidence::Require { path, .. } if path == "failbot")));
    }

    #[test]
    fn match_by_sub_require() {
        let gems = vec![gem("activesupport", &["active_support"], &[])];
        let scan = scan(&[("active_support/core_ext", "app.rb")], &[]);
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["activesupport"].status, GemStatus::Used);
    }

    #[test]
    fn match_by_module_name() {
        let gems = vec![gem("graphql", &[], &["GraphQL"])];
        let scan = scan(&[], &[("GraphQL", "app.rb")]);
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["graphql"].status, GemStatus::Used);
        assert!(results["graphql"].evidence.iter().any(|e| matches!(e, Evidence::Module { .. })));
    }

    #[test]
    fn match_by_module_prefix() {
        let gems = vec![gem("graphql", &[], &["GraphQL"])];
        let scan = scan(&[], &[("GraphQL::Schema", "schema.rb")]);
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["graphql"].status, GemStatus::Used);
    }

    #[test]
    fn no_match_unused() {
        let gems = vec![gem("unused-gem", &["unused_gem"], &["UnusedGem"])];
        let scan = scan(&[("other", "app.rb")], &[("OtherModule", "app.rb")]);
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["unused-gem"].status, GemStatus::Unused);
        assert!(results["unused-gem"].evidence.is_empty());
    }

    #[test]
    fn bundler_require_all() {
        let gems = vec![gem("dotenv", &["dotenv"], &[])];
        let mut scan = scan(&[], &[]);
        scan.bundler_require_all = true;
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["dotenv"].status, GemStatus::BundlerRequired);
        assert!(results["dotenv"].evidence.iter().any(|e| matches!(e, Evidence::BundlerRequire)));
    }

    #[test]
    fn bundler_require_group() {
        let mut entry = gem("rspec", &["rspec"], &[]);
        entry.groups = vec!["test".to_string()];
        let gems = vec![entry];
        let mut scan = scan(&[], &[]);
        scan.bundler_groups.insert("test".to_string());
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["rspec"].status, GemStatus::BundlerRequired);
    }

    #[test]
    fn require_false_status() {
        let mut entry = gem("bootsnap", &[], &[]);
        entry.require_false = true;
        let gems = vec![entry];
        let results = match_gem_usage(&gems, &scan(&[], &[]));
        assert_eq!(results["bootsnap"].status, GemStatus::RequireFalse);
    }

    #[test]
    fn short_module_name_filtered() {
        // "Cli" (3 chars) should be filtered for gem "something"
        let gems = vec![gem("something", &[], &["Cli"])];
        let scan = scan(&[], &[("Cli", "app.rb")]);
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["something"].status, GemStatus::Unused);
    }

    #[test]
    fn short_module_name_allowed_when_matches_gem() {
        // "JWT" (3 chars) should be allowed for gem "jwt"
        let gems = vec![gem("jwt", &[], &["JWT"])];
        let scan = scan(&[], &[("JWT", "app.rb")]);
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["jwt"].status, GemStatus::Used);
    }

    #[test]
    fn short_module_name_allowed_simplified_match() {
        // "Aws" for gem "aws-sdk" → gem_simple = "awssdk", mod_down = "aws" — won't match
        // But "Aws" for gem "aws" → gem_simple = "aws", mod_down = "aws" — matches
        let gems = vec![gem("aws", &[], &["Aws"])];
        let scan = scan(&[], &[("Aws", "app.rb")]);
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["aws"].status, GemStatus::Used);
    }

    #[test]
    fn module_match_case_insensitive() {
        let gems = vec![gem("graphql", &[], &["GraphQL"])];
        let scan = scan(&[], &[("Graphql", "app.rb")]);
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["graphql"].status, GemStatus::Used);
    }

    #[test]
    fn multiple_evidence_types() {
        let gems = vec![gem("failbot", &["failbot"], &["Failbot"])];
        let scan = scan(&[("failbot", "a.rb")], &[("Failbot", "b.rb")]);
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["failbot"].status, GemStatus::Used);
        let has_require = results["failbot"].evidence.iter().any(|e| matches!(e, Evidence::Require { .. }));
        let has_module = results["failbot"].evidence.iter().any(|e| matches!(e, Evidence::Module { .. }));
        assert!(has_require);
        assert!(has_module);
    }
}
