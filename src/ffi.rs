//! FFI signature change detection.
//!
//! `extern "C"` functions and `#[no_mangle]` exports are the Rust side of a
//! contract with foreign code (C consumers, dynamic libraries, JNI, wasm
//! embedders). When a signature changes, the blast radius leaves Rust
//! entirely — our static analysis cannot follow into the foreign language —
//! so every change is surfaced at `High` severity. Confidence is intentionally
//! `Likely 0.95` rather than `Proven`: the *textual* signature change is
//! exact, but whether downstream FFI callers actually exist is outside our
//! analysis scope.

use crate::finding::{Finding, FindingKind, Tier};
use anyhow::Result;
use quote::ToTokens;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use syn::{ForeignItem, Item};

pub fn find_ffi_changes(root: &Path, rel_file: &Path, since: &str) -> Result<Vec<Finding>> {
    let wt = match std::fs::read_to_string(root.join(rel_file)) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };
    // New file — no HEAD version means every FFI symbol is "added".
    let head: String = git_show(root, since, rel_file)?.unwrap_or_default();

    let Some(wt_ast) = crate::cfg::parse_and_filter(&wt) else {
        return Ok(Vec::new());
    };
    let head_ast = crate::cfg::parse_and_filter(&head);

    let wt_sigs = ffi_signatures(&wt_ast);
    let head_sigs = head_ast.as_ref().map(ffi_signatures).unwrap_or_default();

    let mut findings = Vec::new();

    for (name, sig) in &wt_sigs {
        match head_sigs.get(name) {
            None => findings.push(mk_finding(name, rel_file, "added")),
            Some(head_sig) if head_sig != sig => {
                findings.push(mk_finding(name, rel_file, "modified"));
            }
            _ => {}
        }
    }
    for name in head_sigs.keys() {
        if !wt_sigs.contains_key(name) {
            findings.push(mk_finding(name, rel_file, "removed"));
        }
    }

    findings.sort_by(|a, b| a.evidence.cmp(&b.evidence));
    Ok(findings)
}

fn mk_finding(name: &str, rel_file: &Path, change: &'static str) -> Finding {
    let evidence = format!(
        "FFI signature `{name}` {change} in {} — blast radius leaves Rust; \
         downstream native consumers cannot be analyzed",
        rel_file.display()
    );
    let kind = FindingKind::FfiSignatureChange {
        symbol: name.to_string(),
        file: rel_file.to_path_buf(),
        change,
    };
    Finding::new("", Tier::Likely, 0.95, kind, evidence)
}

/// Collect FFI-relevant signatures from a parsed file, keyed by symbol name.
/// Picks up:
/// * `extern "C" { fn foo(...); static BAR: ...; }` foreign-function items
/// * Top-level fns with `#[no_mangle]` or explicit `extern "<abi>"`
fn ffi_signatures(ast: &syn::File) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for item in &ast.items {
        match item {
            Item::ForeignMod(fm) => {
                for fi in &fm.items {
                    match fi {
                        ForeignItem::Fn(f) => {
                            out.insert(
                                f.sig.ident.to_string(),
                                f.sig.to_token_stream().to_string(),
                            );
                        }
                        ForeignItem::Static(s) => {
                            out.insert(s.ident.to_string(), s.to_token_stream().to_string());
                        }
                        _ => {}
                    }
                }
            }
            Item::Fn(f) => {
                let is_no_mangle = f.attrs.iter().any(|a| a.path().is_ident("no_mangle"));
                let is_extern = f.sig.abi.is_some();
                if is_no_mangle || is_extern {
                    out.insert(f.sig.ident.to_string(), f.sig.to_token_stream().to_string());
                }
            }
            _ => {}
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
        .output()?;
    if output.status.success() {
        Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn git_fixture(initial: &str, modified: Option<&str>) -> (TempDir, std::path::PathBuf) {
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
        if let Some(new) = modified {
            fs::write(root.join(&rel), new).unwrap();
        }
        (dir, rel)
    }

    #[test]
    fn detects_added_extern_c_fn() {
        let (dir, rel) = git_fixture(
            "pub fn regular() {}\n",
            Some(
                "pub fn regular() {}\n\
                 extern \"C\" { fn foreign_call(x: i32) -> i32; }\n",
            ),
        );
        let hits = find_ffi_changes(dir.path(), &rel, "HEAD").unwrap();
        assert_eq!(hits.len(), 1);
        match &hits[0].kind {
            FindingKind::FfiSignatureChange { symbol, change, .. } => {
                assert_eq!(symbol, "foreign_call");
                assert_eq!(*change, "added");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(hits[0].severity, crate::finding::SeverityClass::High);
        assert_eq!(hits[0].tier, Tier::Likely);
        assert_eq!(hits[0].confidence, 0.95);
    }

    #[test]
    fn detects_modified_extern_c_signature() {
        let (dir, rel) = git_fixture(
            "extern \"C\" { fn callback(x: i32) -> i32; }\n",
            Some("extern \"C\" { fn callback(x: i64) -> i32; }\n"),
        );
        let hits = find_ffi_changes(dir.path(), &rel, "HEAD").unwrap();
        assert_eq!(hits.len(), 1);
        match &hits[0].kind {
            FindingKind::FfiSignatureChange { symbol, change, .. } => {
                assert_eq!(symbol, "callback");
                assert_eq!(*change, "modified");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn detects_removed_extern_c_symbol() {
        let (dir, rel) = git_fixture(
            "extern \"C\" { fn gone(); fn stays(); }\n",
            Some("extern \"C\" { fn stays(); }\n"),
        );
        let hits = find_ffi_changes(dir.path(), &rel, "HEAD").unwrap();
        let payloads: Vec<_> = hits
            .iter()
            .filter_map(|h| match &h.kind {
                FindingKind::FfiSignatureChange { symbol, change, .. } => {
                    Some((symbol.clone(), *change))
                }
                _ => None,
            })
            .collect();
        assert_eq!(payloads, vec![("gone".to_string(), "removed")]);
    }

    #[test]
    fn detects_no_mangle_fn_change() {
        let (dir, rel) = git_fixture(
            "#[no_mangle]\npub extern \"C\" fn exported(x: i32) -> i32 { x }\n",
            Some("#[no_mangle]\npub extern \"C\" fn exported(x: u32) -> i32 { x as i32 }\n"),
        );
        let hits = find_ffi_changes(dir.path(), &rel, "HEAD").unwrap();
        assert_eq!(hits.len(), 1);
        match &hits[0].kind {
            FindingKind::FfiSignatureChange { symbol, change, .. } => {
                assert_eq!(symbol, "exported");
                assert_eq!(*change, "modified");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn ignores_unchanged_ffi_symbols() {
        let body = "extern \"C\" { fn stable(); }\n";
        let (dir, rel) = git_fixture(body, Some(body));
        let hits = find_ffi_changes(dir.path(), &rel, "HEAD").unwrap();
        assert!(hits.is_empty());
    }
}
