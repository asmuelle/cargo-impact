//! Per-method trait ripple classification.
//!
//! Complements the blanket [`crate::traits`] scan (which flags every `impl
//! Trait for T` when the trait's containing file changed) by explaining
//! *what* about the trait changed — required vs default method, added vs
//! removed, signature vs body, supertraits, generic bounds. See README §3B
//! for the classification rules; each case maps to a [`TraitChange`] variant
//! with its own tier/severity/confidence, computed on the variant itself.
//!
//! The detector compares the trait definition at the `since` revision
//! (default `HEAD`) against the working-tree version, trait-by-trait. A
//! trait that was added or removed as a whole is intentionally *not*
//! reported here — added/removed traits are covered by the top-level
//! diff's `Added` / `Removed` items in [`crate::diff`], and duplicating
//! that signal would only add noise.

use crate::finding::{Finding, FindingKind, TraitChange};
use anyhow::{Context, Result};
use quote::ToTokens;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use syn::{ItemTrait, TraitItem};

/// A classified change to a single trait in a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitChangeRecord {
    pub trait_name: String,
    pub file: PathBuf,
    pub method: Option<String>,
    pub change: TraitChange,
}

impl TraitChangeRecord {
    pub fn into_finding(self) -> Finding {
        let tier = self.change.tier();
        let confidence = self.change.confidence();
        let severity = self.change.severity();
        let evidence = match &self.method {
            Some(m) => format!(
                "trait `{}`: {} — method `{m}` ({})",
                self.trait_name,
                self.change.phrase(),
                self.file.display()
            ),
            None => format!(
                "trait `{}`: {} ({})",
                self.trait_name,
                self.change.phrase(),
                self.file.display()
            ),
        };
        let kind = FindingKind::TraitDefinitionChange {
            trait_name: self.trait_name,
            file: self.file,
            method: self.method,
            change: self.change,
        };
        Finding::new("", tier, confidence, kind, evidence).with_severity(severity)
    }
}

/// Classify trait-definition changes in `rel_file` between `since` and WT.
/// Returns an empty vec on any failure (missing git, unparseable source,
/// file absent from `since`) — this analyzer is best-effort.
pub fn classify_changes_in_file(
    root: &Path,
    rel_file: &Path,
    since: &str,
) -> Result<Vec<TraitChangeRecord>> {
    let Ok(wt_src) = std::fs::read_to_string(root.join(rel_file)) else {
        return Ok(Vec::new());
    };
    let head_src = match git_show(root, since, rel_file)? {
        Some(s) => s,
        None => return Ok(Vec::new()),
    };

    let Some(wt_ast) = crate::cfg::parse_and_filter(&wt_src) else {
        return Ok(Vec::new());
    };
    let Some(head_ast) = crate::cfg::parse_and_filter(&head_src) else {
        return Ok(Vec::new());
    };

    let head_traits = extract_traits(&head_ast);
    let wt_traits = extract_traits(&wt_ast);

    let mut out = Vec::new();
    for (name, wt_trait) in &wt_traits {
        if let Some(head_trait) = head_traits.get(name) {
            diff_trait(name, head_trait, wt_trait, rel_file, &mut out);
        }
        // Traits added in WT but absent from HEAD are covered by diff::diff_file
        // (they appear as Added SymbolKind::Trait). Don't double-count.
    }
    Ok(out)
}

fn diff_trait(
    name: &str,
    head: &ItemTrait,
    wt: &ItemTrait,
    file: &Path,
    out: &mut Vec<TraitChangeRecord>,
) {
    let head_methods = methods_map(head);
    let wt_methods = methods_map(wt);

    for (m_name, wt_entry) in &wt_methods {
        match head_methods.get(m_name) {
            None => {
                let change = if wt_entry.has_body {
                    TraitChange::DefaultMethodAdded
                } else {
                    TraitChange::RequiredMethodAdded
                };
                out.push(TraitChangeRecord {
                    trait_name: name.to_string(),
                    file: file.to_path_buf(),
                    method: Some(m_name.clone()),
                    change,
                });
            }
            Some(head_entry) => {
                if wt_entry.sig != head_entry.sig {
                    out.push(TraitChangeRecord {
                        trait_name: name.to_string(),
                        file: file.to_path_buf(),
                        method: Some(m_name.clone()),
                        change: TraitChange::RequiredMethodSignatureChanged,
                    });
                } else if wt_entry.body != head_entry.body
                    && (wt_entry.has_body || head_entry.has_body)
                {
                    out.push(TraitChangeRecord {
                        trait_name: name.to_string(),
                        file: file.to_path_buf(),
                        method: Some(m_name.clone()),
                        change: TraitChange::DefaultMethodBodyChanged,
                    });
                }
            }
        }
    }

    for m_name in head_methods.keys() {
        if !wt_methods.contains_key(m_name) {
            out.push(TraitChangeRecord {
                trait_name: name.to_string(),
                file: file.to_path_buf(),
                method: Some(m_name.clone()),
                change: TraitChange::MethodRemoved,
            });
        }
    }

    let head_frame = trait_frame(head);
    let wt_frame = trait_frame(wt);
    if head_frame != wt_frame {
        out.push(TraitChangeRecord {
            trait_name: name.to_string(),
            file: file.to_path_buf(),
            method: None,
            change: TraitChange::SupertraitOrBoundChanged,
        });
    }
}

/// Non-method "envelope" of a trait — generics, supertrait list, where
/// clause, unsafe / auto keyword. Method-level diffs are done separately;
/// this catches changes to the trait's outer signature that affect all
/// impls or downstream generic consumers.
fn trait_frame(t: &ItemTrait) -> String {
    let parts = [
        t.unsafety.to_token_stream().to_string(),
        t.auto_token.to_token_stream().to_string(),
        t.generics.to_token_stream().to_string(),
        t.supertraits.to_token_stream().to_string(),
    ];
    parts.join("|")
}

fn extract_traits(ast: &syn::File) -> BTreeMap<String, &ItemTrait> {
    let mut out = BTreeMap::new();
    walk_items(&ast.items, &mut out);
    out
}

fn walk_items<'a>(items: &'a [syn::Item], out: &mut BTreeMap<String, &'a ItemTrait>) {
    for item in items {
        match item {
            syn::Item::Trait(t) => {
                out.insert(t.ident.to_string(), t);
            }
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    walk_items(inner, out);
                }
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone)]
struct MethodEntry {
    sig: String,
    body: String,
    has_body: bool,
}

fn methods_map(t: &ItemTrait) -> BTreeMap<String, MethodEntry> {
    let mut out = BTreeMap::new();
    for item in &t.items {
        if let TraitItem::Fn(f) = item {
            let name = f.sig.ident.to_string();
            let sig = f.sig.to_token_stream().to_string();
            let (body, has_body) = match &f.default {
                Some(b) => (b.to_token_stream().to_string(), true),
                None => (String::new(), false),
            };
            out.insert(
                name,
                MethodEntry {
                    sig,
                    body,
                    has_body,
                },
            );
        }
    }
    out
}

fn git_show(root: &Path, rev: &str, rel: &Path) -> Result<Option<String>> {
    let spec = format!("{rev}:{}", rel.to_string_lossy().replace('\\', "/"));
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("show")
        .arg(&spec)
        .output()
        .with_context(|| format!("git show {spec}"))?;
    if output.status.success() {
        Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::SeverityClass;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn git_fixture(initial: &str, modified: &str) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
            &["config", "commit.gpgsign", "false"],
            // Windows git defaults core.autocrlf = true, which rewrites
            // line endings in the index and breaks our diff assertions.
            &["config", "core.autocrlf", "false"],
        ] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(root)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let rel = std::path::PathBuf::from("src.rs");
        fs::write(root.join(&rel), initial).unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["add", "src.rs"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["commit", "-q", "-m", "init"])
                .status()
                .unwrap()
                .success()
        );
        fs::write(root.join(&rel), modified).unwrap();
        (dir, rel)
    }

    fn first_change(records: &[TraitChangeRecord]) -> TraitChange {
        assert!(!records.is_empty(), "expected at least one change");
        records[0].change
    }

    #[test]
    fn detects_required_method_added() {
        let (dir, rel) = git_fixture(
            "pub trait Greeter { fn hi(&self); }\n",
            "pub trait Greeter { fn hi(&self); fn hello(&self); }\n",
        );
        let recs = classify_changes_in_file(dir.path(), &rel, "HEAD").unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].change, TraitChange::RequiredMethodAdded);
        assert_eq!(recs[0].method.as_deref(), Some("hello"));
        assert_eq!(recs[0].change.severity(), SeverityClass::High);
    }

    #[test]
    fn detects_default_method_added() {
        let (dir, rel) = git_fixture(
            "pub trait Greeter { fn hi(&self); }\n",
            "pub trait Greeter { fn hi(&self); fn hello(&self) {} }\n",
        );
        let recs = classify_changes_in_file(dir.path(), &rel, "HEAD").unwrap();
        assert_eq!(first_change(&recs), TraitChange::DefaultMethodAdded);
        assert_eq!(recs[0].change.severity(), SeverityClass::Low);
    }

    #[test]
    fn detects_method_removed() {
        let (dir, rel) = git_fixture(
            "pub trait Greeter { fn hi(&self); fn bye(&self); }\n",
            "pub trait Greeter { fn hi(&self); }\n",
        );
        let recs = classify_changes_in_file(dir.path(), &rel, "HEAD").unwrap();
        assert_eq!(first_change(&recs), TraitChange::MethodRemoved);
        assert_eq!(recs[0].method.as_deref(), Some("bye"));
    }

    #[test]
    fn detects_required_method_signature_change() {
        let (dir, rel) = git_fixture(
            "pub trait Greeter { fn hi(&self) -> i32; }\n",
            "pub trait Greeter { fn hi(&self) -> String; }\n",
        );
        let recs = classify_changes_in_file(dir.path(), &rel, "HEAD").unwrap();
        assert_eq!(
            first_change(&recs),
            TraitChange::RequiredMethodSignatureChanged
        );
        assert_eq!(recs[0].change.confidence(), 0.95);
    }

    #[test]
    fn detects_default_body_change() {
        let (dir, rel) = git_fixture(
            "pub trait Greeter { fn hi(&self) -> u32 { 1 } }\n",
            "pub trait Greeter { fn hi(&self) -> u32 { 2 } }\n",
        );
        let recs = classify_changes_in_file(dir.path(), &rel, "HEAD").unwrap();
        assert_eq!(first_change(&recs), TraitChange::DefaultMethodBodyChanged);
        assert_eq!(recs[0].change.severity(), SeverityClass::Low);
    }

    #[test]
    fn detects_supertrait_change() {
        let (dir, rel) = git_fixture(
            "pub trait Greeter { fn hi(&self); }\n",
            "pub trait Greeter: Send { fn hi(&self); }\n",
        );
        let recs = classify_changes_in_file(dir.path(), &rel, "HEAD").unwrap();
        assert!(
            recs.iter()
                .any(|r| r.change == TraitChange::SupertraitOrBoundChanged)
        );
    }

    #[test]
    fn no_changes_when_trait_body_identical() {
        let body = "pub trait Greeter { fn hi(&self); }\n";
        let (dir, rel) = git_fixture(body, body);
        let recs = classify_changes_in_file(dir.path(), &rel, "HEAD").unwrap();
        assert!(recs.is_empty());
    }

    #[test]
    fn ignores_added_traits_delegating_to_top_level_diff() {
        // Trait didn't exist in HEAD at all — diff::diff_file handles this
        // as "trait added", no per-method classification fires here.
        let (dir, rel) = git_fixture(
            "pub fn seed() {}\n",
            "pub fn seed() {}\npub trait New { fn m(&self); }\n",
        );
        let recs = classify_changes_in_file(dir.path(), &rel, "HEAD").unwrap();
        assert!(recs.is_empty());
    }

    #[test]
    fn emits_multiple_changes_in_single_trait() {
        let (dir, rel) = git_fixture(
            "pub trait T { fn a(&self); fn b(&self) {} }\n",
            "pub trait T { fn a(&self, x: i32); fn c(&self); }\n",
        );
        let recs = classify_changes_in_file(dir.path(), &rel, "HEAD").unwrap();
        let changes: Vec<_> = recs.iter().map(|r| r.change).collect();
        assert!(changes.contains(&TraitChange::RequiredMethodSignatureChanged));
        assert!(changes.contains(&TraitChange::RequiredMethodAdded));
        assert!(changes.contains(&TraitChange::MethodRemoved));
    }

    #[test]
    fn record_renders_into_finding_with_correct_fields() {
        let rec = TraitChangeRecord {
            trait_name: "Greeter".into(),
            file: std::path::PathBuf::from("src/lib.rs"),
            method: Some("hello".into()),
            change: TraitChange::RequiredMethodAdded,
        };
        let f = rec.into_finding();
        assert_eq!(f.severity, SeverityClass::High);
        assert_eq!(f.tier, crate::finding::Tier::Likely);
        assert_eq!(f.confidence, 0.95);
        assert!(f.evidence.contains("Greeter"));
        assert!(f.evidence.contains("hello"));
        match f.kind {
            FindingKind::TraitDefinitionChange { change, method, .. } => {
                assert_eq!(change, TraitChange::RequiredMethodAdded);
                assert_eq!(method.as_deref(), Some("hello"));
            }
            other => panic!("expected TraitDefinitionChange, got {other:?}"),
        }
    }
}
