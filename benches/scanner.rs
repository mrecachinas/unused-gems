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
        files: (visible_files + ignored_files) as u64,
        bytes: visible_bytes + ignored_bytes,
    }
}

fn bench_source_tree(c: &mut Criterion) {
    let mut group = c.benchmark_group("source_tree");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(5));

    for files in [200, 2_000] {
        let corpus = ruby_corpus(files);
        group.throughput(Throughput::Bytes(corpus.bytes));
        group.bench_with_input(BenchmarkId::new("ruby", files), &corpus, |b, corpus| {
            b.iter(|| {
                scanner::scan_source_tree(
                    corpus.path(),
                    &scanner::default_skip_dirs(),
                    Arc::new(cache::FileCache::disabled()),
                )
            });
        });
    }

    let corpus = comment_heavy_corpus(1_000);
    group.throughput(Throughput::Bytes(corpus.bytes));
    group.bench_function("comment_heredoc_1000", |b| {
        b.iter(|| {
            scanner::scan_source_tree(
                corpus.path(),
                &scanner::default_skip_dirs(),
                Arc::new(cache::FileCache::disabled()),
            )
        });
    });

    let corpus = ignore_heavy_corpus(250, 4_000);
    group.throughput(Throughput::Elements(corpus.files));
    group.bench_function("gitignore_250_of_4250", |b| {
        b.iter(|| {
            scanner::scan_source_tree(
                corpus.path(),
                &scanner::default_skip_dirs(),
                Arc::new(cache::FileCache::disabled()),
            )
        });
    });

    group.finish();
}

criterion_group!(benches, bench_source_tree);
criterion_main!(benches);
