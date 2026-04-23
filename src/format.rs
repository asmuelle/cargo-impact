//! Output format dispatch: text, markdown, and JSON.
//!
//! The JSON envelope matches the shape documented in README §8 so agents
//! calling the future MCP server (v0.3) get identical structure from the
//! CLI today. Markdown output is designed to be pasted directly into an AI
//! chat window — it leads with a summary and lists findings by severity.

use crate::finding::{Finding, FindingKind, SeverityClass, TierSummary};
use clap::ValueEnum;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, ValueEnum, Default)]
#[value(rename_all = "kebab-case")]
pub enum Format {
    #[default]
    Text,
    Markdown,
    Json,
    /// SARIF v2.1.0 — the format GitHub code scanning, GitLab, Sonar,
    /// and every major scanner UI consumes. Emit this from CI and
    /// upload via `github/codeql-action/upload-sarif` (or equivalent)
    /// to get inline-on-PR-diff annotations for free.
    Sarif,
    /// Markdown optimized for GitHub PR comments — collapsed
    /// `<details>` per severity, compact tables instead of bullets,
    /// no verification checklist. Pair with an action that posts
    /// the output as a sticky PR comment.
    PrComment,
}

/// Top-level JSON envelope. Stable across releases; additions go at the end.
#[derive(Debug, Serialize)]
pub struct Report<'a> {
    pub version: &'static str,
    pub changed_files: &'a [PathBuf],
    pub candidate_symbols: &'a [String],
    pub findings: &'a [Finding],
    pub summary: ReportSummary,
}

#[derive(Debug, Serialize)]
pub struct ReportSummary {
    pub total: usize,
    pub by_severity: BTreeMap<String, u32>,
    pub by_tier: TierSummary,
}

impl ReportSummary {
    pub fn build(findings: &[Finding]) -> Self {
        let mut by_severity: BTreeMap<String, u32> = BTreeMap::new();
        for f in findings {
            *by_severity
                .entry(f.severity.as_label().to_lowercase())
                .or_insert(0) += 1;
        }
        Self {
            total: findings.len(),
            by_severity,
            by_tier: TierSummary::from_findings(findings),
        }
    }
}

pub fn render(
    format: Format,
    changed_files: &[PathBuf],
    candidate_symbols: &[String],
    findings: &[Finding],
) -> anyhow::Result<String> {
    render_with_budget(format, changed_files, candidate_symbols, findings, 0)
}

/// Like [`render`] but with a character budget applied to the markdown
/// format. `0` = unlimited (matches the `render` wrapper above). Text
/// and JSON ignore the budget — text is for terminal humans who can
/// scroll; JSON is for programmatic consumers who can filter.
pub fn render_with_budget(
    format: Format,
    changed_files: &[PathBuf],
    candidate_symbols: &[String],
    findings: &[Finding],
    budget: usize,
) -> anyhow::Result<String> {
    match format {
        Format::Text => Ok(render_text(changed_files, candidate_symbols, findings)),
        Format::Markdown => Ok(render_markdown(
            changed_files,
            candidate_symbols,
            findings,
            budget,
        )),
        Format::Json => render_json(changed_files, candidate_symbols, findings),
        Format::Sarif => render_sarif(findings),
        Format::PrComment => Ok(render_pr_comment(
            changed_files,
            candidate_symbols,
            findings,
            budget,
        )),
    }
}

fn render_text(
    changed_files: &[PathBuf],
    candidate_symbols: &[String],
    findings: &[Finding],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("cargo-impact v{}\n\n", env!("CARGO_PKG_VERSION")));

    out.push_str(&format!("Changed files ({}):\n", changed_files.len()));
    for f in changed_files {
        out.push_str(&format!("  {}\n", f.display()));
    }
    out.push('\n');

    out.push_str(&format!(
        "Candidate symbols ({}):\n",
        candidate_symbols.len()
    ));
    for s in candidate_symbols {
        out.push_str(&format!("  {s}\n"));
    }
    out.push('\n');

    let grouped = group_by_severity(findings);
    for severity in [
        SeverityClass::High,
        SeverityClass::Medium,
        SeverityClass::Low,
        SeverityClass::Unknown,
    ] {
        let group = grouped.get(&severity).map_or(&[][..], |v| v.as_slice());
        out.push_str(&format!(
            "{icon} {label} ({n})\n",
            icon = severity.icon(),
            label = severity.as_label(),
            n = group.len()
        ));
        for f in group {
            out.push_str(&format!(
                "  [{id}] {summary} · {tier:?} {conf:.2}\n",
                id = f.id,
                summary = finding_summary(f),
                tier = f.tier,
                conf = f.confidence,
            ));
        }
        out.push('\n');
    }

    out
}

fn render_markdown(
    changed_files: &[PathBuf],
    candidate_symbols: &[String],
    findings: &[Finding],
    budget: usize,
) -> String {
    let mut out = String::new();
    render_markdown_header(&mut out, changed_files, candidate_symbols, findings);

    // Findings arrive pre-sorted by (severity, tier, kind, evidence, id),
    // so emitting them in order already implements our "priority first"
    // truncation policy — we just stop writing when the budget would be
    // exceeded. Tracking the pre-sections offset keeps rendered sections
    // accounted for separately from the header so we never drop the
    // summary even under a tiny budget.
    let unlimited = budget == 0;
    let mut omitted_findings = 0usize;
    let mut omitted_checklist = 0usize;

    let grouped = group_by_severity(findings);
    for severity in [
        SeverityClass::High,
        SeverityClass::Medium,
        SeverityClass::Low,
        SeverityClass::Unknown,
    ] {
        let group = grouped.get(&severity).map_or(&[][..], |v| v.as_slice());
        if group.is_empty() {
            continue;
        }
        let header = format!(
            "## {icon} {label} ({n})\n\n",
            icon = severity.icon(),
            label = severity.as_label(),
            n = group.len()
        );
        // Only commit the section header if at least one finding will fit
        // under it — otherwise we'd emit an orphan "## HIGH (3)" with no
        // bullets, which looks broken.
        let mut header_written = false;
        for f in group {
            let body = render_finding_bullet(f);
            if !header_written {
                if !unlimited && out.len() + header.len() + body.len() > budget {
                    omitted_findings += 1;
                    continue;
                }
                out.push_str(&header);
                header_written = true;
            }
            if !unlimited && out.len() + body.len() > budget {
                omitted_findings += 1;
                continue;
            }
            out.push_str(&body);
        }
        if header_written {
            out.push('\n');
        }
    }

    let checklist_header = "## Verification checklist\n\n";
    if findings.is_empty() {
        out.push_str(checklist_header);
        out.push_str("_No findings — nothing to verify._\n");
    } else {
        let mut header_written = false;
        for f in findings {
            let body = render_checklist_line(f);
            if !header_written {
                if !unlimited && out.len() + checklist_header.len() + body.len() > budget {
                    omitted_checklist += 1;
                    continue;
                }
                out.push_str(checklist_header);
                header_written = true;
            }
            if !unlimited && out.len() + body.len() > budget {
                omitted_checklist += 1;
                continue;
            }
            out.push_str(&body);
        }
    }

    if !unlimited && (omitted_findings > 0 || omitted_checklist > 0) {
        out.push_str(&format!(
            "\n---\n\n> **Budget truncation:** {omitted_findings} findings and \
             {omitted_checklist} checklist items omitted because the rendered \
             markdown would exceed the `--budget={budget}` character limit. \
             Priority order was severity → tier → confidence, so what you see \
             is what matters most. Re-run with `--budget=0` or `--format=json` \
             to get everything.\n"
        ));
    }

    out
}

fn render_markdown_header(
    out: &mut String,
    changed_files: &[PathBuf],
    candidate_symbols: &[String],
    findings: &[Finding],
) {
    out.push_str(&format!(
        "# cargo-impact v{} blast radius\n\n",
        env!("CARGO_PKG_VERSION")
    ));
    let summary = ReportSummary::build(findings);
    out.push_str("## Summary\n\n");
    out.push_str(&format!(
        "- **Changed files:** {}\n- **Candidate symbols:** {}\n- **Findings:** {} \
         ({} high, {} medium, {} low, {} unknown)\n\n",
        changed_files.len(),
        candidate_symbols.len(),
        summary.total,
        summary.by_severity.get("high").unwrap_or(&0),
        summary.by_severity.get("medium").unwrap_or(&0),
        summary.by_severity.get("low").unwrap_or(&0),
        summary.by_severity.get("unknown").unwrap_or(&0),
    ));
}

fn render_finding_bullet(f: &Finding) -> String {
    let mut body = format!(
        "- **[{id}]** {summary} — *{tier:?} {conf:.2}* — {evidence}\n",
        id = f.id,
        summary = finding_summary(f),
        tier = f.tier,
        conf = f.confidence,
        evidence = f.evidence,
    );
    if let Some(action) = &f.suggested_action {
        body.push_str(&format!("  - Suggested: `{action}`\n"));
    }
    body
}

fn render_checklist_line(f: &Finding) -> String {
    format!(
        "- [ ] **{label}** {summary} — *{tier:?} {conf:.2}*\n",
        label = f.severity.as_label(),
        summary = finding_summary(f),
        tier = f.tier,
        conf = f.confidence,
    )
}

fn render_json(
    changed_files: &[PathBuf],
    candidate_symbols: &[String],
    findings: &[Finding],
) -> anyhow::Result<String> {
    let report = Report {
        version: env!("CARGO_PKG_VERSION"),
        changed_files,
        candidate_symbols,
        findings,
        summary: ReportSummary::build(findings),
    };
    Ok(serde_json::to_string_pretty(&report)?)
}

// ---------------------------------------------------------------------------
// SARIF v2.1.0 — inherits every major scanner UI's "annotations inline on
// PR diff" rendering via `github/codeql-action/upload-sarif` and friends.
// Spec: https://docs.oasis-open.org/sarif/sarif/v2.1.0/
//
// Design notes
// ------------
// * `tool.driver.rules[]` enumerates every `FindingKind` tag so scanners
//   can hyperlink each result back to a rule description.
// * `level` maps from severity: High→error, Medium→warning, Low→note,
//   Unknown→none. Tier/confidence/severity also live in `properties` so
//   consumers that care about our richer tiering can still access it.
// * `partialFingerprints.primaryLocationLineHash` reuses our content-
//   hashed finding id. Scanners dedupe using fingerprints, so the
//   same finding across runs collapses into one tracked issue.
// * Paths forward-slash-normalized to keep Windows + Unix results
//   comparable under the same SARIF upload.

fn render_sarif(findings: &[Finding]) -> anyhow::Result<String> {
    let rules = FindingKind::all_tags()
        .iter()
        .map(|tag| sarif_rule(tag))
        .collect::<Vec<_>>();

    let results = findings.iter().map(sarif_result).collect::<Vec<_>>();

    let doc = serde_json::json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [
            {
                "tool": {
                    "driver": {
                        "name": "cargo-impact",
                        "version": env!("CARGO_PKG_VERSION"),
                        "informationUri": "https://github.com/asmuelle/cargo-impact",
                        "rules": rules,
                    }
                },
                "results": results,
            }
        ]
    });
    Ok(serde_json::to_string_pretty(&doc)?)
}

fn sarif_rule(tag: &str) -> serde_json::Value {
    serde_json::json!({
        "id": tag,
        "name": sarif_rule_name(tag),
        "shortDescription": { "text": sarif_rule_short(tag) },
        "helpUri": "https://github.com/asmuelle/cargo-impact#README",
    })
}

fn sarif_rule_name(tag: &str) -> String {
    // `camelCase` name per SARIF convention; scanner UIs sometimes
    // display `name` instead of `id`.
    tag.split('_')
        .enumerate()
        .map(|(i, part)| {
            if i == 0 {
                part.to_string()
            } else {
                let mut chars = part.chars();
                match chars.next() {
                    Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect()
}

fn sarif_rule_short(tag: &str) -> &'static str {
    match tag {
        "test_reference" => "Test function references a changed symbol",
        "trait_impl" => "impl of a changed trait",
        "derived_trait_impl" => "#[derive(…)] of a changed trait",
        "dyn_dispatch" => "dyn Trait use of a changed trait",
        "doc_drift_link" => "Intra-doc link to a changed symbol",
        "doc_drift_keyword" => "Bare mention of a changed symbol in prose",
        "ffi_signature_change" => "FFI signature added/removed/modified",
        "build_script_changed" => "build.rs modified",
        "semver_check" => "cargo-semver-checks reports public API change",
        "trait_definition_change" => "Method added/removed/changed on a trait",
        "resolved_reference" => "rust-analyzer-resolved reference to a changed symbol",
        "runtime_surface" => "Framework runtime surface (axum route, clap command) affected",
        _ => "cargo-impact finding",
    }
}

fn sarif_result(f: &Finding) -> serde_json::Value {
    let mut location = serde_json::json!({});
    if let Some(path) = f.primary_path() {
        let uri = path.to_string_lossy().replace('\\', "/");
        let mut physical = serde_json::json!({
            "artifactLocation": { "uri": uri },
        });
        if let Some(line) = finding_line(f) {
            physical["region"] = serde_json::json!({ "startLine": line });
        }
        location = serde_json::json!({ "physicalLocation": physical });
    }

    let mut locations = Vec::new();
    if !location.as_object().is_none_or(serde_json::Map::is_empty) {
        locations.push(location);
    }

    serde_json::json!({
        "ruleId": f.kind.tag(),
        "level": sarif_level(f.severity),
        "message": { "text": f.evidence },
        "locations": locations,
        "partialFingerprints": {
            "primaryLocationLineHash": f.id
        },
        "properties": {
            "tier": format!("{:?}", f.tier).to_lowercase(),
            "confidence": f.confidence,
            "severity": f.severity.as_label().to_lowercase(),
            "suggestedAction": f.suggested_action,
        }
    })
}

fn sarif_level(severity: SeverityClass) -> &'static str {
    // SARIF levels: error | warning | note | none.
    match severity {
        SeverityClass::High => "error",
        SeverityClass::Medium => "warning",
        SeverityClass::Low => "note",
        SeverityClass::Unknown => "none",
    }
}

/// Extract a line number when the finding carries one. Only doc-drift
/// findings have a line; everything else leaves `region` unset and
/// SARIF consumers fall back to file-level annotation.
fn finding_line(f: &Finding) -> Option<u32> {
    match &f.kind {
        FindingKind::DocDriftLink { line, .. } | FindingKind::DocDriftKeyword { line, .. } => {
            Some(*line)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// PR-comment markdown — tuned for posting as a sticky comment on a GitHub
// PR. Drops the verification checklist (reviewers don't tick boxes from
// the diff view), uses collapsed `<details>` per severity so a scrollable
// body degrades gracefully, tables instead of bullets for density.

fn render_pr_comment(
    changed_files: &[PathBuf],
    candidate_symbols: &[String],
    findings: &[Finding],
    budget: usize,
) -> String {
    let summary = ReportSummary::build(findings);
    let unlimited = budget == 0;
    let mut out = String::new();

    // One-line header with severity-badge counts, always emits so the
    // comment is never empty even under the tightest budget.
    out.push_str(&format!(
        "### 🎯 cargo-impact — {total} findings across {nfiles} file{s}\n\
         `🔴 {h} · 🟡 {m} · 🔵 {l} · ⚪ {u}`\n\n",
        total = summary.total,
        nfiles = changed_files.len(),
        s = if changed_files.len() == 1 { "" } else { "s" },
        h = summary.by_severity.get("high").unwrap_or(&0),
        m = summary.by_severity.get("medium").unwrap_or(&0),
        l = summary.by_severity.get("low").unwrap_or(&0),
        u = summary.by_severity.get("unknown").unwrap_or(&0),
    ));

    if findings.is_empty() {
        out.push_str("_No findings — nothing to verify._\n");
        return out;
    }

    // Expand the HIGH section by default so the important stuff is
    // immediately visible; collapse the rest. Reviewers who care about
    // MEDIUM/LOW/UNKNOWN can click in.
    let grouped = group_by_severity(findings);
    let mut omitted = 0usize;
    for (severity, expand_default) in [
        (SeverityClass::High, true),
        (SeverityClass::Medium, false),
        (SeverityClass::Low, false),
        (SeverityClass::Unknown, false),
    ] {
        let group = grouped.get(&severity).map_or(&[][..], |v| v.as_slice());
        if group.is_empty() {
            continue;
        }
        let open = if expand_default { " open" } else { "" };
        let block_header = format!(
            "<details{open}><summary>{icon} {label} ({n})</summary>\n\n\
             | Kind | Location | Evidence |\n\
             |---|---|---|\n",
            icon = severity.icon(),
            label = severity.as_label(),
            n = group.len(),
        );
        let mut rows = String::new();
        for f in group {
            let row = format!(
                "| `{kind}` | {loc} | {evidence} |\n",
                kind = f.kind.tag(),
                loc = pr_comment_location(f),
                evidence = escape_pipe(&f.evidence),
            );
            if !unlimited && out.len() + block_header.len() + rows.len() + row.len() > budget {
                omitted += 1;
                continue;
            }
            rows.push_str(&row);
        }
        if !rows.is_empty() {
            out.push_str(&block_header);
            out.push_str(&rows);
            out.push_str("\n</details>\n\n");
        }
    }

    // Context + candidate symbols collapsed into one block — useful
    // context, but not what the reviewer needs to see first.
    out.push_str(&format!(
        "<details><summary>📁 {n} changed file{s} · {k} candidate symbol{ks}</summary>\n\n",
        n = changed_files.len(),
        s = if changed_files.len() == 1 { "" } else { "s" },
        k = candidate_symbols.len(),
        ks = if candidate_symbols.len() == 1 {
            ""
        } else {
            "s"
        },
    ));
    out.push_str("**Changed files:**\n\n");
    for f in changed_files {
        out.push_str(&format!("- `{}`\n", f.display()));
    }
    if !candidate_symbols.is_empty() {
        out.push_str("\n**Candidate symbols:** ");
        out.push_str(
            &candidate_symbols
                .iter()
                .map(|s| format!("`{s}`"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        out.push('\n');
    }
    out.push_str("\n</details>\n");

    if !unlimited && omitted > 0 {
        out.push_str(&format!(
            "\n> ⚠ {omitted} findings omitted to fit the `--budget={budget}` cap.\n"
        ));
    }

    out.push_str(&format!(
        "\n<sub>Generated by [cargo-impact v{}](https://github.com/asmuelle/cargo-impact) · [full report](#) · [raw JSON](#)</sub>\n",
        env!("CARGO_PKG_VERSION")
    ));

    out
}

fn pr_comment_location(f: &Finding) -> String {
    match f.primary_path() {
        Some(p) => match finding_line(f) {
            Some(line) => format!("`{}:{line}`", p.display()),
            None => format!("`{}`", p.display()),
        },
        None => "—".to_string(),
    }
}

fn escape_pipe(s: &str) -> String {
    // Markdown tables use `|` as the column separator; evidence text
    // may contain pipes that we need to escape.
    s.replace('|', "\\|").replace('\n', " ")
}

fn group_by_severity(findings: &[Finding]) -> BTreeMap<SeverityClass, Vec<&Finding>> {
    let mut out: BTreeMap<SeverityClass, Vec<&Finding>> = BTreeMap::new();
    for f in findings {
        out.entry(f.severity).or_default().push(f);
    }
    out
}

/// One-line human summary of a finding — renders across all output formats.
fn finding_summary(f: &Finding) -> String {
    match &f.kind {
        FindingKind::TestReference {
            test,
            matched_symbols,
        } => format!(
            "test `{}` ({}) references {}",
            test.symbol,
            test.file.display(),
            matched_symbols.join(", ")
        ),
        FindingKind::TraitImpl {
            trait_name,
            impl_for,
            impl_site,
        } => format!(
            "impl `{trait_name}` for `{impl_for}` ({})",
            impl_site.file.display()
        ),
        FindingKind::DerivedTraitImpl {
            trait_name,
            impl_for,
            derive_site,
        } => format!(
            "`#[derive({trait_name})]` on `{impl_for}` ({})",
            derive_site.file.display()
        ),
        FindingKind::DynDispatch { trait_name, site } => {
            format!("`dyn {trait_name}` used in {}", site.file.display())
        }
        FindingKind::DocDriftLink { symbol, doc, line } => format!(
            "intra-doc link to `{symbol}` in {}:{line}",
            doc.file.display()
        ),
        FindingKind::DocDriftKeyword { symbol, doc, line } => {
            format!("`{symbol}` mentioned in {}:{line}", doc.file.display())
        }
        FindingKind::FfiSignatureChange {
            symbol,
            file,
            change,
        } => format!("FFI `{symbol}` {change} in {}", file.display()),
        FindingKind::BuildScriptChanged { file } => {
            format!("`build.rs` changed ({})", file.display())
        }
        FindingKind::SemverCheck { level, .. } => {
            format!("cargo-semver-checks reports `{level}` public-API change")
        }
        FindingKind::ResolvedReference {
            source_symbol,
            target,
        } => format!(
            "resolved reference: `{source_symbol}` used by `{}` in {}",
            target.symbol,
            target.file.display()
        ),
        FindingKind::RuntimeSurface {
            framework,
            identifier,
            site,
        } => format!(
            "{framework} runtime surface `{identifier}` ({})",
            site.file.display()
        ),
        FindingKind::TraitDefinitionChange {
            trait_name,
            method,
            change,
            file,
        } => match method {
            Some(m) => format!(
                "trait `{trait_name}`: {} — `{m}` ({})",
                change.phrase(),
                file.display()
            ),
            None => format!(
                "trait `{trait_name}`: {} ({})",
                change.phrase(),
                file.display()
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::{Finding, FindingKind, Location, Tier};
    use std::path::PathBuf;

    fn sample_finding(id: &str) -> Finding {
        let kind = FindingKind::TestReference {
            test: Location {
                file: PathBuf::from("tests/smoke.rs"),
                symbol: "smoke".into(),
            },
            matched_symbols: vec!["login".into()],
        };
        Finding::new(id, Tier::Likely, 0.85, kind, "direct ref")
    }

    #[test]
    fn json_envelope_has_documented_fields() {
        let findings = vec![sample_finding("f-0001")];
        let out =
            render_json(&[PathBuf::from("src/lib.rs")], &["login".into()], &findings).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["version"].is_string());
        assert_eq!(v["changed_files"].as_array().unwrap().len(), 1);
        assert_eq!(v["findings"].as_array().unwrap().len(), 1);
        assert_eq!(v["summary"]["total"], 1);
        assert_eq!(v["findings"][0]["kind"], "test_reference");
    }

    #[test]
    fn markdown_renders_severity_sections_and_checklist() {
        let findings = vec![sample_finding("f-0001")];
        let md = render_markdown(
            &[PathBuf::from("src/lib.rs")],
            &["login".into()],
            &findings,
            0,
        );
        assert!(md.contains("# cargo-impact"));
        assert!(md.contains("🟡 MEDIUM"));
        assert!(md.contains("## Verification checklist"));
        assert!(md.contains("- [ ]"));
        assert!(md.contains("f-0001"));
    }

    #[test]
    fn markdown_budget_zero_matches_unlimited_behavior() {
        let findings = vec![
            sample_finding("a"),
            sample_finding("b"),
            sample_finding("c"),
        ];
        let changed = [PathBuf::from("src/lib.rs")];
        let symbols = ["login".into()];
        let unlimited = render_markdown(&changed, &symbols, &findings, 0);
        let also_unlimited = render_markdown(&changed, &symbols, &findings, usize::MAX);
        assert_eq!(unlimited, also_unlimited);
        assert!(!unlimited.contains("Budget truncation"));
    }

    #[test]
    fn markdown_budget_truncates_and_emits_footer_with_accurate_counts() {
        // 20 findings; a tight budget that fits the header+summary plus
        // at most a handful of bullets. The renderer must stop emitting,
        // then tell us *exactly* how many were dropped on each list so
        // the agent can decide whether to re-request with a larger cap
        // or switch to --format=json.
        let findings: Vec<Finding> = (0..20)
            .map(|i| sample_finding(&format!("f-{i:02}")))
            .collect();
        let md = render_markdown(
            &[PathBuf::from("src/lib.rs")],
            &["login".into()],
            &findings,
            1200,
        );

        // Header + summary always render — never drop them, even under a
        // hostile budget.
        assert!(md.contains("# cargo-impact"));
        assert!(md.contains("## Summary"));
        // Truncation footer names the limit and includes non-zero counts.
        assert!(md.contains("Budget truncation"));
        assert!(md.contains("--budget=1200"));
        // Output itself stayed under the budget (we never exceed).
        assert!(
            md.len() <= 1200 + 600, // footer itself is allowed to exceed slightly
            "rendered {} chars for budget 1200 — should stay close",
            md.len()
        );
    }

    #[test]
    fn markdown_budget_preserves_highest_priority_findings() {
        // Mix High + Medium + Low. The sort order in lib.rs is severity
        // ascending (High first), so emitting top-down hits High before
        // Low. Tiny budget should keep at least one HIGH finding.
        let mk = |id: &str, sev: crate::finding::SeverityClass| {
            let mut f = sample_finding(id);
            f.severity = sev;
            f
        };
        use crate::finding::SeverityClass as S;
        let findings = vec![
            mk("high-1", S::High),
            mk("high-2", S::High),
            mk("med-1", S::Medium),
            mk("med-2", S::Medium),
            mk("low-1", S::Low),
        ];
        let md = render_markdown(
            &[PathBuf::from("src/lib.rs")],
            &["login".into()],
            &findings,
            900,
        );
        assert!(
            md.contains("high-1"),
            "highest-priority finding must survive truncation"
        );
        // And the truncation message confirms omission happened.
        assert!(md.contains("Budget truncation"));
    }

    #[test]
    fn markdown_budget_header_always_emitted_even_if_it_alone_exceeds() {
        // Pathological: budget smaller than the header. We still emit
        // the header + summary (the renderer's contract is "shape stays
        // intact"), then the truncation footer explains what happened.
        let findings = vec![sample_finding("f-0001")];
        let md = render_markdown(
            &[PathBuf::from("src/lib.rs")],
            &["login".into()],
            &findings,
            10,
        );
        assert!(md.contains("# cargo-impact"));
        assert!(md.contains("## Summary"));
        // Every finding and every checklist item should be omitted.
        assert!(md.contains("Budget truncation"));
    }

    #[test]
    fn text_renders_all_severity_buckets_even_when_empty() {
        let text = render_text(
            &[PathBuf::from("src/lib.rs")],
            &["login".into()],
            &[sample_finding("f-0001")],
        );
        // Every severity bucket header must appear so the output is consistent
        // across runs, even when a bucket is empty.
        assert!(text.contains("HIGH (0)"));
        assert!(text.contains("MEDIUM (1)"));
        assert!(text.contains("LOW (0)"));
        assert!(text.contains("UNKNOWN (0)"));
    }

    #[test]
    fn empty_findings_produce_valid_json() {
        let out = render_json(&[], &[], &[]).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["summary"]["total"], 0);
        assert_eq!(v["findings"].as_array().unwrap().len(), 0);
    }

    // --- SARIF ---

    #[test]
    fn sarif_emits_stable_envelope_and_schema() {
        let findings = vec![sample_finding("f-0001")];
        let out = render_sarif(&findings).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();

        assert_eq!(v["version"], "2.1.0");
        assert_eq!(
            v["$schema"],
            "https://json.schemastore.org/sarif-2.1.0.json"
        );
        let driver = &v["runs"][0]["tool"]["driver"];
        assert_eq!(driver["name"], "cargo-impact");
        assert!(driver["rules"].as_array().unwrap().len() >= 12);

        let result = &v["runs"][0]["results"][0];
        assert_eq!(result["ruleId"], "test_reference");
        assert_eq!(result["message"]["text"], "direct ref");
        assert_eq!(
            result["partialFingerprints"]["primaryLocationLineHash"],
            "f-0001"
        );
    }

    #[test]
    fn sarif_maps_severity_to_sarif_level() {
        assert_eq!(sarif_level(SeverityClass::High), "error");
        assert_eq!(sarif_level(SeverityClass::Medium), "warning");
        assert_eq!(sarif_level(SeverityClass::Low), "note");
        assert_eq!(sarif_level(SeverityClass::Unknown), "none");
    }

    #[test]
    fn sarif_rule_names_use_camel_case() {
        assert_eq!(sarif_rule_name("test_reference"), "testReference");
        assert_eq!(
            sarif_rule_name("ffi_signature_change"),
            "ffiSignatureChange"
        );
        assert_eq!(sarif_rule_name("trait_impl"), "traitImpl");
    }

    #[test]
    fn sarif_rules_cover_every_finding_kind() {
        // The SARIF rules list must enumerate all FindingKind tags so
        // scanners can hyperlink every result back to a rule.
        let rules_tags: std::collections::BTreeSet<String> = FindingKind::all_tags()
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(rules_tags.len(), 12);
        for tag in FindingKind::all_tags() {
            assert!(
                !sarif_rule_short(tag).is_empty(),
                "missing rule description for {tag}"
            );
        }
    }

    #[test]
    fn sarif_includes_region_only_when_line_available() {
        // DocDriftLink carries a line number — region should be present.
        let drift = FindingKind::DocDriftLink {
            symbol: "Foo".into(),
            doc: crate::finding::Location {
                file: PathBuf::from("docs/arch.md"),
                symbol: "Foo".into(),
            },
            line: 42,
        };
        let f = Finding::new("f-x", Tier::Likely, 0.9, drift, "intra-doc link");
        let out = render_sarif(&[f]).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["region"]["startLine"],
            42
        );

        // TestReference has no line — region should be absent.
        let test_ref = sample_finding("f-y");
        let out2 = render_sarif(&[test_ref]).unwrap();
        let v2: serde_json::Value = serde_json::from_str(&out2).unwrap();
        assert!(
            v2["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["region"].is_null(),
            "region should be absent when no line is known"
        );
    }

    #[test]
    fn sarif_handles_empty_findings() {
        let out = render_sarif(&[]).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["runs"][0]["results"].as_array().unwrap().len(), 0);
        // Rules list is still populated — schema-defined even when no results.
        assert!(
            v["runs"][0]["tool"]["driver"]["rules"]
                .as_array()
                .unwrap()
                .len()
                >= 12
        );
    }

    // --- PR comment ---

    #[test]
    fn pr_comment_shape_is_stable() {
        let findings = vec![sample_finding("f-0001")];
        let out = render_pr_comment(
            &[PathBuf::from("src/lib.rs")],
            &["login".into()],
            &findings,
            0,
        );
        // Header line with severity badges.
        assert!(out.starts_with("### 🎯 cargo-impact — "));
        // One <details> block per severity that has findings.
        assert!(
            out.contains("<details open><summary>🔴 HIGH")
                || out.contains("<details><summary>🟡 MEDIUM")
        );
        // Context block at the end.
        assert!(out.contains("📁 1 changed file · 1 candidate symbol"));
        // Tool attribution sub-footer.
        assert!(out.contains("cargo-impact v"));
    }

    #[test]
    fn pr_comment_expands_high_by_default_collapses_others() {
        let mk = |id: &str, sev: SeverityClass| {
            let mut f = sample_finding(id);
            f.severity = sev;
            f
        };
        let findings = vec![
            mk("high-1", SeverityClass::High),
            mk("med-1", SeverityClass::Medium),
            mk("low-1", SeverityClass::Low),
        ];
        let out = render_pr_comment(
            &[PathBuf::from("src/lib.rs")],
            &["login".into()],
            &findings,
            0,
        );
        assert!(out.contains("<details open><summary>🔴 HIGH (1)"));
        assert!(out.contains("<details><summary>🟡 MEDIUM (1)"));
        assert!(out.contains("<details><summary>🔵 LOW (1)"));
    }

    #[test]
    fn pr_comment_empty_findings_still_emits_header() {
        let out = render_pr_comment(&[PathBuf::from("src/lib.rs")], &[], &[], 0);
        assert!(out.contains("0 findings"));
        assert!(out.contains("_No findings — nothing to verify._"));
    }

    #[test]
    fn pr_comment_escapes_pipes_in_evidence() {
        let kind = FindingKind::TestReference {
            test: crate::finding::Location {
                file: PathBuf::from("tests/t.rs"),
                symbol: "t".into(),
            },
            matched_symbols: vec!["f".into()],
        };
        let f = Finding::new(
            "f-pipe",
            Tier::Likely,
            0.8,
            kind,
            "evidence with | pipe char",
        );
        let out = render_pr_comment(&[], &[], &[f], 0);
        assert!(out.contains("with \\| pipe"));
    }

    #[test]
    fn pr_comment_budget_truncates_with_notice() {
        let findings: Vec<Finding> = (0..30)
            .map(|i| sample_finding(&format!("f-{i:02}")))
            .collect();
        let out = render_pr_comment(
            &[PathBuf::from("src/lib.rs")],
            &["login".into()],
            &findings,
            1000,
        );
        assert!(out.contains("findings omitted"));
        assert!(out.contains("--budget=1000"));
    }
}
