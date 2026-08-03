//! Controlled 1/2/4/8/12-thread scanner benchmarks for issue #13.
//!
//! Sweeps the walker fan-out at fixed thread counts via
//! `scan_source_tree_with_threads` so results are directly comparable across
//! counts. This does NOT mutate the rayon global pool: only the walker thread
//! count changes between runs, and the same corpus is scanned identically at
//! every count.
//!
//! Corpora exercise the three regimes called out in issue #13:
//!   * `ruby_2000`         — large cold Ruby scan (favors more walker threads)
//!   * `comment_heredoc_1000` — CPU-heavy comment/heredoc filtering
//!   * `gitignore_250_of_4250` — ignore-heavy walk (regresses with more threads)
//!
//! Warm-cache sweep uses a shared in-memory `FileCache` populated once before
//! the timed iterations so we measure the mostly-cache-hit walker path.
//!
//! NOTE: Run Criterion serially (`cargo bench --bench scanner_threads`, not
//! concurrently with other benches) so thread counts are the only variable.

#![allow(dead_code, unused_imports)]

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tempfile::TempDir;

#[path = "../src/cache.rs"]
mod cache;
#[path = "../src/scanner.rs"]
mod scanner;
#[path = "../src/scripts.rs"]
mod scripts;
#[path = "../src/types.rs"]
mod types;

const THREAD_COUNTS: &[usize] = &[1, 2, 4, 8, 12];

struct Corpus {
    _dir: TempDir,
    files: u64,
    bytes: u64,
}

impl Corpus {
    fn path(&self) -> &std::path::Path {
        self._dir.path()
    }
}

fn write_files(dir: &std::path::Path, count: usize, body: &str) -> u64 {
    fs::create_dir_all(dir).unwrap();
    for i in 0..count {
        fs::write(dir.join(format!("file_{i}.rb")), body).unwrap();
    }
    (count * body.len()) as u64
}

fn ruby_corpus(files: usize) -> Corpus {
    let dir = TempDir::new().unwrap();
    let body = r#"
require "active_support"
require_relative "../config/environment"
autoload :Report, "reports/report"

module Billing
  class Invoice
    def call
      Gem.bin_path("rubocop", "rubocop")
    end
  end
end
"#;
    let bytes = write_files(&dir.path().join("app/models"), files, body);
    Corpus {
        _dir: dir,
        files: files as u64,
        bytes,
    }
}

fn comment_heavy_corpus(files: usize) -> Corpus {
    let dir = TempDir::new().unwrap();
    let body = r##"
# require "commented_out"
template = <<~RUBY
  require "inside_heredoc"
  module FakeModule
  end
RUBY

=begin
require "inside_block_comment"
module BlockComment
end
=end

require "real_dependency" # require "inline_comment"
module RealModule
  VALUE = "# not a comment"
end
"##;
    let bytes = write_files(&dir.path().join("app/services"), files, body);
    Corpus {
        _dir: dir,
        files: files as u64,
        bytes,
    }
}

fn ignore_heavy_corpus(visible_files: usize, ignored_files: usize) -> Corpus {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join(".gitignore"), "generated/\n").unwrap();
    let body = "require \"rack\"\nmodule Visible\nend\n";
    let visible_bytes = write_files(&dir.path().join("app"), visible_files, body);
    let ignored_bytes = write_files(&dir.path().join("generated"), ignored_files, body);
    Corpus {
        _dir: dir,
        files: visible_files as u64,
        bytes: visible_bytes + ignored_bytes,
    }
}

fn bench_thread_sweep(c: &mut Criterion) {
    let mut group = c.benchmark_group("scanner_threads");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(6));

    // Cold Ruby scan: expected to improve with more walker threads.
    let ruby = ruby_corpus(2_000);
    group.throughput(Throughput::Bytes(ruby.bytes));
    for &t in THREAD_COUNTS {
        group.bench_with_input(BenchmarkId::new("ruby_2000_cold", t), &t, |b, &t| {
            b.iter(|| {
                scanner::scan_source_tree_with_threads(
                    ruby.path(),
                    &scanner::default_skip_dirs(),
                    Arc::new(cache::FileCache::disabled()),
                    Some(t),
                )
            });
        });
    }

    // Comment/heredoc-heavy: measures scan CPU cost, not walker cost.
    let comments = comment_heavy_corpus(1_000);
    group.throughput(Throughput::Bytes(comments.bytes));
    for &t in THREAD_COUNTS {
        group.bench_with_input(BenchmarkId::new("comment_heredoc_1000", t), &t, |b, &t| {
            b.iter(|| {
                scanner::scan_source_tree_with_threads(
                    comments.path(),
                    &scanner::default_skip_dirs(),
                    Arc::new(cache::FileCache::disabled()),
                    Some(t),
                )
            });
        });
    }

    // Ignore-heavy: expected to regress with more walker threads (issue #13).
    let ignore = ignore_heavy_corpus(250, 4_000);
    group.throughput(Throughput::Elements(ignore.files));
    for &t in THREAD_COUNTS {
        group.bench_with_input(BenchmarkId::new("gitignore_250_of_4250", t), &t, |b, &t| {
            b.iter(|| {
                scanner::scan_source_tree_with_threads(
                    ignore.path(),
                    &scanner::default_skip_dirs(),
                    Arc::new(cache::FileCache::disabled()),
                    Some(t),
                )
            });
        });
    }

    // Warm-cache sweep: populate a shared cache once, then sweep threads.
    // Uses a disk-backed cache scoped to the corpus so the cache-hit walker
    // path is what varies with thread count.
    let warm = ruby_corpus(2_000);
    // Prime and persist the cache before entering the timed region.
    {
        let (_r, updates) = scanner::scan_source_tree_with_threads(
            warm.path(),
            &scanner::default_skip_dirs(),
            Arc::new(cache::FileCache::load(warm.path())),
            Some(1),
        );
        let mut mut_cache = cache::FileCache::load(warm.path());
        for (rp, mt, sz, reqs, mods, bg, ba) in updates {
            mut_cache.put(rp, mt, sz, &reqs, mods, &bg, ba);
        }
        mut_cache.save();
    }
    let warm_cache = Arc::new(cache::FileCache::load(warm.path()));
    group.throughput(Throughput::Bytes(warm.bytes));
    for &t in THREAD_COUNTS {
        group.bench_with_input(BenchmarkId::new("ruby_2000_warm", t), &t, |b, &t| {
            b.iter(|| {
                scanner::scan_source_tree_with_threads(
                    warm.path(),
                    &scanner::default_skip_dirs(),
                    Arc::clone(&warm_cache),
                    Some(t),
                )
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_thread_sweep);
criterion_main!(benches);
