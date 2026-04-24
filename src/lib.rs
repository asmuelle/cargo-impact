//! `cargo-impact` — blast-radius analysis for Rust workspaces.
//!
//! This is v0.4 per the README §11 roadmap. The headline shipped
//! surface, end-to-end:
//!
//! Core analyzers — [`Finding`], [`FindingKind`], [`Tier`]
//! * Confidence tiers (`Proven` / `Likely` / `Possible` / `Unknown`)
//!   with numeric scores; only RA-backed resolution reaches `Proven`.
//! * Test-reference detection, trait ripple (`impl Trait for T`),
//!   `dyn Trait` dispatch, derive-macro impl fan-out, documentation
//!   drift (intra-doc links + keyword fallback), FFI signature
//!   changes, `build.rs` change detection, per-method trait-definition
//!   classification (required vs. default vs. signature vs. body).
//! * Framework adapters: axum / clap (v0.3) + actix-web / rocket
//!   (v0.4-stretch), HTTP-verb attribute macros shared across.
//!
//! Public-API precision
//! * `cargo-semver-checks` integration (opt-in via `--semver-checks`).
//! * rust-analyzer LSP client for `Proven`-tier resolved references;
//!   per-reference severity refinement based on enclosing container
//!   (test fn → `Low`, impl block → `High`, caller → `Medium`).
//! * Macro expansion via `cargo expand` (opt-in via `--macro-expand`)
//!   for derive/attribute-macro impls that syn-only analysis can't see.
//!
//! Orchestration
//! * Content-hashed finding IDs, stable across runs — powers
//!   `impact_explain` round-trip by ID.
//! * syn/RA dedup: syn-only findings covered by a Proven
//!   `ResolvedReference` at the same `(name, file)` pair are dropped.
//! * Depth-1 `--feature-powerset` (baseline + no-default + all-features)
//!   with evidence annotation identifying the set that revealed each
//!   finding.
//! * cfg-aware AST filtering against the resolved feature set
//!   (`--features` / `--all-features` / `--no-default-features`).
//!
//! Output
//! * `--format={text,markdown,json,sarif,pr-comment}` — SARIF v2.1.0
//!   renders on GitHub code scanning; pr-comment is optimized for
//!   sticky PR comments (collapsed `<details>` per severity).
//! * Deterministic: two runs over the same diff produce byte-identical
//!   output across every format.
//! * `--budget=<N>` chars for rendered markdown, for agent context
//!   windows.
//! * `--context` emits a newline-delimited file list for piping into
//!   `cargo-context --files-from -`.
//! * `--confidence-min` and `--fail-on={high,medium,low}` for CI gating.
//!
//! MCP surface (`cargo impact mcp`)
//! * Six tools: `impact_analyze`, `impact_test_filter`, `impact_surface`,
//!   `impact_semver`, `impact_explain`, `impact_version`.
//! * `impact_analyze` streams `notifications/message` progress events
//!   at analyzer stage boundaries so long runs give live feedback.
//!
//! Honest caveats (surface when asked "why didn't cargo-impact flag X?"):
//! * `cfg_attr(feature = "x", derive(…))` is invisible to our analyzer
//!   — over-counts slightly when users conditionally derive.
//! * Macro expansion is opt-in and points to a synthetic `<expanded>`
//!   file rather than source-mapping back to the derive site.
//! * `log-miss` records stay on disk only (`target/ai-tools-cache/`);
//!   we never phone home.
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
mod macro_expand;
pub mod mcp;
mod nextest;
mod ref_context;
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

    /// Opt in to `cargo-expand`-backed trait-impl detection for impls
    /// synthesized by derive and attribute macros (serde, tokio,
    /// clap, thiserror, …). Requires `cargo-expand` on PATH; install
    /// via `cargo install cargo-expand`. When absent or expansion
    /// fails, a stderr warning is printed and the check is skipped
    /// (non-fatal). Off by default because the expansion pass adds
    /// 10-60s depending on crate size.
    #[arg(long)]
    pub macro_expand: bool,

    /// Run analysis across a depth-1 feature powerset (baseline +
    /// `--no-default-features` + `--all-features`) and surface
    /// findings that the baseline view missed. Intended for CI —
    /// triples analyzer cost so off by default. Findings visible
    /// only under a non-baseline set are annotated in evidence
    /// with the feature set that revealed them. A full feature
    /// powerset (O(2^N) over N features) is out of scope; the
    /// depth-1 view catches the usual feature-gated blast radius
    /// (std/no_std, sync/async, feature = "foo" paths) without
    /// combinatorial blow-up.
    #[arg(long)]
    pub feature_powerset: bool,
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

/// Single progress update emitted during an analysis run. Surfaced via
/// [`analyze_with_progress`] so long-running invocations (typically
/// `--rust-analyzer` or `--semver-checks`) can give the caller a live
/// signal instead of a 30-second silence.
///
/// Stages are a small fixed vocabulary so consumers can map them to
/// UI strings or progress bars without scraping free text:
///
/// * `"symbols"` — collecting top-level items from changed files.
///   `current`/`total` count files processed.
/// * `"analyzers"` — running the per-file/per-symbol analyzers.
///   `total` is the number of analyzer passes; `current` is how many
///   have completed.
/// * `"semver_checks"` — invoking `cargo-semver-checks`. Only emitted
///   when the flag is on; `current`/`total` are both 1 (start/done).
/// * `"rust_analyzer"` — driving the RA LSP subprocess. Only emitted
///   when the flag is on; `current`/`total` are both 1.
/// * `"done"` — final emit, always sent at the end of a successful
///   run. `current == total`.
#[derive(Debug, Clone)]
pub struct ProgressEvent<'a> {
    pub stage: &'a str,
    pub current: usize,
    pub total: usize,
    pub detail: Option<&'a str>,
}

/// Run every analyzer and return a structured report.
///
/// This is the single source of truth for "what does cargo-impact think
/// about this diff?" — [`run`] wraps it with CLI printing / exit-code
/// handling, the MCP server serializes its output into JSON content, and
/// integration tests call it directly.
///
/// Equivalent to [`analyze_with_progress`] with a no-op callback.
pub fn analyze(args: &ImpactArgs) -> Result<AnalysisReport> {
    analyze_with_progress(args, |_| {})
}

/// Run every analyzer with a progress callback invoked at stage
/// boundaries. Use this instead of [`analyze`] when the caller wants
/// live feedback during long invocations — typically the MCP server
/// bridging the callback to `notifications/message` or an interactive
/// CLI printing to stderr.
///
/// The callback is synchronous: it runs on the analyzer thread, so
/// keep it cheap (writing a few hundred bytes is fine; blocking on a
/// network call is not). Order of `stage` values is stable; see
/// [`ProgressEvent`] for the vocabulary.
pub fn analyze_with_progress<F>(args: &ImpactArgs, mut progress: F) -> Result<AnalysisReport>
where
    F: FnMut(&ProgressEvent<'_>),
{
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
    let baseline_features = cfg::resolve_features(
        &root,
        &args.features,
        args.no_default_features,
        args.all_features,
    )?;
    let baseline = cfg::with_features(baseline_features, || {
        analyze_inner(args, &root, &mut progress)
    })?;

    if !args.feature_powerset {
        return Ok(baseline);
    }

    // Depth-1 powerset: baseline + no-default-features + all-features.
    // Each is a separate analyzer run (git diff, symbol extraction,
    // all analyzer passes) under a different cfg view; combined cost
    // is roughly 3x a normal run, which is why this is flagged off by
    // default and documented as CI-only.
    progress(&ProgressEvent {
        stage: "powerset_no_default",
        current: 1,
        total: 3,
        detail: None,
    });
    let nodef_features = cfg::resolve_features(&root, &[], true, false)?;
    let nodef_report =
        cfg::with_features(nodef_features, || analyze_inner(args, &root, &mut progress))?;

    progress(&ProgressEvent {
        stage: "powerset_all_features",
        current: 2,
        total: 3,
        detail: None,
    });
    let all_features_set = cfg::resolve_features(&root, &[], false, true)?;
    let all_report = cfg::with_features(all_features_set, || {
        analyze_inner(args, &root, &mut progress)
    })?;

    Ok(merge_powerset_reports(baseline, nodef_report, all_report))
}

/// Combine three analyzer reports (baseline, no-default-features,
/// all-features) into one. The baseline's findings are kept verbatim;
/// findings that appear in one of the other passes but NOT in the
/// baseline are appended with an evidence suffix identifying the
/// feature set that revealed them. Dedup is by finding ID (content-
/// hashed, so identical findings across passes carry identical IDs).
///
/// A finding that appears only under `--no-default-features` indicates
/// a code path that your baseline analysis misses — typically a
/// `#[cfg(not(feature = "std"))]` branch or a fallback path. A finding
/// that appears only under `--all-features` means some optional feature
/// introduces blast radius the default view doesn't see. Both are the
/// signal a CI-gated powerset run is supposed to catch.
fn merge_powerset_reports(
    baseline: AnalysisReport,
    nodef: AnalysisReport,
    all: AnalysisReport,
) -> AnalysisReport {
    use std::collections::BTreeSet;

    let baseline_ids: BTreeSet<String> = baseline.findings.iter().map(|f| f.id.clone()).collect();
    let mut combined = baseline.findings;
    let mut added_ids = baseline_ids.clone();

    for (extra_report, label) in [(nodef, "--no-default-features"), (all, "--all-features")] {
        for mut f in extra_report.findings {
            if added_ids.contains(&f.id) {
                continue;
            }
            f.evidence = format!("{} (only visible with {label})", f.evidence);
            added_ids.insert(f.id.clone());
            combined.push(f);
        }
    }

    // Re-sort so feature-revealed findings interleave by severity with
    // baseline findings rather than always landing at the bottom.
    combined.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then_with(|| b.tier.rank().cmp(&a.tier.rank()))
            .then_with(|| a.kind.tag().cmp(b.kind.tag()))
            .then_with(|| a.evidence.cmp(&b.evidence))
            .then_with(|| a.id.cmp(&b.id))
    });

    AnalysisReport {
        changed_files: baseline.changed_files,
        candidate_symbols: baseline.candidate_symbols,
        findings: combined,
    }
}

fn analyze_inner<F>(
    args: &ImpactArgs,
    root: &std::path::Path,
    progress: &mut F,
) -> Result<AnalysisReport>
where
    F: FnMut(&ProgressEvent<'_>),
{
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
    let total_files = changed_files.len();
    for (i, rel) in changed_files.iter().enumerate() {
        progress(&ProgressEvent {
            stage: "symbols",
            current: i,
            total: total_files,
            detail: rel.to_str(),
        });
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

    // Six syn-based analyzers run in sequence. Emit a stage start per
    // analyzer so consumers can render a progress bar. Names are
    // stable; keep them aligned with the source order below.
    const ANALYZER_STAGES: &[&str] = &[
        "tests_scan",
        "traits",
        "derive",
        "dyn_dispatch",
        "doc_drift",
        "adapters",
    ];
    let emit_analyzer = |i: usize, progress: &mut F| {
        progress(&ProgressEvent {
            stage: "analyzers",
            current: i,
            total: ANALYZER_STAGES.len(),
            detail: Some(ANALYZER_STAGES[i]),
        });
    };

    let mut findings = Vec::new();
    emit_analyzer(0, progress);
    findings.extend(tests_scan::find_affected_tests(root, &symbol_names)?);
    emit_analyzer(1, progress);
    findings.extend(traits::find_trait_impls(root, &changed_trait_names)?);
    emit_analyzer(2, progress);
    findings.extend(derive::find_derive_impls(root, &changed_trait_names)?);
    emit_analyzer(3, progress);
    findings.extend(dyn_dispatch::find_dyn_dispatch_sites(
        root,
        &changed_trait_names,
    )?);
    emit_analyzer(4, progress);
    findings.extend(doc_drift::find_doc_drift(root, &symbol_names)?);
    emit_analyzer(5, progress);
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

    if args.semver_checks {
        progress(&ProgressEvent {
            stage: "semver_checks",
            current: 0,
            total: 1,
            detail: None,
        });
    }
    match semver_checks::run(root, &args.since, args.semver_checks) {
        Ok(hits) => findings.extend(hits),
        Err(e) => eprintln!("cargo-impact: semver-checks failed: {e:#}"),
    }

    if args.rust_analyzer {
        progress(&ProgressEvent {
            stage: "rust_analyzer",
            current: 0,
            total: 1,
            detail: None,
        });
    }
    match rust_analyzer::run(root, &changed_files, &symbol_names, args.rust_analyzer) {
        Ok(hits) => findings.extend(hits),
        Err(e) => eprintln!("cargo-impact: rust-analyzer failed: {e:#}"),
    }

    if args.macro_expand {
        progress(&ProgressEvent {
            stage: "macro_expand",
            current: 0,
            total: 1,
            detail: None,
        });
    }
    match macro_expand::run(root, &changed_trait_names, &symbol_names, args.macro_expand) {
        Ok(hits) => findings.extend(hits),
        Err(e) => eprintln!("cargo-impact: macro-expand failed: {e:#}"),
    }

    // Drop syn-only Likely findings that a Proven RA ResolvedReference
    // already covers at the same (name, file) pair. Runs before ignore /
    // confidence filtering and ID assignment so shadowed syn findings
    // never consume IDs or affect summary counts. No-op when RA is off
    // or returned nothing.
    dedup::dedup_syn_under_proven(&mut findings);

    // Drop macro-expansion TestReference findings whose test name is
    // already covered by a raw-source TestReference. Expansion-backed
    // findings share the `<expanded>` sentinel path so they'd otherwise
    // double-count tests the syn-only walker already caught. No-op when
    // --macro-expand is off or expansion produced no test-refs.
    dedup::dedup_expanded_under_raw(&mut findings);

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

    progress(&ProgressEvent {
        stage: "done",
        current: 1,
        total: 1,
        detail: None,
    });

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

    mod powerset {
        use super::*;
        use crate::finding::Location;

        fn mk_finding(id: &str, evidence: &str) -> Finding {
            let mut f = Finding::new(
                "",
                Tier::Likely,
                0.85,
                FindingKind::TestReference {
                    test: Location {
                        file: PathBuf::from("tests/a.rs"),
                        symbol: format!("test_{id}"),
                    },
                    matched_symbols: vec![id.to_string()],
                },
                evidence,
            );
            f.id = id.to_string();
            f
        }

        fn report(findings: Vec<Finding>) -> AnalysisReport {
            AnalysisReport {
                changed_files: Vec::new(),
                candidate_symbols: Vec::new(),
                findings,
            }
        }

        #[test]
        fn baseline_findings_pass_through_unchanged() {
            let baseline = report(vec![mk_finding("a", "base evidence")]);
            let nodef = report(vec![]);
            let all = report(vec![]);
            let merged = merge_powerset_reports(baseline, nodef, all);
            assert_eq!(merged.findings.len(), 1);
            assert_eq!(merged.findings[0].evidence, "base evidence");
        }

        #[test]
        fn finding_only_in_nodef_is_annotated_and_appended() {
            let baseline = report(vec![mk_finding("a", "base")]);
            let nodef = report(vec![mk_finding("b", "from-nodef")]);
            let all = report(vec![]);
            let merged = merge_powerset_reports(baseline, nodef, all);
            assert_eq!(merged.findings.len(), 2);
            let b = merged.findings.iter().find(|f| f.id == "b").unwrap();
            assert!(
                b.evidence.contains("--no-default-features"),
                "expected no-default annotation in evidence: {}",
                b.evidence
            );
            assert!(b.evidence.starts_with("from-nodef"));
        }

        #[test]
        fn finding_only_in_all_features_is_annotated_and_appended() {
            let baseline = report(vec![]);
            let nodef = report(vec![]);
            let all = report(vec![mk_finding("c", "from-all")]);
            let merged = merge_powerset_reports(baseline, nodef, all);
            assert_eq!(merged.findings.len(), 1);
            let c = &merged.findings[0];
            assert!(c.evidence.contains("--all-features"));
            assert!(c.evidence.starts_with("from-all"));
        }

        #[test]
        fn finding_visible_in_baseline_and_nodef_keeps_baseline_evidence() {
            // Same ID in baseline and nodef — baseline wins, no
            // annotation. The ID dedup prevents double-counting.
            let shared = mk_finding("dup", "baseline-text");
            let also_shared = mk_finding("dup", "nodef-text");
            let baseline = report(vec![shared]);
            let nodef = report(vec![also_shared]);
            let merged = merge_powerset_reports(baseline, nodef, report(vec![]));
            assert_eq!(merged.findings.len(), 1);
            assert_eq!(merged.findings[0].evidence, "baseline-text");
        }

        #[test]
        fn same_id_in_both_extras_only_annotates_once() {
            // A finding absent from baseline, present in both extras,
            // gets appended exactly once — with whichever annotation
            // the first-encountered extra (nodef) supplied.
            let baseline = report(vec![]);
            let nodef = report(vec![mk_finding("x", "ev")]);
            let all = report(vec![mk_finding("x", "ev")]);
            let merged = merge_powerset_reports(baseline, nodef, all);
            assert_eq!(merged.findings.len(), 1);
            assert!(
                merged.findings[0]
                    .evidence
                    .contains("--no-default-features")
            );
            assert!(!merged.findings[0].evidence.contains("--all-features"));
        }

        #[test]
        fn merged_results_stay_sorted_by_severity_then_tier() {
            // High+Likely < Medium+Likely < Low+Likely in our sort
            // (severity ascending, then tier desc by rank). Feed them
            // in reverse order across the three reports and verify
            // the output ordering.
            let high = {
                let mut f = Finding::new(
                    "",
                    Tier::Likely,
                    0.9,
                    FindingKind::BuildScriptChanged {
                        file: PathBuf::from("build.rs"),
                    },
                    "build.rs changed",
                );
                f.id = "high".into();
                f
            };
            let med = mk_finding("med", "medium finding");
            let low = {
                let mut f = Finding::new(
                    "",
                    Tier::Likely,
                    0.4,
                    FindingKind::DocDriftKeyword {
                        symbol: "foo".into(),
                        doc: Location {
                            file: PathBuf::from("README.md"),
                            symbol: "foo".into(),
                        },
                        line: 1,
                    },
                    "doc drift",
                );
                f.id = "low".into();
                f
            };
            let baseline = report(vec![low]);
            let nodef = report(vec![med]);
            let all = report(vec![high]);
            let merged = merge_powerset_reports(baseline, nodef, all);
            let severities: Vec<_> = merged.findings.iter().map(|f| f.severity).collect();
            assert_eq!(
                severities,
                vec![
                    SeverityClass::High,
                    SeverityClass::Medium,
                    SeverityClass::Low
                ]
            );
        }
    }
}
