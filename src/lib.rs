//! `cargo-impact` — blast-radius analysis for Rust workspaces.
//!
//! This is v0.2 per the README §11 roadmap. Shipped:
//!
//! * Confidence tiers with numeric scores ([`finding::Tier`])
//! * Test-reference detection (`Likely 0.85`)
//! * Trait ripple — `impl Trait for T` blocks flagged when the trait
//!   definition lives in a changed file (`Likely 0.80`, High severity)
//! * `dyn Trait` dispatch sites for changed traits (`Likely 0.75`)
//! * Documentation drift — intra-doc links (`Likely 0.90`) and
//!   keyword mentions (`Possible 0.40`) for changed symbols
//! * `--format={text,markdown,json}` — JSON envelope matches README §8
//! * `--confidence-min` and `--fail-on={high,medium,low}` for CI
//!
//! Deferred to a follow-up pass (still within v0.2 scope):
//! macro expansion, `cargo-semver-checks` integration, and live
//! `--features` / `--all-features` re-analysis. See §11.
//!
//! # Programmatic use
//!
//! ```
//! use cargo_impact::{nextest_filter, Finding, FindingKind, Location, Tier};
//! use std::path::PathBuf;
//!
//! let kind = FindingKind::TestReference {
//!     test: Location { file: PathBuf::from("tests/a.rs"), symbol: "smoke".into() },
//!     matched_symbols: vec!["login".into()],
//! };
//! let findings = [Finding::new("f-0001", Tier::Likely, 0.85, kind, "ref")];
//! assert_eq!(nextest_filter(&findings), "test(smoke)");
//! ```

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::BTreeSet;
use std::path::PathBuf;

mod doc_drift;
mod dyn_dispatch;
pub mod finding;
pub mod format;
mod git;
mod nextest;
mod symbols;
mod tests_scan;
mod traits;

pub use finding::{Finding, FindingKind, Location, SeverityClass, Tier, TierSummary};
pub use format::{render as render_report, Format};
pub use nextest::filter_expression as nextest_filter;

/// Command-line arguments for `cargo impact`.
#[derive(Parser, Debug)]
#[command(
    name = "cargo-impact",
    bin_name = "cargo-impact",
    version,
    about = "Blast-radius analysis for Rust workspaces",
    long_about = None,
)]
pub struct ImpactArgs {
    /// Emit a `cargo-nextest` filter expression instead of the structured
    /// report. Overrides `--format`.
    #[arg(long)]
    pub test: bool,

    /// Output format for the structured report.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,

    /// Git ref to diff against. Uncommitted (staged + unstaged) changes are
    /// always included regardless of this value.
    #[arg(long, default_value = "HEAD")]
    pub since: String,

    /// Repository root. Defaults to the current working directory.
    #[arg(long)]
    pub manifest_dir: Option<PathBuf>,

    /// Hide findings whose confidence is below this threshold (0.0–1.0).
    #[arg(long, default_value_t = 0.0)]
    pub confidence_min: f64,

    /// Exit non-zero if any finding at this severity class or above is
    /// emitted. Useful for CI gating. `high` only gates on high-severity
    /// findings; `medium` gates on medium+high; `low` gates on any non-unknown.
    #[arg(long, value_enum)]
    pub fail_on: Option<FailOn>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum FailOn {
    High,
    Medium,
    Low,
}

impl FailOn {
    fn triggers(self, sev: SeverityClass) -> bool {
        matches!(
            (self, sev),
            (Self::High, SeverityClass::High)
                | (Self::Medium, SeverityClass::High | SeverityClass::Medium)
                | (
                    Self::Low,
                    SeverityClass::High | SeverityClass::Medium | SeverityClass::Low,
                )
        )
    }
}

/// Run the analysis and print the configured output. Returns `Ok(exit_code)`
/// where a non-zero code indicates `--fail-on` triggered; errors during
/// analysis (I/O, git, parse failures in the orchestrator) propagate via the
/// `Result` and map to exit code 2 in `main`.
pub fn run(args: &ImpactArgs) -> Result<i32> {
    let root = match &args.manifest_dir {
        Some(p) => p.clone(),
        None => std::env::current_dir().context("reading current directory")?,
    };

    let changed_files = git::changed_rust_files(&root, &args.since)?;
    if changed_files.is_empty() {
        if args.test {
            println!();
        } else if matches!(args.format, Format::Text) {
            println!(
                "cargo-impact: no Rust files changed relative to {}",
                args.since
            );
        } else {
            // Structured empty report so agents don't have to special-case
            // "no output" vs. "no changes".
            let out = render_report(args.format, &[], &[], &[])?;
            println!("{out}");
        }
        return Ok(0);
    }

    // Collect symbols from each changed file. Parse errors downgrade to a
    // stderr notice and skip the file — a single bad file must not kill the
    // whole run.
    let mut all_symbols: Vec<symbols::TopLevelSymbol> = Vec::new();
    for rel in &changed_files {
        let abs = root.join(rel);
        match symbols::top_level_symbols(&abs) {
            Ok(syms) => all_symbols.extend(syms),
            Err(e) => eprintln!("cargo-impact: skipping {}: {e:#}", rel.display()),
        }
    }
    let symbol_names: BTreeSet<String> = all_symbols.iter().map(|s| s.name.clone()).collect();
    let changed_trait_names = traits::changed_trait_names(&all_symbols);

    // Run analyzers. Each returns findings with empty IDs which the
    // orchestrator assigns sequentially below.
    let mut findings = Vec::new();
    findings.extend(tests_scan::find_affected_tests(&root, &symbol_names)?);
    findings.extend(traits::find_trait_impls(&root, &changed_trait_names)?);
    findings.extend(dyn_dispatch::find_dyn_dispatch_sites(
        &root,
        &changed_trait_names,
    )?);
    findings.extend(doc_drift::find_doc_drift(&root, &symbol_names)?);

    // Filter by confidence before assigning IDs so IDs are stable for the
    // visible findings.
    findings.retain(|f| f.confidence >= args.confidence_min);

    // Stable sort: severity (High first), then tier (Proven first), then by
    // content so IDs are deterministic across runs.
    findings.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then_with(|| b.tier.rank().cmp(&a.tier.rank()))
            .then_with(|| a.kind.tag().cmp(b.kind.tag()))
            .then_with(|| a.evidence.cmp(&b.evidence))
    });

    for (i, f) in findings.iter_mut().enumerate() {
        f.id = format!("f-{:04}", i + 1);
    }

    // Nextest filter short-circuit — ignores format flag.
    if args.test {
        println!("{}", nextest::filter_expression(&findings));
        return Ok(0);
    }

    let sorted_symbols: Vec<String> = {
        let mut v: Vec<String> = symbol_names.into_iter().collect();
        v.sort();
        v
    };

    let out = render_report(args.format, &changed_files, &sorted_symbols, &findings)?;
    println!("{out}");

    // --fail-on evaluation.
    if let Some(gate) = args.fail_on {
        let tripped = findings.iter().any(|f| gate.triggers(f.severity));
        if tripped {
            return Ok(1);
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fail_on_high_triggers_on_high_only() {
        assert!(FailOn::High.triggers(SeverityClass::High));
        assert!(!FailOn::High.triggers(SeverityClass::Medium));
        assert!(!FailOn::High.triggers(SeverityClass::Low));
        assert!(!FailOn::High.triggers(SeverityClass::Unknown));
    }

    #[test]
    fn fail_on_medium_triggers_on_medium_and_above() {
        assert!(FailOn::Medium.triggers(SeverityClass::High));
        assert!(FailOn::Medium.triggers(SeverityClass::Medium));
        assert!(!FailOn::Medium.triggers(SeverityClass::Low));
    }

    #[test]
    fn fail_on_low_triggers_on_everything_but_unknown() {
        assert!(FailOn::Low.triggers(SeverityClass::High));
        assert!(FailOn::Low.triggers(SeverityClass::Medium));
        assert!(FailOn::Low.triggers(SeverityClass::Low));
        assert!(!FailOn::Low.triggers(SeverityClass::Unknown));
    }
}
