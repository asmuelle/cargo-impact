//! Trait ripple detection.
//!
//! Given a set of trait names whose definitions live in changed files, walk
//! the workspace for `impl Trait for T` blocks and emit one finding per impl
//! site. Per README §3B the ideal implementation distinguishes
//! required-vs-default-vs-new-method changes — that requires a before/after
//! AST diff which v0.2 does not yet perform. Until then, any change inside a
//! file that defines a trait is treated as a blanket impact on every impl,
//! tiered `Likely 0.80` with the limitation spelled out in the evidence field.

use crate::finding::{Finding, FindingKind, Location, Tier};
use crate::tests_scan::workspace_rust_files;
use anyhow::Result;
use std::collections::BTreeSet;
use std::path::Path;
use syn::visit::Visit;
use syn::{ItemImpl, Path as SynPath, Type, TypePath};

/// Find every `impl Trait for T` block in `root` where `Trait` is in
/// `changed_traits`. Returns findings with blank IDs — orchestrator fills them.
pub fn find_trait_impls(root: &Path, changed_traits: &BTreeSet<String>) -> Result<Vec<Finding>> {
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
        let mut visitor = ImplVisitor {
            changed_traits,
            hits: Vec::new(),
        };
        visitor.visit_file(&ast);

        for (trait_name, impl_for) in visitor.hits {
            let evidence = format!(
                "`impl {trait_name} for {impl_for}` — trait definition lives in a changed file \
                 (syn-only analysis cannot distinguish required vs default method changes; \
                 flagged blanket as Likely)"
            );
            let kind = FindingKind::TraitImpl {
                trait_name: trait_name.clone(),
                impl_for: impl_for.clone(),
                impl_site: Location {
                    file: rel.clone(),
                    symbol: format!("impl {trait_name} for {impl_for}"),
                },
            };
            findings.push(Finding::new("", Tier::Likely, 0.80, kind, evidence));
        }
    }

    Ok(findings)
}

struct ImplVisitor<'a> {
    changed_traits: &'a BTreeSet<String>,
    /// (trait_name, impl_for) pairs.
    hits: Vec<(String, String)>,
}

impl<'ast> Visit<'ast> for ImplVisitor<'_> {
    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        if let Some((_, trait_path, _)) = &node.trait_
            && let Some(trait_name) = last_ident(trait_path)
            && self.changed_traits.contains(&trait_name)
        {
            let impl_for = type_to_string(&node.self_ty);
            self.hits.push((trait_name, impl_for));
        }
        // Still descend in case there are nested modules with more impls.
        syn::visit::visit_item_impl(self, node);
    }
}

fn last_ident(path: &SynPath) -> Option<String> {
    path.segments.last().map(|s| s.ident.to_string())
}

/// Render `node.self_ty` as a stable short form suitable for the finding
/// payload. For the common case `Type::Path(...)` this is the last path
/// segment; for everything else we fall back to `to_token_stream`.
fn type_to_string(ty: &Type) -> String {
    if let Type::Path(TypePath { qself: None, path }) = ty
        && let Some(seg) = path.segments.last()
    {
        return seg.ident.to_string();
    }
    use quote::ToTokens;
    let raw = ty.to_token_stream().to_string();
    // Collapse syn's whitespace-between-tokens rendering into something
    // terser for human output. We keep the original for non-path types only.
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Extension to [`FindingKind::TraitImpl`] lookup: the parallel trait-ripple
/// analyzer also needs to know whether a trait's definition was changed. This
/// is the simplest filter — trait names among the changed symbols, no more.
pub fn changed_trait_names(changed_symbols: &[crate::symbols::TopLevelSymbol]) -> BTreeSet<String> {
    changed_symbols
        .iter()
        .filter(|s| s.kind == crate::symbols::SymbolKind::Trait)
        .map(|s| s.name.clone())
        .collect()
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

    #[test]
    fn finds_impl_of_changed_trait() {
        let dir = setup(&[(
            "src/lib.rs",
            "struct Foo;\n\
             pub trait Greeter { fn hi(&self); }\n\
             impl Greeter for Foo { fn hi(&self) {} }\n",
        )]);
        let hits = find_trait_impls(dir.path(), &traits(&["Greeter"])).unwrap();
        assert_eq!(hits.len(), 1);
        match &hits[0].kind {
            FindingKind::TraitImpl {
                trait_name,
                impl_for,
                ..
            } => {
                assert_eq!(trait_name, "Greeter");
                assert_eq!(impl_for, "Foo");
            }
            other => panic!("expected TraitImpl, got {other:?}"),
        }
        assert_eq!(hits[0].tier, Tier::Likely);
    }

    #[test]
    fn ignores_inherent_impls() {
        let dir = setup(&[(
            "src/lib.rs",
            "struct Foo;\n\
             impl Foo { fn hi(&self) {} }\n",
        )]);
        // `impl Foo` has no trait_ field; shouldn't match anything.
        let hits = find_trait_impls(dir.path(), &traits(&["Foo"])).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn ignores_impls_of_unrelated_traits() {
        let dir = setup(&[(
            "src/lib.rs",
            "struct Foo;\n\
             impl std::fmt::Debug for Foo {\n    \
                 fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { Ok(()) }\n\
             }\n",
        )]);
        let hits = find_trait_impls(dir.path(), &traits(&["Greeter"])).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn matches_trait_via_last_path_segment() {
        // Users often impl crate::greetings::Greeter — we match on the last segment.
        let dir = setup(&[(
            "src/lib.rs",
            "struct Foo;\n\
             impl crate::greetings::Greeter for Foo {}\n",
        )]);
        let hits = find_trait_impls(dir.path(), &traits(&["Greeter"])).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn changed_trait_names_filters_to_traits_only() {
        use crate::symbols::{SymbolKind, TopLevelSymbol};
        let symbols = vec![
            TopLevelSymbol {
                name: "Foo".into(),
                kind: SymbolKind::Struct,
            },
            TopLevelSymbol {
                name: "Greeter".into(),
                kind: SymbolKind::Trait,
            },
            TopLevelSymbol {
                name: "helper".into(),
                kind: SymbolKind::Fn,
            },
        ];
        let names = changed_trait_names(&symbols);
        assert_eq!(names.len(), 1);
        assert!(names.contains("Greeter"));
    }
}
