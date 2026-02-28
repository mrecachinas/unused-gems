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

```bash
cargo install --path .
```

Or build a release binary:

```bash
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
    --ignore-file <PATH>  Path to ignore file (default: .unused_gems.yml)
    --diff <BASELINE>     Compare against previous JSON output
    --skip-dirs <DIRS>    Additional directories to skip (comma-separated)
-h, --help                Print help
```

## Architecture

| Module | Purpose |
|--------|---------|
| `gemfile.rs` | Parses Gemfile/gemspec using ruby-prism AST |
| `scanner.rs` | Parallel source tree scanning with rayon + compiled regex |
| `introspect.rs` | Discovers gem require paths and module names from installed gems |
| `matcher.rs` | Matches gems against scan results to determine usage |
| `scripts.rs` | Scans Makefiles, Dockerfiles, CI configs for gem references |
| `lockfile.rs` | Parses Gemfile.lock for transitive dependency counting |
| `output.rs` | Text, JSON, GitHub Actions output formatters |
| `diff.rs` | Baseline comparison mode |
| `ignore.rs` | `.unused_gems.yml` ignore file support |

## Performance

Built with Rust for maximum performance:
- **ruby-prism** for accurate Gemfile AST parsing (same parser as CRuby 3.3+)
- **rayon** for parallel file scanning across all CPU cores
- **Pre-compiled regex** via `once_cell` for zero per-file compilation overhead
- **Release profile** with LTO and single codegen unit for optimal binary size

Typically **4-6x faster** than the equivalent Ruby implementation.
