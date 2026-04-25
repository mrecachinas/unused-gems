//! Accuracy harness: runs `unused-gems` against a labeled corpus of small
//! Ruby fixtures and computes precision / recall / F1 for the binary
//! "unused" prediction.
//!
//! Run with: `cargo test --test accuracy -- --nocapture`
//!
//! See `tests/accuracy_fixtures/README.md` for fixture format.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Label {
    Used,
    Unused,
}

impl Label {
    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "used" => Some(Self::Used),
            "unused" => Some(Self::Unused),
            _ => None,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Confusion {
    tp: u32,  // labelled unused, predicted unused
    fp: u32,  // labelled used,   predicted unused
    fn_: u32, // labelled unused, predicted used
    tn: u32,  // labelled used,   predicted used
}

impl Confusion {
    fn add(&mut self, other: &Confusion) {
        self.tp += other.tp;
        self.fp += other.fp;
        self.fn_ += other.fn_;
        self.tn += other.tn;
    }

    fn precision(&self) -> f64 {
        let denom = self.tp + self.fp;
        if denom == 0 {
            1.0
        } else {
            self.tp as f64 / denom as f64
        }
    }

    fn recall(&self) -> f64 {
        let denom = self.tp + self.fn_;
        if denom == 0 {
            1.0
        } else {
            self.tp as f64 / denom as f64
        }
    }

    fn f1(&self) -> f64 {
        let p = self.precision();
        let r = self.recall();
        if p + r == 0.0 {
            0.0
        } else {
            2.0 * p * r / (p + r)
        }
    }

    fn accuracy(&self) -> f64 {
        let total = self.tp + self.fp + self.fn_ + self.tn;
        if total == 0 {
            1.0
        } else {
            (self.tp + self.tn) as f64 / total as f64
        }
    }
}

/// Minimal YAML reader for the simple `key: value` map shape the
/// fixtures use. We deliberately avoid pulling `serde_yaml` into
/// the test target — the grammar here is trivial.
fn parse_expected(path: &Path) -> BTreeMap<String, Label> {
    let body =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    let mut out = BTreeMap::new();
    for (lineno, raw) in body.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (k, v) = line
            .split_once(':')
            .unwrap_or_else(|| panic!("{}:{}: expected `name: label`", path.display(), lineno + 1));
        let label = Label::parse(v).unwrap_or_else(|| {
            panic!(
                "{}:{}: unknown label '{}', expected 'used' or 'unused'",
                path.display(),
                lineno + 1,
                v.trim()
            )
        });
        out.insert(k.trim().to_string(), label);
    }
    out
}

fn run_tool(fixture: &Path) -> serde_json::Value {
    let bin = env!("CARGO_BIN_EXE_unused-gems");
    let output = Command::new(bin)
        .arg("--output")
        .arg("json")
        .arg(fixture)
        .output()
        .unwrap_or_else(|e| panic!("invoke {bin}: {e}"));

    // Exit code is 0 when nothing is unused, 1 when something is — both
    // are valid here. Any other code is a real failure (missing Gemfile,
    // IO error, etc.) and should fail the test.
    let code = output.status.code().unwrap_or(-1);
    assert!(
        code == 0 || code == 1,
        "unused-gems on {} exited {}: stderr={}",
        fixture.display(),
        code,
        String::from_utf8_lossy(&output.stderr)
    );

    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("parse JSON from {}: {}", fixture.display(), e))
}

fn predict_from_status(status: &str) -> Label {
    // Anything other than "unused" counts as a positive usage signal.
    if status == "unused" {
        Label::Unused
    } else {
        Label::Used
    }
}

struct FixtureReport {
    name: String,
    confusion: Confusion,
    mismatches: Vec<String>,
}

fn evaluate_fixture(fixture: &Path) -> FixtureReport {
    let name = fixture.file_name().unwrap().to_string_lossy().into_owned();
    let expected = parse_expected(&fixture.join("expected.yml"));
    let json = run_tool(fixture);

    let results = json
        .get("results")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("{}: missing 'results' array", name));

    let mut predicted: BTreeMap<String, Label> = BTreeMap::new();
    for entry in results {
        let gem = entry["name"].as_str().unwrap_or("").to_string();
        let status = entry["status"].as_str().unwrap_or("");
        predicted.insert(gem, predict_from_status(status));
    }

    let mut conf = Confusion::default();
    let mut mismatches = Vec::new();

    for (gem, &exp) in &expected {
        let pred = predicted.get(gem).copied().unwrap_or_else(|| {
            panic!(
                "{}: gem '{}' is in expected.yml but tool produced no result for it",
                name, gem
            )
        });
        match (exp, pred) {
            (Label::Unused, Label::Unused) => conf.tp += 1,
            (Label::Used, Label::Used) => conf.tn += 1,
            (Label::Used, Label::Unused) => {
                conf.fp += 1;
                mismatches.push(format!("  FP: {gem} — labelled used, tool said unused"));
            }
            (Label::Unused, Label::Used) => {
                conf.fn_ += 1;
                mismatches.push(format!("  FN: {gem} — labelled unused, tool said used"));
            }
        }
    }

    // Surface gems the tool reported but that are missing from labels —
    // every gem in the Gemfile must be labeled.
    for gem in predicted.keys() {
        if !expected.contains_key(gem) {
            panic!(
                "{}: gem '{}' is in tool output but missing from expected.yml — \
                 every gem in the fixture's Gemfile must have a label",
                name, gem
            );
        }
    }

    FixtureReport {
        name,
        confusion: conf,
        mismatches,
    }
}

fn discover_fixtures() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("accuracy_fixtures");
    let mut fixtures: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read {}: {}", root.display(), e))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("Gemfile").exists() && p.join("expected.yml").exists())
        .collect();
    fixtures.sort();
    assert!(
        !fixtures.is_empty(),
        "no fixtures found under {}",
        root.display()
    );
    fixtures
}

#[test]
fn accuracy_corpus() {
    let fixtures = discover_fixtures();
    let mut total = Confusion::default();
    let mut reports = Vec::new();

    for fix in &fixtures {
        let report = evaluate_fixture(fix);
        total.add(&report.confusion);
        reports.push(report);
    }

    println!();
    println!("=== unused-gems accuracy corpus ===");
    println!(
        "{:<40} {:>4} {:>4} {:>4} {:>4}   {:>5}  {:>5}  {:>5}  {:>5}",
        "fixture", "TP", "FP", "FN", "TN", "Prec", "Rec", "F1", "Acc"
    );
    for r in &reports {
        let c = &r.confusion;
        println!(
            "{:<40} {:>4} {:>4} {:>4} {:>4}   {:>5.3}  {:>5.3}  {:>5.3}  {:>5.3}",
            r.name,
            c.tp,
            c.fp,
            c.fn_,
            c.tn,
            c.precision(),
            c.recall(),
            c.f1(),
            c.accuracy()
        );
        for line in &r.mismatches {
            println!("{line}");
        }
    }
    println!(
        "{:-<40} {:->4} {:->4} {:->4} {:->4}   {:->5}  {:->5}  {:->5}  {:->5}",
        "", "", "", "", "", "", "", "", ""
    );
    println!(
        "{:<40} {:>4} {:>4} {:>4} {:>4}   {:>5.3}  {:>5.3}  {:>5.3}  {:>5.3}",
        "TOTAL",
        total.tp,
        total.fp,
        total.fn_,
        total.tn,
        total.precision(),
        total.recall(),
        total.f1(),
        total.accuracy()
    );
    println!();

    // Hard regression gate: the bundled corpus must score perfectly.
    // External corpora (added later) should be wired in via a
    // separate, looser-threshold test.
    assert_eq!(
        total.fp, 0,
        "false positives detected on bundled corpus (see lines marked 'FP:' above)"
    );
    assert_eq!(
        total.fn_, 0,
        "false negatives detected on bundled corpus (see lines marked 'FN:' above)"
    );
}
