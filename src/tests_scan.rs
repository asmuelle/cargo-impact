use crate::finding::{Finding, FindingKind, Location, Tier};
use anyhow::Result;
use quote::ToTokens;
use std::collections::BTreeSet;
use std::path::Path;
use syn::visit::Visit;
use syn::{Attribute, ItemFn};
use walkdir::{DirEntry, WalkDir};

/// Scan `root` for test functions whose body references any changed symbol.
/// Returns findings without IDs — the orchestrator assigns final IDs after
/// aggregating all analyzers.
///
/// Confidence tier is `Likely` (0.85) rather than `Proven`: without resolved
/// name lookup (rust-analyzer integration lands in v0.3) we cannot distinguish
/// a real call from a shadowed identifier, so perfectly honest syn-only
/// analysis tops out at this tier.
pub fn find_affected_tests(
    root: &Path,
    changed_symbols: &BTreeSet<String>,
) -> Result<Vec<Finding>> {
    if changed_symbols.is_empty() {
        return Ok(Vec::new());
    }

    let mut findings = Vec::new();

    for entry in workspace_rust_files(root) {
        let path = entry.path();
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        let Some(ast) = crate::cfg::parse_and_filter(&src) else {
            continue;
        };

        let mut visitor = TestVisitor {
            changed: changed_symbols,
            hits: Vec::new(),
        };
        visitor.visit_file(&ast);

        let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        for (test_name, matched) in visitor.hits {
            let matched_vec: Vec<String> = matched.into_iter().collect();
            let evidence = format!(
                "test body references {} (syntactic match, no name resolution)",
                matched_vec.join(", ")
            );
            let kind = FindingKind::TestReference {
                test: Location {
                    file: rel.clone(),
                    symbol: test_name.clone(),
                },
                matched_symbols: matched_vec,
            };
            findings.push(
                Finding::new("", Tier::Likely, 0.85, kind, evidence)
                    .with_suggested_action(format!("cargo nextest run -E 'test({test_name})'")),
            );
        }
    }

    Ok(findings)
}

/// Shared workspace iterator used by every analyzer. Yields `.rs` files under
/// `root`, skipping `target/` and hidden directories (`.git`, `.github`).
pub(crate) fn workspace_rust_files(root: &Path) -> impl Iterator<Item = DirEntry> {
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

struct TestVisitor<'a> {
    changed: &'a BTreeSet<String>,
    hits: Vec<(String, BTreeSet<String>)>,
}

impl<'ast> Visit<'ast> for TestVisitor<'_> {
    fn visit_item_fn(&mut self, f: &'ast ItemFn) {
        if is_test_fn(&f.attrs) {
            let body = f.block.to_token_stream().to_string();
            let matched: BTreeSet<String> = self
                .changed
                .iter()
                .filter(|sym| tokens_contain_ident(&body, sym))
                .cloned()
                .collect();
            if !matched.is_empty() {
                self.hits.push((f.sig.ident.to_string(), matched));
            }
        }
        syn::visit::visit_item_fn(self, f);
    }
}

fn is_test_fn(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path()
            .segments
            .last()
            .map(|s| matches!(s.ident.to_string().as_str(), "test" | "rstest" | "bench"))
            .unwrap_or(false)
    })
}

/// Word-boundary identifier search. Relies on `quote`'s token-stream producing
/// whitespace between adjacent tokens, so splitting on non-word characters is
/// reliable — `"user"` never matches `"user_profile"`.
fn tokens_contain_ident(haystack: &str, needle: &str) -> bool {
    haystack
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|tok| tok == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        for (rel, body) in files {
            let p = dir.path().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        dir
    }

    fn symbols(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn matches_test_that_references_changed_symbol() {
        let dir = setup(&[(
            "tests/smoke.rs",
            r#"
                #[test]
                fn smoke() {
                    let _ = login();
                }
            "#,
        )]);
        let hits = find_affected_tests(dir.path(), &symbols(&["login"])).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].tier, Tier::Likely);
        assert_eq!(hits[0].confidence, 0.85);
        match &hits[0].kind {
            FindingKind::TestReference {
                test,
                matched_symbols,
            } => {
                assert_eq!(test.symbol, "smoke");
                assert_eq!(matched_symbols, &vec!["login".to_string()]);
            }
            other => panic!("expected TestReference, got {other:?}"),
        }
        assert!(
            hits[0]
                .suggested_action
                .as_deref()
                .is_some_and(|s| s.contains("test(smoke)"))
        );
    }

    #[test]
    fn ignores_non_test_functions() {
        let dir = setup(&[("src/lib.rs", "fn helper() { let _ = login(); }")]);
        let hits = find_affected_tests(dir.path(), &symbols(&["login"])).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn matches_tokio_test_and_rstest_attributes() {
        let dir = setup(&[(
            "tests/async.rs",
            r#"
                #[tokio::test]
                async fn async_smoke() { login().await; }

                #[rstest]
                fn parametrized(#[case] _x: u32) { let _ = login; }
            "#,
        )]);
        let hits = find_affected_tests(dir.path(), &symbols(&["login"])).unwrap();
        let names: Vec<_> = hits
            .iter()
            .map(|h| match &h.kind {
                FindingKind::TestReference { test, .. } => test.symbol.clone(),
                _ => String::new(),
            })
            .collect();
        assert!(names.contains(&"async_smoke".to_string()));
        assert!(names.contains(&"parametrized".to_string()));
    }

    #[test]
    fn does_not_match_substring_identifiers() {
        let dir = setup(&[(
            "tests/false_positive.rs",
            "#[test] fn t() { let login_helper = 1; let _ = login_helper; }",
        )]);
        let hits = find_affected_tests(dir.path(), &symbols(&["login"])).unwrap();
        assert!(hits.is_empty(), "unexpected hits: {hits:?}");
    }

    #[test]
    fn empty_changed_set_returns_empty() {
        let dir = TempDir::new().unwrap();
        let hits = find_affected_tests(dir.path(), &BTreeSet::new()).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn skips_target_and_hidden_directories() {
        let dir = setup(&[
            (
                "target/debug/build.rs",
                "#[test] fn should_not_match() { login(); }",
            ),
            (
                ".git/hooks/pre-commit.rs",
                "#[test] fn should_not_match_either() { login(); }",
            ),
            ("tests/real.rs", "#[test] fn real() { let _ = login(); }"),
        ]);
        let hits = find_affected_tests(dir.path(), &symbols(&["login"])).unwrap();
        let names: Vec<_> = hits
            .iter()
            .map(|h| match &h.kind {
                FindingKind::TestReference { test, .. } => test.symbol.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(names, vec!["real".to_string()]);
    }
}
