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
- `scripts.rs` — scans scripts (Rakefile/`*.rake`, shell, Make, Docker, Procfile, CI YAML) for gem executables run in command position (`bundle exec`, `bin/`, first word after `&&`/`;`/`|`…), mapping executables to gems via installed gemspecs, a curated table and `-`/`_` normalisation

## Key Conventions

**Error handling:** No custom error types or error crates. Graceful degradation — missing gems, bad caches, etc. are logged via `eprintln!` and skipped. Fatal errors (missing Gemfile, invalid directory, unreadable/invalid `--diff` baseline) exit 2. Otherwise the exit code is 0 if no unused gems were found and 1 if some were (subject to `--fail-on`).

**Performance patterns:**
- Byte-level scanning with `memchr::memmem::Finder` instead of regex in hot paths (scanner.rs)
- `once_cell::sync::Lazy` for static regex and lookup table initialization
- `rayon` for parallelism: `rayon::join()` for concurrent stages, `par_iter()` for batch operations
- Bincode file cache keyed by mtime (nanosecond precision) + size to skip re-scanning unchanged files

**Comment filtering:** All scanners (Ruby and non-Ruby) filter `#` comment lines. Ruby files additionally filter `=begin`/`=end` blocks, heredoc content, and inline comments (e.g., `x = 1 # require 'foo'`). Script scanning strips comment lines before extracting commands; only command words (not arguments) are matched against gems.

**Require path resolution is multi-layered:** (1) introspect installed gem, (2) vendor/cache tarball metadata, (3) hard-coded `KNOWN_REQUIRE_PATHS` table in main.rs, (4) heuristic name transforms (hyphen→underscore, snake→PascalCase).

**Bundler.require handling:** When `Bundler.require(*Rails.groups)` is detected, only `:default` group gems are marked `BundlerRequired`. Dev/test-only gems without direct code references are reported as `Unused` unless `--rails-env` (comma-separated/repeatable) activates their groups.

**Gemspec parsing:** Root gemspecs are only parsed when a bare `gemspec` directive (without `path:`) is present in the Gemfile. Gemspecs with explicit `path:` args are parsed from those directories.

**Module organization:** Flat structure under `src/`, function-based design (no traits). Shared state uses `Arc<Mutex<>>` (e.g., file cache in scanner).

**Testing:** Integration tests in `tests/` validate byte scanner correctness against regex baselines. Unit tests in each module cover edge cases: inline comments, heredocs, YAML comments, bundler group handling, and gemspec directive detection.
