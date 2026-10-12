# unused-gems

High-performance scanner for unused gems in Ruby projects, built in Rust with [ruby-prism](https://github.com/ruby/prism) (Ruby's official parser).

## Features

- **Gemfile parsing** via ruby-prism AST — handles `gem`, `group`, `platforms`, `install_if`, `require:`, `path:`, `git:`/`github:`/custom `git_source`, `path`/`git`/`github`/`source` blocks, `eval_gemfile` and `gemspec` directives; `Gemfile` or `gems.rb`
- **Parallel source scanning** with rayon — scans Ruby/ERB/HAML/YAML files concurrently
- **Gem introspection** — discovers actual require paths and top-level module names from installed gems (rbenv, rvm, mise, asdf, chruby, GEM_PATH/GEM_HOME, BUNDLE_PATH, git checkouts), without letting namespace-extension folders or reopened constants claim other gems' usage. Lockfile versions and git revisions constrain introspection: a missing locked installation warns once per gem and falls back to curated metadata, heuristics and matching `vendor/cache` artifacts, never another installed version or stale checkout. Without a lockfile constraint, registry gems use the newest installed version.
- **Script/CI detection** — finds gem executables in command position in Rake tasks, shell scripts, Makefiles, Dockerfiles, Procfiles, CI configs (including Drone `commands:` lists), and `mise.toml` / `.mise.toml` tasks (`run` strings or arrays and `[tasks]` shorthand); descriptions and command arguments do not count
- **Lockfile analysis** — counts unique transitive dependencies from `Gemfile.lock` / `gems.locked`
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

Requires [Rust](https://www.rust-lang.org/tools/install) 1.88+ (also tested in CI).

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

# Skip additional directories (names, or paths relative to the project root)
unused-gems --skip-dirs tmp,cache,app/legacy /path/to/project

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
                          (default: Gemfile, then gems.rb)
    --diff <BASELINE>     Compare against previous JSON output (text, json or github per -o)
    --fail-on <POLICY>    none (always 0), any (default: 1 if unused), new (1 only for new unused in --diff)
    --sarif               Emit SARIF JSON (ignored with --diff)
    --skip-dirs <DIRS>    Additional directories to skip (comma-separated names or root-relative paths)
    --rails-env <ENVS>    Rails environment group(s) considered active (comma-separated or repeated)
    --allow-missing-gemfile  Exit 0 with a notice when the Gemfile is missing
-h, --help                Print help
```

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | No unused gems (or `--fail-on none`, or `--fail-on new` without new unused gems) |
| `1` | Unused gems found (per `--fail-on`) |
| `2` | Fatal error: invalid directory, missing, non-file, unreadable or syntactically invalid Gemfile, unreadable or invalid `--diff` baseline, bad CLI arguments |

`--fail-on new` only has an effect together with `--diff`; without it a warning is printed
and the exit code is `0`. With `-o json`, `--diff` prints
`{"schema_version": "1", "diff": {"newly_unused": [...], "no_longer_unused": [...],
"status_changed": [...], "added": [...], "removed": [...]}, "new_unused_count": N}`; with
`-o github` it emits a warning annotation per newly unused gem.

## Gemfile semantics

The default Gemfile is `<dir>/Gemfile`, falling back to `<dir>/gems.rb`. Each analyzed
Gemfile is paired with its own lockfile like Bundler does: `gems.locked` for `gems.rb`,
otherwise `<gemfile>.lock` (so `--gemfile gemfiles/rails7.gemfile` reads
`gemfiles/rails7.gemfile.lock`). Lockfiles of Gemfiles that are not analyzed are ignored.

An existing `Gemfile` takes precedence over `gems.rb`, even if it is invalid.
Every analyzed Gemfile (including resolved `eval_gemfile` targets) must be a readable
regular file with valid Ruby syntax; otherwise analysis exits `2` with an error.
`--allow-missing-gemfile` does not suppress these input errors.

Every `gem` declaration is kept separately, as Bundler keeps each duplicate as its own
dependency. A gem counts as loaded by `Bundler.require` when **any** declaration is in an
active group, applies to the current platform (declarations for other platforms are
dropped before matching) and has autorequire enabled (`require: false`, `require: nil`
and `require: []` disable it). Group and platform names may be symbols or strings, the
options hash may be braced (`gem "x", ">= 0", { require: false }`), `groups:` takes
precedence over `group:` and `platform:` over `platforms:`. `install_if` conditions
(option or block) are evaluated when they are literals such as `-> { false }`, `true` or
`nil`: a statically false condition is never `Bundler.require`d; any other condition is
treated as possibly true.

`Bundler.require` accepts explicit symbol or string group names, including multiline
parenthesized calls. Only arguments inside the call are considered; trailing code
and comments do not activate groups. `Rails.groups` conditions and `Rails.env`
continue to use the active environments selected by `--rails-env`.

A gem's source comes from `path:`, `git:`, the `github:`/`gitlab:`/`gist:`/`bitbucket:`
shorthands, any custom `git_source(:name)` key, or an enclosing `path`/`git`/`github`
block (`source` blocks reset it; `eval_gemfile` inherits it). Gems in a path source
resolve to the directory itself or to the immediate subdirectory holding
`<name>.gemspec`, following Bundler's `{,*,*/*}.gemspec` glob; `~` is expanded.
For path gems inside the scanned tree, files under the gem's own directory do not
count as usage evidence for that gem, but still count for other dependencies.
The project itself (a root `gemspec` or `path: '.'`) is exempt.

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
different versions stay ambiguous, while equal versions collapse to one candidate). Following Bundler ≥ 2.6, the gemspec's own
`spec.name` is registered as a `:default`-group, all-platform path source and its
**development** dependencies are added only to the configured development group — even
when `gemspec` sits inside a `group`/`platforms` block. Runtime dependencies are left to
Bundler's transitive resolution rather than being reported as top-level gems. Matching Bundler, an explicit Gemfile
declaration always wins over a duplicate gemspec development dependency, in either
declaration order.

## Architecture

| Module | Purpose |
|--------|---------|
| `gemfile.rs` | Parses Gemfile/gemspec using ruby-prism AST |
| `scanner.rs` | Parallel source tree scanning with hand-rolled byte scanners |
| `introspect.rs` | Discovers gem require paths and module names from installed gems or vendor/cache tarballs |
| `matcher.rs` | Matches gems against scan results to determine usage |
| `scripts.rs` | Scans Rake tasks, Makefiles, Dockerfiles, Procfiles, CI configs for gem executables run as commands |
| `lockfile.rs` | Parses Gemfile.lock / gems.locked for transitive dependency counting |
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

Active Storage selections in `config/application.rb`, `config/environments/*.rb`
and attachment `service:` options credit the selected backends in `config/storage.yml`
(or `.yml.erb`). YAML comments, aliases and merge keys are supported, and `Mirror`
services recursively credit their `primary` and `mirrors` backends with cycle
protection. Ruby selections inside comments, heredocs and literal text are ignored.

Rails system tests are recognised too: referencing `ActionDispatch::SystemTestCase`
credits `capybara`, and `driven_by :selenium` (or `Capybara.default_driver =`/
`javascript_driver = :selenium_*`, `:cuprite`, …) credits the driver gem. In JSON output
this configuration evidence has `"type": "database_adapter"` (kept for compatibility)
plus `kind` (`database`, `cable`, `storage`, `cache`, `system_test`) and `source` (file).

## Performance

Built with Rust for maximum performance:
- **ruby-prism** for accurate Gemfile AST parsing (same parser as CRuby 3.3+)
- **Hand-rolled byte scanners** with SIMD-accelerated `memchr` — no per-file regex overhead
- **Parallel file walking** via `ignore` crate (scans generated sources even when gitignored)
- **Bincode file cache** — unchanged files skip scanning on repeat runs (entries for removed files are pruned; the cache is replaced atomically)
- **Zero-alloc hot path** — extension checks, comment detection all avoid heap allocation
- **vendor/cache support** — lists require paths from packed `.gem` tarballs (size-bounded, only for gems that aren't installed)

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
The end-to-end benchmark uses `UNUSED_GEMS_BIN` when set; otherwise it looks in
`$CARGO_TARGET_DIR/release` or `<manifest>/target/release`, with the platform's
executable suffix. Use `UNUSED_GEMS_BIN` for other profiles, cross-compilation,
or target directories configured only through Cargo configuration files.
