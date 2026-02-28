# unused-gems

High-performance scanner for unused gems in Ruby projects, built in Rust with [ruby-prism](https://github.com/ruby/prism) (Ruby's official parser).

## Features

- **Gemfile parsing** via ruby-prism AST — handles `gem`, `group`, `require:`, `gemspec` directives
- **Parallel source scanning** with rayon — scans Ruby/ERB/HAML/YAML files concurrently
- **Gem introspection** — discovers actual require paths and module names from installed gems
- **Script/CI detection** — finds gem usage in Makefiles, Dockerfiles, CI configs
- **Gemfile.lock analysis** — counts transitive dependency chains
- **Multiple output formats** — text, JSON, GitHub Actions annotations
- **Diff mode** — compare against a baseline JSON to detect regressions
- **Ignore file** — `.unused_gems.yml` support

## Installation

### Prebuilt binaries

Download the latest release for your platform from [GitHub Releases](https://github.com/mrecachinas/unused-gems/releases):

```bash
# macOS (Apple Silicon)
curl -sL https://github.com/mrecachinas/unused-gems/releases/latest/download/unused-gems-aarch64-apple-darwin.tar.gz | tar xz
sudo mv unused-gems /usr/local/bin/

# macOS (Intel)
curl -sL https://github.com/mrecachinas/unused-gems/releases/latest/download/unused-gems-x86_64-apple-darwin.tar.gz | tar xz
sudo mv unused-gems /usr/local/bin/

# Linux (x86_64)
curl -sL https://github.com/mrecachinas/unused-gems/releases/latest/download/unused-gems-x86_64-unknown-linux-gnu.tar.gz | tar xz
sudo mv unused-gems /usr/local/bin/

# Linux (aarch64)
curl -sL https://github.com/mrecachinas/unused-gems/releases/latest/download/unused-gems-aarch64-unknown-linux-gnu.tar.gz | tar xz
sudo mv unused-gems /usr/local/bin/
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

# Skip additional directories
unused-gems --skip-dirs tmp,cache /path/to/project
```

## CLI Options

```
-o, --output <FORMAT>     Output format: text (default), json, github
-j, --jobs <N>            Number of parallel scanning threads (default: CPU count)
-v, --verbose             Show file locations and require paths
-q, --quiet               Suppress progress messages
    --no-cache            Disable file scan cache
    --ignore-file <PATH>  Path to ignore file (default: .unused_gems.yml)
    --diff <BASELINE>     Compare against previous JSON output
    --skip-dirs <DIRS>    Additional directories to skip (comma-separated)
-h, --help                Print help
```

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

## Performance

Built with Rust for maximum performance:
- **ruby-prism** for accurate Gemfile AST parsing (same parser as CRuby 3.3+)
- **Hand-rolled byte scanners** with SIMD-accelerated `memchr` — no per-file regex overhead
- **Parallel file walking** via `ignore` crate (respects `.gitignore`)
- **Bincode file cache** — unchanged files skip scanning on repeat runs
- **Zero-alloc hot path** — extension checks, comment detection all avoid heap allocation
- **vendor/cache support** — extracts gemspec metadata from packed `.gem` tarballs

On github/github (~53K Ruby files): **~2.8s** (vs scc line counting at 1.85s).
