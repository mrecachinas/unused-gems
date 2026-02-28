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

use std::collections::HashSet;
use std::path::Path;

use clap::Parser;
use rayon::prelude::*;

use types::GemStatus;

#[derive(Parser)]
#[command(name = "unused-gems")]
#[command(about = "High-performance scanner for unused gems in Ruby projects")]
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

    /// Directory to scan (default: current directory)
    #[arg(default_value = ".")]
    directory: String,
}

fn main() {
    let cli = Cli::parse();

    let dir = Path::new(&cli.directory).canonicalize().unwrap_or_else(|_| {
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
                eprintln!("  {:30} {:>6.2}ms", $label, _start.elapsed().as_secs_f64() * 1000.0);
            }
            _result
        }};
    }

    if !cli.quiet {
        eprintln!("Scanning for unused gems in {}...", dir.display());
        eprintln!();
    }

    // 1. Parse Gemfile
    let mut gems = timed!("parse_gemfile", gemfile::parse_gemfile(&dir));
    if !cli.quiet {
        eprintln!("Found {} gem(s) in Gemfile", gems.len());
    }

    // 2. Build gem index AND scan source tree concurrently
    let gem_names: Vec<String> = gems.iter().map(|g| g.name.clone()).collect();
    let ((_gem_paths, gem_index), scan_results) = timed!("index+scan", rayon::join(
        || {
            let gp = introspect::find_gem_paths(&dir);
            let gi = introspect::build_gem_index(&gp);
            (gp, gi)
        },
        || scanner::scan_source_tree(&dir, &skip_dirs),
    ));

    // 2b. Script scanning (needs gem_index for exe→gem mapping)
    let script_refs = timed!("scan_scripts", scripts::scan_scripts_for_gem_usage(&dir, &gem_names, &gem_index));

    // 3. Resolve require_paths (fast: just readdir of lib/)
    timed!("resolve_require_paths", gems.par_iter_mut().for_each(|gem| {
        if gem.require_paths.is_empty() && !gem.require_false {
            gem.require_paths = resolve_require_paths(gem, &gem_index);
        }
    }));

    // 4. First pass match on require paths only (no module names yet)
    let mut results = timed!("match_gem_usage", matcher::match_gem_usage(&gems, &scan_results));

    // 4b. Merge database adapter evidence
    let db_adapters = scan_database_adapters(&dir);
    for (gem_name, adapter) in &db_adapters {
        if let Some(result) = results.get_mut(gem_name) {
            result.evidence.push(types::Evidence::DatabaseAdapter { adapter: adapter.clone() });
            if result.status != GemStatus::Used {
                result.files.push("config/database.yml".to_string());
                result.files.sort();
                result.status = GemStatus::Used;
            }
        }
    }

    // 5. Merge script/CI evidence before lazy introspection check
    for (gem_name, files) in &script_refs {
        if let Some(result) = results.get_mut(gem_name) {
            let mut all_files: HashSet<String> = result.files.iter().cloned().collect();
            for f in files {
                result.evidence.push(types::Evidence::Script { file: f.clone() });
                all_files.insert(f.clone());
            }
            result.files = all_files.into_iter().collect();
            result.files.sort();
            if !result.files.is_empty() {
                result.status = GemStatus::Used;
            }
        }
    }

    // 6. Lazy introspection: only resolve module names for gems not yet matched
    let unresolved: Vec<usize> = gems
        .iter()
        .enumerate()
        .filter(|(_, gem)| {
            gem.module_names.is_empty()
                && results.get(&gem.name).map_or(true, |r| r.status != GemStatus::Used)
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
        // Re-match only the unresolved gems with module names
        timed!("rematch_unresolved", {
            let updated = matcher::match_gem_usage(&gems, &scan_results);
            for (name, new_result) in updated {
                if let Some(existing) = results.get(&name) {
                    if existing.status != GemStatus::Used {
                        results.insert(name, new_result);
                    }
                }
            }
        });
        // Re-merge script refs for newly resolved gems
        for (gem_name, files) in &script_refs {
            if let Some(result) = results.get_mut(gem_name) {
                if result.status != GemStatus::Used && !files.is_empty() {
                    let mut all_files: HashSet<String> = result.files.iter().cloned().collect();
                    for f in files {
                        result.evidence.push(types::Evidence::Script { file: f.clone() });
                        all_files.insert(f.clone());
                    }
                    result.files = all_files.into_iter().collect();
                    result.files.sort();
                    result.status = GemStatus::Used;
                }
            }
        }
    }

    if !cli.quiet {
        eprintln!(
            "Scanned source tree ({} unique requires found)",
            scan_results.requires.len()
        );
    }

    // 7. Load ignore file and apply
    let ignore_config = crate::ignore::load_ignore_file(&dir, cli.ignore_file.as_deref());
    for (name, result) in &mut results {
        if ignore_config.ignored.contains(name) {
            result.status = GemStatus::Ignored;
        }
    }
    if let Some(ref path) = ignore_config.path {
        if !cli.quiet {
            let ignored_count = results.values().filter(|v| v.status == GemStatus::Ignored).count();
            if ignored_count > 0 {
                eprintln!("Loaded {} ({} ignored)", path, ignored_count);
            }
        }
    }

    // 7. Parse Gemfile.lock for transitive dep info
    let lock_deps = lockfile::parse_gemfile_lock(&dir);

    if !cli.quiet {
        eprintln!();
    }

    // 8. Diff mode: compare and exit
    if let Some(ref baseline_path) = cli.diff {
        let unused_count = diff::print_diff(&results, baseline_path);
        std::process::exit(if unused_count == 0 { 0 } else { 1 });
    }

    // 9. Output results
    let unused_count = match cli.output.as_str() {
        "json" => output::print_results_json(&results, &lock_deps),
        "github" => output::print_results_github(&results, &dir, &lock_deps),
        _ => output::print_results_text(&results, cli.verbose, &lock_deps),
    };

    std::process::exit(if unused_count == 0 { 0 } else { 1 });
}

/// Resolve require paths for a gem: check installed gem first, then use heuristics.
fn resolve_require_paths(gem: &types::GemEntry, gem_index: &std::collections::HashMap<String, std::path::PathBuf>) -> Vec<String> {
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
            m.insert("ruby-kafka", &["kafka"][..]);
            m.insert("google-protobuf", &["google/protobuf"][..]);
            m.insert("ddtrace", &["datadog", "ddtrace"][..]);
            m.insert("grpc", &["grpc"][..]);
            m.insert("sqlite3", &["sqlite3"][..]);
            m.insert("ruby-openai", &["openai"][..]);
            m.insert("ruby-progressbar", &["ruby-progressbar", "progressbar"][..]);
            m.insert("attr_encrypted", &["attr_encrypted"][..]);
            m.insert("google-cloud-storage", &["google/cloud/storage"][..]);
            m.insert("google-cloud-pubsub", &["google/cloud/pubsub"][..]);
            m.insert("ruby-saml", &["onelogin/ruby-saml"][..]);
            m.insert("activerecord-import", &["activerecord-import"][..]);
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
    let first_segment = gem.name.split('-').next().unwrap_or(&gem.name);
    if first_segment != gem.name {
        paths.push(first_segment.to_string());
    }
    paths.dedup();
    paths
}

/// Resolve module names for a gem: check installed gem first, then generate candidates.
fn resolve_module_names(gem: &types::GemEntry, gem_index: &std::collections::HashMap<String, std::path::PathBuf>) -> Vec<String> {
    let discovered = introspect::discover_gem_module_names(&gem.name, gem_index);
    if !discovered.is_empty() {
        return discovered;
    }

    let mut names = Vec::new();
    let pascal = gem
        .name
        .split(&['-', '_'][..])
        .map(capitalize)
        .collect::<String>();
    names.push(pascal);
    if gem.name.contains('-') {
        let ns = gem
            .name
            .split('-')
            .map(|p| p.split('_').map(capitalize).collect::<String>())
            .collect::<Vec<_>>()
            .join("::");
        names.push(ns);
    }
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

/// Scan config/database.yml for adapter: values and map them to gem names.
fn scan_database_adapters(dir: &std::path::Path) -> std::collections::HashMap<String, String> {
    let db_yml = dir.join("config").join("database.yml");
    let content = match std::fs::read_to_string(&db_yml) {
        Ok(c) => c,
        Err(_) => return std::collections::HashMap::new(),
    };

    static ADAPTER_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"(?m)^\s*adapter:\s*(\S+)").unwrap());

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
