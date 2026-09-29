# Copilot Instructions for unused-gems

## Build & Test

```bash
cargo build                        # debug build
cargo build --release              # optimized build (LTO, single codegen unit)
cargo test                         # all tests
cargo test --test byte_scanner_test # single integration test
cargo install --path .             # install locally
```

Set `BENCH=1` to log per-step timing via the `timed!` macro in main.rs.

## Architecture

**Pipeline (main.rs orchestrates everything linearly):**

```
Parse Gemfile (ruby-prism AST)
  → [parallel] Build gem index + Scan source tree (rayon::join)
  → Scan storage adapters (config/storage.yml + attachment `service:` names from the walk)
  → Resolve require_paths per gem (parallel, multi-layer heuristics)
  → Match gem usage against scan results
  → Scan database adapters (config/database.yml)
  → Scan scripts (Makefile, Dockerfile, CI configs)
  → [lazy] Only unresolved gems: discover module names → re-match
  → Apply ignore file (.unused_gems.yml)
  → Parse Gemfile.lock (transitive dep counts)
  → Output (text/json/github) or diff against baseline
```

**Core types (types.rs):** `GemEntry` → enriched with require paths/module names → matched against `ScanResults` → produces `GemResult` with `Evidence` proving usage.

**Key modules:**
- `gemfile.rs` — ruby-prism AST parsing for Gemfile/gemspec
- `scanner.rs` — hand-rolled byte scanners (memchr/SIMD), no per-file regex
- `introspect.rs` — discovers require paths and module names from installed gems or vendor/cache tarballs
- `matcher.rs` — matches gems to scan results, produces evidence-backed results
- `scripts.rs` — scans non-Ruby files (Rake, shell, CI) for gem references

## Key Conventions

**Error handling:** No custom error types or error crates. Graceful degradation — missing gems, bad caches, etc. are logged via `eprintln!` and skipped. Only a missing Gemfile is fatal (`exit(1)`). Exit code is 0 if no unused gems found, 1 otherwise.

**Performance patterns:**
- Byte-level scanning with `memchr::memmem::Finder` instead of regex in hot paths (scanner.rs)
- `once_cell::sync::Lazy` for static regex and lookup table initialization
- `rayon` for parallelism: `rayon::join()` for concurrent stages, `par_iter()` for batch operations
- Bincode file cache keyed by mtime (nanosecond precision) + size to skip re-scanning unchanged files; entries hold everything a file contributes (requires, its unique constant names, bundler groups, attachment storage services), unvisited entries are pruned on save, and the file is written atomically (temp file + rename). Bump `CACHE_VERSION` when entry contents change
- Never re-read source files outside the walker: collect new per-file facts in `scan_file_bytes_with` and expose them via `FileResult`/`ScanResults`
- Per-file dedup (borrowed `&[u8]` sets) before touching thread-wide maps; lexer memo caches (`LexCache`) keep malformed input linear

**Comment filtering:** All scanners (Ruby and non-Ruby) filter `#` comment lines. Ruby files additionally filter `=begin`/`=end` blocks, heredoc content, string/regex/percent-literal interiors (keeping `#{}` code), the `__END__` data section, and inline comments (e.g., `x = 1 # require 'foo'`). Haml/Slim tag lines are parsed so only attributes, `=` output, script lines (with `,`/`\` continuations) and `#{}` are treated as Ruby. Script scanning strips comment lines before regex matching.

**RuboCop configs:** `.rubocop*.yml` / `.standard*.yml` are parsed with serde_yaml (`require`/`plugins` strings, lists or single-key maps; `inherit_gem` keys) with a line-based fallback for non-YAML (ERB) configs.

**Walking:** Symlinks are followed (loops are skipped). `--skip-dirs` entries match directory names, or paths relative to the scan root when they contain a separator (`app/legacy`).

**Require path resolution is multi-layered:** (1) introspect installed gem, (2) vendor/cache tarball metadata, (3) hard-coded `KNOWN_REQUIRE_PATHS` table in main.rs, (4) heuristic name transforms (hyphen→underscore, snake→PascalCase).

**Bundler.require handling:** When `Bundler.require(*Rails.groups)` is detected, only `:default` group gems are marked `BundlerRequired`. Dev/test-only gems without direct code references are reported as `Unused`.

**Gemspec parsing:** Root gemspecs are only parsed when a bare `gemspec` directive (without `path:`) is present in the Gemfile. Gemspecs with explicit `path:` args are parsed from those directories.

**Module organization:** Flat structure under `src/`, function-based design (no traits). Shared state uses `Arc<Mutex<>>` (e.g., file cache in scanner).

**Testing:** Integration tests in `tests/` validate byte scanner correctness against regex baselines. Unit tests in each module cover edge cases: inline comments, heredocs, YAML comments, bundler group handling, and gemspec directive detection.
