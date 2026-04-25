# Accuracy Fixtures

Each subdirectory is a self-contained mini Ruby project used to measure
the precision and recall of `unused-gems`.

## Layout

```
tests/accuracy_fixtures/
  <fixture_name>/
    Gemfile           # the project's gem declarations
    Gemfile.lock      # (optional) pinned versions
    expected.yml      # ground-truth labels (see below)
    <source files>    # a minimal source tree exercising the scenario
```

## Label format

`expected.yml` is a YAML map of `gem_name -> label`. Labels:

* `used`   — the gem is actually exercised by the project (directly or
             via Bundler/`require: false` patterns); reporting it as
             `unused` would be a **false positive**.
* `unused` — the gem is not used anywhere; failing to flag it would be
             a **false negative**.

Example:

```yaml
foo_lib: used
bar_lib: unused
```

## How fixtures map to the tool's output

The harness collapses the tool's 4-status output to a binary class:

| Tool status        | Predicted |
|--------------------|-----------|
| `unused`           | unused    |
| `used`             | used      |
| `bundler_required` | used      |
| `require_false`    | used      |

## Adding a new fixture

1. Create a directory under `tests/accuracy_fixtures/`.
2. Add a small `Gemfile` and just enough source code to exercise the
   scenario you care about.
3. Write `expected.yml` with the ground-truth labels for every gem in
   the Gemfile.
4. Run `cargo test --test accuracy -- --nocapture` to see the
   confusion matrix and per-fixture details.
