use anyhow::Result;
use quote::ToTokens;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use syn::visit::Visit;
use syn::{Attribute, ItemFn};
use walkdir::{DirEntry, WalkDir};

/// A test function that references at least one of the changed symbols.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AffectedTest {
    /// Test function name (the leaf identifier, not the fully-qualified path).
    pub name: String,
    /// Source file containing the test, relative to the repository root.
    pub file: PathBuf,
    /// The subset of changed symbols this test body references.
    pub matched_symbols: Vec<String>,
}

/// Scan `root` for test functions whose body references any of
/// `changed_symbols`. Files that fail to parse as Rust are skipped silently —
/// the scan is best-effort and must not fail the whole run.
pub fn find_affected_tests(
    root: &Path,
    changed_symbols: &BTreeSet<String>,
) -> Result<Vec<AffectedTest>> {
    if changed_symbols.is_empty() {
        return Ok(Vec::new());
    }

    let mut hits: BTreeMap<(PathBuf, String), BTreeSet<String>> = BTreeMap::new();

    let walker = WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_skippable(e));

    for entry in walker.filter_map(std::result::Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(ast) = syn::parse_file(&src) else {
            continue;
        };

        let mut visitor = TestVisitor {
            changed: changed_symbols,
            hits: Vec::new(),
        };
        visitor.visit_file(&ast);

        let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        for (name, matched) in visitor.hits {
            hits.entry((rel.clone(), name)).or_default().extend(matched);
        }
    }

    Ok(hits
        .into_iter()
        .map(|((file, name), matched)| AffectedTest {
            name,
            file,
            matched_symbols: matched.into_iter().collect(),
        })
        .collect())
}

fn is_skippable(entry: &DirEntry) -> bool {
    // Always descend into the starting directory itself.
    if entry.depth() == 0 {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    // Hidden directories (`.git`, `.github`) and the cargo `target` dir.
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
        // Still recurse so nested `mod tests { #[test] fn ... }` is visited.
        syn::visit::visit_item_fn(self, f);
    }
}

/// True if any attribute on the function marks it as a test. Uses the last
/// path segment so `#[test]`, `#[tokio::test]`, `#[rstest]`, `#[test_case(...)]`,
/// and `#[bench]` all qualify without enumerating every framework.
fn is_test_fn(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path()
            .segments
            .last()
            .map(|s| matches!(s.ident.to_string().as_str(), "test" | "rstest" | "bench"))
            .unwrap_or(false)
    })
}

/// Word-boundary identifier search against a `quote`-produced token string.
/// The haystack has whitespace between every token, so splitting on
/// non-word characters is reliable — `"user"` won't match `"user_profile"`.
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
        let changed: BTreeSet<String> = ["login"].iter().map(|s| s.to_string()).collect();
        let hits = find_affected_tests(dir.path(), &changed).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "smoke");
        assert_eq!(hits[0].matched_symbols, vec!["login"]);
    }

    #[test]
    fn ignores_non_test_functions() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"
                fn helper() { let _ = login(); }
            "#,
        )]);
        let changed: BTreeSet<String> = ["login"].iter().map(|s| s.to_string()).collect();
        let hits = find_affected_tests(dir.path(), &changed).unwrap();
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
        let changed: BTreeSet<String> = ["login"].iter().map(|s| s.to_string()).collect();
        let hits = find_affected_tests(dir.path(), &changed).unwrap();
        let names: Vec<_> = hits.iter().map(|h| h.name.as_str()).collect();
        assert!(names.contains(&"async_smoke"));
        assert!(names.contains(&"parametrized"));
    }

    #[test]
    fn does_not_match_substring_identifiers() {
        let dir = setup(&[(
            "tests/false_positive.rs",
            r#"
                #[test]
                fn t() { let login_helper = 1; let _ = login_helper; }
            "#,
        )]);
        let changed: BTreeSet<String> = ["login"].iter().map(|s| s.to_string()).collect();
        let hits = find_affected_tests(dir.path(), &changed).unwrap();
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
        let changed: BTreeSet<String> = ["login"].iter().map(|s| s.to_string()).collect();
        let hits = find_affected_tests(dir.path(), &changed).unwrap();
        let names: Vec<_> = hits.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["real"]);
    }
}
