use std::collections::{HashMap, HashSet};

use rayon::prelude::*;

use crate::types::{Evidence, GemEntry, GemResult, GemStatus, ScanResults};

/// Source module candidate: (original module name, files referencing it).
type ModuleEntry<'a> = (&'a str, &'a HashSet<String>);

/// Shared indexes built once from scan results and reused across all gem matching.
struct MatchIndexes<'a> {
    requires: &'a HashMap<String, HashSet<String>>,
    prefix_map: HashMap<&'a str, Vec<(&'a str, &'a HashSet<String>)>>,
    declared_require_paths: HashSet<&'a str>,
    /// Source modules keyed by the ASCII-lowercased top "::" segment. Only
    /// populated for top segments that some gem's module pattern can match, so
    /// a first pass without module patterns never walks the module map.
    modules_by_top: HashMap<String, Vec<ModuleEntry<'a>>>,
    bundler_groups: &'a HashSet<String>,
    bundler_require_all: bool,
}

fn lower_top_segment(name: &str) -> String {
    name.split("::").next().unwrap_or(name).to_ascii_lowercase()
}

/// Index only the source modules whose top segment matches a wanted
/// (lowercased) top segment. Lowercases into a stack buffer so the common
/// non-matching module costs one hash lookup and no allocation.
fn index_modules<'a>(
    modules: &'a HashMap<String, HashSet<String>>,
    wanted: HashSet<String>,
) -> HashMap<String, Vec<ModuleEntry<'a>>> {
    const BUF_LEN: usize = 128;
    if wanted.is_empty() {
        return HashMap::new();
    }
    let max_len = wanted.iter().map(String::len).max().unwrap_or(0);
    let hits: Vec<(&String, ModuleEntry<'a>)> = modules
        .par_iter()
        .filter_map(|(name, files)| {
            let top = name.split("::").next().unwrap_or(name);
            if top.len() > max_len || top.len() > BUF_LEN {
                return None;
            }
            let mut buf = [0u8; BUF_LEN];
            let lowered = &mut buf[..top.len()];
            lowered.copy_from_slice(top.as_bytes());
            lowered.make_ascii_lowercase();
            let key = std::str::from_utf8(lowered).ok()?;
            wanted
                .get(key)
                .map(|wanted_key| (wanted_key, (name.as_str(), files)))
        })
        .collect();
    let mut by_top: HashMap<String, Vec<ModuleEntry<'a>>> = HashMap::new();
    for (key, entry) in hits {
        by_top.entry(key.clone()).or_default().push(entry);
    }
    by_top
}

impl<'a> MatchIndexes<'a> {
    fn build(gems: &'a [GemEntry], indices: &[usize], scan_results: &'a ScanResults) -> Self {
        let requires = &scan_results.requires;

        let mut prefix_map: HashMap<&str, Vec<(&str, &HashSet<String>)>> = HashMap::new();
        for (req_name, files) in requires {
            let top_segment = req_name.split('/').next().unwrap_or(req_name);
            prefix_map
                .entry(top_segment)
                .or_default()
                .push((req_name.as_str(), files));
        }

        let wanted_tops: HashSet<String> = indices
            .iter()
            .flat_map(|&i| gems[i].module_names.iter())
            .map(|pattern| lower_top_segment(pattern))
            .collect();

        MatchIndexes {
            requires,
            prefix_map,
            declared_require_paths: gems
                .iter()
                .flat_map(|gem| gem.require_paths.iter().map(String::as_str))
                .collect(),
            modules_by_top: index_modules(&scan_results.modules, wanted_tops),
            bundler_groups: &scan_results.bundler_groups,
            bundler_require_all: scan_results.bundler_require_all,
        }
    }
}

/// Per-gem precomputed lowercased forms used during matching.
struct GemMeta {
    name_lower: String,
    simple: String,
    segments_lower: Vec<String>,
}

impl GemMeta {
    fn for_gem(gem: &GemEntry) -> Self {
        let name_lower = gem.name.to_lowercase();
        let simple = gem.name.replace(['-', '_'], "").to_lowercase();
        let segments_lower = gem.name.split('-').map(|s| s.to_lowercase()).collect();
        GemMeta {
            name_lower,
            simple,
            segments_lower,
        }
    }
}

/// Build a single GemResult for one gem entry against the shared indexes.
fn build_gem_result(gem_entry: &GemEntry, meta: &GemMeta, indexes: &MatchIndexes<'_>) -> GemResult {
    let mut found_files: HashSet<String> = HashSet::new();
    let mut evidence: Vec<Evidence> = Vec::new();

    // Check each require path for this gem
    for req_path in &gem_entry.require_paths {
        if let Some(files) = indexes.requires.get(req_path) {
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
        if let Some(entries) = indexes.prefix_map.get(top) {
            let prefix = format!("{}/", req_path);
            for (req_name, files) in entries {
                if req_name.starts_with(&prefix) {
                    if *req_name != req_path && indexes.declared_require_paths.contains(*req_name) {
                        continue;
                    }
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

    // Check module/class name references against pre-lowercased source modules.
    // Use the modules-by-top-segment index to limit work to candidates that share
    // the pattern's lowercased top segment. Note: `mod_down` may itself be a
    // multi-segment name (e.g. "aws::s3"), so we look up by ITS top segment,
    // then match the full pattern below.
    for mod_pattern in &gem_entry.module_names {
        let mod_down = mod_pattern.to_ascii_lowercase();
        // Skip very short module names that cause false positives (e.g., "Cli", "Api")
        // UNLESS they match the gem name exactly (e.g. "aws", "jwt", "csv"),
        // its de-punctuated form, or any of its hyphen segments (>= 2 chars).
        // This lets `Pry` match `pry-rails` / `pry-byebug` / `pry-rescue`.
        if mod_down.len() < 4
            && mod_down != meta.simple
            && mod_down != meta.name_lower
            && !(mod_down.len() >= 2 && meta.segments_lower.iter().any(|s| s == &mod_down))
        {
            continue;
        }

        let mod_top = mod_down.split("::").next().unwrap_or(&mod_down);
        let Some(entries) = indexes.modules_by_top.get(mod_top) else {
            continue;
        };
        let mod_len = mod_down.len();
        let mod_bytes = mod_down.as_bytes();
        for (original_name, files) in entries {
            // Match either an exact module name or a `mod_down::...` descendant
            // (ASCII case-insensitive), without allocating per candidate.
            let nb = original_name.as_bytes();
            let matches = if nb.len() == mod_len {
                nb.eq_ignore_ascii_case(mod_bytes)
            } else if nb.len() > mod_len + 2 && nb[..mod_len].eq_ignore_ascii_case(mod_bytes) {
                nb[mod_len] == b':' && nb[mod_len + 1] == b':'
            } else {
                false
            };
            if matches {
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
    let is_bundler_required = gem_entry.bundler_autorequired(|group| {
        (indexes.bundler_require_all && group == "default")
            || indexes.bundler_groups.contains(group)
    });

    let status = if !found_files.is_empty() {
        GemStatus::Used
    } else if is_bundler_required {
        evidence.push(Evidence::BundlerRequire);
        GemStatus::BundlerRequired
    } else {
        GemStatus::Unused
    };

    let mut files: Vec<String> = found_files.into_iter().collect();
    files.sort();

    evidence.sort();
    evidence.dedup();

    GemResult {
        status,
        files,
        evidence,
        gem: gem_entry.clone(),
    }
}

/// Match a subset of gems (by index) against scan results. Builds shared indexes once
/// and processes gems in parallel via rayon.
pub fn match_gem_subset(
    gems: &[GemEntry],
    indices: &[usize],
    scan_results: &ScanResults,
) -> HashMap<String, GemResult> {
    let indexes = MatchIndexes::build(gems, indices, scan_results);

    // Precompute per-gem lowercased forms once (avoids repeated allocations
    // inside the parallel match loop and per inner pattern iteration).
    let metas: Vec<GemMeta> = indices
        .iter()
        .map(|&i| GemMeta::for_gem(&gems[i]))
        .collect();

    indices
        .par_iter()
        .zip(metas.par_iter())
        .map(|(&i, meta)| {
            let gem_entry = &gems[i];
            (
                gem_entry.name.clone(),
                build_gem_result(gem_entry, meta, &indexes),
            )
        })
        .collect()
}

pub fn match_gem_usage(
    gems: &[GemEntry],
    scan_results: &ScanResults,
) -> HashMap<String, GemResult> {
    let all_indices: Vec<usize> = (0..gems.len()).collect();
    match_gem_subset(gems, &all_indices, scan_results)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gem(name: &str, require_paths: &[&str], module_names: &[&str]) -> GemEntry {
        GemEntry {
            name: name.to_string(),
            groups: vec!["default".to_string()],
            platforms: Vec::new(),
            require_paths: require_paths.iter().map(|s| s.to_string()).collect(),
            module_names: module_names.iter().map(|s| s.to_string()).collect(),
            require_false: false,
            source: None,
            declarations: Vec::new(),
        }
    }

    fn scan(requires: &[(&str, &str)], modules: &[(&str, &str)]) -> ScanResults {
        let mut req_map: HashMap<String, HashSet<String>> = HashMap::new();
        for (name, file) in requires {
            req_map
                .entry(name.to_string())
                .or_default()
                .insert(file.to_string());
        }
        let mut mod_map: HashMap<String, HashSet<String>> = HashMap::new();
        for (name, file) in modules {
            mod_map
                .entry(name.to_string())
                .or_default()
                .insert(file.to_string());
        }
        ScanResults {
            requires: req_map,
            modules: mod_map,
            bundler_groups: HashSet::new(),
            bundler_require_all: false,
            script_files: Vec::new(),
        }
    }

    #[test]
    fn match_by_require_path() {
        let gems = vec![gem("failbot", &["failbot"], &[])];
        let results = match_gem_usage(&gems, &scan(&[("failbot", "app.rb")], &[]));
        assert_eq!(results["failbot"].status, GemStatus::Used);
        assert!(results["failbot"]
            .evidence
            .iter()
            .any(|e| matches!(e, Evidence::Require { path, .. } if path == "failbot")));
    }

    #[test]
    fn match_by_sub_require() {
        let gems = vec![gem("activesupport", &["active_support"], &[])];
        let scan = scan(&[("active_support/core_ext", "app.rb")], &[]);
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["activesupport"].status, GemStatus::Used);
    }

    #[test]
    fn exact_require_owner_wins_over_parent_prefix() {
        let gems = vec![
            gem("base", &["foo"], &[]),
            gem("extension", &["foo/bar"], &[]),
        ];
        let scan = scan(&[("foo/bar", "app.rb")], &[]);
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["base"].status, GemStatus::Unused);
        assert_eq!(results["extension"].status, GemStatus::Used);
    }

    #[test]
    fn match_by_module_name() {
        let gems = vec![gem("graphql", &[], &["GraphQL"])];
        let scan = scan(&[], &[("GraphQL", "app.rb")]);
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["graphql"].status, GemStatus::Used);
        assert!(results["graphql"]
            .evidence
            .iter()
            .any(|e| matches!(e, Evidence::Module { .. })));
    }

    #[test]
    fn match_by_multi_segment_module_pattern() {
        // Regression: gem.module_names may contain multi-segment names like "Aws::S3"
        // (from KNOWN_MODULE_NAMES). Lookup must succeed for source modules in the
        // same namespace.
        let gems = vec![gem("aws-sdk-s3", &[], &["Aws::S3"])];
        let scan = scan(&[], &[("Aws::S3::Client", "app.rb")]);
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["aws-sdk-s3"].status, GemStatus::Used);
    }

    #[test]
    fn multi_segment_module_pattern_does_not_overmatch_sibling() {
        // `Aws::S3` should NOT match a sibling like `Aws::SQS::Client`.
        let gems = vec![gem("aws-sdk-s3", &[], &["Aws::S3"])];
        let scan = scan(&[], &[("Aws::SQS::Client", "app.rb")]);
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["aws-sdk-s3"].status, GemStatus::Unused);
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
        // :default group gem should be BundlerRequired when bundler_require_all is set
        let gems = vec![gem("dotenv", &["dotenv"], &[])];
        let mut scan = scan(&[], &[]);
        scan.bundler_require_all = true;
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["dotenv"].status, GemStatus::BundlerRequired);
        assert!(results["dotenv"]
            .evidence
            .iter()
            .any(|e| matches!(e, Evidence::BundlerRequire)));
    }

    #[test]
    fn bundler_require_all_dev_only_gem_is_unused() {
        // dev-only gem with no code references should be Unused even with bundler_require_all
        let mut entry = gem("pry-byebug", &["pry-byebug"], &[]);
        entry.groups = vec!["development".to_string()];
        let gems = vec![entry];
        let mut scan = scan(&[], &[]);
        scan.bundler_require_all = true;
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["pry-byebug"].status, GemStatus::Unused);
    }

    #[test]
    fn bundler_require_all_test_only_gem_is_unused() {
        let mut entry = gem("rspec", &["rspec"], &[]);
        entry.groups = vec!["test".to_string()];
        let gems = vec![entry];
        let mut scan = scan(&[], &[]);
        scan.bundler_require_all = true;
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["rspec"].status, GemStatus::Unused);
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
        assert_eq!(results["bootsnap"].status, GemStatus::Unused);
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
    fn short_module_name_allowed_for_hyphen_segment() {
        // "Pry" (3 chars) should match gem "pry-rails" via its first hyphen segment.
        let gems = vec![gem("pry-rails", &[], &["Pry"])];
        let scan = scan(&[], &[("Pry", "config/initializers/pry.rb")]);
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["pry-rails"].status, GemStatus::Used);
        assert!(results["pry-rails"]
            .evidence
            .iter()
            .any(|e| matches!(e, Evidence::Module { name, .. } if name == "Pry")));
    }

    #[test]
    fn short_module_name_allowed_for_hyphen_segment_with_descendant() {
        // Pry::Commands should also match for `pry-byebug` via segment "pry".
        let gems = vec![gem("pry-byebug", &[], &["Pry"])];
        let scan = scan(&[], &[("Pry::Commands", "app.rb")]);
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["pry-byebug"].status, GemStatus::Used);
    }

    #[test]
    fn module_match_case_insensitive() {
        let gems = vec![gem("graphql", &[], &["GraphQL"])];
        let scan = scan(&[], &[("Graphql", "app.rb")]);
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["graphql"].status, GemStatus::Used);
    }

    #[test]
    fn module_index_skipped_without_module_patterns() {
        let gems = vec![gem("failbot", &["failbot"], &[])];
        let scan = scan(&[], &[("Failbot", "a.rb"), ("Other::Thing", "b.rb")]);
        let indexes = MatchIndexes::build(&gems, &[0], &scan);
        assert!(indexes.modules_by_top.is_empty());
    }

    #[test]
    fn module_index_only_keeps_wanted_top_segments() {
        let gems = vec![gem("graphql", &[], &["GraphQL::Batch"])];
        let scan = scan(
            &[],
            &[("Graphql::Batch::Loader", "a.rb"), ("Other::Thing", "b.rb")],
        );
        let indexes = MatchIndexes::build(&gems, &[0], &scan);
        assert_eq!(indexes.modules_by_top.len(), 1);
        assert_eq!(
            indexes.modules_by_top["graphql"],
            vec![(
                "Graphql::Batch::Loader",
                &scan.modules["Graphql::Batch::Loader"]
            )]
        );
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["graphql"].status, GemStatus::Used);
    }

    #[test]
    fn multiple_evidence_types() {
        let gems = vec![gem("failbot", &["failbot"], &["Failbot"])];
        let scan = scan(&[("failbot", "a.rb")], &[("Failbot", "b.rb")]);
        let results = match_gem_usage(&gems, &scan);
        assert_eq!(results["failbot"].status, GemStatus::Used);
        let has_require = results["failbot"]
            .evidence
            .iter()
            .any(|e| matches!(e, Evidence::Require { .. }));
        let has_module = results["failbot"]
            .evidence
            .iter()
            .any(|e| matches!(e, Evidence::Module { .. }));
        assert!(has_require);
        assert!(has_module);
    }
}
