use std::process::Command;

fn scan(file_name: &str, contents: &str) -> serde_json::Value {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("Gemfile"),
        "gem 'rubocop', require: false\ngem 'puma', require: false\ngem 'rspec', require: false\n",
    )
    .unwrap();
    std::fs::write(project.path().join(file_name), contents).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_unused-gems"))
        .args(["--output", "json", "--no-cache", "--quiet"])
        .arg(project.path())
        .output()
        .unwrap();
    assert!(
        matches!(output.status.code(), Some(0 | 1)),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn assert_usage(json: &serde_json::Value, used: &[&str]) {
    let results = json["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    for result in results {
        let name = result["name"].as_str().unwrap();
        assert_eq!(
            result["status"].as_str().unwrap() != "unused",
            used.contains(&name),
            "{result}"
        );
    }
}

#[test]
fn drone_commands_lists() {
    for file_name in [".drone.yml", ".drone.yaml"] {
        let json = scan(
            file_name,
            "kind: pipeline\nsteps:\n  - name: test\n    commands:\n      - puma -C config/puma.rb\n      - rspec spec/\n    environment:\n      NOTES: rubocop\n",
        );
        assert_usage(&json, &["puma", "rspec"]);
    }
}

#[test]
fn mise_task_run_string() {
    for file_name in ["mise.toml", ".mise.toml"] {
        let json = scan(
            file_name,
            "[tasks.lint]\nrun = \"rubocop --parallel\" # puma\n",
        );
        assert_usage(&json, &["rubocop"]);
    }
}

#[test]
fn mise_task_run_array() {
    for file_name in ["mise.toml", ".mise.toml"] {
        let json = scan(
            file_name,
            "[tasks.test]\nrun = [\n  'rubocop',\n  'RAILS_ENV=test rspec',\n  './bin/puma',\n]\n",
        );
        assert_usage(&json, &["rubocop", "puma", "rspec"]);
    }
}

#[test]
fn mise_short_tasks() {
    for file_name in ["mise.toml", ".mise.toml"] {
        let json = scan(
            file_name,
            "[tasks]\n\"lint.all\" = \"rubocop\"\ntest = 'echo ready && rspec'\n",
        );
        assert_usage(&json, &["rubocop", "rspec"]);
    }
}

#[test]
fn mise_decodes_escapes_and_multiline_strings() {
    let json = scan(
        "mise.toml",
        r##"[tasks.lint]
run = "\u0072ubocop"
[tasks.test]
run = """
echo "# rspec is next"
rspec
# puma is not run
"""
"##,
    );
    assert_usage(&json, &["rubocop", "rspec"]);
}

#[test]
fn drone_prose_comments_and_arguments_do_not_count() {
    let json = scan(
        ".drone.yml",
        "kind: pipeline\nsteps:\n  - name: bundle exec rubocop\n    description: bin/puma\n    commands:\n      # rspec\n      - echo rubocop puma rspec\n      - echo 'bundle exec rubocop; bin/puma'\n    environment:\n      NOTES: rspec\n",
    );
    assert_usage(&json, &[]);
}

#[test]
fn mise_prose_comments_and_arguments_do_not_count() {
    for file_name in ["mise.toml", ".mise.toml"] {
        let json = scan(
            file_name,
            r#"# bundle exec rspec
[env]
run = "puma"
NOTES = "bundle exec rubocop"
[tools]
rubocop = "latest"
[tasks.rspec]
description = "bundle exec rspec; bin/puma"
depends = ["rubocop"]
run = ["echo rubocop puma rspec", "echo 'bundle exec rubocop; bin/puma'"]
[tasks.rspec.env]
run = "rubocop"
"#,
        );
        assert_usage(&json, &[]);
    }
}

#[test]
fn mise_invalid_toml_is_not_scanned_as_shell() {
    let json = scan("mise.toml", "[tasks.lint\nrun = \"bundle exec rubocop\"\n");
    assert_usage(&json, &[]);
}

#[test]
fn mise_arguments_do_not_count_in_any_task_form() {
    for contents in [
        "[tasks.lint]\nrun = \"echo rubocop puma rspec\"\n",
        "[tasks.lint]\nrun = [\"echo rubocop\", \"echo puma\", \"echo rspec\"]\n",
        "[tasks]\nlint = \"echo rubocop puma rspec\"\n",
    ] {
        assert_usage(&scan("mise.toml", contents), &[]);
    }
}

#[test]
fn drone_quoted_commands() {
    let json = scan(
        ".drone.yml",
        "kind: pipeline\nsteps:\n  - name: test\n    commands:\n      - 'bundle exec rubocop'\n      - \"rspec spec/\"\n      - 'echo puma'\n",
    );
    assert_usage(&json, &["rubocop", "rspec"]);
}
