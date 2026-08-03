use std::fs;
use std::process::{Command, Stdio};
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use tempfile::TempDir;

fn create_project(files: usize, gems: usize) -> TempDir {
    let dir = TempDir::new().unwrap();
    let mut gemfile = String::from("source \"https://rubygems.org\"\n");
    let mut lock_specs = String::new();

    for i in 0..gems {
        gemfile.push_str(&format!(
            "gem \"bench-gem-{i}\", require: \"bench_gem_{i}\"\n"
        ));
        lock_specs.push_str(&format!("    bench-gem-{i} (1.0.0)\n"));
    }

    fs::write(dir.path().join("Gemfile"), gemfile).unwrap();
    fs::write(
        dir.path().join("Gemfile.lock"),
        format!(
            "GEM\n  remote: https://rubygems.org/\n  specs:\n{lock_specs}\nPLATFORMS\n  ruby\n\nDEPENDENCIES\n"
        ),
    )
    .unwrap();

    let app = dir.path().join("app/models");
    fs::create_dir_all(&app).unwrap();
    for i in 0..files {
        fs::write(
            app.join(format!("model_{i}.rb")),
            format!(
                "require \"bench_gem_{}\"\nmodule BenchModel{}\nend\n",
                i % gems,
                i
            ),
        )
        .unwrap();
    }

    dir
}

fn binary_path() -> std::path::PathBuf {
    std::env::var_os("UNUSED_GEMS_BIN")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/release/unused-gems")
        })
}

fn run(binary: &std::path::Path, project: &std::path::Path, no_cache: bool) {
    let mut command = Command::new(binary);
    command
        .arg("--quiet")
        .arg("--fail-on")
        .arg("none")
        .arg("--output")
        .arg("json");
    if no_cache {
        command.arg("--no-cache");
    }
    let status = command
        .arg(project)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}

fn bench_end_to_end(c: &mut Criterion) {
    let binary = binary_path();
    assert!(
        binary.is_file(),
        "build the release binary first with `cargo build --release`"
    );
    let project = create_project(2_000, 200);

    let mut group = c.benchmark_group("end_to_end");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(8));

    group.bench_function("cold_2000_files_200_gems", |b| {
        b.iter(|| run(&binary, project.path(), true));
    });

    run(&binary, project.path(), false);
    group.bench_function("warm_2000_files_200_gems", |b| {
        b.iter(|| run(&binary, project.path(), false));
    });

    group.finish();
}

criterion_group!(benches, bench_end_to_end);
criterion_main!(benches);
