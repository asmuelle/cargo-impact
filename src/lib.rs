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

mod adapters;
mod cfg;
mod config;
mod dedup;
mod derive;
mod diff;
mod doc_drift;
mod dyn_dispatch;
mod ffi;
pub mod finding;
pub mod format;
mod git;
mod ignore;
pub mod log_miss;
pub mod mcp;
mod nextest;
mod rust_analyzer;
mod semver_checks;
mod symbols;
mod tests_scan;
mod trait_methods;
mod traits;

pub use finding::{Finding, FindingKind, Location, SeverityClass, Tier, TierSummary};
pub use format::{Format, render as render_report, render_with_budget};
pub use nextest::filter_expression as nextest_filter;

/// Deduped list of files implicated by the blast radius. Combines the
/// raw `changed_files` from git with each finding's `primary_path`.
/// Used by the `--context` short-circuit and exposed publicly so
/// downstream tooling can compute the same set without re-running the
/// analyzers.
pub fn context_file_list(report: &AnalysisReport) -> Vec<std::path::PathBuf> {
    let mut set: std::collections::BTreeSet<std::path::PathBuf> =
        report.changed_files.iter().cloned().collect();
    for f in &report.findings {
        if let Some(p) = f.primary_path() {
            set.insert(p.to_path_buf());
        }
    }
    set.into_iter().collect()
}

/// Command-line arguments for `cargo impact`.
#[derive(Parser, Debug, Clone)]
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

    /// Opt in to public-API breakage detection via `cargo-semver-checks`.
    /// Off by default because the underlying tool builds rustdoc JSON twice
    /// and typically takes 10–30 seconds. Requires `cargo-semver-checks` on
    /// `PATH`; if absent, a stderr warning is printed and the check is
    /// skipped (non-fatal).
    #[arg(long)]
    pub semver_checks: bool,

    /// Opt in to `rust-analyzer`-backed analysis for `Proven`-tier findings.
    /// v0.3-alpha scaffolding: the flag is wired through and the tool's
    /// presence on `PATH` is detected, but the LSP integration itself is a
    /// stub that returns no findings. Full implementation lands in a
    /// follow-up v0.3 release (see README §11).
    #[arg(long)]
    pub rust_analyzer: bool,

    /// Character budget for the rendered output — useful for keeping
    /// `--format markdown` inside an AI agent's context window. `0` (the
    /// default) means unlimited. Only affects the markdown renderer;
    /// text is for human terminals, JSON is for programmatic consumers
    /// who can filter themselves. Chars ≈ ¼ token for mainstream models
    /// (claude, gpt-4-ish tokenizers), so `--budget=32000` fits ≈ 8k
    /// tokens. The header + summary always render even if they alone
    /// exceed the budget; severity sections and the checklist are
    /// truncated in priority order (severity → tier → confidence) with
    /// a footer noting how many findings were dropped.
    #[arg(long, default_value_t = 0)]
    pub budget: usize,

    /// Emit a newline-delimited list of files implicated by the blast
    /// radius (one repo-relative path per line) instead of the normal
    /// report. Pipes directly into
    /// [`cargo-context`](https://github.com/asmuelle/cargo-context)'s
    /// `--files-from -` flag for the canonical handoff:
    /// `cargo impact --context | cargo context --files-from -`.
    /// Also consumable by any file-list tool (`xargs cat`, `grep -l`,
    /// etc.). Unique paths only. Overrides `--format` and `--test`.
    #[arg(long)]
    pub context: bool,

    /// Activate these Cargo features for cfg evaluation. Accepts a
    /// comma-separated list and/or repeated flags. Takes precedence over
    /// the manifest's `default` set and is unioned with it unless
    /// `--no-default-features` is also supplied. Transitively expands
    /// feature dependencies per the manifest's `[features]` table.
    #[arg(long = "features", value_delimiter = ',')]
    pub features: Vec<String>,

    /// Activate every feature declared in the manifest's `[features]`
    /// table. Mutually useful with `--no-default-features` when you want
    /// to audit the full surface instead of just the default view.
    #[arg(long, conflicts_with = "no_default_features")]
    pub all_features: bool,

    /// Skip the manifest's `default` feature list. Mirrors cargo's
    /// `--no-default-features`.
    #[arg(long)]
    pub no_default_features: bool,
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

/// Result of running every analyzer against the workspace. Produced by
/// [`analyze`] and consumed by both the CLI ([`run`]) and the MCP server
/// (`cargo impact mcp`).
///
/// Findings already carry stable IDs, are sorted by severity / tier, and
/// have been filtered against `args.confidence_min` — downstream renderers
/// can emit them directly.
#[derive(Debug, Clone)]
pub struct AnalysisReport {
    pub changed_files: Vec<PathBuf>,
    pub candidate_symbols: Vec<String>,
    pub findings: Vec<Finding>,
}

/// Run every analyzer and return a structured report.
///
/// This is the single source of truth for "what does cargo-impact think
/// about this diff?" — [`run`] wraps it with CLI printing / exit-code
/// handling, the MCP server serializes its output into JSON content, and
/// integration tests call it directly.
pub fn analyze(args: &ImpactArgs) -> Result<AnalysisReport> {
    let root = match &args.manifest_dir {
        Some(p) => p.clone(),
        None => std::env::current_dir().context("reading current directory")?,
    };

    // Merge cargo-impact.toml defaults into the args struct before
    // anything else consults those fields. apply_config only overrides
    // values that look clap-default, so explicit CLI flags always win.
    let cfg_file = config::ConfigFile::load(&root);
    let mut args = args.clone();
    config::apply_config(&cfg_file.defaults, &mut args);
    let args = &args;

    // Resolve features once, install as thread-local for the whole analyzer
    // block. `cfg::parse_and_filter` — used by every analyzer in place of
    // `syn::parse_file` — reads the thread-local to strip items whose cfg
    // gates don't match.
    let features = cfg::resolve_features(
        &root,
        &args.features,
        args.no_default_features,
        args.all_features,
    )?;
    cfg::with_features(features, || analyze_inner(args, &root))
}

fn analyze_inner(args: &ImpactArgs, root: &std::path::Path) -> Result<AnalysisReport> {
    let changed_files = git::changed_rust_files(root, &args.since)?;
    if changed_files.is_empty() {
        return Ok(AnalysisReport {
            changed_files,
            candidate_symbols: Vec::new(),
            findings: Vec::new(),
        });
    }

    // Collect symbols per changed file, diff-aware when possible; fall back
    // to blanket file-level analysis when the diff can't be computed.
    let mut all_symbols: Vec<symbols::TopLevelSymbol> = Vec::new();
    for rel in &changed_files {
        match diff::diff_file(root, rel, &args.since) {
            Ok(Some(items)) => {
                for it in items {
                    all_symbols.push(symbols::TopLevelSymbol {
                        name: it.name,
                        kind: it.kind,
                    });
                }
            }
            Ok(None) => {
                let abs = root.join(rel);
                match symbols::top_level_symbols(&abs) {
                    Ok(syms) => all_symbols.extend(syms),
                    Err(e) => eprintln!("cargo-impact: skipping {}: {e:#}", rel.display()),
                }
            }
            Err(e) => eprintln!("cargo-impact: diff failed for {}: {e:#}", rel.display()),
        }
    }
    let symbol_names: BTreeSet<String> = all_symbols.iter().map(|s| s.name.clone()).collect();
    let changed_trait_names = traits::changed_trait_names(&all_symbols);

    let mut findings = Vec::new();
    findings.extend(tests_scan::find_affected_tests(root, &symbol_names)?);
    findings.extend(traits::find_trait_impls(root, &changed_trait_names)?);
    findings.extend(derive::find_derive_impls(root, &changed_trait_names)?);
    findings.extend(dyn_dispatch::find_dyn_dispatch_sites(
        root,
        &changed_trait_names,
    )?);
    findings.extend(doc_drift::find_doc_drift(root, &symbol_names)?);
    findings.extend(adapters::find_runtime_surfaces(root, &symbol_names)?);

    for rel in &changed_files {
        match ffi::find_ffi_changes(root, rel, &args.since) {
            Ok(hits) => findings.extend(hits),
            Err(e) => eprintln!("cargo-impact: ffi scan failed for {}: {e:#}", rel.display()),
        }
    }

    for rel in &changed_files {
        match trait_methods::classify_changes_in_file(root, rel, &args.since) {
            Ok(records) => findings.extend(records.into_iter().map(|r| r.into_finding())),
            Err(e) => eprintln!(
                "cargo-impact: trait-method classification failed for {}: {e:#}",
                rel.display()
            ),
        }
    }

    for rel in &changed_files {
        let is_build_rs = rel
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "build.rs");
        if is_build_rs {
            let evidence = format!(
                "build script `{}` changed — build scripts can invalidate \
                 downstream compilation in non-obvious ways (env vars, \
                 rerun-if-*, generated code, linker flags)",
                rel.display()
            );
            let kind = FindingKind::BuildScriptChanged { file: rel.clone() };
            findings.push(Finding::new("", Tier::Likely, 0.90, kind, evidence));
        }
    }

    match semver_checks::run(root, &args.since, args.semver_checks) {
        Ok(hits) => findings.extend(hits),
        Err(e) => eprintln!("cargo-impact: semver-checks failed: {e:#}"),
    }

    match rust_analyzer::run(root, &changed_files, &symbol_names, args.rust_analyzer) {
        Ok(hits) => findings.extend(hits),
        Err(e) => eprintln!("cargo-impact: rust-analyzer failed: {e:#}"),
    }

    // Drop syn-only Likely findings that a Proven RA ResolvedReference
    // already covers at the same (name, file) pair. Runs before ignore /
    // confidence filtering and ID assignment so shadowed syn findings
    // never consume IDs or affect summary counts. No-op when RA is off
    // or returned nothing.
    dedup::dedup_syn_under_proven(&mut findings);

    // Apply .impactignore filtering before confidence threshold and ID
    // assignment — ignored findings shouldn't consume ID slots or affect
    // the summary counts. Findings with no primary path (SemverCheck) are
    // never ignored; the ignore file is about file-scoped noise.
    let ignore_set = ignore::IgnoreSet::load(root);
    if !ignore_set.is_empty() {
        findings.retain(|f| match f.primary_path() {
            Some(p) => !ignore_set.is_ignored(p),
            None => true,
        });
    }

    findings.retain(|f| f.confidence >= args.confidence_min);

    // Assign content-hashed IDs *before* sorting so the same finding
    // receives the same ID across runs — required for impact_explain to
    // round-trip by ID. Ties on (severity, tier, kind, evidence) are
    // broken by ID as a deterministic last-resort key.
    for f in &mut findings {
        f.id = f.content_id();
    }

    findings.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then_with(|| b.tier.rank().cmp(&a.tier.rank()))
            .then_with(|| a.kind.tag().cmp(b.kind.tag()))
            .then_with(|| a.evidence.cmp(&b.evidence))
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut candidate_symbols: Vec<String> = symbol_names.into_iter().collect();
    candidate_symbols.sort();

    Ok(AnalysisReport {
        changed_files,
        candidate_symbols,
        findings,
    })
}

/// CLI entry: runs [`analyze`], prints the configured format, honors the
/// `--test` short-circuit and `--fail-on` gate. Returns the intended exit
/// code (0 = clean / no gate tripped, 1 = `--fail-on` matched).
pub fn run(args: &ImpactArgs) -> Result<i32> {
    let report = analyze(args)?;

    if report.changed_files.is_empty() {
        if args.test {
            println!();
        } else if matches!(args.format, Format::Text) {
            println!(
                "cargo-impact: no Rust files changed relative to {}",
                args.since
            );
        } else {
            let out = render_with_budget(args.format, &[], &[], &[], args.budget)?;
            println!("{out}");
        }
        return Ok(0);
    }

    if args.context {
        for path in context_file_list(&report) {
            println!("{}", path.display());
        }
        return Ok(0);
    }

    if args.test {
        println!("{}", nextest::filter_expression(&report.findings));
        return Ok(0);
    }

    let out = render_with_budget(
        args.format,
        &report.changed_files,
        &report.candidate_symbols,
        &report.findings,
        args.budget,
    )?;
    println!("{out}");

    if let Some(gate) = args.fail_on {
        let tripped = report.findings.iter().any(|f| gate.triggers(f.severity));
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
