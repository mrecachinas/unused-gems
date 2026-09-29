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
  → Parse Gemfile.lock / gems.locked (transitive dep counts)
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

**Error handling:** No custom error types or error crates. Graceful degradation — missing gems, bad caches, etc. are logged via `eprintln!` and skipped. Only a missing Gemfile (or gems.rb) is fatal. Exit code is 0 if no unused gems found, 1 otherwise.

**Performance patterns:**
- Byte-level scanning with `memchr::memmem::Finder` instead of regex in hot paths (scanner.rs)
- `once_cell::sync::Lazy` for static regex and lookup table initialization
- `rayon` for parallelism: `rayon::join()` for concurrent stages, `par_iter()` for batch operations
- Bincode file cache keyed by mtime (nanosecond precision) + size to skip re-scanning unchanged files

**Comment filtering:** All scanners (Ruby and non-Ruby) filter `#` comment lines. Ruby files additionally filter `=begin`/`=end` blocks, heredoc content, and inline comments (e.g., `x = 1 # require 'foo'`). Script scanning strips comment lines before regex matching.

**Require path resolution is multi-layered:** (1) introspect installed gem, (2) vendor/cache tarball metadata, (3) hard-coded `KNOWN_REQUIRE_PATHS` table in main.rs, (4) heuristic name transforms (hyphen→underscore, snake→PascalCase).

**Bundler.require handling:** When `Bundler.require(*Rails.groups)` is detected, only `:default` group gems are marked `BundlerRequired`. Dev/test-only gems without direct code references are reported as `Unused`.

**Gemspec parsing:** Root gemspecs are only parsed when a bare `gemspec` directive (without `path:`) is present in the Gemfile. Gemspecs with explicit `path:` args are parsed from those directories. Gemspec entries follow Bundler ≥ 2.6: the gem itself is `:default` on all platforms and development dependencies are only in `development_group`, ignoring enclosing `group`/`platforms` blocks.

**Per-declaration semantics:** Duplicate `gem` declarations are not merged away — each is a `GemDeclaration` (groups, platforms, autorequire, `install_if`, file:line) on `GemEntry.declarations`; `groups`/`platforms`/`require_false` are aggregates. Bundler.require status is decided per declaration (`GemEntry::bundler_autorequired`), and declarations for other platforms are dropped in main.rs before matching.

**Gemfile discovery:** `Gemfile`, then `gems.rb`; each Gemfile's lockfile is `gems.locked` (for `gems.rb`) or `<gemfile>.lock`.

**Module organization:** Flat structure under `src/`, function-based design (no traits). Shared state uses `Arc<Mutex<>>` (e.g., file cache in scanner).

**Testing:** Integration tests in `tests/` validate byte scanner correctness against regex baselines. Unit tests in each module cover edge cases: inline comments, heredocs, YAML comments, bundler group handling, and gemspec directive detection.
