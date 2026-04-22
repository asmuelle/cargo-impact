//! `dyn Trait` dispatch detection.
//!
//! For each trait name whose definition lives in a changed file, walk the
//! workspace for `dyn Trait` references — `&dyn T`, `&mut dyn T`,
//! `Box<dyn T>`, `Arc<dyn T>`, etc. Each site becomes a `Likely 0.75` finding:
//! we can statically see the trait name, but not which concrete impl will be
//! selected at runtime without name resolution, so the confidence is honestly
//! below the trait-impl case.

use crate::finding::{Finding, FindingKind, Location, Tier};
use crate::tests_scan::workspace_rust_files;
use anyhow::Result;
use std::collections::BTreeSet;
use std::path::Path;
use syn::visit::Visit;
use syn::{TypeParamBound, TypeTraitObject};

pub fn find_dyn_dispatch_sites(
    root: &Path,
    changed_traits: &BTreeSet<String>,
) -> Result<Vec<Finding>> {
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
        let mut visitor = DynVisitor {
            changed_traits,
            hits: BTreeSet::new(),
        };
        visitor.visit_file(&ast);

        for trait_name in visitor.hits {
            let evidence = format!(
                "`dyn {trait_name}` used in this file — concrete impl resolves at runtime, \
                 static analysis cannot predict which"
            );
            let kind = FindingKind::DynDispatch {
                trait_name: trait_name.clone(),
                site: Location {
                    file: rel.clone(),
                    symbol: format!("dyn {trait_name}"),
                },
            };
            findings.push(Finding::new("", Tier::Likely, 0.75, kind, evidence));
        }
    }

    Ok(findings)
}

struct DynVisitor<'a> {
    changed_traits: &'a BTreeSet<String>,
    /// Set of trait names we've already recorded for *this file*. Multiple
    /// `dyn Trait` uses in one file collapse to a single finding — it's the
    /// same signal, repeating it is noise.
    hits: BTreeSet<String>,
}

impl<'ast> Visit<'ast> for DynVisitor<'_> {
    fn visit_type_trait_object(&mut self, node: &'ast TypeTraitObject) {
        for bound in &node.bounds {
            if let TypeParamBound::Trait(tb) = bound {
                if let Some(seg) = tb.path.segments.last() {
                    let name = seg.ident.to_string();
                    if self.changed_traits.contains(&name) {
                        self.hits.insert(name);
                    }
                }
            }
        }
        syn::visit::visit_type_trait_object(self, node);
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

    #[test]
    fn detects_box_dyn_trait() {
        let dir = setup(&[(
            "src/lib.rs",
            "trait Handler {}\n\
             fn register(h: Box<dyn Handler>) { let _ = h; }\n",
        )]);
        let hits = find_dyn_dispatch_sites(dir.path(), &traits(&["Handler"])).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].tier, Tier::Likely);
        assert_eq!(hits[0].confidence, 0.75);
    }

    #[test]
    fn detects_reference_dyn_trait() {
        let dir = setup(&[(
            "src/lib.rs",
            "trait Handler {}\n\
             fn dispatch(h: &dyn Handler) { let _ = h; }\n\
             fn dispatch_mut(h: &mut dyn Handler) { let _ = h; }\n",
        )]);
        let hits = find_dyn_dispatch_sites(dir.path(), &traits(&["Handler"])).unwrap();
        // Multiple uses in one file collapse to a single finding.
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn ignores_dyn_of_unrelated_traits() {
        let dir = setup(&[(
            "src/lib.rs",
            "fn run(f: Box<dyn std::error::Error>) { let _ = f; }\n",
        )]);
        let hits = find_dyn_dispatch_sites(dir.path(), &traits(&["Handler"])).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn matches_qualified_trait_path_via_last_segment() {
        let dir = setup(&[(
            "src/lib.rs",
            "fn run(f: &dyn crate::Handler) { let _ = f; }\n",
        )]);
        let hits = find_dyn_dispatch_sites(dir.path(), &traits(&["Handler"])).unwrap();
        assert_eq!(hits.len(), 1);
    }
}
