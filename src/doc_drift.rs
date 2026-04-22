//! Documentation drift detection.
//!
//! Two signal strengths:
//!
//! 1. **Intra-doc links** — `` [`Symbol`] `` or `[Symbol]` in markdown or `///`
//!    comments. High confidence (`Likely 0.90`) because the doc *explicitly*
//!    references the symbol as a link.
//!
//! 2. **Keyword hits** — a bare identifier appearing as a word in doc prose.
//!    Weaker (`Possible 0.40`) and gated by a minimum symbol length (≥ 6
//!    chars) to avoid false positives on common words like `User` or `Foo`.
//!    One finding per (symbol, doc-file) pair; multiple occurrences collapse.

use crate::finding::{Finding, FindingKind, Location, Tier};
use anyhow::Result;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use walkdir::{DirEntry, WalkDir};

const KEYWORD_MIN_LEN: usize = 6;

pub fn find_doc_drift(root: &Path, changed_symbols: &BTreeSet<String>) -> Result<Vec<Finding>> {
    if changed_symbols.is_empty() {
        return Ok(Vec::new());
    }

    let mut findings = Vec::new();

    // Markdown files under the repo.
    for entry in walk_docs(root) {
        let path = entry.path();
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        scan_lines(&src, &rel, changed_symbols, &mut findings);
    }

    // `///` and `//!` doc comments inside Rust source files.
    for entry in walk_rust(root) {
        let path = entry.path();
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        let docs = extract_doc_lines(&src);
        scan_lines_with_source_map(&docs, &rel, changed_symbols, &mut findings);
    }

    Ok(findings)
}

fn walk_docs(root: &Path) -> impl Iterator<Item = DirEntry> {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_skippable(e))
        .filter_map(std::result::Result::ok)
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("md"))
}

fn walk_rust(root: &Path) -> impl Iterator<Item = DirEntry> {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_skippable(e))
        .filter_map(std::result::Result::ok)
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("rs"))
}

fn is_skippable(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    (name.starts_with('.') && entry.file_type().is_dir()) || name == "target"
}

/// Extract `///` and `//!` comment bodies from Rust source, paired with their
/// 1-based line numbers. The body retains the original content (minus the
/// `///` or `//!` prefix and at most one leading space).
fn extract_doc_lines(src: &str) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for (idx, line) in src.lines().enumerate() {
        let trimmed = line.trim_start();
        let body = trimmed
            .strip_prefix("///")
            .or_else(|| trimmed.strip_prefix("//!"));
        if let Some(b) = body {
            // Strip at most one leading space so markdown rendering is stable.
            let cleaned = b.strip_prefix(' ').unwrap_or(b);
            out.push((
                u32::try_from(idx + 1).unwrap_or(u32::MAX),
                cleaned.to_string(),
            ));
        }
    }
    out
}

fn scan_lines(src: &str, rel: &Path, changed: &BTreeSet<String>, out: &mut Vec<Finding>) {
    let mut seen_link: BTreeSet<(String, PathBuf)> = BTreeSet::new();
    let mut seen_keyword: BTreeSet<(String, PathBuf)> = BTreeSet::new();
    for (idx, line) in src.lines().enumerate() {
        let lineno = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        emit_for_line(
            line,
            lineno,
            rel,
            changed,
            &mut seen_link,
            &mut seen_keyword,
            out,
        );
    }
}

fn scan_lines_with_source_map(
    doc_lines: &[(u32, String)],
    rel: &Path,
    changed: &BTreeSet<String>,
    out: &mut Vec<Finding>,
) {
    let mut seen_link: BTreeSet<(String, PathBuf)> = BTreeSet::new();
    let mut seen_keyword: BTreeSet<(String, PathBuf)> = BTreeSet::new();
    for (lineno, line) in doc_lines {
        emit_for_line(
            line,
            *lineno,
            rel,
            changed,
            &mut seen_link,
            &mut seen_keyword,
            out,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_for_line(
    line: &str,
    lineno: u32,
    rel: &Path,
    changed: &BTreeSet<String>,
    seen_link: &mut BTreeSet<(String, PathBuf)>,
    seen_keyword: &mut BTreeSet<(String, PathBuf)>,
    out: &mut Vec<Finding>,
) {
    for bracketed in extract_bracketed(line) {
        if changed.contains(&bracketed) {
            let key = (bracketed.clone(), rel.to_path_buf());
            if seen_link.insert(key) {
                let kind = FindingKind::DocDriftLink {
                    symbol: bracketed.clone(),
                    doc: Location {
                        file: rel.to_path_buf(),
                        symbol: bracketed.clone(),
                    },
                    line: lineno,
                };
                let evidence = format!(
                    "`{bracketed}` referenced via intra-doc link in {}:{lineno}",
                    rel.display()
                );
                out.push(Finding::new("", Tier::Likely, 0.90, kind, evidence));
            }
        }
    }

    for tok in line.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if tok.len() >= KEYWORD_MIN_LEN && changed.contains(tok) {
            let key = (tok.to_string(), rel.to_path_buf());
            if seen_keyword.insert(key.clone()) && !seen_link.contains(&key) {
                let kind = FindingKind::DocDriftKeyword {
                    symbol: tok.to_string(),
                    doc: Location {
                        file: rel.to_path_buf(),
                        symbol: tok.to_string(),
                    },
                    line: lineno,
                };
                let evidence = format!(
                    "`{tok}` mentioned in {}:{lineno} (plain keyword, not an intra-doc link)",
                    rel.display()
                );
                out.push(Finding::new("", Tier::Possible, 0.40, kind, evidence));
            }
        }
    }
}

/// Extract identifiers appearing as `[Ident]` or `` [`Ident`] ``. For
/// qualified paths like `` [`Trait::method`] `` we extract the leading
/// identifier (`Trait`) — at v0.2 file-level precision that is the most
/// useful signal. Ignores markdown link-with-URL form `[text](url)` by
/// rejecting bracketed content immediately followed by `(`.
fn extract_bracketed(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            if let Some(close) = line[i + 1..].find(']') {
                let inner = &line[i + 1..i + 1 + close];
                let after = i + 2 + close;
                // Skip `[text](url)` — that's a hyperlink, not an intra-doc link.
                if after < bytes.len() && bytes[after] == b'(' {
                    i = after;
                    continue;
                }
                let stripped = inner.trim().trim_matches('`');
                if let Some(ident) = leading_ident(stripped) {
                    out.push(ident);
                }
                i = after;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Return the leading identifier of `s` if it is either a plain identifier
/// (`Greeter`) or the head of a qualified path (`Greeter::hi` → `Greeter`).
/// Anything else — operators, lifetimes, numeric literals — returns `None`.
fn leading_ident(s: &str) -> Option<String> {
    let head = s.split("::").next().unwrap_or(s).trim();
    if is_plain_ident(head) {
        Some(head.to_string())
    } else {
        None
    }
}

fn is_plain_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn symbols(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn extracts_bracketed_intra_doc_link_with_backticks() {
        let got = extract_bracketed("See [`PaymentGateway`] for details.");
        assert_eq!(got, vec!["PaymentGateway".to_string()]);
    }

    #[test]
    fn extracts_bracketed_plain_form() {
        let got = extract_bracketed("See [Greeter] for the trait.");
        assert_eq!(got, vec!["Greeter".to_string()]);
    }

    #[test]
    fn ignores_markdown_url_link() {
        let got = extract_bracketed("[docs](https://example.com)");
        assert!(got.is_empty());
    }

    #[test]
    fn flags_intra_doc_link_in_markdown_file() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("docs.md"),
            "See [`Greeter`] for the greeting trait.\n",
        )
        .unwrap();
        let hits = find_doc_drift(dir.path(), &symbols(&["Greeter"])).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].tier, Tier::Likely);
        assert_eq!(hits[0].confidence, 0.90);
        match &hits[0].kind {
            FindingKind::DocDriftLink { symbol, line, .. } => {
                assert_eq!(symbol, "Greeter");
                assert_eq!(*line, 1);
            }
            other => panic!("expected DocDriftLink, got {other:?}"),
        }
    }

    #[test]
    fn flags_intra_doc_link_inside_rust_doc_comment() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("lib.rs"),
            "/// Call [`Greeter::hi`] to greet.\npub fn go() {}\n",
        )
        .unwrap();
        let hits = find_doc_drift(dir.path(), &symbols(&["Greeter"])).unwrap();
        // Qualified paths resolve to their leading identifier, so
        // `[`Greeter::hi`]` matches the trait name at its usual (Likely) tier.
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind.tag(), "doc_drift_link");
        match &hits[0].kind {
            FindingKind::DocDriftLink { symbol, line, .. } => {
                assert_eq!(symbol, "Greeter");
                assert_eq!(*line, 1);
            }
            other => panic!("expected DocDriftLink, got {other:?}"),
        }
    }

    #[test]
    fn flags_keyword_mention_only_for_long_identifiers() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("doc.md"),
            "The Greeter trait is important.\nAlso see Foo.\n",
        )
        .unwrap();
        // "Greeter" (7 chars) → keyword hit. "Foo" (3 chars) → filtered out.
        let hits = find_doc_drift(dir.path(), &symbols(&["Greeter", "Foo"])).unwrap();
        let tags: Vec<_> = hits.iter().map(|h| h.kind.tag()).collect();
        assert_eq!(tags, vec!["doc_drift_keyword"]);
        match &hits[0].kind {
            FindingKind::DocDriftKeyword { symbol, .. } => assert_eq!(symbol, "Greeter"),
            other => panic!("expected DocDriftKeyword, got {other:?}"),
        }
    }

    #[test]
    fn link_finding_suppresses_duplicate_keyword_finding() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("doc.md"),
            "See [`PaymentGateway`] — the PaymentGateway struct.\n",
        )
        .unwrap();
        let hits = find_doc_drift(dir.path(), &symbols(&["PaymentGateway"])).unwrap();
        let tags: Vec<_> = hits.iter().map(|h| h.kind.tag()).collect();
        // Exactly one DocDriftLink; the keyword scan must not also fire on the
        // same (symbol, file) pair.
        assert_eq!(tags, vec!["doc_drift_link"]);
    }

    #[test]
    fn skips_target_directory() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("target/doc")).unwrap();
        fs::write(
            dir.path().join("target/doc/generated.md"),
            "See [`Greeter`] in the docs.\n",
        )
        .unwrap();
        let hits = find_doc_drift(dir.path(), &symbols(&["Greeter"])).unwrap();
        assert!(hits.is_empty());
    }
}
