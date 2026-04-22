use crate::finding::{Finding, FindingKind};

/// Build a `cargo-nextest` filter expression matching every test referenced
/// by a [`FindingKind::TestReference`] finding. Returns an empty string when
/// no test findings exist so callers can cheaply detect the no-op case.
///
/// Non-test findings are ignored — this is strictly the "which tests should
/// I run?" projection of the blast radius. Duplicates are deduped so the
/// expression remains compact even when several analyzers reference the
/// same test.
pub fn filter_expression(findings: &[Finding]) -> String {
    use std::collections::BTreeSet;
    let names: BTreeSet<&str> = findings
        .iter()
        .filter_map(|f| match &f.kind {
            FindingKind::TestReference { test, .. } => Some(test.symbol.as_str()),
            _ => None,
        })
        .collect();
    if names.is_empty() {
        return String::new();
    }
    names
        .into_iter()
        .map(|n| format!("test({n})"))
        .collect::<Vec<_>>()
        .join(" + ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::{Finding, FindingKind, Location, Tier};
    use std::path::PathBuf;

    fn test_ref(name: &str) -> Finding {
        let kind = FindingKind::TestReference {
            test: Location {
                file: PathBuf::from("tests/t.rs"),
                symbol: name.into(),
            },
            matched_symbols: vec!["login".into()],
        };
        Finding::new("", Tier::Likely, 0.85, kind, "")
    }

    fn trait_impl() -> Finding {
        let kind = FindingKind::TraitImpl {
            trait_name: "Greeter".into(),
            impl_for: "Foo".into(),
            impl_site: Location {
                file: PathBuf::from("src/lib.rs"),
                symbol: "impl Greeter for Foo".into(),
            },
        };
        Finding::new("", Tier::Likely, 0.8, kind, "")
    }

    #[test]
    fn empty_input_yields_empty_filter() {
        assert_eq!(filter_expression(&[]), "");
    }

    #[test]
    fn single_test_reference() {
        assert_eq!(filter_expression(&[test_ref("foo")]), "test(foo)");
    }

    #[test]
    fn multiple_tests_joined_with_or() {
        let out = filter_expression(&[test_ref("a"), test_ref("b"), test_ref("c")]);
        assert_eq!(out, "test(a) + test(b) + test(c)");
    }

    #[test]
    fn non_test_findings_are_skipped() {
        let out = filter_expression(&[test_ref("real"), trait_impl()]);
        assert_eq!(out, "test(real)");
    }

    #[test]
    fn duplicate_test_names_collapse() {
        let out = filter_expression(&[test_ref("dup"), test_ref("dup"), test_ref("other")]);
        assert_eq!(out, "test(dup) + test(other)");
    }
}
