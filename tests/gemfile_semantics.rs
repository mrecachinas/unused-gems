//! End-to-end regression tests for Gemfile evaluation semantics:
//! nested `eval_gemfile` resolution/context and `gemspec` directive handling.
//!
//! These drive the real binary so that Gemfile parsing, matching and output all
//! agree on the semantics implemented in `src/gemfile.rs`.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

/// Run the tool over `project` and return `name -> (status, groups)`.
fn run_tool(project: &Path) -> BTreeMap<String, (String, Vec<String>)> {
    run_json(project, &[])
        .into_iter()
        .map(|(name, entry)| {
            let status = entry["status"].as_str().unwrap().to_string();
            let groups = entry["groups"]
                .as_array()
                .map(|g| {
                    g.iter()
                        .map(|v| v.as_str().unwrap().to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            (name, (status, groups))
        })
        .collect()
}

/// Run the tool with extra CLI args and return `name -> JSON record`.
fn run_json(project: &Path, extra: &[&str]) -> BTreeMap<String, serde_json::Value> {
    let bin = env!("CARGO_BIN_EXE_unused-gems");
    let output = Command::new(bin)
        .arg("--output")
        .arg("json")
        .arg("--no-cache")
        .arg("--quiet")
        .args(extra)
        .arg(project)
        .env(
            "UNUSED_GEMS_RUBY_RUNTIME",
            "ruby|x86_64-linux|3.4.8|x86_64-linux",
        )
        .output()
        .unwrap_or_else(|e| panic!("invoke {bin}: {e}"));

    let code = output.status.code().unwrap_or(-1);
    assert!(
        code == 0 || code == 1,
        "unused-gems on {} exited {}: stderr={}",
        project.display(),
        code,
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "parse JSON ({}): {}",
            e,
            String::from_utf8_lossy(&output.stdout)
        )
    });

    json["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|entry| (entry["name"].as_str().unwrap().to_string(), entry.clone()))
        .collect()
}

fn groups_of(results: &BTreeMap<String, (String, Vec<String>)>, gem: &str) -> Vec<String> {
    results
        .get(gem)
        .unwrap_or_else(|| panic!("gem '{}' missing from results: {:?}", gem, results.keys()))
        .1
        .clone()
}

fn status_of(results: &BTreeMap<String, (String, Vec<String>)>, gem: &str) -> String {
    results
        .get(gem)
        .unwrap_or_else(|| panic!("gem '{}' missing from results: {:?}", gem, results.keys()))
        .0
        .clone()
}

#[test]
fn nested_eval_gemfile_context_and_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write(
        &root.join("Gemfile"),
        r#"source 'https://rubygems.org'

gem 'rails'

eval_gemfile File.expand_path('../gemfiles/shared.gemfile', __FILE__)

group :test do
  eval_gemfile 'gemfiles/test.gemfile'
end
"#,
    );

    // Relative eval nested inside another evaluated Gemfile must resolve against
    // the file that declared it, not the project root.
    write(
        &root.join("gemfiles/shared.gemfile"),
        "gem 'shared_gem'\neval_gemfile 'nested/deep.gemfile'\n",
    );
    write(
        &root.join("gemfiles/nested/deep.gemfile"),
        "gem 'deep_gem'\neval_gemfile '../shared.gemfile'\n",
    );
    write(
        &root.join("gemfiles/test.gemfile"),
        "gem 'rspec'\nplatforms :jruby do\n  gem 'jdbc_gem'\nend\n",
    );

    write(
        &root.join("app.rb"),
        "require 'rails'\nrequire 'deep_gem'\n",
    );

    let results = run_tool(root);

    assert_eq!(status_of(&results, "rails"), "used");
    assert_eq!(status_of(&results, "deep_gem"), "used");
    assert_eq!(status_of(&results, "shared_gem"), "unused");
    // Enclosing group context is inherited by the evaluated Gemfile.
    assert_eq!(groups_of(&results, "rspec"), vec!["test"]);
    // Enclosing group + nested platform block.
    assert_eq!(groups_of(&results, "jdbc_gem"), vec!["test"]);
    assert_eq!(status_of(&results, "jdbc_gem"), "platform_excluded");
    // The cycle back to shared.gemfile must not duplicate or hang.
    assert_eq!(results.keys().count(), 5);
}

#[test]
fn gemspec_directive_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write(
        &root.join("Gemfile"),
        "source 'https://rubygems.org'\ngemspec path: 'engines/core', name: 'core', development_group: :dev\n",
    );
    // File name deliberately differs from spec.name: `name:` matches spec.name.
    write(
        &root.join("engines/core/project.gemspec"),
        r#"Gem::Specification.new do |spec|
  spec.name = 'core'
  spec.add_dependency 'runtime_only_dep'
  spec.add_runtime_dependency 'another_runtime_dep'
  spec.add_development_dependency 'rspec'
end
"#,
    );
    // A decoy gemspec in the same directory; `name:` must pick core.gemspec.
    write(
        &root.join("engines/core/decoy.gemspec"),
        "Gem::Specification.new do |spec|\n  spec.name = 'decoy'\n  spec.add_development_dependency 'decoy_dev'\nend\n",
    );
    write(&root.join("engines/core/lib/core.rb"), "module Core; end\n");
    write(&root.join("app.rb"), "require 'core'\n");

    let results = run_tool(root);

    // The gemspec's own gem is declared as a default-group path source.
    assert_eq!(groups_of(&results, "core"), vec!["default"]);
    assert_eq!(status_of(&results, "core"), "used");
    // Development dependencies land in the configured development group.
    assert_eq!(groups_of(&results, "rspec"), vec!["dev"]);
    // Runtime dependencies of the gemspec are not top-level Gemfile gems, and the
    // decoy gemspec contributes nothing.
    let names: Vec<&String> = results.keys().collect();
    assert_eq!(names, vec!["core", "rspec"], "unexpected gems: {:?}", names);
}

#[test]
fn gemspec_reached_through_eval_gemfile() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write(
        &root.join("Gemfile"),
        "eval_gemfile 'engines/admin/Gemfile'\n",
    );
    write(&root.join("engines/admin/Gemfile"), "gemspec\n");
    write(
        &root.join("engines/admin/admin.gemspec"),
        "Gem::Specification.new do |spec|\n  spec.name = 'admin'\n  spec.add_development_dependency 'factory_bot'\nend\n",
    );
    write(
        &root.join("engines/admin/lib/admin.rb"),
        "module Admin; end\n",
    );
    write(&root.join("app.rb"), "require 'admin'\n");

    let results = run_tool(root);

    assert_eq!(status_of(&results, "admin"), "used");
    assert_eq!(groups_of(&results, "admin"), vec!["default"]);
    assert_eq!(groups_of(&results, "factory_bot"), vec!["development"]);
    assert_eq!(status_of(&results, "factory_bot"), "unused");
}

#[test]
fn unresolvable_directives_are_warned_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write(
        &root.join("Gemfile"),
        r#"eval_gemfile Bundler.root.join('missing.gemfile')
eval_gemfile 'does_not_exist.gemfile'
gemspec path: 'no_such_dir'
gem 'rails'
"#,
    );
    write(&root.join("app.rb"), "require 'rails'\n");

    let bin = env!("CARGO_BIN_EXE_unused-gems");
    let output = Command::new(bin)
        .arg("--output")
        .arg("json")
        .arg("--no-cache")
        .arg(root)
        .output()
        .unwrap();

    let code = output.status.code().unwrap_or(-1);
    assert!(code == 0 || code == 1, "unexpected exit code {}", code);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.matches("Warning:").count() >= 3,
        "expected warnings for each unresolvable directive, got: {}",
        stderr
    );

    let results = run_tool(root);
    assert_eq!(status_of(&results, "rails"), "used");
}

#[test]
fn repeated_eval_gemfile_contributes_each_context() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write(
        &root.join("Gemfile"),
        r#"eval_gemfile 'shared.gemfile'
group :test do
  eval_gemfile File.join(__dir__, '/shared.gemfile')
end
"#,
    );
    write(&root.join("shared.gemfile"), "gem 'shared_gem'\n");
    write(&root.join("app.rb"), "# nothing required\n");

    let results = run_tool(root);

    // Evaluated twice (plain + inside `group :test`); both contexts survive, and
    // File.join must not restart the path at the leading '/' of the 2nd component.
    assert_eq!(groups_of(&results, "shared_gem"), vec!["default", "test"]);
    assert_eq!(results.keys().count(), 1);
}

#[test]
fn gemfile_declaration_wins_over_gemspec_development_dependency() {
    for (label, gemfile) in [
        ("gemfile first", "gem 'rspec', group: :test\ngemspec\n"),
        ("gemspec first", "gemspec\ngem 'rspec', group: :test\n"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("Gemfile"), gemfile);
        write(
            &root.join("my_gem.gemspec"),
            "Gem::Specification.new do |spec|\n  spec.name = 'my_gem'\n  spec.add_development_dependency 'rspec'\nend\n",
        );
        write(&root.join("lib/my_gem.rb"), "module MyGem; end\n");
        write(&root.join("spec/a_spec.rb"), "require 'rspec'\n");

        let results = run_tool(root);
        assert_eq!(
            groups_of(&results, "rspec"),
            vec!["test"],
            "{}: Gemfile declaration must win over the gemspec development dependency",
            label
        );
    }
}

#[test]
fn same_name_gemspecs_at_different_versions_are_ambiguous() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write(&root.join("Gemfile"), "gemspec\n");
    write(
        &root.join("b.gemspec"),
        "Gem::Specification.new do |spec|\n  spec.name = 'same'\n  spec.version = '2.0.0'\n  spec.add_development_dependency 'from_b'\nend\n",
    );
    write(
        &root.join("a.gemspec"),
        "Gem::Specification.new do |spec|\n  spec.name = 'same'\n  spec.version = '1.0.0'\n  spec.add_development_dependency 'from_a'\nend\n",
    );
    write(&root.join("lib/same.rb"), "module Same; end\n");

    let bin = env!("CARGO_BIN_EXE_unused-gems");
    let output = Command::new(bin)
        .arg("--output")
        .arg("json")
        .arg("--no-cache")
        .arg(root)
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("found multiple gemspecs")
            && stderr.contains("same-1.0.0")
            && stderr.contains("same-2.0.0"),
        "expected an ambiguity warning naming both versions, got: {}",
        stderr
    );

    // Deterministic fallback: the first candidate by file name.
    let results = run_tool(root);
    let names: Vec<&String> = results.keys().collect();
    assert_eq!(
        names,
        vec!["from_a", "same"],
        "unexpected gems: {:?}",
        names
    );
}

#[test]
fn same_name_and_version_gemspecs_are_not_ambiguous() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write(&root.join("Gemfile"), "gemspec\n");
    write(
        &root.join("a.gemspec"),
        "Gem::Specification.new do |spec|\n  spec.name = 'same'\n  spec.version = '1.0'\n  spec.add_development_dependency 'rspec'\nend\n",
    );
    write(
        &root.join("b.gemspec"),
        "Gem::Specification.new do |spec|\n  spec.name = 'same'\n  spec.version = '1.0.0'\nend\n",
    );
    write(&root.join("lib/same.rb"), "module Same; end\n");

    let bin = env!("CARGO_BIN_EXE_unused-gems");
    let output = Command::new(bin)
        .arg("--output")
        .arg("json")
        .arg("--no-cache")
        .arg(root)
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("found multiple gemspecs"),
        "equal versions must not be ambiguous, got: {}",
        stderr
    );

    let results = run_tool(root);
    let names: Vec<&String> = results.keys().collect();
    assert_eq!(names, vec!["rspec", "same"], "unexpected gems: {:?}", names);
}

#[test]
fn gems_rb_is_discovered_with_gems_locked() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("gems.rb"), "gem 'a'\n");
    write(
        &root.join("gems.locked"),
        "GEM\n  specs:\n    a (1.0)\n      b (>= 0)\n    b (1.0)\n\nDEPENDENCIES\n  a\n",
    );
    write(&root.join("app.rb"), "# nothing\n");

    let results = run_json(root, &[]);
    assert_eq!(results["a"]["status"], "unused");
    assert_eq!(results["a"]["transitive_dep_count"], 1);
}

#[test]
fn explicit_gemfile_uses_its_own_lockfile_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("Gemfile"), "gem 'a'\n");
    write(
        &root.join("Gemfile.lock"),
        "GEM\n  specs:\n    a (1.0)\n      b (>= 0)\n      c (>= 0)\n    b (1.0)\n    c (1.0)\n",
    );
    write(&root.join("gemfiles/rails7.gemfile"), "gem 'a'\n");
    write(
        &root.join("gemfiles/rails7.gemfile.lock"),
        "GEM\n  specs:\n    a (1.0)\n    b (1.0)\n",
    );
    write(&root.join("app.rb"), "# nothing\n");

    let results = run_json(root, &["--gemfile", "gemfiles/rails7.gemfile"]);
    assert_eq!(results["a"]["status"], "unused");
    // Root Gemfile.lock (a -> b, c) must not leak into the rails7 analysis.
    assert_eq!(results["a"]["transitive_dep_count"], 0);
}
