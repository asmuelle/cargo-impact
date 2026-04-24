//! Macro-expansion-backed trait-impl detection.
//!
//! Shells out to `cargo expand` (requires `cargo install cargo-expand`
//! on the user side — we don't bundle it) to reveal trait impls
//! synthesized by derive and attribute macros. The syn-only
//! `derive.rs` analyzer only flags derives of traits defined in
//! changed files; this module catches impls that macros expand to at
//! compile time even when the trait itself lives in an external crate.
//!
//! Motivating cases
//! ----------------
//! * `#[derive(Serialize, Deserialize)]` on a changed struct — the
//!   generated `impl Serialize for S` block appears only after
//!   expansion. A downstream consumer calling `serde_json::to_string`
//!   on that struct would compile against the expanded impl, and
//!   changing the struct's fields changes that impl's behavior.
//! * `#[tokio::main]` / `#[tracing::instrument]` — these wrap the
//!   user's fn body with additional tokens that are invisible to
//!   syn-only walkers. Their bodies include references that syn-only
//!   analyzers can't reach.
//! * `#[clap::Parser]` / `#[thiserror::Error]` — similar story: impls
//!   of `clap::Parser` / `std::error::Error` get synthesized.
//!
//! Scope in this release
//! ---------------------
//! The MVP here only emits trait-impl findings for traits in the
//! `changed_traits` set. `cargo expand` output is parsed as a single
//! large `syn::File` (cargo-expand merges a whole crate into one
//! stream) and walked for `impl Trait for T` sites. Full coverage
//! (attribute-macro body re-analysis, proper source-map back to the
//! unexpanded file) is deferred.
//!
//! Graceful degradation
//! --------------------
//! The gate is `--macro-expand`. If the flag is off, this module is a
//! no-op. If the flag is on but `cargo-expand` isn't on PATH, the tool
//! fails to spawn, or expansion takes longer than
//! [`MACRO_EXPAND_TIMEOUT`], we log a stderr notice and return an
//! empty finding list — consistent with the project-wide "never fail
//! the whole run because an optional tool is missing" policy.

use crate::finding::{Finding, FindingKind, Location, Tier};
use anyhow::Result;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;
use syn::visit::Visit;
use syn::{ItemImpl, Path as SynPath, Type, TypePath};

const TOOL_BIN: &str = "cargo-expand";

/// Wall-clock budget for a single `cargo expand` invocation. Cold
/// builds on a mid-sized crate can hit 30s; we allow 90s for headroom
/// but kill runs that stall past that so a misbehaving expansion
/// doesn't hang the whole pipeline.
const MACRO_EXPAND_TIMEOUT: Duration = Duration::from_secs(90);

/// Run `cargo expand` and emit findings for trait impls that appear in
/// the expanded AST and target a changed trait. Dedup against existing
/// findings is the orchestrator's responsibility (the content-hashed
/// ID plus the dedup pass already handle it).
///
/// Blank IDs on returned findings; the orchestrator fills them in.
pub fn run(root: &Path, changed_traits: &BTreeSet<String>, enabled: bool) -> Result<Vec<Finding>> {
    if !enabled {
        return Ok(Vec::new());
    }
    if changed_traits.is_empty() {
        return Ok(Vec::new());
    }
    if !is_installed() {
        eprintln!(
            "cargo-impact: --macro-expand requested but `{TOOL_BIN}` not found on PATH. \
             Install it via `cargo install cargo-expand`; skipping."
        );
        return Ok(Vec::new());
    }

    let expanded = match run_cargo_expand(root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cargo-impact: cargo-expand invocation failed: {e:#}; skipping.");
            return Ok(Vec::new());
        }
    };
    Ok(find_impls_in_expanded(&expanded, changed_traits))
}

/// Parse the expanded-source string and walk for trait impls matching
/// `changed_traits`. Pulled out of `run` so it's testable without
/// spawning cargo-expand — any syn-parseable string serves as a
/// fixture.
pub(crate) fn find_impls_in_expanded(
    expanded: &str,
    changed_traits: &BTreeSet<String>,
) -> Vec<Finding> {
    let Ok(ast) = syn::parse_file(expanded) else {
        eprintln!(
            "cargo-impact: cargo-expand output didn't parse as a syn::File; skipping. \
             This is usually a stability bug in the expansion; report with the expanded \
             output attached."
        );
        return Vec::new();
    };
    let mut visitor = ImplVisitor {
        changed_traits,
        hits: Vec::new(),
    };
    visitor.visit_file(&ast);

    visitor
        .hits
        .into_iter()
        .map(|(trait_name, impl_for)| {
            let evidence = format!(
                "`impl {trait_name} for {impl_for}` — revealed by macro expansion (syn-only \
                 analysis doesn't see impls synthesized by derive/attribute macros like \
                 serde, tokio, clap, thiserror)"
            );
            let kind = FindingKind::TraitImpl {
                trait_name: trait_name.clone(),
                impl_for: impl_for.clone(),
                impl_site: Location {
                    // Synthesized impls don't have a stable source
                    // location — they live in the expansion of the
                    // derive that produced them. Use `<expanded>` as a
                    // sentinel so consumers know not to jump-to-file.
                    file: std::path::PathBuf::from("<expanded>"),
                    symbol: format!("impl {trait_name} for {impl_for}"),
                },
            };
            Finding::new("", Tier::Likely, 0.75, kind, evidence)
        })
        .collect()
}

fn is_installed() -> bool {
    which(TOOL_BIN).is_some()
}

fn which(name: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let with_exe = candidate.with_extension("exe");
            if with_exe.is_file() {
                return Some(with_exe);
            }
        }
    }
    None
}

fn run_cargo_expand(root: &Path) -> Result<String> {
    // `cargo expand --lib` targets the library crate root. For
    // binary-only crates this will fail; we keep the current
    // behavior (stderr note + empty findings) rather than trying a
    // secondary `--bin` pass — users whose workspace is binary-first
    // can rely on the RA-backed analysis instead, which doesn't need
    // cargo-expand.
    let mut cmd = Command::new("cargo");
    cmd.arg("expand")
        .arg("--lib")
        .arg("--color=never")
        .arg("--ugly") // no rustfmt — faster, syn doesn't care
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn()?;
    let start = std::time::Instant::now();

    // Poll with a wall-clock budget. Can't use `wait_timeout` without
    // pulling a new dep; the poll loop keeps us std-only.
    loop {
        if let Some(status) = child.try_wait()? {
            let out = child.wait_with_output()?;
            if !status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                anyhow::bail!("cargo expand exited with status {status:?}; stderr:\n{stderr}");
            }
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        if start.elapsed() > MACRO_EXPAND_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "cargo expand did not finish within {:?}",
                MACRO_EXPAND_TIMEOUT
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------------------
// Visitor (duplicated in shape from traits.rs but kept local because
// the module-level semantics differ — we never match against a per-
// file path, only against the single merged expansion stream).
// ---------------------------------------------------------------------------

struct ImplVisitor<'a> {
    changed_traits: &'a BTreeSet<String>,
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
        syn::visit::visit_item_impl(self, node);
    }
}

fn last_ident(path: &SynPath) -> Option<String> {
    path.segments.last().map(|s| s.ident.to_string())
}

fn type_to_string(ty: &Type) -> String {
    use quote::ToTokens;
    if let Type::Path(TypePath { qself: None, path }) = ty
        && let Some(seg) = path.segments.last()
    {
        return seg.ident.to_string();
    }
    ty.to_token_stream().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn empty_changed_set_returns_no_findings() {
        let src = "impl Serialize for S {}";
        let hits = find_impls_in_expanded(src, &BTreeSet::new());
        assert!(hits.is_empty());
    }

    #[test]
    fn matches_derived_impl_on_changed_trait() {
        let src = "struct S; impl Greeter for S { fn hi(&self) {} }";
        let hits = find_impls_in_expanded(src, &changed(&["Greeter"]));
        assert_eq!(hits.len(), 1);
        let FindingKind::TraitImpl {
            trait_name,
            impl_for,
            ..
        } = &hits[0].kind
        else {
            panic!("wrong kind");
        };
        assert_eq!(trait_name, "Greeter");
        assert_eq!(impl_for, "S");
    }

    #[test]
    fn evidence_calls_out_macro_expansion_source() {
        let src = "impl Greeter for S { fn hi(&self) {} }";
        let hits = find_impls_in_expanded(src, &changed(&["Greeter"]));
        assert!(
            hits[0].evidence.contains("revealed by macro expansion"),
            "evidence should mark the finding as expansion-derived: {}",
            hits[0].evidence
        );
    }

    #[test]
    fn ignores_impls_on_unchanged_traits() {
        let src = "impl Unrelated for S { }";
        let hits = find_impls_in_expanded(src, &changed(&["Greeter"]));
        assert!(hits.is_empty());
    }

    #[test]
    fn matches_impl_via_last_path_segment() {
        // Expanded output often carries fully-qualified paths;
        // match on the trailing segment only, same as traits.rs.
        let src = "impl ::serde::Serialize for S { }";
        let hits = find_impls_in_expanded(src, &changed(&["Serialize"]));
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn multiple_matches_in_one_stream_all_emitted() {
        let src = "
            impl A for X { }
            impl A for Y { }
            impl B for Z { }
        ";
        let hits = find_impls_in_expanded(src, &changed(&["A", "B"]));
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn unparseable_input_returns_empty_without_panicking() {
        let hits = find_impls_in_expanded("this is {{ not syn parseable", &changed(&["X"]));
        assert!(hits.is_empty());
    }

    #[test]
    fn disabled_flag_short_circuits_before_calling_cargo() {
        // If the tool weren't installed this would still return Ok(empty)
        // because `enabled = false` short-circuits. Proves the flag gate.
        let findings = run(Path::new("/nonexistent"), &changed(&["X"]), false).unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn empty_changed_traits_short_circuits_before_spawning() {
        let findings = run(Path::new("/nonexistent"), &BTreeSet::new(), true).unwrap();
        assert!(findings.is_empty());
    }
}
