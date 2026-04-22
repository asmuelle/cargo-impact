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
#[value(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Text,
    Markdown,
    Json,
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
    match format {
        Format::Text => Ok(render_text(changed_files, candidate_symbols, findings)),
        Format::Markdown => Ok(render_markdown(changed_files, candidate_symbols, findings)),
        Format::Json => render_json(changed_files, candidate_symbols, findings),
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
) -> String {
    let mut out = String::new();
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
        out.push_str(&format!(
            "## {icon} {label} ({n})\n\n",
            icon = severity.icon(),
            label = severity.as_label(),
            n = group.len()
        ));
        for f in group {
            out.push_str(&format!(
                "- **[{id}]** {summary} — *{tier:?} {conf:.2}* — {evidence}\n",
                id = f.id,
                summary = finding_summary(f),
                tier = f.tier,
                conf = f.confidence,
                evidence = f.evidence,
            ));
            if let Some(action) = &f.suggested_action {
                out.push_str(&format!("  - Suggested: `{action}`\n"));
            }
        }
        out.push('\n');
    }

    out.push_str("## Verification checklist\n\n");
    if findings.is_empty() {
        out.push_str("_No findings — nothing to verify._\n");
    } else {
        for f in findings {
            out.push_str(&format!(
                "- [ ] **{label}** {summary} — *{tier:?} {conf:.2}*\n",
                label = f.severity.as_label(),
                summary = finding_summary(f),
                tier = f.tier,
                conf = f.confidence,
            ));
        }
    }

    out
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
        let md = render_markdown(&[PathBuf::from("src/lib.rs")], &["login".into()], &findings);
        assert!(md.contains("# cargo-impact"));
        assert!(md.contains("🟡 MEDIUM"));
        assert!(md.contains("## Verification checklist"));
        assert!(md.contains("- [ ]"));
        assert!(md.contains("f-0001"));
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
}
