//! Compare the production scanner with legacy regex oracles. Intentional
//! differences (Ruby lexical filtering and Bundler semantics) are asserted
//! separately rather than treating the regexes as the specification.

use std::sync::Arc;

use regex::Regex;
use unused_gems::{cache::FileCache, scanner, types::ScanResults};

fn scan(input: &str) -> ScanResults {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("input.rb"), input).unwrap();
    scanner::scan_source_tree_with_threads(
        dir.path(),
        &[],
        Arc::new(FileCache::disabled()),
        Some(1),
    )
    .0
}

fn regex_captures(pattern: &str, input: &str) -> Vec<String> {
    Regex::new(pattern)
        .unwrap()
        .captures_iter(input)
        .map(|c| c[1].to_string())
        .collect()
}

fn regex_requires(input: &str) -> Vec<String> {
    regex_captures(
        r"(?:require|require_dependency)\s*[\(]?\s*['\x22]([^'\x22]+)['\x22]\s*[\)]?",
        input,
    )
}

fn regex_autoloads(input: &str) -> Vec<String> {
    regex_captures(
        r"autoload\s*[\(]?\s*:\w+\s*,\s*['\x22]([^'\x22]+)['\x22]",
        input,
    )
}

fn regex_gem_bin(input: &str) -> Vec<String> {
    regex_captures(
        r"Gem\.(?:activate_)?bin_path\s*\(\s*['\x22]([^'\x22]+)['\x22]",
        input,
    )
}

fn regex_modules(input: &str) -> Vec<String> {
    regex_captures(r"\b([A-Z][a-zA-Z0-9]+(?:::[A-Z][a-zA-Z0-9]+)*)\b", input)
}

fn regex_bundler(input: &str) -> (Vec<String>, bool) {
    let mut groups = Vec::new();
    let mut all = false;
    for rest in regex_captures(r"Bundler\.require\b(.*)", input) {
        let rest = rest.trim();
        if rest.is_empty()
            || rest.starts_with('#')
            || rest.contains("Rails.groups")
            || rest.contains("Rails.env")
        {
            all = true;
        } else {
            groups.extend(regex_captures(r":(\w+)", rest));
        }
    }
    (groups, all)
}

fn sorted(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut values: Vec<_> = values.into_iter().collect();
    values.sort();
    values.dedup();
    values
}

#[test]
fn requires_match_regex_including_multiline_calls() {
    for input in [
        "require 'json'",
        "require \"json\"",
        "require('json')",
        "require_dependency 'foo/bar'",
        "  require 'json'",
        "require\t'json'",
        "require_relative 'foo'",
        "require_all 'foo'",
        "required 'foo'",
        "require ( 'json' )",
        "require ''",
        "require\n'json'",
        "require(\n'json')",
    ] {
        assert_eq!(
            sorted(scan(input).requires.into_keys()),
            sorted(regex_requires(input)),
            "{input:?}"
        );
    }
    assert!(scan("require(\n'json')").requires.contains_key("json"));
}

#[test]
fn autoloads_match_regex_including_multiline_calls() {
    for input in [
        "autoload :Foo, 'foo'",
        "autoload(:Foo, 'foo')",
        "autoload :Foo, \"foo/bar\"",
        "autoload :, 'foo'",
        "autoload\n:Foo, 'foo'",
    ] {
        assert_eq!(
            sorted(scan(input).requires.into_keys()),
            sorted(regex_autoloads(input)),
            "{input:?}"
        );
    }
}

#[test]
fn gem_bin_matches_regex_including_multiline_calls() {
    for input in [
        "Gem.bin_path('rake')",
        "Gem.activate_bin_path('rake')",
        "Gem.bin_path( 'rake' )",
        "Gem.bin_path\n('rake')",
    ] {
        assert_eq!(
            sorted(scan(input).requires.into_keys()),
            sorted(regex_gem_bin(input)),
            "{input:?}"
        );
    }
    assert!(scan("Gem.activate_bin_path 'rake', 'rake'")
        .requires
        .contains_key("rake"));
}

#[test]
fn require_and_autoload_keywords_need_word_boundaries() {
    assert_eq!(regex_requires("gem_require 'foo'"), ["foo"]);
    assert!(scan("gem_require 'foo'").requires.is_empty());
    assert_eq!(regex_autoloads("my_autoload :Foo, 'foo'"), ["foo"]);
    assert!(scan("my_autoload :Foo, 'foo'").requires.is_empty());
}

#[test]
fn requires_use_matching_quotes_not_the_regex_quote_class() {
    for (input, expected) in [
        (r#"require 'foo"bar'"#, "foo\"bar"),
        (r#"require "foo'bar""#, "foo'bar"),
        (r#"autoload :Foo, 'bar"baz'"#, "bar\"baz"),
        (r#"Gem.bin_path('foo"bar')"#, "foo\"bar"),
    ] {
        assert_eq!(
            sorted(scan(input).requires.into_keys()),
            [expected],
            "{input:?}"
        );
    }
    assert_eq!(regex_requires(r#"require 'foo"bar'"#), ["foo"]);
    assert_eq!(regex_requires("require \"foo\nbar\""), ["foo\nbar"]);
    assert!(scan("require \"foo\nbar\"").requires.is_empty());
    assert_eq!(regex_requires(r"require 'foo\'bar'"), [r"foo\"]);
    assert_eq!(
        sorted(scan(r"require 'foo\'bar'").requires.into_keys()),
        [r"foo\"]
    );
}

#[test]
fn modules_match_regex_for_constants_and_namespace_backtracking() {
    for input in [
        "Rails",
        "ActiveRecord::Base",
        "Foo::Bar::Baz",
        "Foo2Bar",
        "A",
        "Fo",
        "use Rails and ActiveSupport::Concern here",
        "_Foo",
        "Foo_",
        "Foo_bar",
        "Foo_Bar",
        "Foo::Bar_Baz",
        "Foo::Bar::Baz_Qux",
        "Foo::Bar3_",
        "Foo::B",
        "Foo::bar",
        "Foo::",
        ".Foo",
        "Foo::Bar and Baz::Qux",
        "use ActiveRecord",
    ] {
        assert_eq!(
            sorted(scan(input).modules.into_keys()),
            sorted(regex_modules(input)),
            "{input:?}"
        );
    }
}

#[test]
fn module_boundaries_are_ascii_based() {
    for (input, expected) in [
        ("Foo\u{00e9}", vec!["Foo"]),
        ("\u{00e9}Foo", vec!["Foo"]),
        ("Foo\u{00e9}Bar", vec!["Bar", "Foo"]),
    ] {
        assert!(regex_modules(input).is_empty());
        assert_eq!(
            sorted(scan(input).modules.into_keys()),
            expected,
            "{input:?}"
        );
    }
}

#[test]
fn symbols_and_variables_are_not_constant_references() {
    for input in [":Foo", "@Foo", "@@Foo", "$Foo"] {
        assert_eq!(regex_modules(input), ["Foo"]);
        assert!(scan(input).modules.is_empty(), "{input:?}");
    }
}

#[test]
fn bundler_explicit_groups_match_regex() {
    for input in [
        "Bundler.require(:default, :development)",
        "Bundler.required",
    ] {
        let result = scan(input);
        let (groups, all) = regex_bundler(input);
        assert_eq!(sorted(result.bundler_groups), sorted(groups), "{input:?}");
        assert_eq!(result.bundler_require_all, all, "{input:?}");
    }
}

#[test]
fn bundler_no_args_means_default_not_all_groups() {
    for input in [
        "Bundler.require",
        "Bundler.require()",
        "Bundler.require # load all",
        "xBundler.require",
    ] {
        let result = scan(input);
        assert_eq!(sorted(result.bundler_groups), ["default"], "{input:?}");
        assert!(!result.bundler_require_all, "{input:?}");
    }
    let result = scan("Bundler.require(:default, Rails.env)");
    assert_eq!(sorted(result.bundler_groups), ["default"]);
    assert!(result.bundler_require_all);
    assert_eq!(
        regex_bundler("Bundler.require(:default, Rails.env)"),
        (vec![], true)
    );
    let result = scan("Bundler.require(*Rails.groups)");
    assert_eq!(sorted(result.bundler_groups), ["default"]);
    assert!(result.bundler_require_all);
}

#[test]
fn production_lexical_filtering_excludes_non_code() {
    let result = scan(
        r##"# require 'commented'
=begin
autoload :Hidden, 'block_comment'
=end
message = "require 'string_literal' HiddenConstant Bundler.require(:test)"
doc = <<~TEXT
  require 'heredoc'
  HiddenConstant
TEXT
require 'real' # require 'inline_comment'
RealConstant.call
Bundler.require(:default) # :test
__END__
require 'data_section'
"##,
    );
    assert_eq!(sorted(result.requires.into_keys()), ["real"]);
    assert_eq!(
        sorted(result.modules.into_keys()),
        ["Bundler", "RealConstant"]
    );
    assert_eq!(sorted(result.bundler_groups), ["default"]);
    assert!(!result.bundler_require_all);
}
