//! Derive-macro recognition.
//!
//! `#[derive(TraitName)]` is the dominant way Rust users apply traits — far
//! more common than hand-written `impl Trait for T`. Without recognizing
//! them we miss most real-world trait impls; with them we cover serde,
//! clap derive, thiserror, strum, typed-builder, and thousands of similar
//! patterns that would otherwise be invisible until rust-analyzer-backed
//! proper macro expansion arrives (v0.3+).
//!
//! Heuristic scope — what this module *does*
//! -----------------------------------------
//! * Plain `#[derive(Ident)]` and `#[derive(Path::To::Ident)]` are both
//!   matched on the *last path segment*, so `Serialize` matches whether
//!   the user writes `#[derive(Serialize)]` or
//!   `#[derive(serde::Serialize)]`.
//! * Multi-trait forms like `#[derive(Debug, Clone, Serialize)]` emit one
//!   finding per matching derive.
//! * Struct, enum, and union items are all inspected.
//! * Nested `mod { ... }` bodies are walked recursively.
//!
//! What this module *does not* do (documented limits)
//! --------------------------------------------------
//! * We do not expand the derive. The specific impl methods, where-clauses,
//!   or generated associated types aren't visible to us — that needs real
//!   `cargo expand` / HIR integration, deferred to a follow-up.
//! * We do not follow derive-alias macros: `#[derive(MyBundle)]` where
//!   `MyBundle` is itself a user-defined proc-macro that further derives
//!   other traits will only flag on `MyBundle` literally, not its members.
//! * `#[cfg_attr(feature = "x", derive(Y))]` **is** recognized now. The
//!   predicate is evaluated against the active feature set: under
//!   `FeatureSet::Permissive` every cfg_attr is treated as active (the
//!   project-wide over-report stance); under `FeatureSet::Exact` we
//!   evaluate the predicate precisely. Derives nested inside cfg_attr
//!   with unsatisfied predicates are dropped.

use crate::finding::{Finding, FindingKind, Location, Tier};
use crate::tests_scan::workspace_rust_files;
use anyhow::Result;
use std::collections::BTreeSet;
use std::path::Path;

pub fn find_derive_impls(root: &Path, changed_traits: &BTreeSet<String>) -> Result<Vec<Finding>> {
    if changed_traits.is_empty() {
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

        let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        let mut hits: Vec<(String, String)> = Vec::new();
        walk_items(&ast.items, changed_traits, &mut hits);

        for (trait_name, impl_for) in hits {
            let evidence = format!(
                "`#[derive({trait_name})]` on `{impl_for}` in {} — derive \
                 expands to `impl {trait_name} for {impl_for}` at compile time \
                 (heuristic match on last path segment; we cannot see the \
                 generated method bodies without macro expansion)",
                rel.display()
            );
            let kind = FindingKind::DerivedTraitImpl {
                trait_name: trait_name.clone(),
                impl_for: impl_for.clone(),
                derive_site: Location {
                    file: rel.clone(),
                    symbol: format!("#[derive({trait_name})] on {impl_for}"),
                },
            };
            findings.push(Finding::new("", Tier::Likely, 0.80, kind, evidence));
        }
    }

    Ok(findings)
}

fn walk_items(
    items: &[syn::Item],
    changed_traits: &BTreeSet<String>,
    hits: &mut Vec<(String, String)>,
) {
    for item in items {
        match item {
            syn::Item::Struct(s) => {
                collect_derives(&s.attrs, &s.ident.to_string(), changed_traits, hits);
            }
            syn::Item::Enum(e) => {
                collect_derives(&e.attrs, &e.ident.to_string(), changed_traits, hits);
            }
            syn::Item::Union(u) => {
                collect_derives(&u.attrs, &u.ident.to_string(), changed_traits, hits);
            }
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    walk_items(inner, changed_traits, hits);
                }
            }
            _ => {}
        }
    }
}

fn collect_derives(
    attrs: &[syn::Attribute],
    type_name: &str,
    changed_traits: &BTreeSet<String>,
    hits: &mut Vec<(String, String)>,
) {
    let features = crate::cfg::current_features();
    for attr in attrs {
        if attr.path().is_ident("derive") {
            collect_from_derive_attr(attr, type_name, changed_traits, hits);
        } else if attr.path().is_ident("cfg_attr") {
            collect_from_cfg_attr(attr, type_name, changed_traits, &features, hits);
        }
    }
}

fn collect_from_derive_attr(
    attr: &syn::Attribute,
    type_name: &str,
    changed_traits: &BTreeSet<String>,
    hits: &mut Vec<(String, String)>,
) {
    // `#[derive(A, B, C)]` → parse the paren contents as a comma-separated
    // list of paths. Ignore attributes that don't parse — better to miss
    // a rare form than to crash on unfamiliar syntax.
    let Ok(paths) = attr.parse_args_with(|input: syn::parse::ParseStream<'_>| {
        let punct: syn::punctuated::Punctuated<syn::Path, syn::Token![,]> =
            syn::punctuated::Punctuated::parse_terminated(input)?;
        Ok(punct.into_iter().collect::<Vec<_>>())
    }) else {
        return;
    };
    for path in paths {
        if let Some(last) = path.segments.last() {
            let name = last.ident.to_string();
            if changed_traits.contains(&name) {
                hits.push((name, type_name.to_string()));
            }
        }
    }
}

/// Handle `#[cfg_attr(predicate, attr1, attr2, ...)]`. If the predicate
/// is true under the active feature set, scan each nested attribute
/// for `derive(...)` calls and harvest their arguments the same way
/// `collect_from_derive_attr` does.
fn collect_from_cfg_attr(
    attr: &syn::Attribute,
    type_name: &str,
    changed_traits: &BTreeSet<String>,
    features: &crate::cfg::FeatureSet,
    hits: &mut Vec<(String, String)>,
) {
    let Ok(metas) = attr.parse_args_with(|input: syn::parse::ParseStream<'_>| {
        let punct: syn::punctuated::Punctuated<syn::Meta, syn::Token![,]> =
            syn::punctuated::Punctuated::parse_terminated(input)?;
        Ok(punct.into_iter().collect::<Vec<_>>())
    }) else {
        return;
    };

    // First element is the predicate; remainder are the nested attributes.
    let Some((predicate, nested)) = metas.split_first() else {
        return;
    };
    if !crate::cfg::eval_cfg_meta(predicate, features) {
        return;
    }

    for meta in nested {
        match meta {
            // `derive(A, B)` nested inside cfg_attr — the common case.
            syn::Meta::List(list) if list.path.is_ident("derive") => {
                let Ok(paths) = list.parse_args_with(|input: syn::parse::ParseStream<'_>| {
                    let punct: syn::punctuated::Punctuated<syn::Path, syn::Token![,]> =
                        syn::punctuated::Punctuated::parse_terminated(input)?;
                    Ok(punct.into_iter().collect::<Vec<_>>())
                }) else {
                    continue;
                };
                for path in paths {
                    if let Some(last) = path.segments.last() {
                        let name = last.ident.to_string();
                        if changed_traits.contains(&name) {
                            hits.push((name, type_name.to_string()));
                        }
                    }
                }
            }
            _ => {
                // Other nested attributes (`doc = "..."`, nested
                // `cfg_attr(...)`, etc.) don't produce derive impls at
                // this level. Stacked `cfg_attr(a, cfg_attr(b, derive))`
                // is rare enough we accept the miss rather than build a
                // recursive Meta→Attribute synthesizer.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for (rel, body) in files {
            let p = dir.path().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        dir
    }

    fn traits(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    fn payloads(findings: &[Finding]) -> Vec<(String, String)> {
        findings
            .iter()
            .filter_map(|f| match &f.kind {
                FindingKind::DerivedTraitImpl {
                    trait_name,
                    impl_for,
                    ..
                } => Some((trait_name.clone(), impl_for.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn detects_bare_derive() {
        let dir = setup(&[("src/lib.rs", "#[derive(Greeter)]\nstruct Foo;\n")]);
        let hits = find_derive_impls(dir.path(), &traits(&["Greeter"])).unwrap();
        assert_eq!(
            payloads(&hits),
            vec![("Greeter".to_string(), "Foo".to_string())]
        );
    }

    #[test]
    fn matches_qualified_derive_path_via_last_segment() {
        let dir = setup(&[("src/lib.rs", "#[derive(serde::Serialize)]\nstruct Foo;\n")]);
        let hits = find_derive_impls(dir.path(), &traits(&["Serialize"])).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn multi_trait_derive_emits_one_finding_per_match() {
        let dir = setup(&[(
            "src/lib.rs",
            "#[derive(Debug, Greeter, Clone)]\nstruct Foo;\n",
        )]);
        let hits = find_derive_impls(dir.path(), &traits(&["Greeter"])).unwrap();
        assert_eq!(hits.len(), 1);
        // Unrelated derives in the same attribute don't add noise.
        let hits2 = find_derive_impls(dir.path(), &traits(&["Debug", "Greeter"])).unwrap();
        assert_eq!(hits2.len(), 2);
    }

    #[test]
    fn works_for_enum_and_union() {
        let dir = setup(&[(
            "src/lib.rs",
            "#[derive(Greeter)]\nenum E { A, B }\n\
             #[derive(Greeter)]\nunion U { a: i32, b: u32 }\n",
        )]);
        let hits = find_derive_impls(dir.path(), &traits(&["Greeter"])).unwrap();
        let pairs = payloads(&hits);
        assert!(pairs.contains(&("Greeter".into(), "E".into())));
        assert!(pairs.contains(&("Greeter".into(), "U".into())));
    }

    #[test]
    fn recurses_into_inline_modules() {
        let dir = setup(&[(
            "src/lib.rs",
            "mod nested { #[derive(Greeter)] pub struct Inner; }\n",
        )]);
        let hits = find_derive_impls(dir.path(), &traits(&["Greeter"])).unwrap();
        assert_eq!(
            payloads(&hits),
            vec![("Greeter".to_string(), "Inner".to_string())]
        );
    }

    #[test]
    fn ignores_unrelated_derives() {
        let dir = setup(&[("src/lib.rs", "#[derive(Debug, Clone)]\nstruct Foo;\n")]);
        let hits = find_derive_impls(dir.path(), &traits(&["Greeter"])).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn finding_carries_expected_tier_and_severity() {
        let dir = setup(&[("src/lib.rs", "#[derive(Greeter)]\nstruct Foo;\n")]);
        let hits = find_derive_impls(dir.path(), &traits(&["Greeter"])).unwrap();
        assert_eq!(hits[0].tier, Tier::Likely);
        assert_eq!(hits[0].confidence, 0.80);
        assert_eq!(hits[0].severity, crate::finding::SeverityClass::High);
    }

    #[test]
    fn empty_changed_set_returns_empty() {
        let dir = setup(&[("src/lib.rs", "#[derive(Greeter)]\nstruct Foo;\n")]);
        let hits = find_derive_impls(dir.path(), &BTreeSet::new()).unwrap();
        assert!(hits.is_empty());
    }

    // --- cfg_attr-wrapped derives ---

    #[test]
    fn cfg_attr_derive_detected_under_permissive() {
        // Default feature set is Permissive — all cfg predicates are
        // treated as active, so cfg_attr-wrapped derives fire.
        let dir = setup(&[(
            "src/lib.rs",
            r#"#[cfg_attr(feature = "serde", derive(Greeter))]
               struct Foo;"#,
        )]);
        let hits = find_derive_impls(dir.path(), &traits(&["Greeter"])).unwrap();
        assert_eq!(
            payloads(&hits),
            vec![("Greeter".to_string(), "Foo".to_string())]
        );
    }

    #[test]
    fn cfg_attr_multi_derive_emits_one_finding_per_match() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"#[cfg_attr(feature = "serde", derive(Debug, Greeter, Clone))]
               struct Foo;"#,
        )]);
        let hits = find_derive_impls(dir.path(), &traits(&["Greeter"])).unwrap();
        assert_eq!(hits.len(), 1);
        let hits_multi =
            find_derive_impls(dir.path(), &traits(&["Debug", "Greeter", "Clone"])).unwrap();
        assert_eq!(hits_multi.len(), 3);
    }

    #[test]
    fn cfg_attr_dropped_when_predicate_false_under_exact_featureset() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"#[cfg_attr(feature = "serde", derive(Greeter))]
               struct Foo;"#,
        )]);
        // Install a FeatureSet that doesn't include "serde" so the
        // predicate evaluates false; the cfg_attr derive should drop.
        let hits =
            crate::cfg::with_features(crate::cfg::FeatureSet::Exact(BTreeSet::new()), || {
                find_derive_impls(dir.path(), &traits(&["Greeter"])).unwrap()
            });
        assert!(hits.is_empty(), "unexpected hits: {hits:?}");
    }

    #[test]
    fn cfg_attr_kept_when_predicate_true_under_exact_featureset() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"#[cfg_attr(feature = "serde", derive(Greeter))]
               struct Foo;"#,
        )]);
        let active: BTreeSet<String> = std::iter::once("serde".to_string()).collect();
        let hits = crate::cfg::with_features(crate::cfg::FeatureSet::Exact(active), || {
            find_derive_impls(dir.path(), &traits(&["Greeter"])).unwrap()
        });
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn cfg_attr_with_not_combinator_evaluates_correctly() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"#[cfg_attr(not(feature = "legacy"), derive(Greeter))]
               struct Foo;"#,
        )]);
        // Empty feature set → `not(feature = "legacy")` is true → derive fires.
        let hits =
            crate::cfg::with_features(crate::cfg::FeatureSet::Exact(BTreeSet::new()), || {
                find_derive_impls(dir.path(), &traits(&["Greeter"])).unwrap()
            });
        assert_eq!(hits.len(), 1);
        // With "legacy" active → predicate false → no finding.
        let active: BTreeSet<String> = std::iter::once("legacy".to_string()).collect();
        let hits = crate::cfg::with_features(crate::cfg::FeatureSet::Exact(active), || {
            find_derive_impls(dir.path(), &traits(&["Greeter"])).unwrap()
        });
        assert!(hits.is_empty());
    }

    #[test]
    fn cfg_attr_composes_with_plain_derives_on_same_item() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"#[derive(Debug)]
               #[cfg_attr(feature = "serde", derive(Greeter))]
               struct Foo;"#,
        )]);
        let hits = find_derive_impls(dir.path(), &traits(&["Debug", "Greeter"])).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn malformed_cfg_attr_is_ignored_without_panicking() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"#[cfg_attr()]
               struct Bar;
               #[cfg_attr(feature = "serde", derive(Greeter))]
               struct Baz;"#,
        )]);
        let hits = find_derive_impls(dir.path(), &traits(&["Greeter"])).unwrap();
        assert_eq!(
            payloads(&hits),
            vec![("Greeter".to_string(), "Baz".to_string())]
        );
    }

    #[test]
    fn handles_malformed_derive_gracefully() {
        // syn should still parse the file; the derive arg list may be weird
        // and we should just skip it without panicking.
        let dir = setup(&[(
            "src/lib.rs",
            "#[derive()]\nstruct Foo;\n\
             #[derive(Greeter)]\nstruct Bar;\n",
        )]);
        let hits = find_derive_impls(dir.path(), &traits(&["Greeter"])).unwrap();
        assert_eq!(
            payloads(&hits),
            vec![("Greeter".to_string(), "Bar".to_string())]
        );
    }
}
