mod adapters;
mod cache;
mod diff;
mod gemfile;
mod ignore;
mod introspect;
mod known;
mod lockfile;
mod matcher;
mod output;
mod platforms;
mod scanner;
mod scripts;
mod types;

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::Parser;
use rayon::prelude::*;

use types::GemStatus;

#[derive(Parser)]
#[command(name = "unused-gems")]
#[command(about = "High-performance scanner for unused gems in Ruby projects")]
#[command(version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    /// Output format: text (default), json, github
    #[arg(short, long, default_value = "text", value_parser = ["text", "json", "github"])]
    output: String,

    /// Number of parallel scanning threads. When set, this also overrides the
    /// scanner's conservative walker thread cap (default 4) — useful for large
    /// cold scans on many-core machines. Leave unset to keep the default cap.
    #[arg(short, long)]
    jobs: Option<NonZeroUsize>,

    /// Show detailed output (file locations, require paths, transitive deps)
    #[arg(short, long)]
    verbose: bool,

    /// Suppress progress messages, only show results
    #[arg(short, long)]
    quiet: bool,

    /// Path to ignore file (default: .unused_gems.yml in project root)
    #[arg(long)]
    ignore_file: Option<String>,

    /// Gemfile path(s) to analyze. Repeat for monorepos. Defaults to <directory>/Gemfile.
    #[arg(long)]
    gemfile: Vec<String>,

    /// Compare against a previous JSON output file and show changes
    #[arg(long)]
    diff: Option<String>,

    /// Additional directories to skip (comma-separated)
    #[arg(long, value_delimiter = ',')]
    skip_dirs: Option<Vec<String>>,

    /// Rails environment whose Gemfile group should be considered active
    #[arg(long)]
    rails_env: Option<String>,

    /// Disable file cache (always scan all files fresh)
    #[arg(long)]
    no_cache: bool,

    /// Exit code policy: none (always 0), any (1 if unused), new (1 only for new unused in diff)
    #[arg(long, default_value = "any", value_parser = ["none", "any", "new"])]
    fail_on: String,

    /// Emit SARIF JSON to stdout (overrides output format)
    #[arg(long)]
    sarif: bool,

    /// Exit 0 with a notice when Gemfile is missing instead of failing
    #[arg(long)]
    allow_missing_gemfile: bool,

    /// Directory to scan (default: current directory)
    #[arg(default_value = ".")]
    directory: String,
}

fn main() {
    let cli = Cli::parse();

    let dir = Path::new(&cli.directory)
        .canonicalize()
        .unwrap_or_else(|_| {
            eprintln!("Error: {} is not a valid directory", cli.directory);
            std::process::exit(1);
        });

    if !dir.is_dir() {
        eprintln!("Error: {} is not a directory", dir.display());
        std::process::exit(1);
    }

    // Configure the rayon global pool when --jobs specified. This affects
    // `par_iter`/`rayon::join` work throughout the pipeline. The walker's
    // thread count is plumbed explicitly below via `scan_source_tree_with_threads`
    // so an explicit `--jobs` actually raises the walker fan-out past the
    // conservative default cap of 4.
    if let Some(jobs) = cli.jobs.map(NonZeroUsize::get) {
        rayon::ThreadPoolBuilder::new()
            .num_threads(jobs)
            .build_global()
            .ok();
    }
    let walker_threads = cli.jobs.map(NonZeroUsize::get);

    let mut skip_dirs: Vec<&str> = scanner::default_skip_dirs();
    let extra_skip: Vec<String> = cli.skip_dirs.unwrap_or_default();
    let extra_refs: Vec<&str> = extra_skip.iter().map(|s| s.as_str()).collect();
    skip_dirs.extend(extra_refs.iter().copied());

    let bench = std::env::var("BENCH").is_ok();
    macro_rules! timed {
        ($label:expr, $block:expr) => {{
            let _start = std::time::Instant::now();
            let _result = $block;
            if bench {
                eprintln!(
                    "  {:30} {:>6.2}ms",
                    $label,
                    _start.elapsed().as_secs_f64() * 1000.0
                );
            }
            _result
        }};
    }

    if !cli.quiet {
        eprintln!("Scanning for unused gems in {}...", dir.display());
        eprintln!();
    }

    let explicit_gemfiles = !cli.gemfile.is_empty();
    let gemfile_paths: Vec<PathBuf> = if explicit_gemfiles {
        cli.gemfile
            .iter()
            .map(|p| {
                let path = PathBuf::from(p);
                if path.is_absolute() {
                    path
                } else {
                    dir.join(path)
                }
            })
            .collect()
    } else {
        vec![dir.join("Gemfile")]
    };

    // Check Gemfile existence before parsing (exit 2 if missing)
    let missing_gemfiles: Vec<&PathBuf> = gemfile_paths.iter().filter(|p| !p.exists()).collect();
    if !missing_gemfiles.is_empty() {
        if cli.allow_missing_gemfile {
            eprintln!(
                "Notice: Missing Gemfile(s): {} — skipping.",
                missing_gemfiles
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            std::process::exit(0);
        } else {
            eprintln!(
                "Error: Missing Gemfile(s): {}",
                missing_gemfiles
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            std::process::exit(2);
        }
    }

    // Load file cache
    let file_cache = if cli.no_cache {
        Arc::new(cache::FileCache::disabled())
    } else {
        Arc::new(timed!("cache_load", cache::FileCache::load(&dir)))
    };

    // 1. Parse Gemfile
    let mut gems = timed!(
        "parse_gemfile",
        if explicit_gemfiles {
            gemfile::parse_gemfiles(&dir, &gemfile_paths)
        } else {
            gemfile::parse_gemfile(&dir)
        }
    );
    if !cli.quiet {
        eprintln!("Found {} gem(s) in Gemfile", gems.len());
    }

    // 2. Concurrent init: (index + scan) || (db_adapters + cable + storage + cache_cfg)
    let gem_names: Vec<String> = gems.iter().map(|g| g.name.clone()).collect();
    let cache_for_scan = Arc::clone(&file_cache);
    let ignore_file_ref = cli.ignore_file.as_deref();
    let ignore_config = timed!(
        "load_ignore_file",
        crate::ignore::load_ignore_file(&dir, ignore_file_ref)
    );
    let adapter_overrides = &ignore_config.adapter_overrides;
    let lockfile = timed!(
        "parse_gemfile_lock",
        if explicit_gemfiles {
            lockfile::parse_gemfile_locks(&dir, &gemfile_paths)
        } else {
            lockfile::parse_gemfile_lock(&dir)
        }
    );
    let needs_runtime_platform = gems.iter().any(|gem| !gem.platforms.is_empty())
        || lockfile.has_platform_specific_registry_artifacts();
    let runtime_platform = needs_runtime_platform.then(|| platforms::resolve_current_runtime(&dir));
    let current_gem_platform = runtime_platform
        .as_ref()
        .and_then(platforms::RuntimePlatform::gem_platform);
    let (
        ((mut gem_index, vendor_path_index), (mut scan_results, cache_updates)),
        (db_adapters, (cable_adapters, (storage_adapters, cache_config_adapters))),
    ) = timed!(
        "concurrent_init",
        rayon::join(
            || {
                rayon::join(
                    || {
                        let gp = introspect::find_gem_paths(&dir);
                        rayon::join(
                            || {
                                introspect::build_gem_index_for_lock(
                                    &gp,
                                    &lockfile,
                                    current_gem_platform,
                                )
                            },
                            || {
                                introspect::build_vendor_cache_path_index_for_lock(
                                    &dir,
                                    &lockfile,
                                    current_gem_platform,
                                )
                            },
                        )
                    },
                    || {
                        scanner::scan_source_tree_with_threads_and_explicit_skips(
                            &dir,
                            &skip_dirs,
                            &extra_refs,
                            cache_for_scan,
                            walker_threads,
                        )
                    },
                )
            },
            || {
                rayon::join(
                    || {
                        timed!(
                            "scan_db_adapters",
                            scan_database_adapters(&dir, adapter_overrides)
                        )
                    },
                    || {
                        rayon::join(
                            || {
                                timed!(
                                    "scan_cable_adapters",
                                    scan_cable_adapters(&dir, adapter_overrides)
                                )
                            },
                            || {
                                rayon::join(
                                    || {
                                        timed!(
                                            "scan_storage_adapters",
                                            scan_storage_adapters(&dir, adapter_overrides)
                                        )
                                    },
                                    || {
                                        timed!(
                                            "scan_cache_config",
                                            scan_cache_config(&dir, adapter_overrides)
                                        )
                                    },
                                )
                            },
                        )
                    },
                )
            },
        )
    );
    introspect::index_declared_gem_sources(&dir, &gems, &mut gem_index);

    // Apply cache updates from the walk
    let mut file_cache = match Arc::try_unwrap(file_cache) {
        Ok(fc) => fc,
        Err(_arc) => {
            if cli.no_cache {
                cache::FileCache::disabled()
            } else {
                timed!("cache_reload", cache::FileCache::load(&dir))
            }
        }
    };
    for (rel_path, mtime, size, reqs, mods, bgroups, breq_all) in cache_updates {
        file_cache.put(rel_path, mtime, size, &reqs, mods, &bgroups, breq_all);
    }

    // Synthesize active bundler groups when `Bundler.require(*Rails.groups)` (or equivalent)
    // is detected. See `synthesize_bundler_groups` for rationale.
    if scan_results.bundler_require_all {
        synthesize_bundler_groups(
            &gems,
            cli.rails_env.as_deref(),
            &mut scan_results.bundler_groups,
        );
    }

    // 2b+3. Script scanning || resolve require paths (concurrent)
    let vendor_memo: Mutex<HashMap<PathBuf, Option<Arc<introspect::GemMetadata>>>> =
        Mutex::new(HashMap::new());
    let (script_refs, _) = timed!(
        "scripts+resolve",
        rayon::join(
            || {
                scripts::scan_scripts_for_gem_usage(
                    &dir,
                    &gem_names,
                    &gem_index,
                    &scan_results.script_files,
                )
            },
            || {
                gems.par_iter_mut().for_each(|gem| {
                    let mut declared = std::mem::take(&mut gem.require_paths);
                    let heuristic = resolve_require_paths(gem, &gem_index);
                    for p in heuristic {
                        if !declared.contains(&p) {
                            declared.push(p);
                        }
                    }
                    // Lazy vendor fallback: extract metadata on demand
                    if declared.len() <= 1 && gem.source.is_none() {
                        if let Some(path) = vendor_path_index.get(&gem.name) {
                            // Compute-outside-lock: check cache first; if miss, decompress
                            // tarball outside the lock, then insert. Avoids serializing
                            // every vendor extraction across all rayon workers.
                            let cached = vendor_memo.lock().unwrap().get(path).cloned();
                            let meta = match cached {
                                Some(m) => m,
                                None => {
                                    let computed =
                                        introspect::extract_vendor_gem_metadata(path).map(Arc::new);
                                    vendor_memo
                                        .lock()
                                        .unwrap()
                                        .entry(path.clone())
                                        .or_insert(computed)
                                        .clone()
                                }
                            };
                            if let Some(ref meta) = meta {
                                for rp in &meta.require_paths {
                                    if rp != "lib" && !declared.contains(rp) {
                                        declared.push(rp.clone());
                                    }
                                }
                            }
                        }
                    }
                    gem.require_paths = declared;
                })
            },
        )
    );

    // 4. First pass match on require paths only (no module names yet)
    let mut results = timed!(
        "match_gem_usage",
        matcher::match_gem_usage(&gems, &scan_results)
    );

    // 4b. Merge adapter evidence (computed in concurrent init)
    let mut adapter_evidence: Vec<(&str, &str, &str, &str)> = Vec::new();
    for (adapter_map, kind, source) in [
        (&db_adapters, "database", "config/database.yml"),
        (&cable_adapters, "cable", "config/cable.yml"),
        (&storage_adapters, "storage", "config/storage.yml"),
    ] {
        for (gem_name, adapter) in adapter_map {
            adapter_evidence.push((gem_name, adapter, kind, source));
        }
    }
    for (gem_name, (store, source)) in &cache_config_adapters {
        adapter_evidence.push((gem_name, store, "cache", source));
    }
    for (gem_name, adapter, kind, source) in adapter_evidence {
        if let Some(result) = results.get_mut(gem_name) {
            result.evidence.push(types::Evidence::DatabaseAdapter {
                adapter: adapter.to_string(),
                kind: kind.to_string(),
                source: source.to_string(),
            });
            if result.status != GemStatus::Used {
                result.files.push(source.to_string());
                result.files.sort();
                result.status = GemStatus::Used;
            }
        }
    }

    // 5. Merge script/CI evidence before lazy introspection check
    merge_script_refs(&mut results, &script_refs, false);

    // 6. Lazy introspection: only resolve module names for gems not yet matched
    let unresolved: Vec<usize> = gems
        .iter()
        .enumerate()
        .filter(|(_, gem)| {
            gem.module_names.is_empty()
                && !results
                    .get(&gem.name)
                    .is_some_and(|r| r.status == GemStatus::Used)
        })
        .map(|(i, _)| i)
        .collect();

    if !unresolved.is_empty() {
        timed!("lazy_introspect_modules", {
            let module_results: Vec<(usize, Vec<String>)> = unresolved
                .par_iter()
                .map(|&i| (i, resolve_module_names(&gems[i], &gem_index)))
                .collect();
            for (i, modules) in module_results {
                gems[i].module_names = modules;
            }
        });
        // Re-match only unresolved gems with module names
        timed!("rematch_unresolved", {
            let updated = matcher::match_gem_subset(&gems, &unresolved, &scan_results);
            for (name, new_result) in updated {
                if let Some(existing) = results.get(&name) {
                    if existing.status != GemStatus::Used {
                        results.insert(name, new_result);
                    }
                }
            }
        });
        // Re-merge script refs for newly resolved gems
        merge_script_refs(&mut results, &script_refs, true);
    }

    if !cli.quiet {
        eprintln!(
            "Scanned source tree ({} unique requires found)",
            scan_results.requires.len()
        );
    }

    // 7. Apply platform filter
    if let Some(runtime_platform) = runtime_platform.as_ref() {
        let current_platforms = runtime_platform.dsl_tokens();
        for result in results.values_mut() {
            if !result.gem.platforms.is_empty()
                && !result
                    .gem
                    .platforms
                    .iter()
                    .any(|p| current_platforms.contains(p.as_str()))
            {
                result.status = GemStatus::PlatformExcluded;
            }
        }
    }

    // 8. Apply ignore file (computed in concurrent init)
    for (name, result) in &mut results {
        if ignore_config.ignored.contains(name) {
            result.status = GemStatus::Ignored;
        }
    }
    if let Some(ref path) = ignore_config.path {
        if !cli.quiet {
            let ignored_count = results
                .values()
                .filter(|v| v.status == GemStatus::Ignored)
                .count();
            if ignored_count > 0 {
                eprintln!("Loaded {} ({} ignored)", path, ignored_count);
            }
        }
    }

    for result in results.values_mut() {
        result.files.sort();
        result.files.dedup();
        result.evidence.sort();
        result.evidence.dedup();
    }

    // Gemfile.lock transitive dep info (computed in concurrent init)

    if !cli.quiet {
        eprintln!();
    }

    // 9. Diff mode: compare and exit
    if let Some(ref baseline_path) = cli.diff {
        let new_unused_count = diff::print_diff(&results, baseline_path);
        // Save cache before exit
        timed!("cache_save", file_cache.save());
        let exit_code = match cli.fail_on.as_str() {
            "none" => 0,
            "new" => {
                if new_unused_count > 0 {
                    1
                } else {
                    0
                }
            }
            _ => {
                // "any"
                let total_unused = results
                    .values()
                    .filter(|r| matches!(r.status, GemStatus::Unused))
                    .count();
                if total_unused > 0 {
                    1
                } else {
                    0
                }
            }
        };
        std::process::exit(exit_code);
    }

    // 10. Output results
    let unused_count = if cli.sarif {
        timed!(
            "output_sarif",
            output::print_results_sarif(&results, &dir, lockfile.dependencies())
        )
    } else {
        timed!(
            "output",
            match cli.output.as_str() {
                "json" => output::print_results_json(&results, lockfile.dependencies()),
                "github" => output::print_results_github(&results, &dir, lockfile.dependencies()),
                _ => output::print_results_text(&results, cli.verbose, lockfile.dependencies(),),
            }
        )
    };

    // Save cache
    timed!("cache_save", file_cache.save());

    let exit_code = match cli.fail_on.as_str() {
        "none" => 0,
        "any" if unused_count > 0 => 1,
        "any" => 0,
        // "new" only meaningful in diff mode; non-diff treats as none
        _ => 0,
    };
    std::process::exit(exit_code);
}

/// Merge script/CI evidence into gem results.
/// When `only_if_not_used` is true, skip gems already marked Used.
fn merge_script_refs(
    results: &mut HashMap<String, types::GemResult>,
    script_refs: &HashMap<String, HashSet<String>>,
    only_if_not_used: bool,
) {
    for (gem_name, files) in script_refs {
        if let Some(result) = results.get_mut(gem_name) {
            if only_if_not_used && (result.status == GemStatus::Used || files.is_empty()) {
                continue;
            }
            for f in files {
                result
                    .evidence
                    .push(types::Evidence::Script { file: f.clone() });
            }
            // Dedup via HashSet then sort once — avoids quadratic per-iteration sort/dedup.
            let mut set: HashSet<String> = std::mem::take(&mut result.files).into_iter().collect();
            for f in files {
                set.insert(f.clone());
            }
            let mut merged: Vec<String> = set.into_iter().collect();
            merged.sort();
            result.files = merged;
            if !result.files.is_empty() {
                result.status = GemStatus::Used;
            }
        }
    }
}

/// Resolve require paths for a gem: check installed gem first, then use heuristics.
fn resolve_require_paths(
    gem: &types::GemEntry,
    gem_index: &std::collections::HashMap<String, std::path::PathBuf>,
) -> Vec<String> {
    // Try introspection first
    if let Some(discovered) = introspect::discover_gem_require_paths(&gem.name, gem_index) {
        let mut paths = discovered;
        if !paths.contains(&gem.name) {
            paths.push(gem.name.clone());
        }
        return paths;
    }

    // Known divergent gem→require mappings (gems where require path ≠ gem name)
    if let Some(known) = known::require_paths_for(gem.name.as_str()) {
        let mut paths: Vec<String> = known.iter().map(|s| s.to_string()).collect();
        if !paths.contains(&gem.name) {
            paths.push(gem.name.clone());
        }
        return paths;
    }

    // Fallback heuristics
    let mut paths = Vec::new();
    if gem.name.contains('-') {
        paths.push(gem.name.replace('-', "/"));
        paths.push(gem.name.replace('-', "_"));
    }
    paths.push(gem.name.clone());
    // NOTE: we deliberately do NOT add the bare first segment as a require path
    // (e.g. "aws" for "aws-sdk-s3"). It almost never corresponds to a real require
    // path and over-attributes usage to unrelated gems in the same family.
    paths.dedup();
    paths
}

/// Resolve module names for a gem: union of discovered names (from installed gem)
/// AND curated KNOWN_MODULE_NAMES AND PascalCase heuristics. Discovery alone can
/// miss canonical module names (e.g. discover picks up a generic helper module
/// instead of the gem's primary constant), so we always merge all sources.
fn resolve_module_names(
    gem: &types::GemEntry,
    gem_index: &std::collections::HashMap<String, std::path::PathBuf>,
) -> Vec<String> {
    let discovered = introspect::discover_gem_module_names(&gem.name, gem_index);

    // Static known module names for gems where PascalCase heuristic fails
    let mut names = Vec::new();

    // Always include discovered names from the installed gem (if any).
    for n in &discovered {
        names.push(n.clone());
    }

    // Lookup known module names — merge with discovered (do not replace).
    if let Some(known) = known::module_names_for(gem.name.as_str()) {
        for n in known {
            names.push(n.to_string());
        }
    }

    // Always also emit heuristic PascalCase variants
    let pascal = gem
        .name
        .split(&['-', '_'][..])
        .map(capitalize)
        .collect::<String>();
    names.push(pascal);
    if gem.name.contains('-') {
        // Full hyphen-split namespace: e.g. "aws-sdk-s3" → "Aws::Sdk::S3"
        let segments: Vec<String> = gem
            .name
            .split('-')
            .map(|p| p.split('_').map(capitalize).collect::<String>())
            .collect();
        let ns = segments.join("::");
        names.push(ns);

        // Emit prefix variants starting from 2 segments (e.g. "Aws::Sdk").
        // We deliberately skip the bare 1-segment prefix (e.g. "Aws" alone) because
        // it over-attributes: code referencing `Aws::S3::Client` should NOT mark
        // unrelated `aws-sdk-cloudwatch` as Used. The curated KNOWN_MODULE_NAMES
        // table provides the precise mappings (e.g. aws-sdk-core → "Aws").
        for i in 2..segments.len() {
            let prefix = segments[..i].join("::");
            names.push(prefix);
        }
    }
    if !gem.name.contains('-') && gem.name.matches('_').count() >= 2 {
        let segments: Vec<&str> = gem.name.split('_').filter(|s| !s.is_empty()).collect();
        if segments.len() >= 3 {
            for i in 2..segments.len() {
                let prefix = segments[..i]
                    .iter()
                    .map(|s| capitalize(s))
                    .collect::<String>();
                if prefix.len() >= 4 {
                    names.push(prefix);
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
}

/// Read a YAML config file, falling back to its `.yml.erb` variant if the plain
/// `.yml` is absent. Many Rails apps template their config files with ERB; the
/// scanners below only match static `key: value` lines, which work fine on raw
/// `.yml.erb` source.
fn read_yaml_or_erb(yml_path: &std::path::Path) -> Option<String> {
    read_yaml_or_erb_raw(yml_path).map(|c| strip_erb_blocks(&c))
}

fn read_yaml_or_erb_raw(yml_path: &std::path::Path) -> Option<String> {
    if let Ok(c) = std::fs::read_to_string(yml_path) {
        return Some(c);
    }
    let erb_path = {
        let mut s = yml_path.as_os_str().to_owned();
        s.push(".erb");
        std::path::PathBuf::from(s)
    };
    std::fs::read_to_string(&erb_path).ok()
}

fn strip_erb_blocks(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(start_rel) = memchr::memmem::find(&bytes[i..], b"<%") {
            let start = i + start_rel;
            out.push_str(&input[i..start]);
            let body_start = start + 2;
            if let Some(end_rel) = memchr::memmem::find(&bytes[body_start..], b"%>") {
                let end = body_start + end_rel;
                for &b in &bytes[start..end + 2] {
                    if b == b'\n' {
                        out.push('\n');
                    } else {
                        out.push(' ');
                    }
                }
                i = end + 2;
            } else {
                for &b in &bytes[start..] {
                    if b == b'\n' {
                        out.push('\n');
                    } else {
                        out.push(' ');
                    }
                }
                break;
            }
        } else {
            out.push_str(&input[i..]);
            break;
        }
    }
    out
}

/// Scan config/database.yml (or .yml.erb) for adapter: values and map them to gem names.
fn scan_database_adapters(
    dir: &std::path::Path,
    overrides: &adapters::AdapterOverrides,
) -> std::collections::HashMap<String, String> {
    let db_yml = dir.join("config").join("database.yml");
    let raw = match read_yaml_or_erb_raw(&db_yml) {
        Some(c) => c,
        None => return std::collections::HashMap::new(),
    };
    let content = strip_erb_blocks(&raw);

    static ADAPTER_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(?m)^[ \t]*adapter:[ \t]*(\S+)").unwrap()
    });
    // `url:` values are read from the raw source so an ERB literal default
    // (`ENV.fetch("DATABASE_URL", "postgres://…")`) can supply the scheme.
    static URL_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(?m)^[ \t]*url:[ \t]*(.+?)[ \t]*$").unwrap()
    });

    let mut found: Vec<String> = ADAPTER_RE
        .captures_iter(&content)
        .map(|cap| cap[1].trim_matches(|c| c == '\'' || c == '"').to_string())
        .collect();
    found.extend(
        URL_RE
            .captures_iter(&raw)
            .filter_map(|cap| adapters::database_url_adapter(&cap[1])),
    );

    let mut adapters = std::collections::HashMap::new();
    for adapter in found {
        for gem_name in adapters::database_gems(&adapter, overrides) {
            adapters.insert(gem_name, adapter.clone());
        }
    }
    adapters
}

/// Scan config/cable.yml (or .yml.erb) for adapter: values and map them to gem names.
fn scan_cable_adapters(
    dir: &std::path::Path,
    overrides: &adapters::AdapterOverrides,
) -> std::collections::HashMap<String, String> {
    let cable_yml = dir.join("config").join("cable.yml");
    let content = match read_yaml_or_erb(&cable_yml) {
        Some(c) => c,
        None => return std::collections::HashMap::new(),
    };

    static ADAPTER_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(?m)^[ \t]*adapter:[ \t]*(\S+)").unwrap()
    });

    let mut adapters = std::collections::HashMap::new();
    for cap in ADAPTER_RE.captures_iter(&content) {
        let adapter = cap[1].trim_matches(|c| c == '\'' || c == '"');
        if let Some(gem_name) = adapters::cable_gem(adapter, overrides) {
            adapters.insert(gem_name, adapter.to_string());
        }
    }
    adapters
}

/// Scan config/storage.yml (or .yml.erb) for service: values and map them to gem names.
fn scan_storage_adapters(
    dir: &std::path::Path,
    overrides: &adapters::AdapterOverrides,
) -> std::collections::HashMap<String, String> {
    let storage_yml = dir.join("config").join("storage.yml");
    let content = match read_yaml_or_erb(&storage_yml) {
        Some(c) => c,
        None => return std::collections::HashMap::new(),
    };

    static SELECTED_SERVICE_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| {
            regex::Regex::new(r"(?m)^[ \t]*config\.active_storage\.service\s*=\s*:(\w+)").unwrap()
        });

    let mut selected = HashSet::new();
    let mut config_files = vec![dir.join("config").join("application.rb")];
    let envs_dir = dir.join("config").join("environments");
    if let Ok(entries) = std::fs::read_dir(envs_dir) {
        config_files.extend(
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "rb")),
        );
    }
    for file in config_files {
        let Ok(config) = std::fs::read_to_string(file) else {
            continue;
        };
        for cap in SELECTED_SERVICE_RE.captures_iter(&config) {
            selected.insert(cap[1].to_string());
        }
    }
    static SERVICE_SYMBOL_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r":(\w+)").unwrap());
    let app_pattern = dir.join("app").join("**").join("*.rb");
    if let Some(pattern) = app_pattern.to_str() {
        if let Ok(paths) = glob::glob(pattern) {
            for path in paths.flatten() {
                let Ok(source) = std::fs::read_to_string(path) else {
                    continue;
                };
                let lines: Vec<&str> = source.lines().collect();
                for (index, line) in lines.iter().enumerate() {
                    if !line.contains("has_one_attached") && !line.contains("has_many_attached") {
                        continue;
                    }
                    let mut statement = line.to_string();
                    for continuation in lines.iter().skip(index + 1).take(5) {
                        let trimmed = continuation.trim_start();
                        if continuation.len() == trimmed.len()
                            || ["class ", "def ", "end", "has_one_", "has_many_", "module "]
                                .iter()
                                .any(|prefix| trimmed.starts_with(prefix))
                        {
                            break;
                        }
                        statement.push(' ');
                        statement.push_str(trimmed);
                    }
                    let Some((_, service_expression)) = statement.split_once("service:") else {
                        continue;
                    };
                    for cap in SERVICE_SYMBOL_RE.captures_iter(service_expression) {
                        selected.insert(cap[1].to_string());
                    }
                }
            }
        }
    }

    let mut services = std::collections::HashMap::new();
    let mut current = None;
    for line in content.lines() {
        if !line.starts_with([' ', '\t']) {
            current = line.trim().strip_suffix(':').map(|name| name.to_string());
            continue;
        }
        let Some(service) = line.trim().strip_prefix("service:") else {
            continue;
        };
        if let Some(name) = current.as_ref() {
            services.insert(
                name.clone(),
                service
                    .trim()
                    .trim_matches(|c| c == '\'' || c == '"')
                    .to_string(),
            );
        }
    }

    let mut adapters = std::collections::HashMap::new();
    for name in selected {
        let Some(service) = services.get(&name) else {
            continue;
        };
        let gem_name = adapters::storage_gem(service, overrides);
        if let Some(gem) = gem_name {
            adapters.insert(gem.to_string(), service.clone());
        }
    }
    adapters
}

/// Scan config/environments/*.rb and config/application.rb for cache_store settings.
/// Maps gem → (store, relative source file).
fn scan_cache_config(
    dir: &std::path::Path,
    overrides: &adapters::AdapterOverrides,
) -> std::collections::HashMap<String, (String, String)> {
    static CACHE_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(
            r"(?m)^[ \t]*(?:config|Rails\.application\.config)\.cache_store\s*=\s*:(\w+)",
        )
        .unwrap()
    });

    let mut adapters = std::collections::HashMap::new();
    let mut files_to_check = vec![dir.join("config").join("application.rb")];
    let envs_dir = dir.join("config").join("environments");
    if envs_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&envs_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().is_some_and(|e| e == "rb") {
                    files_to_check.push(p);
                }
            }
        }
    }

    for file in files_to_check {
        let content = match std::fs::read_to_string(&file) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for cap in CACHE_RE.captures_iter(&content) {
            let store = &cap[1];
            let gem_name = adapters::cache_gem(store, overrides);
            if let Some(gem) = gem_name {
                let source = file
                    .strip_prefix(dir)
                    .unwrap_or(&file)
                    .to_string_lossy()
                    .replace('\\', "/");
                adapters.insert(gem, (store.to_string(), source));
            }
        }
    }
    adapters
}

/// `Rails.groups` always includes `default`, but its environment-specific
/// groups cannot be inferred safely without executing the application.
fn synthesize_bundler_groups(
    _gems: &[types::GemEntry],
    rails_env: Option<&str>,
    target: &mut HashSet<String>,
) {
    const CONDITION_PREFIX: &str = "__rails_group_condition__:";
    let conditions: Vec<String> = target
        .iter()
        .filter(|group| group.starts_with(CONDITION_PREFIX))
        .cloned()
        .collect();
    for condition in &conditions {
        target.remove(condition);
    }
    target.insert("default".to_string());
    if let Some(env) = rails_env {
        target.insert(env.to_string());
        for condition in conditions {
            let Some((group, environments)) = condition
                .trim_start_matches(CONDITION_PREFIX)
                .split_once(':')
            else {
                continue;
            };
            if environments.split(',').any(|candidate| candidate == env) {
                target.insert(group.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn gem(name: &str, groups: &[&str]) -> types::GemEntry {
        types::GemEntry {
            name: name.to_string(),
            groups: groups.iter().map(|s| s.to_string()).collect(),
            platforms: vec![],
            require_paths: vec![],
            module_names: vec![],
            require_false: false,
            source: None,
        }
    }

    #[test]
    fn synthesize_bundler_groups_does_not_assume_environment_groups() {
        let gems = vec![
            gem("rails", &[]),
            gem("rspec-rails", &["test"]),
            gem("pry", &["development", "test"]),
            gem("newrelic", &["production", "staging"]),
        ];
        let mut groups = HashSet::new();
        synthesize_bundler_groups(&gems, None, &mut groups);
        assert!(groups.contains("default"));
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn synthesize_bundler_groups_always_includes_default() {
        let gems = vec![gem("rails", &[])];
        let mut groups = HashSet::new();
        synthesize_bundler_groups(&gems, None, &mut groups);
        assert!(groups.contains("default"));
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn synthesize_bundler_groups_ignores_declared_groups() {
        let gems = vec![
            gem("a", &["test"]),
            gem("b", &["test"]),
            gem("c", &["test", "development"]),
        ];
        let mut groups = HashSet::new();
        synthesize_bundler_groups(&gems, None, &mut groups);
        assert_eq!(groups, HashSet::from(["default".to_string()]));
    }

    #[test]
    fn synthesize_bundler_groups_includes_explicit_rails_environment() {
        let gems = vec![gem("rails", &[])];
        let mut groups = HashSet::new();
        synthesize_bundler_groups(&gems, Some("test"), &mut groups);
        assert_eq!(
            groups,
            HashSet::from(["default".to_string(), "test".to_string()])
        );
    }

    #[test]
    fn synthesize_bundler_groups_resolves_conditional_group() {
        let gems = vec![gem("rails", &[])];
        let mut groups = HashSet::from([String::from(
            "__rails_group_condition__:assets:development,test",
        )]);
        synthesize_bundler_groups(&gems, Some("development"), &mut groups);
        assert_eq!(
            groups,
            HashSet::from([
                "default".to_string(),
                "development".to_string(),
                "assets".to_string()
            ])
        );
    }

    #[test]
    fn resolve_require_paths_drops_bare_first_segment_for_hyphenated() {
        // aws-sdk-s3: must NOT include bare "aws" as a require path
        let g = gem("some-unknown-vendor-pkg", &[]);
        let idx = std::collections::HashMap::new();
        let paths = resolve_require_paths(&g, &idx);
        assert!(
            !paths.iter().any(|p| p == "some"),
            "bare first segment 'some' should not be emitted; got {:?}",
            paths
        );
        // Should still emit the full slashed form and the original name
        assert!(paths.iter().any(|p| p == "some/unknown/vendor/pkg"));
        assert!(paths.iter().any(|p| p == "some-unknown-vendor-pkg"));
    }

    #[test]
    fn resolve_module_names_drops_bare_one_segment_prefix_for_hyphenated() {
        // For an unknown gem like "some-unknown-vendor-pkg", we should NOT emit
        // the bare 1-segment prefix "Some" alone; it over-attributes.
        let g = gem("some-unknown-vendor-pkg", &[]);
        let idx = std::collections::HashMap::new();
        let names = resolve_module_names(&g, &idx);
        assert!(
            !names.iter().any(|n| n == "Some"),
            "bare 1-segment prefix 'Some' should not be emitted; got {:?}",
            names
        );
        // 2-segment prefix should still appear
        assert!(names.iter().any(|n| n == "Some::Unknown"));
        // Full namespace and full pascal should still appear
        assert!(names.iter().any(|n| n == "Some::Unknown::Vendor::Pkg"));
    }

    #[test]
    fn resolve_module_names_unions_known_with_pascal_heuristic() {
        // A gem present in KNOWN_MODULE_NAMES (e.g. "graphql" → "GraphQL") should
        // emit BOTH the curated name AND the heuristic PascalCase form.
        let g = gem("graphql", &[]);
        let idx = std::collections::HashMap::new();
        let names = resolve_module_names(&g, &idx);
        assert!(names.iter().any(|n| n == "GraphQL"));
        assert!(names.iter().any(|n| n == "Graphql"));
    }

    #[test]
    fn resolve_module_names_avoids_shared_extension_root() {
        let g = gem("graphql-batch", &[]);
        let idx = std::collections::HashMap::new();
        let names = resolve_module_names(&g, &idx);
        assert!(names.iter().any(|n| n == "GraphQL::Batch"));
        assert!(!names.iter().any(|n| n == "GraphQL"));
    }

    #[test]
    fn resolve_module_names_adds_underscore_compound_prefixes() {
        let g = gem("active_model_serializers", &[]);
        let idx = std::collections::HashMap::new();
        let names = resolve_module_names(&g, &idx);
        assert!(names.iter().any(|n| n == "ActiveModel"));
        assert!(names.iter().any(|n| n == "ActiveModelSerializers"));
        assert!(!names.iter().any(|n| n == "Active"));
    }

    #[test]
    fn scan_database_adapters_reads_yml_erb_when_yml_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        let mut f = std::fs::File::create(cfg.join("database.yml.erb")).unwrap();
        writeln!(f, "default: &default").unwrap();
        writeln!(f, "  adapter: postgresql").unwrap();
        writeln!(f, "  host: <%= ENV['DB_HOST'] %>").unwrap();
        drop(f);

        let adapters = scan_database_adapters(tmp.path(), &adapters::AdapterOverrides::default());
        assert_eq!(adapters.get("pg").map(|s| s.as_str()), Some("postgresql"));
    }

    #[test]
    fn scan_database_adapters_strips_dynamic_erb_values() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("database.yml.erb"),
            "default:\n  adapter: <%= ENV['DB_ADAPTER'] %>\n  replica_adapter: postgresql\n",
        )
        .unwrap();

        let adapters = scan_database_adapters(tmp.path(), &adapters::AdapterOverrides::default());
        assert!(!adapters.contains_key("<%="));
        assert!(adapters.is_empty());
    }

    #[test]
    fn scan_database_adapters_maps_third_party_adapters_and_urls() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("database.yml"),
            "default:\n  adapter: postgis\nother:\n  adapter: sqlserver\ndev:\n  url: mysql2://localhost/app\nt:\n  adapter: \"trilogy\" # comment\nprod:\n  url: <%= ENV.fetch(\"DATABASE_URL\", \"sqlite3:db/prod.sqlite3\") %>\n",
        )
        .unwrap();

        let adapters = scan_database_adapters(tmp.path(), &adapters::AdapterOverrides::default());
        for (gem, adapter) in [
            ("activerecord-postgis-adapter", "postgis"),
            ("pg", "postgis"),
            ("activerecord-sqlserver-adapter", "sqlserver"),
            ("tiny_tds", "sqlserver"),
            ("mysql2", "mysql2"),
            ("trilogy", "trilogy"),
            ("sqlite3", "sqlite3"),
        ] {
            assert_eq!(
                adapters.get(gem).map(String::as_str),
                Some(adapter),
                "{gem}"
            );
        }
        assert!(!adapters.contains_key("postgis"));
        assert!(!adapters.contains_key("sqlserver"));
    }

    #[test]
    fn scan_cable_adapters_skips_builtin_adapters() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("cable.yml"),
            "development:\n  adapter: async\ntest:\n  adapter: test\nproduction:\n  adapter: redis\n",
        )
        .unwrap();

        let adapters = scan_cable_adapters(tmp.path(), &adapters::AdapterOverrides::default());
        assert_eq!(adapters.len(), 1);
        assert_eq!(adapters.get("redis").map(String::as_str), Some("redis"));
    }

    #[test]
    fn strip_erb_blocks_preserves_static_yaml_lines() {
        let stripped =
            strip_erb_blocks("<% if ENV['DATABASE_URL'] %>\n  adapter: postgresql\n<% end %>\n");
        assert!(stripped.contains("  adapter: postgresql"));
        assert!(!stripped.contains("<%"));
        assert!(!stripped.contains("%>"));
    }

    #[test]
    fn scan_cable_adapters_reads_yml_erb() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("cable.yml.erb"),
            "production:\n  adapter: redis\n  url: <%= ENV['REDIS_URL'] %>\n",
        )
        .unwrap();

        let adapters = scan_cable_adapters(tmp.path(), &adapters::AdapterOverrides::default());
        assert_eq!(adapters.get("redis").map(|s| s.as_str()), Some("redis"));
    }

    #[test]
    fn scan_storage_adapters_reads_yml_erb() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("storage.yml.erb"),
            "amazon:\n  service: S3\n  bucket: <%= ENV['BUCKET'] %>\n",
        )
        .unwrap();
        let envs = cfg.join("environments");
        std::fs::create_dir_all(&envs).unwrap();
        std::fs::write(
            envs.join("production.rb"),
            "config.active_storage.service = :amazon\n",
        )
        .unwrap();

        let adapters = scan_storage_adapters(tmp.path(), &adapters::AdapterOverrides::default());
        assert_eq!(adapters.get("aws-sdk-s3").map(|s| s.as_str()), Some("S3"));
    }

    #[test]
    fn scan_storage_adapters_includes_attachment_service() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        let models = tmp.path().join("app").join("models");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(
            cfg.join("storage.yml"),
            "local:\n  service: Disk\namazon:\n  service: S3\n",
        )
        .unwrap();
        std::fs::write(
            models.join("user.rb"),
            "class User\n  has_one_attached :avatar, service: :amazon\nend\n",
        )
        .unwrap();

        let adapters = scan_storage_adapters(tmp.path(), &adapters::AdapterOverrides::default());
        assert_eq!(adapters.get("aws-sdk-s3").map(String::as_str), Some("S3"));
    }

    #[test]
    fn scan_storage_adapters_includes_dynamic_attachment_services() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        let models = tmp.path().join("app").join("models");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(
            cfg.join("storage.yml"),
            "europe:\n  service: S3\nusa:\n  service: GCS\n",
        )
        .unwrap();
        std::fs::write(
            models.join("user.rb"),
            "class User\n  has_one_attached :avatar,\n    service: ->(user) { user.eu? ? :europe : :usa }\nend\n",
        )
        .unwrap();

        let adapters = scan_storage_adapters(tmp.path(), &adapters::AdapterOverrides::default());
        assert_eq!(adapters.get("aws-sdk-s3").map(String::as_str), Some("S3"));
        assert_eq!(
            adapters.get("google-cloud-storage").map(String::as_str),
            Some("GCS")
        );
    }

    #[test]
    fn scan_cache_config_ignores_commented_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config").join("environments");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("production.rb"),
            "# config.cache_store = :mem_cache_store\n",
        )
        .unwrap();

        let adapters = scan_cache_config(tmp.path(), &adapters::AdapterOverrides::default());
        assert!(!adapters.contains_key("dalli"));
    }

    #[test]
    fn scan_cache_config_records_source_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config").join("environments");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("production.rb"),
            "config.cache_store = :redis_cache_store\nconfig.cache_store = :memory_store\n",
        )
        .unwrap();

        let adapters = scan_cache_config(tmp.path(), &adapters::AdapterOverrides::default());
        assert_eq!(adapters.len(), 1);
        assert_eq!(
            adapters.get("redis"),
            Some(&(
                "redis_cache_store".to_string(),
                "config/environments/production.rb".to_string()
            ))
        );
    }

    #[test]
    fn scan_database_adapters_prefers_yml_over_yml_erb() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("database.yml"), "default:\n  adapter: mysql2\n").unwrap();
        std::fs::write(
            cfg.join("database.yml.erb"),
            "default:\n  adapter: postgresql\n",
        )
        .unwrap();

        let adapters = scan_database_adapters(tmp.path(), &adapters::AdapterOverrides::default());
        // .yml takes precedence
        assert_eq!(adapters.get("mysql2").map(|s| s.as_str()), Some("mysql2"));
        assert!(!adapters.contains_key("pg"));
    }

    #[test]
    fn derived_platform_set_does_not_include_invalid_os_tokens() {
        let platforms = platforms::derive_platform_tokens("ruby", "x86_64-linux", "3.4.0");
        assert!(!platforms.contains("darwin"));
        assert!(!platforms.contains("linux"));
        assert!(!platforms.contains("java"));
        assert!(
            platforms
                .iter()
                .filter(|platform| platform.starts_with("mri_"))
                .count()
                <= 1
        );
    }
}
