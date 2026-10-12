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

fn assert_gemfile_error(project: &Path, extra: &[&str], path: &Path, message: &str) {
    let output = Command::new(env!("CARGO_BIN_EXE_unused-gems"))
        .args(["--output", "json", "--no-cache", "--quiet"])
        .args(extra)
        .arg(project)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(output.stdout.is_empty(), "must not emit successful results");
    assert!(stderr.contains("Error:"), "{stderr}");
    assert!(stderr.contains(message), "{stderr}");
    assert!(stderr.contains(&path.display().to_string()), "{stderr}");
}

#[test]
fn invalid_gemfiles_fail_instead_of_returning_partial_results() {
    for filename in ["Gemfile", "gems.rb", "custom.gemfile"] {
        for source in ["gem(", "gem 'valid_gem'\ngem("] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(filename);
            write(&path, source);
            if filename == "Gemfile" {
                write(&dir.path().join("gems.rb"), "");
            }
            let extra = if filename == "custom.gemfile" {
                vec!["--gemfile", filename, "--allow-missing-gemfile"]
            } else {
                vec!["--allow-missing-gemfile"]
            };
            assert_gemfile_error(dir.path(), &extra, &path, "Could not parse Gemfile");
        }
    }
}

#[test]
fn gemfile_directories_fail_even_with_a_valid_fallback() {
    for filename in ["Gemfile", "gems.rb", "custom.gemfile"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(filename);
        std::fs::create_dir(&path).unwrap();
        if filename == "Gemfile" {
            write(&dir.path().join("gems.rb"), "");
        }
        let extra = if filename == "custom.gemfile" {
            vec!["--gemfile", filename, "--allow-missing-gemfile"]
        } else {
            vec!["--allow-missing-gemfile"]
        };
        assert_gemfile_error(dir.path(), &extra, &path, "not a regular file");
    }
}

#[test]
fn invalid_eval_gemfile_targets_fail() {
    for directory in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("Gemfile"), "eval_gemfile 'shared.gemfile'");
        let path = dir.path().join("shared.gemfile");
        let message = if directory {
            std::fs::create_dir(&path).unwrap();
            "not a regular file"
        } else {
            write(&path, "gem(");
            "Could not parse Gemfile"
        };
        assert_gemfile_error(dir.path(), &[], &path, message);
    }
}

#[cfg(unix)]
#[test]
fn unreadable_gemfiles_fail() {
    use std::os::unix::fs::PermissionsExt;

    for filename in ["Gemfile", "gems.rb", "custom.gemfile", "shared.gemfile"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(filename);
        write(&path, "gem 'valid_gem'");
        if filename == "shared.gemfile" {
            write(&dir.path().join("Gemfile"), "eval_gemfile 'shared.gemfile'");
        }
        let permissions = std::fs::metadata(&path).unwrap().permissions();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o0)).unwrap();
        if std::fs::read(&path).is_ok() {
            std::fs::set_permissions(&path, permissions).unwrap();
            eprintln!("Skipping unreadable Gemfile test: process bypasses file permissions");
            return;
        }
        let extra = if filename == "custom.gemfile" {
            vec!["--gemfile", filename, "--allow-missing-gemfile"]
        } else {
            vec!["--allow-missing-gemfile"]
        };
        assert_gemfile_error(dir.path(), &extra, &path, "Could not read Gemfile");
        std::fs::set_permissions(&path, permissions).unwrap();
    }
}

#[test]
fn empty_gemfiles_remain_valid() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("Gemfile"), "");
    assert!(run_tool(dir.path()).is_empty());
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

fn path_gem_project(root: &Path) {
    write(
        &root.join("Gemfile"),
        "gem 'foopath', path: 'engines/foopath', require: false\n",
    );
    write(
        &root.join("engines/foopath/foopath.gemspec"),
        "Gem::Specification.new do |s|\n  s.name = 'foopath'\n  s.version = '1.0'\nend\n",
    );
    write(
        &root.join("engines/foopath/lib/foopath.rb"),
        "module Foopath\nend\n",
    );
}

#[test]
fn path_gem_cannot_use_its_own_requires_declarations_or_constants() {
    for code in [
        "require 'foopath'\n",
        "require 'foopath/internal'\n",
        "module Foopath::Internal\nend\n",
        "Foopath::Internal.new\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        path_gem_project(dir.path());
        write(&dir.path().join("engines/foopath/lib/internal.rb"), code);
        let results = run_json(dir.path(), &[]);
        assert_eq!(results["foopath"]["status"], "unused", "{code}");
        assert!(results["foopath"].get("evidence").is_none(), "{code}");
        assert!(results["foopath"].get("files").is_none(), "{code}");
    }
}

#[test]
fn path_gem_is_used_by_app_requires_or_constants_only() {
    for code in [
        "require 'foopath'\n",
        "require 'foopath/internal'\n",
        "Foopath::Internal.new\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        path_gem_project(dir.path());
        write(
            &dir.path().join("engines/foopath/lib/internal.rb"),
            "require 'foopath'\nrequire 'foopath/internal'\nFoopath::Internal.new\n",
        );
        write(&dir.path().join("app/usage.rb"), code);
        let results = run_json(dir.path(), &[]);
        assert_eq!(results["foopath"]["status"], "used", "{code}");
        assert_eq!(
            results["foopath"]["files"],
            serde_json::json!(["app/usage.rb"])
        );
        for evidence in results["foopath"]["evidence"].as_array().unwrap() {
            assert_eq!(evidence["file"], "app/usage.rb");
        }
    }
}

#[test]
fn path_gem_files_still_credit_other_gems() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    path_gem_project(root);
    write(
        &root.join("Gemfile"),
        "gem 'foopath', path: 'engines/foopath', require: false\n\
         gem 'json', require: false\n\
         gem 'redis', require: false\n",
    );
    write(
        &root.join("engines/foopath/lib/internal.rb"),
        "require 'json'\nRedis.new\n",
    );
    let results = run_json(root, &[]);
    assert_eq!(results["foopath"]["status"], "unused");
    for gem in ["json", "redis"] {
        assert_eq!(results[gem]["status"], "used");
        assert_eq!(
            results[gem]["files"],
            serde_json::json!(["engines/foopath/lib/internal.rb"])
        );
    }
}

#[test]
fn path_gem_directory_boundary_does_not_exclude_sibling_files() {
    let dir = tempfile::tempdir().unwrap();
    path_gem_project(dir.path());
    write(
        &dir.path().join("engines/foopath_extra/usage.rb"),
        "Foopath.new\n",
    );
    let results = run_json(dir.path(), &[]);
    assert_eq!(results["foopath"]["status"], "used");
    assert_eq!(
        results["foopath"]["files"],
        serde_json::json!(["engines/foopath_extra/usage.rb"])
    );
}

#[test]
fn path_gem_scripts_only_credit_external_dependencies() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    path_gem_project(root);
    write(
        &root.join("Gemfile"),
        "gem 'foopath', path: 'engines/foopath', require: false\n\
         gem 'rake', require: false\n",
    );
    write(
        &root.join("engines/foopath/Makefile"),
        "check:\n\tbundle exec foopath && bundle exec rake\n",
    );
    let results = run_json(root, &[]);
    assert_eq!(results["foopath"]["status"], "unused");
    assert_eq!(results["rake"]["status"], "used");
    assert_eq!(
        results["rake"]["files"],
        serde_json::json!(["engines/foopath/Makefile"])
    );
    write(&root.join("Makefile"), "check:\n\tbundle exec foopath\n");
    let results = run_json(root, &[]);
    assert_eq!(results["foopath"]["status"], "used");
    assert_eq!(results["foopath"]["files"], serde_json::json!(["Makefile"]));
}

#[test]
fn root_project_path_gems_are_exempt_from_self_evidence_filtering() {
    for gemfile in ["gem 'foopath', path: '.', require: false\n", "gemspec\n"] {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("Gemfile"), gemfile);
        write(
            &dir.path().join("foopath.gemspec"),
            "Gem::Specification.new do |s|\n  s.name = 'foopath'\n  s.version = '1.0'\nend\n",
        );
        write(&dir.path().join("lib/foopath.rb"), "module Foopath\nend\n");
        let results = run_json(dir.path(), &[]);
        assert_eq!(results["foopath"]["status"], "used", "{gemfile}");
        assert_eq!(
            results["foopath"]["files"],
            serde_json::json!(["lib/foopath.rb"])
        );
    }
}

#[cfg(unix)]
#[test]
fn path_gem_ownership_follows_symlink_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    path_gem_project(root);
    std::os::unix::fs::symlink(root.join("engines/foopath"), root.join("linked_foopath")).unwrap();
    write(
        &root.join("Gemfile"),
        "gem 'foopath', path: 'linked_foopath', require: false\n",
    );
    let results = run_json(root, &[]);
    assert_eq!(results["foopath"]["status"], "unused");
    assert!(results["foopath"].get("evidence").is_none());
}

#[test]
fn missing_locked_sources_do_not_supply_usage_evidence_and_warn_once() {
    for git in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let source = if git {
            ", git: 'https://example.test/locked_probe.git'"
        } else {
            ""
        };
        write(
            &root.join("Gemfile"),
            &format!("gem 'locked_probe', require: false{source}\n"),
        );
        write(&root.join(".bundle/config"), "BUNDLE_PATH: vendor/bundle\n");
        let installed = root.join("vendor/bundle/gems/locked_probe-1.0.0/lib");
        write(
            &installed.join("legacy_thing.rb"),
            "module LegacyThing; end\n",
        );
        write(
            &installed.join("locked_probe.rb"),
            "module LegacyThing; end\n",
        );
        if git {
            let checkout = root.join("vendor/bundle/bundler/gems/locked_probe-aaaaaaaaaaaa");
            write(
                &checkout.join("lib/legacy_thing.rb"),
                "module LegacyThing; end\n",
            );
            write(
                &checkout.join("lib/locked_probe.rb"),
                "module LegacyThing; end\n",
            );
            write(&checkout.join("locked_probe.gemspec"), "");
        }
        write(
            &root.join("app/probe.rb"),
            "require 'legacy_thing'\nLegacyThing.call\n",
        );
        for constrained in [false, true] {
            if constrained {
                let lock = if git {
                    "GIT\n  remote: https://example.test/locked_probe.git\n  revision: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n  specs:\n    locked_probe (1.0.0)\n"
                } else {
                    "GEM\n  specs:\n    locked_probe (2.0.0)\n"
                };
                write(&root.join("Gemfile.lock"), lock);
            }
            let output = Command::new(env!("CARGO_BIN_EXE_unused-gems"))
                .args(["--output", "json", "--no-cache", "--quiet"])
                .arg(root)
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(
                output.status.code(),
                Some(i32::from(constrained)),
                "git={git}, constrained={constrained}: {stderr}"
            );
            let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(
                json["results"][0]["status"],
                if constrained { "unused" } else { "used" },
                "git={git}, constrained={constrained}"
            );
            assert_eq!(
                stderr
                    .matches("Warning: locked artifact for gem 'locked_probe'")
                    .count(),
                usize::from(constrained),
                "{stderr}"
            );
        }
    }
}

#[test]
fn bundler_require_multiline_string_groups_and_trailing_code() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("Gemfile"),
        "source 'https://rubygems.org'\n\
         gem 'issue27default'\n\
         group :assets do\n  gem 'issue27assets'\nend\n\
         group :test do\n  gem 'issue27test'\nend\n",
    );
    write(
        &root.join("config/application.rb"),
        "Bundler.require(\n  :default,\n  \"assets\"\n); foo(:test)\n",
    );
    let results = run_tool(root);
    assert_eq!(status_of(&results, "issue27default"), "bundler_required");
    assert_eq!(status_of(&results, "issue27assets"), "bundler_required");
    assert_eq!(status_of(&results, "issue27test"), "unused");

    write(
        &root.join("config/application.rb"),
        "Bundler.require(:default); foo(:test)\n",
    );
    let results = run_tool(root);
    assert_eq!(status_of(&results, "issue27default"), "bundler_required");
    assert_eq!(status_of(&results, "issue27assets"), "unused");
    assert_eq!(status_of(&results, "issue27test"), "unused");
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

fn write_engine(dir: &Path) {
    write(
        &dir.join("foo_bar_engine.gemspec"),
        "Gem::Specification.new do |s|\n  s.name = \"foo_bar_engine\"\n  s.version = \"0.1\"\nend\n",
    );
    write(&dir.join("lib/fbe.rb"), "module Fbe; end\n");
}

#[test]
fn path_and_git_source_blocks_and_shorthands_are_introspected() {
    // The engine gem `foo_bar_engine` only provides lib/fbe.rb, so usage can
    // only be detected by introspecting the declared source checkout.
    let cases = [
        (
            "path block",
            "path \"engines/foo_bar_engine\" do\n  gem \"foo_bar_engine\"\nend\n",
        ),
        (
            "monorepo path block",
            "path \"engines\" do\n  gem \"foo_bar_engine\"\nend\n",
        ),
        (
            "path option",
            "gem \"foo_bar_engine\", path: \"engines/foo_bar_engine\"\n",
        ),
        (
            "github option",
            "gem \"foo_bar_engine\", github: \"x/foo_bar_engine\"\n",
        ),
        (
            "custom git_source option",
            "git_source(:corp) { |r| \"https://git.corp/#{r}.git\" }\ngem \"foo_bar_engine\", corp: \"x/foo_bar_engine\"\n",
        ),
        (
            "git block",
            "git \"https://example.com/x/foo_bar_engine.git\" do\n  gem \"foo_bar_engine\"\nend\n",
        ),
        (
            "github block",
            "github \"x/foo_bar_engine\" do\n  gem \"foo_bar_engine\"\nend\n",
        ),
    ];
    for (label, gemfile) in cases {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("Gemfile"), gemfile);
        write_engine(&root.join("engines/foo_bar_engine"));
        write_engine(
            &root.join("vendor/bundle/ruby/3.4.0/bundler/gems/foo_bar_engine-abc123def456"),
        );
        write(&root.join("app/x.rb"), "require \"fbe\"\n");

        let results = run_tool(root);
        assert_eq!(status_of(&results, "foo_bar_engine"), "used", "{label}");
    }
}

#[test]
fn per_declaration_bundler_require_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("Gemfile"),
        r#"source "https://rubygems.org"
gem "strgroup", group: "test"
group "development" do
  gem "strblock"
end
gem "strplat", platforms: "jruby"
platforms "jruby" do
  gem "strplatblock"
end
gem "installif", install_if: -> { false }
install_if -> { false } do
  gem "installifblock"
end
gem "installunknown", install_if: -> { ENV["CI"] }
group :development do
  gem "dupreq", require: false
end
gem "dupreq"
gem "reqnil", require: nil
gem "platboth", platform: :jruby, platforms: :ruby
gem "braces", ">= 0", { require: false }
platforms :jruby do
  gem "mixdup"
end
group :test do
  gem "mixdup"
end
"#,
    );
    write(&root.join("app/boot.rb"), "Bundler.require(:default)\n");

    let results = run_tool(root);
    // String groups/platforms are to_sym'd by Bundler.
    assert_eq!(groups_of(&results, "strgroup"), vec!["test"]);
    assert_eq!(status_of(&results, "strgroup"), "unused");
    assert_eq!(groups_of(&results, "strblock"), vec!["development"]);
    assert_eq!(status_of(&results, "strblock"), "unused");
    assert_eq!(status_of(&results, "strplat"), "platform_excluded");
    assert_eq!(status_of(&results, "strplatblock"), "platform_excluded");
    // `platform:` wins over `platforms:`.
    assert_eq!(status_of(&results, "platboth"), "platform_excluded");
    // Statically false install_if is never Bundler.require'd.
    assert_eq!(status_of(&results, "installif"), "unused");
    assert_eq!(status_of(&results, "installifblock"), "unused");
    assert_eq!(status_of(&results, "installunknown"), "bundler_required");
    // Any declaration with autorequire in an active group loads the gem.
    assert_eq!(status_of(&results, "dupreq"), "bundler_required");
    assert_eq!(status_of(&results, "reqnil"), "unused");
    assert_eq!(status_of(&results, "braces"), "unused");
    // The :default declaration is JRuby-only; on MRI only the :test one applies.
    assert_eq!(status_of(&results, "mixdup"), "unused");
    assert_eq!(groups_of(&results, "mixdup"), vec!["test"]);
}

#[test]
fn gemspec_in_group_block_uses_bundler_2_6_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("Gemfile"),
        "group :ci do\n  platforms :jruby do\n    gemspec\n  end\nend\n",
    );
    write(
        &root.join("mygem.gemspec"),
        "Gem::Specification.new do |s|\n  s.name = \"mygem\"\n  s.add_development_dependency \"devdep\"\nend\n",
    );
    write(
        &root.join("app/boot.rb"),
        "Bundler.require(:default, :development)\n",
    );

    let results = run_tool(root);
    assert_eq!(groups_of(&results, "mygem"), vec!["default"]);
    assert_eq!(status_of(&results, "mygem"), "bundler_required");
    assert_eq!(groups_of(&results, "devdep"), vec!["development"]);
    assert_eq!(status_of(&results, "devdep"), "bundler_required");
}

#[test]
fn github_annotations_point_at_declaring_file_and_line() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("Gemfile"),
        "source 'https://rubygems.org'\ngem 'root_gem'\neval_gemfile 'gemfiles/extra.gemfile'\n",
    );
    write(
        &root.join("gemfiles/extra.gemfile"),
        "\n\ngem 'extra_gem'\n",
    );
    write(&root.join("app.rb"), "# nothing\n");

    let bin = env!("CARGO_BIN_EXE_unused-gems");
    let output = Command::new(bin)
        .args(["--output", "github", "--no-cache", "--quiet"])
        .arg(root)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("file=Gemfile,line=2::Gem 'root_gem'"),
        "{stdout}"
    );
    assert!(
        stdout.contains("file=gemfiles/extra.gemfile,line=3::Gem 'extra_gem'"),
        "{stdout}"
    );
}
