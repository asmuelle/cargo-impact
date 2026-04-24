//! Cross-analyzer finding dedup.
//!
//! Multiple analyzers can land on the same (changed symbol, containing
//! file) pair — e.g., syn's `TestReference` says "test `t_login` in
//! `tests/auth.rs` mentions the identifier `login`" while RA's
//! `ResolvedReference` says "a use of `login` at `tests/auth.rs:42`
//! name-resolves to the `login` we changed." Two findings, same site,
//! different tiers. We keep the Proven RA finding and drop the Likely
//! syn one so the report doesn't double-report and the tier counts
//! reflect actual coverage.
//!
//! Only syn-only findings whose "target name × file" match a Proven
//! `ResolvedReference` are dropped. Findings without a primary path
//! (SemverCheck), findings at Proven tier, and findings whose kind
//! doesn't carry a targetable name (BuildScriptChanged, FfiSignatureChange,
//! DocDriftLink/Keyword — those attach to the doc file, not a code
//! callsite) pass through untouched.
//!
//! A second dedup pass, [`dedup_expanded_under_raw`], drops macro-expansion
//! findings (file = `<expanded>`) whose test name is already covered by a
//! raw-source `TestReference`. Expanded trait-impls are kept — they typically
//! come from derives whose synthesized impls don't appear in any source file,
//! so the raw-source analyzer never emits them in the first place.

use crate::finding::{Finding, FindingKind, Tier};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const EXPANDED_SENTINEL: &str = "<expanded>";

/// Drop syn-only findings that are redundant with Proven RA findings at
/// the same (name, file) pair. Runs after all analyzers have pushed
/// their findings but before ID assignment so the dropped findings
/// never occupy IDs, summary slots, or render budget.
pub fn dedup_syn_under_proven(findings: &mut Vec<Finding>) {
    let proven_keys = collect_proven_keys(findings);
    if proven_keys.is_empty() {
        return;
    }
    findings.retain(|f| !is_shadowed(f, &proven_keys));
}

fn collect_proven_keys(findings: &[Finding]) -> BTreeSet<(String, PathBuf)> {
    let mut keys = BTreeSet::new();
    for f in findings {
        if f.tier != Tier::Proven {
            continue;
        }
        if let FindingKind::ResolvedReference {
            source_symbol,
            target,
        } = &f.kind
        {
            keys.insert((source_symbol.clone(), target.file.clone()));
        }
    }
    keys
}

fn is_shadowed(f: &Finding, keys: &BTreeSet<(String, PathBuf)>) -> bool {
    if f.tier == Tier::Proven {
        return false;
    }
    let Some(file) = f.primary_path().map(Path::to_path_buf) else {
        return false;
    };
    match &f.kind {
        FindingKind::TestReference {
            matched_symbols, ..
        } => matched_symbols
            .iter()
            .any(|s| keys.contains(&(s.clone(), file.clone()))),
        FindingKind::TraitImpl { trait_name, .. }
        | FindingKind::DerivedTraitImpl { trait_name, .. }
        | FindingKind::DynDispatch { trait_name, .. } => keys.contains(&(trait_name.clone(), file)),
        _ => false,
    }
}

/// Drop macro-expansion `TestReference` findings whose test name is
/// already covered by a raw-source `TestReference`. Without this, a
/// test that genuinely calls a changed symbol in its raw body (caught
/// by `tests_scan.rs`) and whose expansion also carries that reference
/// (caught here) would fire twice. The raw-source finding wins because
/// it has a real file path and a stronger confidence (0.85 vs 0.75).
pub fn dedup_expanded_under_raw(findings: &mut Vec<Finding>) {
    let raw_test_names = collect_raw_test_names(findings);
    if raw_test_names.is_empty() {
        return;
    }
    findings.retain(|f| !is_expanded_test_shadowed(f, &raw_test_names));
}

fn collect_raw_test_names(findings: &[Finding]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for f in findings {
        if let FindingKind::TestReference { test, .. } = &f.kind
            && test.file != Path::new(EXPANDED_SENTINEL)
        {
            names.insert(test.symbol.clone());
        }
    }
    names
}

fn is_expanded_test_shadowed(f: &Finding, raw_test_names: &BTreeSet<String>) -> bool {
    let FindingKind::TestReference { test, .. } = &f.kind else {
        return false;
    };
    test.file == Path::new(EXPANDED_SENTINEL) && raw_test_names.contains(&test.symbol)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::{Finding, FindingKind, Location, Tier};
    use std::path::PathBuf;

    fn proven_ref(target_file: &str, source_symbol: &str) -> Finding {
        let file = PathBuf::from(target_file);
        Finding::new(
            "",
            Tier::Proven,
            0.98,
            FindingKind::ResolvedReference {
                source_symbol: source_symbol.into(),
                target: Location {
                    file: file.clone(),
                    symbol: format!("{}:1", file.display()),
                },
            },
            "ra ref",
        )
    }

    fn test_ref(file: &str, test_symbol: &str, matched: &[&str]) -> Finding {
        Finding::new(
            "",
            Tier::Likely,
            0.85,
            FindingKind::TestReference {
                test: Location {
                    file: PathBuf::from(file),
                    symbol: test_symbol.into(),
                },
                matched_symbols: matched.iter().map(|s| (*s).to_string()).collect(),
            },
            "syn test",
        )
    }

    fn trait_impl(file: &str, trait_name: &str, impl_for: &str) -> Finding {
        Finding::new(
            "",
            Tier::Likely,
            0.80,
            FindingKind::TraitImpl {
                trait_name: trait_name.into(),
                impl_for: impl_for.into(),
                impl_site: Location {
                    file: PathBuf::from(file),
                    symbol: format!("impl {trait_name} for {impl_for}"),
                },
            },
            "syn impl",
        )
    }

    fn dyn_dispatch(file: &str, trait_name: &str) -> Finding {
        Finding::new(
            "",
            Tier::Likely,
            0.75,
            FindingKind::DynDispatch {
                trait_name: trait_name.into(),
                site: Location {
                    file: PathBuf::from(file),
                    symbol: format!("dyn {trait_name}"),
                },
            },
            "syn dyn",
        )
    }

    #[test]
    fn empty_set_leaves_findings_untouched() {
        let mut findings = vec![test_ref("tests/a.rs", "t1", &["login"])];
        dedup_syn_under_proven(&mut findings);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn proven_ref_shadows_test_reference_at_same_file_and_symbol() {
        let mut findings = vec![
            proven_ref("tests/auth.rs", "login"),
            test_ref("tests/auth.rs", "t_login", &["login"]),
        ];
        dedup_syn_under_proven(&mut findings);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].tier, Tier::Proven);
    }

    #[test]
    fn proven_ref_at_different_file_does_not_shadow() {
        let mut findings = vec![
            proven_ref("src/other.rs", "login"),
            test_ref("tests/auth.rs", "t_login", &["login"]),
        ];
        dedup_syn_under_proven(&mut findings);
        assert_eq!(findings.len(), 2);
    }

    #[test]
    fn proven_ref_for_different_symbol_does_not_shadow() {
        let mut findings = vec![
            proven_ref("tests/auth.rs", "logout"),
            test_ref("tests/auth.rs", "t_login", &["login"]),
        ];
        dedup_syn_under_proven(&mut findings);
        assert_eq!(findings.len(), 2);
    }

    #[test]
    fn test_reference_shadowed_by_any_matched_symbol() {
        // Syn emits one finding matching multiple symbols; if RA proves
        // any one of them, the syn finding is redundant.
        let mut findings = vec![
            proven_ref("tests/auth.rs", "login"),
            test_ref("tests/auth.rs", "t_auth", &["logout", "login", "refresh"]),
        ];
        dedup_syn_under_proven(&mut findings);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].tier, Tier::Proven);
    }

    #[test]
    fn trait_impl_shadowed_by_proven_ref_on_trait_name() {
        let mut findings = vec![
            proven_ref("src/widget.rs", "Greeter"),
            trait_impl("src/widget.rs", "Greeter", "Widget"),
        ];
        dedup_syn_under_proven(&mut findings);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].tier, Tier::Proven);
    }

    #[test]
    fn dyn_dispatch_shadowed_by_proven_ref_on_trait_name() {
        let mut findings = vec![
            proven_ref("src/bus.rs", "Handler"),
            dyn_dispatch("src/bus.rs", "Handler"),
        ];
        dedup_syn_under_proven(&mut findings);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].tier, Tier::Proven);
    }

    #[test]
    fn semver_finding_without_path_is_never_shadowed() {
        let semver = Finding::new(
            "",
            Tier::Likely,
            0.9,
            FindingKind::SemverCheck {
                level: "breaking".into(),
                details: "foo removed".into(),
            },
            "semver",
        );
        let mut findings = vec![proven_ref("src/lib.rs", "foo"), semver];
        dedup_syn_under_proven(&mut findings);
        assert_eq!(findings.len(), 2);
    }

    #[test]
    fn multiple_proven_refs_deduplicate_multiple_syn_findings() {
        let mut findings = vec![
            proven_ref("tests/a.rs", "login"),
            proven_ref("tests/b.rs", "logout"),
            test_ref("tests/a.rs", "t1", &["login"]),
            test_ref("tests/b.rs", "t2", &["logout"]),
            test_ref("tests/c.rs", "t3", &["other"]),
        ];
        dedup_syn_under_proven(&mut findings);
        assert_eq!(findings.len(), 3);
        // Surviving: 2 proven + the c.rs syn finding (no RA coverage).
        assert_eq!(
            findings.iter().filter(|f| f.tier == Tier::Proven).count(),
            2
        );
        assert_eq!(
            findings.iter().filter(|f| f.tier == Tier::Likely).count(),
            1
        );
    }

    // --- dedup_expanded_under_raw ---

    fn expanded_test_ref(test_symbol: &str, matched: &[&str]) -> Finding {
        Finding::new(
            "",
            Tier::Likely,
            0.75,
            FindingKind::TestReference {
                test: Location {
                    file: PathBuf::from(EXPANDED_SENTINEL),
                    symbol: test_symbol.into(),
                },
                matched_symbols: matched.iter().map(|s| (*s).to_string()).collect(),
            },
            "expanded test",
        )
    }

    #[test]
    fn expanded_test_shadowed_by_raw_with_same_name() {
        let mut findings = vec![
            test_ref("tests/auth.rs", "t_login", &["login"]),
            expanded_test_ref("t_login", &["login"]),
        ];
        dedup_expanded_under_raw(&mut findings);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].primary_path().unwrap(),
            Path::new("tests/auth.rs")
        );
    }

    #[test]
    fn expanded_test_with_no_raw_counterpart_survives() {
        // The `sqlx::query!` case: the raw source never fires a
        // TestReference for `t_query` because the changed symbol only
        // appears after expansion. No raw to dedup against → keep it.
        let mut findings = vec![expanded_test_ref("t_query", &["User"])];
        dedup_expanded_under_raw(&mut findings);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn expanded_trait_impls_are_never_shadowed_by_raw_test_refs() {
        // Derived trait impls from macros (e.g., `#[derive(Serialize)]`)
        // don't have a source counterpart — they should pass through.
        let expanded_impl = Finding::new(
            "",
            Tier::Likely,
            0.75,
            FindingKind::TraitImpl {
                trait_name: "Serialize".into(),
                impl_for: "User".into(),
                impl_site: Location {
                    file: PathBuf::from(EXPANDED_SENTINEL),
                    symbol: "impl Serialize for User".into(),
                },
            },
            "expanded impl",
        );
        let mut findings = vec![
            test_ref("tests/auth.rs", "t_login", &["login"]),
            expanded_impl,
        ];
        dedup_expanded_under_raw(&mut findings);
        assert_eq!(findings.len(), 2);
    }

    #[test]
    fn empty_findings_is_noop() {
        let mut findings: Vec<Finding> = Vec::new();
        dedup_expanded_under_raw(&mut findings);
        assert!(findings.is_empty());
    }

    #[test]
    fn expanded_with_different_test_name_survives() {
        let mut findings = vec![
            test_ref("tests/auth.rs", "t_login", &["login"]),
            expanded_test_ref("t_query", &["User"]),
        ];
        dedup_expanded_under_raw(&mut findings);
        assert_eq!(findings.len(), 2);
    }
}
