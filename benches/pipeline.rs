//! Criterion benchmarks for the analyzer pipeline.
//!
//! Establishes baselines so README §9's latency claims become
//! load-bearing rather than aspirational. Three shapes:
//!
//! * `symbols_small` — syn-parse + top-level extraction on a
//!   hand-rolled small file. Fastest; tests raw syn overhead.
//! * `analyze_no_changes` — full `analyze()` pipeline on a git
//!   fixture with zero changes. The short-circuit path.
//! * `analyze_with_changes` — full pipeline on a fixture with a
//!   trait-signature change + impl + test. Exercises every
//!   analyzer end-to-end.
//!
//! Run locally: `cargo +1.95 bench --bench pipeline`
//! CI publishes the JSON reports as artifacts; SLO-regression
//! gating (fail CI if p50 exceeds baseline + 20%) is a follow-up
//! once we have a stable baseline committed to the repo.

use cargo_impact::{Finding, FindingKind, Format, Location, Tier, render_with_budget};
use criterion::{Criterion, criterion_group, criterion_main};
use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

fn manifest() -> &'static str {
    "[package]\nname=\"fixture\"\nversion=\"0.1.0\"\nedition=\"2021\"\n\n[lib]\npath=\"src/lib.rs\"\n"
}

fn seed_repo(initial: &[(&str, &str)], modifications: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "t@t"]);
    git(root, &["config", "user.name", "t"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    git(root, &["config", "core.autocrlf", "false"]);
    for (rel, body) in initial {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "init"]);
    for (rel, body) in modifications {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }
    dir
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

fn bench_render_formats(c: &mut Criterion) {
    // Render-layer microbenchmark — useful because format rendering is
    // the hot path when a client re-renders the same AnalysisReport in
    // several formats (e.g. CI producing both SARIF and pr-comment).
    let findings: Vec<Finding> = (0..50)
        .map(|i| {
            let kind = FindingKind::TestReference {
                test: Location {
                    file: PathBuf::from(format!("tests/t{i}.rs")),
                    symbol: format!("test_{i}"),
                },
                matched_symbols: vec!["login".into()],
            };
            Finding::new(
                format!("f-{i:04}"),
                Tier::Likely,
                0.85,
                kind,
                "synthetic evidence line long enough to matter for markdown budgets",
            )
        })
        .collect();
    let changed = vec![PathBuf::from("src/lib.rs"); 10];
    let symbols = vec!["login".to_string(), "logout".to_string()];

    let mut group = c.benchmark_group("render");
    for format in [
        Format::Text,
        Format::Markdown,
        Format::Json,
        Format::Sarif,
        Format::PrComment,
    ] {
        let name = format!("{format:?}");
        group.bench_function(name, |b| {
            b.iter(|| {
                let out = render_with_budget(
                    black_box(format),
                    black_box(&changed),
                    black_box(&symbols),
                    black_box(&findings),
                    black_box(0),
                )
                .unwrap();
                black_box(out);
            });
        });
    }
    group.finish();
}

fn bench_analyze_clean(c: &mut Criterion) {
    // "Nothing changed" is the most common case in practice (a user
    // just committed, their working tree is clean) — the short-circuit
    // exit path should be fast. This benchmark floors the cost of
    // invoking cargo-impact against a git repo with no diff.
    let dir = seed_repo(
        &[
            ("Cargo.toml", manifest()),
            ("src/lib.rs", "pub fn a() {}\n"),
        ],
        &[], // clean working tree
    );
    let root = dir.path().to_path_buf();

    c.bench_function("analyze_no_changes", |b| {
        b.iter(|| {
            let args = cargo_impact::ImpactArgs {
                test: false,
                format: Format::Json,
                since: "HEAD".into(),
                manifest_dir: Some(root.clone()),
                confidence_min: 0.0,
                fail_on: None,
                semver_checks: false,
                rust_analyzer: false,
                features: Vec::new(),
                all_features: false,
                no_default_features: false,
                budget: 0,
                context: false,
                feature_powerset: false,
                macro_expand: false,
            };
            let report = cargo_impact::analyze(&args).unwrap();
            black_box(report);
        });
    });
}

fn bench_analyze_with_changes(c: &mut Criterion) {
    // Realistic small-diff case: one trait whose required-method
    // signature changed, one impl, one test. Every analyzer that
    // exists today has a path here (trait_methods, traits, derive,
    // tests_scan, diff).
    let dir = seed_repo(
        &[
            ("Cargo.toml", manifest()),
            (
                "src/lib.rs",
                "pub trait Greeter { fn hi(&self) -> u32; }\n\
                 pub struct Friend;\n\
                 impl Greeter for Friend { fn hi(&self) -> u32 { 1 } }\n\
                 #[cfg(test)]\n\
                 mod tests {\n\
                   use super::*;\n\
                   #[test] fn greets() { Friend.hi(); }\n\
                 }\n",
            ),
        ],
        &[(
            "src/lib.rs",
            "pub trait Greeter { fn hi(&self) -> String; }\n\
             pub struct Friend;\n\
             impl Greeter for Friend { fn hi(&self) -> String { String::new() } }\n\
             #[cfg(test)]\n\
             mod tests {\n\
               use super::*;\n\
               #[test] fn greets() { let _ = Friend.hi(); }\n\
             }\n",
        )],
    );
    let root = dir.path().to_path_buf();

    c.bench_function("analyze_with_changes", |b| {
        b.iter(|| {
            let args = cargo_impact::ImpactArgs {
                test: false,
                format: Format::Json,
                since: "HEAD".into(),
                manifest_dir: Some(root.clone()),
                confidence_min: 0.0,
                fail_on: None,
                semver_checks: false,
                rust_analyzer: false,
                features: Vec::new(),
                all_features: false,
                no_default_features: false,
                budget: 0,
                context: false,
                feature_powerset: false,
                macro_expand: false,
            };
            let report = cargo_impact::analyze(&args).unwrap();
            assert!(
                !report.findings.is_empty(),
                "fixture should produce findings"
            );
            black_box(report);
        });
    });
}

criterion_group!(
    benches,
    bench_render_formats,
    bench_analyze_clean,
    bench_analyze_with_changes,
);
criterion_main!(benches);
