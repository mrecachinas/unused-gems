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
  → [lazy] Only unresolved gems: discover module names (+ vendor/cache listing for uninstalled gems) → re-match
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
- `scripts.rs` — scans scripts (Rakefile/`*.rake`, shell, Make, Docker, Procfile, CI YAML) for gem executables run in command position (`bundle exec`, `bin/`, first word after `&&`/`;`/`|`…), mapping executables to gems via installed gemspecs, a curated table and `-`/`_` normalisation

## Key Conventions

**Error handling:** No custom error types or error crates. Graceful degradation — missing gems, bad caches, etc. are logged via `eprintln!` and skipped. Fatal errors (missing Gemfile/gems.rb, invalid directory, unreadable/invalid `--diff` baseline) exit 2. Otherwise the exit code is 0 if no unused gems were found and 1 if some were (subject to `--fail-on`).

**Performance patterns:**
- Byte-level scanning with `memchr::memmem::Finder` instead of regex in hot paths (scanner.rs)
- `once_cell::sync::Lazy` for static regex and lookup table initialization
- `rayon` for parallelism: `rayon::join()` for concurrent stages, `par_iter()` for batch operations
- Bincode file cache keyed by mtime (nanosecond precision) + size to skip re-scanning unchanged files; entries hold everything a file contributes (requires, its unique constant names, bundler groups, attachment storage services), unvisited entries are pruned on save, and the file is written atomically (temp file + rename). Bump `CACHE_VERSION` when entry contents change
- Never re-read source files outside the walker: collect new per-file facts in `scan_file_bytes_with` and expose them via `FileResult`/`ScanResults`
- Per-file dedup (borrowed `&[u8]` sets) before touching thread-wide maps; lexer memo caches (`LexCache`) keep malformed input linear

**Comment filtering:** All scanners (Ruby and non-Ruby) filter `#` comment lines. Ruby files additionally filter `=begin`/`=end` blocks, heredoc content, string/regex/percent-literal interiors (keeping `#{}` code), the `__END__` data section, and inline comments (e.g., `x = 1 # require 'foo'`). Haml/Slim tag lines are parsed so only attributes, `=` output, script lines (with `,`/`\` continuations) and `#{}` are treated as Ruby. Script scanning strips comment lines before extracting commands; only command words (not arguments) are matched against gems.

**RuboCop configs:** `.rubocop*.yml` / `.standard*.yml` are parsed with serde_yaml (`require`/`plugins` strings, lists or single-key maps; `inherit_gem` keys) with a line-based fallback for non-YAML (ERB) configs.

**Walking:** Symlinks are followed (loops are skipped). `--skip-dirs` entries match directory names, or paths relative to the scan root when they contain a separator (`app/legacy`).

**Gem path discovery (introspect.rs `find_gem_paths`):** BUNDLE_PATH (`ruby/<api>` of the active Ruby, else newest) → GEM_PATH + GEM_HOME → filesystem probes for rbenv, rvm, mise, asdf, chruby and a version-manager `ruby` on PATH (each yields `<prefix>/lib/ruby/gems/<api>` + `~/.gem/ruby/<api>`) → `gem environment gempath` run in the project dir as a last resort. Installed versions are ordered with `Gem::Version` semantics (prereleases sort first).

**Require path resolution is multi-layered:** (1) introspect the installed gem (require dirs from `<base>/specifications/<full_name>.gemspec` or the checkout's `*.gemspec`) UNIONed with (2) the curated `known::REQUIRE_PATHS` table; (3) heuristic name transforms (hyphen→slash/underscore) only when neither applies; (4) lazily, for uninstalled gems still unresolved, the real file listing of the `vendor/cache` `.gem` (bounded reads). Only `lib/*.rb` stems, folders named after the gem (incl. hyphen→slash like `rubocop/performance`) and folders with their own `<folder>.rb` that no other Gemfile gem claims are require roots; other folders are namespace extensions whose files are listed individually (so `solid_cache`'s `lib/active_support/` never claims `require "active_support"`).

**Module names:** top-level declarations of `lib/<gem>.rb` (ruby-prism) ∪ `known::MODULE_NAMES` ∪ PascalCase heuristics. `introspect::NamespaceClaims` filters constants and prefix heuristics owned by another gem (Gemfile, installed, known table; longest prefix wins) or shared by a gem family (`Google::Cloud`), except namespaces of undeclared direct lockfile dependencies. Ruby core/stdlib constants are never used as evidence.

**Bundler.require handling:** When `Bundler.require(*Rails.groups)` is detected, only `:default` group gems are marked `BundlerRequired`. Dev/test-only gems without direct code references are reported as `Unused` unless `--rails-env` (comma-separated/repeatable) activates their groups.

**Gemspec parsing:** Root gemspecs are only parsed when a bare `gemspec` directive (without `path:`) is present in the Gemfile. Gemspecs with explicit `path:` args are parsed from those directories. Gemspec entries follow Bundler ≥ 2.6: the gem itself is `:default` on all platforms and development dependencies are only in `development_group`, ignoring enclosing `group`/`platforms` blocks.

**Per-declaration semantics:** Duplicate `gem` declarations are not merged away — each is a `GemDeclaration` (groups, platforms, autorequire, `install_if`, file:line) on `GemEntry.declarations`; `groups`/`platforms`/`require_false` are aggregates. Bundler.require status is decided per declaration (`GemEntry::bundler_autorequired`), and declarations for other platforms are dropped in main.rs before matching.

**Gemfile discovery:** `Gemfile`, then `gems.rb`; each Gemfile's lockfile is `gems.locked` (for `gems.rb`) or `<gemfile>.lock`.

**Module organization:** Flat structure under `src/`, function-based design (no traits). Shared state uses `Arc<Mutex<>>` (e.g., file cache in scanner).

**Testing:** Integration tests in `tests/` validate byte scanner correctness against regex baselines. Unit tests in each module cover edge cases: inline comments, heredocs, YAML comments, bundler group handling, and gemspec directive detection.
