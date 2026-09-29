# unused-gems

High-performance scanner for unused gems in Ruby projects, built in Rust with [ruby-prism](https://github.com/ruby/prism) (Ruby's official parser).

## Features

- **Gemfile parsing** via ruby-prism AST — handles `gem`, `group`, `platforms`, `require:`, `path:`, `git:`, `eval_gemfile` and `gemspec` directives
- **Parallel source scanning** with rayon — scans Ruby/ERB/HAML/YAML files concurrently
- **Gem introspection** — discovers actual require paths and module names from installed gems
- **Script/CI detection** — finds gem usage in Makefiles, Dockerfiles, CI configs
- **Gemfile.lock analysis** — counts transitive dependency chains
- **Monorepo support** — merge multiple explicit Gemfiles with one source scan
- **Multiple output formats** — text, JSON, GitHub Actions annotations
- **Diff mode** — compare against a baseline JSON to detect regressions
- **Ignore file** — `.unused_gems.yml` support for ignores and custom adapter mappings

## Installation

### Prebuilt binaries

Download the latest release for your platform from [GitHub Releases](https://github.com/mrecachinas/unused-gems/releases).

Requires [GitHub CLI](https://cli.github.com/) (`gh`):

```bash
# macOS (Apple Silicon)
gh release download --repo mrecachinas/unused-gems --pattern 'unused-gems-aarch64-apple-darwin.tar.gz'
tar xzf unused-gems-aarch64-apple-darwin.tar.gz && sudo mv unused-gems /usr/local/bin/

# macOS (Intel)
gh release download --repo mrecachinas/unused-gems --pattern 'unused-gems-x86_64-apple-darwin.tar.gz'
tar xzf unused-gems-x86_64-apple-darwin.tar.gz && sudo mv unused-gems /usr/local/bin/

# Linux (x86_64)
gh release download --repo mrecachinas/unused-gems --pattern 'unused-gems-x86_64-unknown-linux-gnu.tar.gz'
tar xzf unused-gems-x86_64-unknown-linux-gnu.tar.gz && sudo mv unused-gems /usr/local/bin/

# Linux (aarch64)
gh release download --repo mrecachinas/unused-gems --pattern 'unused-gems-aarch64-unknown-linux-gnu.tar.gz'
tar xzf unused-gems-aarch64-unknown-linux-gnu.tar.gz && sudo mv unused-gems /usr/local/bin/

# Windows (x86_64) — extract unused-gems.exe from zip
gh release download --repo mrecachinas/unused-gems --pattern 'unused-gems-x86_64-pc-windows-msvc.zip'
```

### Build from source

Requires [Rust](https://www.rust-lang.org/tools/install) 1.70+.

```bash
# Install to ~/.cargo/bin
cargo install --path .

# Or build a release binary
cargo build --release
# Binary at ./target/release/unused-gems
```

## Usage

```bash
# Scan current directory
unused-gems

# Scan a specific project
unused-gems /path/to/ruby/project

# JSON output
unused-gems -o json /path/to/project

# Verbose output with file locations
unused-gems -v /path/to/project

# GitHub Actions annotations
unused-gems -o github /path/to/project

# Compare against baseline
unused-gems -o json /path/to/project > baseline.json
# ... later ...
unused-gems --diff baseline.json /path/to/project
# Fail only when a gem becomes unused or a newly added gem is unused
unused-gems --diff baseline.json --fail-on new /path/to/project

# Skip additional directories
unused-gems --skip-dirs tmp,cache /path/to/project

# Treat the development and test groups as active for Bundler.require(*Rails.groups)
unused-gems --rails-env development,test /path/to/project

# Scan a monorepo with multiple Gemfiles
unused-gems --gemfile engines/a/Gemfile --gemfile engines/b/Gemfile /path/to/project
```

## CLI Options

```
-o, --output <FORMAT>     Output format: text (default), json, github
-j, --jobs <N>            Number of parallel scanning threads (default walker cap: 4)
-v, --verbose             Show file locations and require paths
-q, --quiet               Suppress progress messages
    --no-cache            Disable file scan cache
    --ignore-file <PATH>  Ignore file; relative paths resolve against the scanned directory (default: .unused_gems.yml)
    --gemfile <PATH>      Gemfile path to analyze; repeat for monorepos
    --diff <BASELINE>     Compare against previous JSON output (text, json or github per -o)
    --fail-on <POLICY>    none (always 0), any (default: 1 if unused), new (1 only for new unused in --diff)
    --sarif               Emit SARIF JSON (ignored with --diff)
    --skip-dirs <DIRS>    Additional directories to skip (comma-separated)
    --rails-env <ENVS>    Rails environment group(s) considered active (comma-separated or repeated)
    --allow-missing-gemfile  Exit 0 with a notice when the Gemfile is missing
-h, --help                Print help
```

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | No unused gems (or `--fail-on none`, or `--fail-on new` without new unused gems) |
| `1` | Unused gems found (per `--fail-on`) |
| `2` | Fatal error: invalid directory, missing Gemfile, unreadable or invalid `--diff` baseline, bad CLI arguments |

`--fail-on new` only has an effect together with `--diff`; without it a warning is printed
and the exit code is `0`. With `-o json`, `--diff` prints
`{"schema_version": "1", "diff": {"newly_unused": [...], "no_longer_unused": [...],
"status_changed": [...], "added": [...], "removed": [...]}, "new_unused_count": N}`; with
`-o github` it emits a warning annotation per newly unused gem.

## Gemfile semantics

`eval_gemfile` is evaluated inline at its declaration site, so gems appear in source
order and inherit the enclosing `group`/`platforms` context. Relative paths resolve
against the Gemfile that declared them, and `File.expand_path`, `File.join`,
`File.dirname`, `__FILE__` and `__dir__` are evaluated with Ruby semantics (note that
`File.expand_path(p, __FILE__)` treats the Gemfile path itself as the base directory,
and `File.join('a', '/b')` is `a/b`). A Gemfile evaluated more than once contributes its
declarations under each context — including through symlink or `..` aliases of the same
file — while cycles are broken by tracking only the active evaluation stack. Unsupported
or unresolvable expressions produce a warning instead of being silently dropped.

`gemspec` honors `path:`, `name:` and `development_group:`. It looks only at the
immediate directory for `*.gemspec` (plus a bare `.gemspec`), matches `name:` against the
parsed `spec.name` rather than the file name, selects deterministically, and warns when
zero gemspecs are found or when the candidates cover more than one `[spec.name,
spec.version]` pair (matching Bundler's grouping — two gemspecs for the same gem at
different versions stay ambiguous, while equal versions collapse to one candidate). The gemspec's own `spec.name` is registered as
a default-group path source; its **development** dependencies are added to the configured
development group, while runtime dependencies are left to Bundler's transitive resolution
rather than being reported as top-level gems. Matching Bundler, an explicit Gemfile
declaration always wins over a duplicate gemspec development dependency, in either
declaration order.

## Architecture

| Module | Purpose |
|--------|---------|
| `gemfile.rs` | Parses Gemfile/gemspec using ruby-prism AST |
| `scanner.rs` | Parallel source tree scanning with hand-rolled byte scanners |
| `introspect.rs` | Discovers gem require paths and module names from installed gems or vendor/cache tarballs |
| `matcher.rs` | Matches gems against scan results to determine usage |
| `scripts.rs` | Scans Makefiles, Dockerfiles, CI configs for gem references |
| `lockfile.rs` | Parses Gemfile.lock for transitive dependency counting |
| `cache.rs` | Bincode file cache — skips re-scanning unchanged files |
| `output.rs` | Text, JSON, GitHub Actions output formatters |
| `diff.rs` | Baseline comparison mode |
| `ignore.rs` | `.unused_gems.yml` ignore file support |
| `adapters.rs` | Built-in and user-provided Rails adapter-to-gem mappings |
| `known.rs` | Curated require path and module name metadata |
| `platforms.rs` | Bundler platform token resolution |

## `.unused_gems.yml`

```yaml
ignore:
  - development-only-gem

adapters:
  database:
    clickhouse: clickhouse-activerecord
  cable:
    custom_cable: custom-cable-gem
  storage:
    CustomStorage: custom-storage-gem
  cache:
    custom_cache_store: custom-cache-gem
```

`ignore` accepts a list of gem names (a single scalar name is also accepted). Missing
explicit `--ignore-file` paths, malformed YAML and invalid entries produce warnings.

Adapter entries map the value used in Rails config to the gem that provides it.

Built-in mappings cover common database adapters (including `postgis`, `sqlserver`,
`oracle_enhanced` and `cockroachdb`, which credit both the ActiveRecord adapter gem and
its driver), static `url:` schemes in `config/database.yml` (`postgres://`, `mysql2://`,
`sqlite3:`, `trilogy://`, or a literal default inside
`<%= ENV.fetch("DATABASE_URL", "…") %>`), and unknown database adapters fall back to a
gem named after the adapter or `activerecord-<adapter>-adapter`. Rails' built-in
backends (`async`/`inline`/`test` cable adapters, `Disk`/`Mirror` storage services,
`memory_store`/`file_store`/`null_store` caches) credit no gem.

## Performance

Built with Rust for maximum performance:
- **ruby-prism** for accurate Gemfile AST parsing (same parser as CRuby 3.3+)
- **Hand-rolled byte scanners** with SIMD-accelerated `memchr` — no per-file regex overhead
- **Parallel file walking** via `ignore` crate (scans generated sources even when gitignored)
- **Bincode file cache** — unchanged files skip scanning on repeat runs
- **Zero-alloc hot path** — extension checks, comment detection all avoid heap allocation
- **vendor/cache support** — extracts gemspec metadata from packed `.gem` tarballs

On github/github (~53K Ruby files): **~2.8s** (vs scc line counting at 1.85s).

Run the reproducible benchmark suite with:

```bash
cargo build --release
cargo bench -- --save-baseline main
# After a change:
cargo bench -- --baseline main
```

It covers cold and warm end-to-end runs, source-tree scaling,
comment/heredoc-heavy Ruby, and gitignored generated-source trees.
