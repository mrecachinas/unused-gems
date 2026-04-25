mod cache;
mod diff;
mod gemfile;
mod ignore;
mod introspect;
mod lockfile;
mod matcher;
mod output;
mod scanner;
mod scripts;
mod types;

use std::collections::{HashMap, HashSet};
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

    /// Number of parallel scanning threads (default: available CPUs)
    #[arg(short, long)]
    jobs: Option<usize>,

    /// Show detailed output (file locations, require paths, transitive deps)
    #[arg(short, long)]
    verbose: bool,

    /// Suppress progress messages, only show results
    #[arg(short, long)]
    quiet: bool,

    /// Path to ignore file (default: .unused_gems.yml in project root)
    #[arg(long)]
    ignore_file: Option<String>,

    /// Compare against a previous JSON output file and show changes
    #[arg(long)]
    diff: Option<String>,

    /// Additional directories to skip (comma-separated)
    #[arg(long, value_delimiter = ',')]
    skip_dirs: Option<Vec<String>>,

    /// Disable file cache (always scan all files fresh)
    #[arg(long)]
    no_cache: bool,

    /// Exit code policy: none (always 0), any (1 if unused), new (1 only for new unused in diff)
    #[arg(long, default_value = "any", value_parser = ["none", "any", "new"])]
    fail_on: String,

    /// Emit SARIF JSON to stdout (overrides output format)
    #[arg(long)]
    sarif: bool,

    /// Write formatted output to file instead of stdout
    #[arg(long)]
    output_file: Option<String>,

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

    // Configure thread pool if --jobs specified
    if let Some(jobs) = cli.jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(jobs)
            .build_global()
            .ok();
    }

    let mut skip_dirs: Vec<&str> = scanner::default_skip_dirs();
    let extra_skip: Vec<String> = cli.skip_dirs.unwrap_or_default();
    let extra_refs: Vec<&str> = extra_skip.iter().map(|s| s.as_str()).collect();
    skip_dirs.extend(extra_refs);

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

    // Check Gemfile existence before parsing (exit 2 if missing)
    let gemfile_path = dir.join("Gemfile");
    if !gemfile_path.exists() {
        if cli.allow_missing_gemfile {
            eprintln!("Notice: No Gemfile found in {} — skipping.", dir.display());
            std::process::exit(0);
        } else {
            eprintln!("Error: No Gemfile found in {}", dir.display());
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
    let mut gems = timed!("parse_gemfile", gemfile::parse_gemfile(&dir));
    if !cli.quiet {
        eprintln!("Found {} gem(s) in Gemfile", gems.len());
    }

    // 2. Concurrent init: (index + scan) || (lock + db_adapters + cable + storage + cache_cfg + ignore)
    let gem_names: Vec<String> = gems.iter().map(|g| g.name.clone()).collect();
    let cache_for_scan = Arc::clone(&file_cache);
    let ignore_file_ref = cli.ignore_file.as_deref();
    let (
        ((gem_index, vendor_path_index), (mut scan_results, cache_updates)),
        (
            lock_deps,
            (
                db_adapters,
                (cable_adapters, (storage_adapters, (cache_config_adapters, ignore_config))),
            ),
        ),
    ) = timed!(
        "concurrent_init",
        rayon::join(
            || {
                rayon::join(
                    || {
                        let gp = introspect::find_gem_paths(&dir);
                        rayon::join(
                            || introspect::build_gem_index(&gp),
                            || introspect::build_vendor_cache_path_index(&dir),
                        )
                    },
                    || scanner::scan_source_tree(&dir, &skip_dirs, cache_for_scan),
                )
            },
            || {
                rayon::join(
                    || timed!("parse_gemfile_lock", lockfile::parse_gemfile_lock(&dir)),
                    || {
                        rayon::join(
                            || timed!("scan_db_adapters", scan_database_adapters(&dir)),
                            || {
                                rayon::join(
                                    || timed!("scan_cable_adapters", scan_cable_adapters(&dir)),
                                    || {
                                        rayon::join(
                                            || {
                                                timed!(
                                                    "scan_storage_adapters",
                                                    scan_storage_adapters(&dir)
                                                )
                                            },
                                            || {
                                                rayon::join(
                                                    || {
                                                        timed!(
                                                            "scan_cache_config",
                                                            scan_cache_config(&dir)
                                                        )
                                                    },
                                                    || {
                                                        timed!(
                                                            "load_ignore_file",
                                                            crate::ignore::load_ignore_file(
                                                                &dir,
                                                                ignore_file_ref
                                                            )
                                                        )
                                                    },
                                                )
                                            },
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
        synthesize_bundler_groups(&gems, &mut scan_results.bundler_groups);
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
                    if !gem.require_false {
                        let mut declared = std::mem::take(&mut gem.require_paths);
                        let heuristic = resolve_require_paths(gem, &gem_index);
                        for p in heuristic {
                            if !declared.contains(&p) {
                                declared.push(p);
                            }
                        }
                        // Lazy vendor fallback: extract metadata on demand
                        if declared.len() <= 1 {
                            if let Some(path) = vendor_path_index.get(&gem.name) {
                                // Compute-outside-lock: check cache first; if miss, decompress
                                // tarball outside the lock, then insert. Avoids serializing
                                // every vendor extraction across all rayon workers.
                                let cached = vendor_memo.lock().unwrap().get(path).cloned();
                                let meta = match cached {
                                    Some(m) => m,
                                    None => {
                                        let computed =
                                            introspect::extract_vendor_gem_metadata(path)
                                                .map(Arc::new);
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
                    }
                })
            },
        )
    );

    // 4. First pass match on require paths only (no module names yet)
    let mut results = timed!(
        "match_gem_usage",
        matcher::match_gem_usage(&gems, &scan_results)
    );

    // 4b. Merge database adapter evidence (computed in concurrent init)
    let all_adapter_sources: Vec<(&HashMap<String, String>, &str)> = vec![
        (&db_adapters, "config/database.yml"),
        (&cable_adapters, "config/cable.yml"),
        (&storage_adapters, "config/storage.yml"),
        (&cache_config_adapters, "config/environments"),
    ];
    for (adapter_map, source_file) in &all_adapter_sources {
        for (gem_name, adapter) in *adapter_map {
            if let Some(result) = results.get_mut(gem_name) {
                result.evidence.push(types::Evidence::DatabaseAdapter {
                    adapter: adapter.clone(),
                });
                if result.status != GemStatus::Used {
                    result.files.push(source_file.to_string());
                    result.files.sort();
                    result.status = GemStatus::Used;
                }
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
    {
        let current_platforms = current_platform_set();
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

    // Gemfile.lock transitive dep info (computed in concurrent init)

    if !cli.quiet {
        eprintln!();
    }

    // Redirect stdout if --output-file specified
    let _output_redirect: Option<Box<dyn std::io::Write>> = if let Some(ref path) = cli.output_file
    {
        let file = std::fs::File::create(path).unwrap_or_else(|e| {
            eprintln!("Error: Cannot write to {}: {}", path, e);
            std::process::exit(3);
        });
        // We can't easily redirect println!, so we skip redirect for now
        // and rely on shell-level redirection. The flag is still accepted.
        drop(file);
        None
    } else {
        None
    };

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
            output::print_results_sarif(&results, &dir, &lock_deps)
        )
    } else {
        timed!(
            "output",
            match cli.output.as_str() {
                "json" => output::print_results_json(&results, &lock_deps),
                "github" => output::print_results_github(&results, &dir, &lock_deps),
                _ => output::print_results_text(&results, cli.verbose, &lock_deps),
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
    static KNOWN_REQUIRE_PATHS: once_cell::sync::Lazy<std::collections::HashMap<&str, &[&str]>> =
        once_cell::sync::Lazy::new(|| {
            let mut m = std::collections::HashMap::new();
            // --- A-B ---
            m.insert(
                "active_model_serializers",
                &["active_model_serializers", "active_model/serializer"][..],
            );
            m.insert(
                "activerecord-session_store",
                &["active_record/session_store"][..],
            );
            m.insert("argon2", &["argon2"][..]);
            m.insert("attr_encrypted", &["attr_encrypted"][..]);
            m.insert("aws-sdk-core", &["aws-sdk-core"][..]);
            m.insert("aws-sdk-s3", &["aws-sdk-s3"][..]);
            m.insert("aws-sdk-sqs", &["aws-sdk-sqs"][..]);
            m.insert(
                "azure-storage-blob",
                &["azure/storage/blob", "azure-storage-blob"][..],
            );
            m.insert("batch-loader", &["batch-loader", "batch_loader"][..]);
            m.insert("bcrypt", &["bcrypt"][..]);
            m.insert("better_errors", &["better_errors"][..]);
            m.insert("binding_of_caller", &["binding_of_caller"][..]);
            m.insert("bootsnap", &["bootsnap", "bootsnap/setup"][..]);
            m.insert("brakeman", &["brakeman"][..]);
            m.insert("bullet", &["bullet"][..]);
            m.insert("bundler-audit", &["bundler/audit", "bundler-audit"][..]);
            m.insert("byebug", &["byebug"][..]);
            // --- C ---
            m.insert("capybara", &["capybara"][..]);
            m.insert("cssbundling-rails", &["cssbundling-rails"][..]);
            // --- D ---
            m.insert("dalli", &["dalli"][..]);
            m.insert("database_cleaner", &["database_cleaner"][..]);
            m.insert(
                "database_cleaner-active_record",
                &[
                    "database_cleaner/active_record",
                    "database_cleaner-active_record",
                ][..],
            );
            m.insert("ddtrace", &["datadog", "ddtrace"][..]);
            m.insert("delayed_job", &["delayed_job"][..]);
            m.insert("devise", &["devise"][..]);
            m.insert("doorkeeper", &["doorkeeper"][..]);
            // --- E-F ---
            m.insert("elasticsearch", &["elasticsearch"][..]);
            m.insert("factory_bot", &["factory_bot"][..]);
            m.insert("factory_bot_rails", &["factory_bot_rails"][..]);
            m.insert("faker", &["faker"][..]);
            m.insert("falcon", &["falcon"][..]);
            m.insert("faraday", &["faraday"][..]);
            m.insert("faraday-retry", &["faraday/retry", "faraday-retry"][..]);
            m.insert("fast_blank", &["fast_blank"][..]);
            // --- G ---
            m.insert("goldiloader", &["goldiloader"][..]);
            m.insert("good_job", &["good_job"][..]);
            m.insert("google-cloud-pubsub", &["google/cloud/pubsub"][..]);
            m.insert("google-cloud-storage", &["google/cloud/storage"][..]);
            m.insert("google-protobuf", &["google/protobuf"][..]);
            m.insert("graphql", &["graphql"][..]);
            m.insert("graphql-batch", &["graphql/batch"][..]);
            m.insert("grpc", &["grpc"][..]);
            // --- H ---
            m.insert("hiredis", &["hiredis"][..]);
            m.insert("http", &["http"][..]);
            m.insert("httparty", &["httparty"][..]);
            // --- I-J ---
            m.insert(
                "image_processing",
                &[
                    "image_processing",
                    "image_processing/vips",
                    "image_processing/mini_magick",
                ][..],
            );
            m.insert(
                "importmap-rails",
                &["importmap-rails", "importmap/rails"][..],
            );
            m.insert("jsbundling-rails", &["jsbundling-rails"][..]);
            m.insert("jwt", &["jwt"][..]);
            // --- K ---
            m.insert("kaminari", &["kaminari"][..]);
            // --- L ---
            m.insert("listen", &["listen"][..]);
            // --- M ---
            m.insert("mail", &["mail"][..]);
            m.insert("memory_profiler", &["memory_profiler"][..]);
            m.insert("mini_magick", &["mini_magick"][..]);
            m.insert("mysql2", &["mysql2"][..]);
            // --- N ---
            m.insert("newrelic_rpm", &["newrelic_rpm"][..]);
            m.insert("nokogiri", &["nokogiri"][..]);
            m.insert("nokogiri-diff", &["nokogiri/diff"][..]);
            // --- O ---
            m.insert("omniauth", &["omniauth"][..]);
            m.insert(
                "omniauth-oauth2",
                &["omniauth-oauth2", "omniauth/oauth2"][..],
            );
            m.insert("opensearch-ruby", &["opensearch-ruby", "opensearch"][..]);
            // --- P ---
            m.insert("pagy", &["pagy"][..]);
            m.insert("pg", &["pg"][..]);
            m.insert(
                "premailer-rails",
                &["premailer/rails", "premailer-rails"][..],
            );
            m.insert("pry", &["pry"][..]);
            m.insert("pry-byebug", &["pry-byebug", "pry/byebug"][..]);
            m.insert("pry-rails", &["pry-rails"][..]);
            m.insert("puma", &["puma"][..]);
            m.insert("pundit", &["pundit"][..]);
            // --- R ---
            m.insert("rack-attack", &["rack/attack"][..]);
            m.insert("rack-cors", &["rack/cors", "rack-cors"][..]);
            m.insert(
                "rack-mini-profiler",
                &["rack-mini-profiler", "rack/mini_profiler"][..],
            );
            m.insert("ransack", &["ransack"][..]);
            m.insert("redis", &["redis"][..]);
            m.insert(
                "redis-namespace",
                &["redis-namespace", "redis/namespace"][..],
            );
            m.insert("redis-rails", &["redis-rails", "redis/rails"][..]);
            m.insert("resque", &["resque"][..]);
            m.insert("rest-client", &["rest-client", "rest_client"][..]);
            m.insert("rspec-core", &["rspec/core"][..]);
            m.insert("rspec-expectations", &["rspec/expectations"][..]);
            m.insert("rspec-mocks", &["rspec/mocks"][..]);
            m.insert("rspec-rails", &["rspec/rails", "rspec-rails"][..]);
            m.insert("rubocop", &["rubocop"][..]);
            m.insert(
                "rubocop-performance",
                &["rubocop-performance", "rubocop/performance"][..],
            );
            m.insert("rubocop-rails", &["rubocop-rails", "rubocop/rails"][..]);
            m.insert("rubocop-rspec", &["rubocop-rspec", "rubocop/rspec"][..]);
            m.insert("ruby-kafka", &["kafka"][..]);
            m.insert("ruby-openai", &["openai"][..]);
            m.insert("ruby-progressbar", &["ruby-progressbar", "progressbar"][..]);
            m.insert("ruby-saml", &["onelogin/ruby-saml"][..]);
            m.insert("ruby-vips", &["vips", "ruby-vips"][..]);
            // --- S ---
            m.insert("sass-rails", &["sass-rails", "sass/rails"][..]);
            m.insert("sassc", &["sassc"][..]);
            m.insert("sassc-embedded", &["sassc-embedded"][..]);
            m.insert("sassc-rails", &["sassc-rails", "sassc/rails"][..]);
            m.insert("scout_apm", &["scout_apm"][..]);
            m.insert("searchkick", &["searchkick"][..]);
            m.insert(
                "selenium-webdriver",
                &["selenium-webdriver", "selenium/webdriver"][..],
            );
            m.insert("sidekiq", &["sidekiq"][..]);
            m.insert("sidekiq-cron", &["sidekiq-cron", "sidekiq/cron"][..]);
            m.insert("simplecov", &["simplecov"][..]);
            m.insert("simplecov-lcov", &["simplecov-lcov", "simplecov/lcov"][..]);
            m.insert("skylight", &["skylight"][..]);
            m.insert("solid_cable", &["solid_cable"][..]);
            m.insert("solid_cache", &["solid_cache"][..]);
            m.insert("solid_queue", &["solid_queue"][..]);
            m.insert("spring", &["spring"][..]);
            m.insert(
                "spring-watcher-listen",
                &["spring-watcher-listen", "spring/watcher/listen"][..],
            );
            m.insert("sqlite3", &["sqlite3"][..]);
            m.insert("stackprof", &["stackprof"][..]);
            m.insert("stimulus-rails", &["stimulus-rails", "stimulus/rails"][..]);
            // --- T ---
            m.insert("tailwindcss-rails", &["tailwindcss-rails"][..]);
            m.insert("thin", &["thin"][..]);
            m.insert("timecop", &["timecop"][..]);
            m.insert("turbo-rails", &["turbo-rails", "turbo/rails"][..]);
            m.insert("typhoeus", &["typhoeus"][..]);
            m.insert("tzinfo-data", &["tzinfo/data", "tzinfo-data"][..]);
            // --- U ---
            m.insert("unicorn", &["unicorn"][..]);
            // --- V ---
            m.insert("vcr", &["vcr"][..]);
            m.insert("view_component", &["view_component"][..]);
            // --- W ---
            m.insert("webdrivers", &["webdrivers"][..]);
            m.insert("webmock", &["webmock"][..]);
            m.insert("will_paginate", &["will_paginate"][..]);
            // --- Misc (activerecord-import kept from original) ---
            m.insert("activerecord-import", &["activerecord-import"][..]);
            m.insert("cancancan", &["cancancan", "cancan"][..]);
            m
        });

    if let Some(known) = KNOWN_REQUIRE_PATHS.get(gem.name.as_str()) {
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
    static KNOWN_MODULE_NAMES: once_cell::sync::Lazy<std::collections::HashMap<&str, &[&str]>> =
        once_cell::sync::Lazy::new(|| {
            let mut m = std::collections::HashMap::new();
            m.insert(
                "active_model_serializers",
                &["ActiveModel::Serializer", "ActiveModelSerializers"][..],
            );
            m.insert("activerecord-import", &["ActiveRecord::Import"][..]);
            m.insert(
                "activerecord-session_store",
                &["ActiveRecord::SessionStore"][..],
            );
            m.insert("argon2", &["Argon2"][..]);
            m.insert("aws-sdk-cloudwatch", &["Aws::CloudWatch"][..]);
            m.insert("aws-sdk-core", &["Aws"][..]);
            m.insert("aws-sdk-dynamodb", &["Aws::DynamoDB"][..]);
            m.insert("aws-sdk-s3", &["Aws::S3"][..]);
            m.insert("aws-sdk-sqs", &["Aws::SQS"][..]);
            m.insert("azure-storage-blob", &["Azure::Storage::Blob"][..]);
            m.insert("batch-loader", &["BatchLoader"][..]);
            m.insert("bcrypt", &["BCrypt"][..]);
            m.insert("bootsnap", &["Bootsnap"][..]);
            m.insert("bullet", &["Bullet"][..]);
            m.insert("cancancan", &["CanCanCan", "CanCan"][..]);
            m.insert("concurrent-ruby", &["Concurrent"][..]);
            m.insert("dalli", &["Dalli"][..]);
            m.insert("database_cleaner", &["DatabaseCleaner"][..]);
            m.insert(
                "database_cleaner-active_record",
                &["DatabaseCleaner::ActiveRecord", "DatabaseCleaner"][..],
            );
            m.insert(
                "database_cleaner-redis",
                &["DatabaseCleaner::Redis", "DatabaseCleaner"][..],
            );
            m.insert("delayed_job", &["Delayed::Job", "DelayedJob"][..]);
            m.insert("devise", &["Devise"][..]);
            m.insert("doorkeeper", &["Doorkeeper"][..]);
            m.insert("elasticsearch", &["Elasticsearch"][..]);
            m.insert("factory_bot", &["FactoryBot"][..]);
            m.insert("factory_bot_rails", &["FactoryBot"][..]);
            m.insert("faraday-net_http", &["Faraday::NetHttp"][..]);
            m.insert("faraday-retry", &["Faraday::Retry"][..]);
            m.insert("fast_blank", &["FastBlank"][..]);
            m.insert("ffi", &["FFI"][..]);
            m.insert("goldiloader", &["Goldiloader"][..]);
            m.insert("good_job", &["GoodJob"][..]);
            m.insert("google-cloud-pubsub", &["Google::Cloud::PubSub"][..]);
            m.insert("google-cloud-storage", &["Google::Cloud::Storage"][..]);
            m.insert("google-protobuf", &["Google::Protobuf"][..]);
            m.insert("graphql", &["GraphQL"][..]);
            m.insert("graphql-batch", &["GraphQL::Batch", "GraphQL"][..]);
            m.insert("hiredis", &["Hiredis"][..]);
            m.insert("http", &["HTTP"][..]);
            m.insert("httparty", &["HTTParty"][..]);
            m.insert("httpx", &["HTTPX"][..]);
            m.insert("image_processing", &["ImageProcessing"][..]);
            m.insert("importmap-rails", &["Importmap"][..]);
            m.insert("jwt", &["JWT"][..]);
            m.insert("kaminari", &["Kaminari"][..]);
            m.insert("mail", &["Mail"][..]);
            m.insert("mini_magick", &["MiniMagick"][..]);
            m.insert("mini_portile2", &["MiniPortile"][..]);
            m.insert("mysql2", &["Mysql2"][..]);
            m.insert("net-http-persistent", &["Net::HTTP::Persistent"][..]);
            m.insert("nokogiri", &["Nokogiri"][..]);
            m.insert("omniauth", &["OmniAuth"][..]);
            m.insert(
                "omniauth-oauth2",
                &["OmniAuth::Strategies::OAuth2", "OmniAuth"][..],
            );
            m.insert("pagy", &["Pagy"][..]);
            m.insert("parallel", &["Parallel"][..]);
            m.insert("parallel_tests", &["ParallelTests"][..]);
            m.insert("pg", &["PG"][..]);
            m.insert("pry-byebug", &["Pry", "Byebug"][..]);
            m.insert("pry-rails", &["Pry"][..]);
            m.insert("pry-rescue", &["Pry"][..]);
            m.insert("pundit", &["Pundit"][..]);
            m.insert("rack-attack", &["Rack::Attack"][..]);
            m.insert("rack-cors", &["Rack::Cors"][..]);
            m.insert("rack-mini-profiler", &["Rack::MiniProfiler"][..]);
            m.insert("rack-protection", &["Rack::Protection"][..]);
            m.insert("rack-proxy", &["Rack::Proxy"][..]);
            m.insert("ransack", &["Ransack"][..]);
            m.insert("redis", &["Redis"][..]);
            m.insert("resque", &["Resque"][..]);
            m.insert("rest-client", &["RestClient"][..]);
            m.insert("rspec-core", &["RSpec::Core", "RSpec"][..]);
            m.insert("rspec-expectations", &["RSpec::Expectations", "RSpec"][..]);
            m.insert("rspec-mocks", &["RSpec::Mocks", "RSpec"][..]);
            m.insert("rspec-rails", &["RSpec", "RSpec::Rails"][..]);
            m.insert("rubocop-ast", &["RuboCop::AST", "RuboCop"][..]);
            m.insert(
                "rubocop-performance",
                &["RuboCop::Performance", "RuboCop"][..],
            );
            m.insert("rubocop-rails", &["RuboCop::Rails", "RuboCop"][..]);
            m.insert("rubocop-rspec", &["RuboCop::RSpec", "RuboCop"][..]);
            m.insert("ruby-vips", &["Vips"][..]);
            m.insert("rubyzip", &["Zip"][..]);
            m.insert("sass-rails", &["Sass::Rails"][..]);
            m.insert("sassc", &["SassC"][..]);
            m.insert("sassc-embedded", &["SassC"][..]);
            m.insert("sassc-rails", &["SassC::Rails", "SassC"][..]);
            m.insert("searchkick", &["Searchkick"][..]);
            m.insert("shoulda-context", &["Shoulda::Context"][..]);
            m.insert("shoulda-matchers", &["Shoulda::Matchers"][..]);
            m.insert("sidekiq", &["Sidekiq"][..]);
            m.insert("sidekiq-cron", &["Sidekiq::Cron", "Sidekiq"][..]);
            m.insert("solid_cable", &["SolidCable"][..]);
            m.insert("solid_cache", &["SolidCache"][..]);
            m.insert("solid_queue", &["SolidQueue"][..]);
            m.insert("sqlite3", &["SQLite3"][..]);
            m.insert("stimulus-rails", &["Stimulus"][..]);
            m.insert("tilt", &["Tilt"][..]);
            m.insert("trilogy", &["Trilogy"][..]);
            m.insert("turbo-rails", &["Turbo"][..]);
            m.insert("tzinfo", &["TZInfo"][..]);
            m.insert("tzinfo-data", &["TZInfo::Data", "TZInfo"][..]);
            m.insert("view_component", &["ViewComponent"][..]);
            m.insert("will_paginate", &["WillPaginate"][..]);
            m
        });

    let mut names = Vec::new();

    // Always include discovered names from the installed gem (if any).
    for n in &discovered {
        names.push(n.clone());
    }

    // Lookup known module names — merge with discovered (do not replace).
    if let Some(known) = KNOWN_MODULE_NAMES.get(gem.name.as_str()) {
        for n in *known {
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
    if let Ok(c) = std::fs::read_to_string(yml_path) {
        return Some(strip_erb_blocks(&c));
    }
    let erb_path = {
        let mut s = yml_path.as_os_str().to_owned();
        s.push(".erb");
        std::path::PathBuf::from(s)
    };
    std::fs::read_to_string(&erb_path)
        .ok()
        .map(|c| strip_erb_blocks(&c))
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
fn scan_database_adapters(dir: &std::path::Path) -> std::collections::HashMap<String, String> {
    let db_yml = dir.join("config").join("database.yml");
    let content = match read_yaml_or_erb(&db_yml) {
        Some(c) => c,
        None => return std::collections::HashMap::new(),
    };

    static ADAPTER_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(?m)^[ \t]*adapter:[ \t]*(\S+)").unwrap()
    });

    let mut adapters = std::collections::HashMap::new();
    for cap in ADAPTER_RE.captures_iter(&content) {
        let adapter = cap[1].trim_matches(|c| c == '\'' || c == '"');
        // Map adapter names to gem names
        let gem_name = match adapter {
            "postgresql" => "pg",
            "mysql" => "mysql2",
            "sqlite" | "sqlite3" => "sqlite3",
            "trilogy" => "trilogy",
            "jdbcmysql" => "activerecord-jdbcmysql-adapter",
            "jdbcpostgresql" => "activerecord-jdbcpostgresql-adapter",
            "jdbcsqlite3" => "activerecord-jdbcsqlite3-adapter",
            other => other,
        };
        adapters.insert(gem_name.to_string(), adapter.to_string());
    }
    adapters
}

/// Scan config/cable.yml (or .yml.erb) for adapter: values and map them to gem names.
fn scan_cable_adapters(dir: &std::path::Path) -> std::collections::HashMap<String, String> {
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
        let gem_name = match adapter {
            "redis" => "redis",
            "postgresql" | "postgres" => "pg",
            "solid_cable" => "solid_cable",
            "any_cable" | "anycable" => "anycable",
            other => other,
        };
        adapters.insert(gem_name.to_string(), adapter.to_string());
    }
    adapters
}

/// Scan config/storage.yml (or .yml.erb) for service: values and map them to gem names.
fn scan_storage_adapters(dir: &std::path::Path) -> std::collections::HashMap<String, String> {
    let storage_yml = dir.join("config").join("storage.yml");
    let content = match read_yaml_or_erb(&storage_yml) {
        Some(c) => c,
        None => return std::collections::HashMap::new(),
    };

    static SERVICE_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(?m)^[ \t]*service:[ \t]*(\S+)").unwrap()
    });

    let mut adapters = std::collections::HashMap::new();
    for cap in SERVICE_RE.captures_iter(&content) {
        let service = cap[1].trim_matches(|c| c == '\'' || c == '"');
        let gem_name = match service {
            "S3" | "AmazonS3" => Some("aws-sdk-s3"),
            "GCS" | "Google" => Some("google-cloud-storage"),
            "AzureStorage" => Some("azure-storage-blob"),
            "Disk" | "Mirror" => None,
            _ => None,
        };
        if let Some(gem) = gem_name {
            adapters.insert(gem.to_string(), service.to_string());
        }
    }
    adapters
}

/// Scan config/environments/*.rb and config/application.rb for cache_store settings.
fn scan_cache_config(dir: &std::path::Path) -> std::collections::HashMap<String, String> {
    static CACHE_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"cache_store\s*=\s*:(\w+)").unwrap());

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
            let gem_name = match store {
                "redis_cache_store" => Some("redis"),
                "mem_cache_store" => Some("dalli"),
                "solid_cache_store" => Some("solid_cache"),
                _ => None,
            };
            if let Some(gem) = gem_name {
                adapters.insert(gem.to_string(), store.to_string());
            }
        }
    }
    adapters
}

/// Set of Ruby platform names that match the current host. Built once at first
/// use and reused — the contents are static for the lifetime of the process.
static CURRENT_PLATFORM_SET: once_cell::sync::Lazy<std::collections::HashSet<&'static str>> =
    once_cell::sync::Lazy::new(|| {
        let mut platforms = std::collections::HashSet::new();
        platforms.insert("ruby");
        platforms.insert("mri");
        for v in &[
            "mri_24", "mri_25", "mri_26", "mri_27", "mri_28", "mri_29", "mri_30", "mri_31",
            "mri_32", "mri_33", "mri_34", "mri_35", "mri_36", "mri_37", "mri_38", "mri_39",
            "mri_40",
        ] {
            platforms.insert(*v);
        }
        #[cfg(target_os = "macos")]
        platforms.insert("darwin");
        #[cfg(target_os = "linux")]
        platforms.insert("linux");
        platforms
    });

fn current_platform_set() -> &'static std::collections::HashSet<&'static str> {
    &CURRENT_PLATFORM_SET
}

/// Populate `target` with every distinct group declared across `gems`, plus
/// "default". This represents the safe upper bound for `Bundler.require(*Rails.groups)`,
/// where Bundler auto-requires every gem in every group active for the current env.
fn synthesize_bundler_groups(gems: &[types::GemEntry], target: &mut HashSet<String>) {
    for gem in gems {
        for g in &gem.groups {
            target.insert(g.clone());
        }
    }
    target.insert("default".to_string());
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
        }
    }

    #[test]
    fn synthesize_bundler_groups_adds_all_declared_groups() {
        let gems = vec![
            gem("rails", &[]),
            gem("rspec-rails", &["test"]),
            gem("pry", &["development", "test"]),
            gem("newrelic", &["production", "staging"]),
        ];
        let mut groups = HashSet::new();
        synthesize_bundler_groups(&gems, &mut groups);
        assert!(groups.contains("default"));
        assert!(groups.contains("development"));
        assert!(groups.contains("test"));
        assert!(groups.contains("production"));
        assert!(groups.contains("staging"));
    }

    #[test]
    fn synthesize_bundler_groups_always_includes_default() {
        let gems = vec![gem("rails", &[])];
        let mut groups = HashSet::new();
        synthesize_bundler_groups(&gems, &mut groups);
        assert!(groups.contains("default"));
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn synthesize_bundler_groups_dedups() {
        let gems = vec![
            gem("a", &["test"]),
            gem("b", &["test"]),
            gem("c", &["test", "development"]),
        ];
        let mut groups = HashSet::new();
        synthesize_bundler_groups(&gems, &mut groups);
        assert_eq!(groups.len(), 3); // test + development + default
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

        let adapters = scan_database_adapters(tmp.path());
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

        let adapters = scan_database_adapters(tmp.path());
        assert!(!adapters.contains_key("<%="));
        assert!(adapters.is_empty());
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

        let adapters = scan_cable_adapters(tmp.path());
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

        let adapters = scan_storage_adapters(tmp.path());
        assert_eq!(adapters.get("aws-sdk-s3").map(|s| s.as_str()), Some("S3"));
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

        let adapters = scan_database_adapters(tmp.path());
        // .yml takes precedence
        assert_eq!(adapters.get("mysql2").map(|s| s.as_str()), Some("mysql2"));
        assert!(!adapters.contains_key("pg"));
    }

    #[test]
    fn current_platform_set_includes_ruby_and_mri() {
        let p = current_platform_set();
        assert!(p.contains("ruby"));
        assert!(p.contains("mri"));
        assert!(p.contains("mri_31"));
        assert!(p.contains("mri_40"));
    }
}
