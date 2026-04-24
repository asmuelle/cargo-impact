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
//! Emits two classes of finding from the expanded `syn::File`
//! (cargo-expand merges a whole crate into one stream):
//!
//! 1. **Expanded trait impls** — `impl Trait for T` blocks where
//!    `Trait` is in `changed_traits`. Catches derives and attribute
//!    macros (`#[derive(Serialize)]`, `#[derive(clap::Parser)]`,
//!    `#[derive(thiserror::Error)]`) that synthesize impls the
//!    syn-only walker never sees.
//! 2. **Expanded test references** — `#[test]` / `#[tokio::test]` /
//!    `#[rstest]` fns whose post-expansion body tokens reference a
//!    name in `changed_symbols`. Catches the `sqlx::query!(...)`
//!    case: raw source has a literal string, but the expansion
//!    names the referenced struct. Dedup against raw-source
//!    `TestReference` findings happens in `dedup.rs` so we don't
//!    double-count tests the syn-only walker already caught.
//!
//! Still deferred: full source-map back to the unexpanded file for
//! jump-to-definition (expansion loses line anchors), and expansion
//! for binary-only crates (we only `cargo expand --lib` today).
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
use crate::tests_scan::{is_test_fn, tokens_contain_ident};
use anyhow::Result;
use quote::ToTokens;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;
use syn::visit::Visit;
use syn::{ItemFn, ItemImpl, Path as SynPath, Type, TypePath};

const TOOL_BIN: &str = "cargo-expand";

/// Wall-clock budget for a single `cargo expand` invocation. Cold
/// builds on a mid-sized crate can hit 30s; we allow 90s for headroom
/// but kill runs that stall past that so a misbehaving expansion
/// doesn't hang the whole pipeline.
const MACRO_EXPAND_TIMEOUT: Duration = Duration::from_secs(90);

/// Run `cargo expand` and emit findings from the expanded AST: trait
/// impls matching `changed_traits` plus test references matching
/// `changed_symbols`. Dedup against existing findings is the
/// orchestrator's responsibility (the content-hashed ID plus the
/// dedup passes already handle it).
///
/// Blank IDs on returned findings; the orchestrator fills them in.
pub fn run(
    root: &Path,
    changed_traits: &BTreeSet<String>,
    changed_symbols: &BTreeSet<String>,
    enabled: bool,
) -> Result<Vec<Finding>> {
    if !enabled {
        return Ok(Vec::new());
    }
    if changed_traits.is_empty() && changed_symbols.is_empty() {
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
    Ok(find_in_expanded(&expanded, changed_traits, changed_symbols))
}

/// Parse the expanded-source string and walk for both trait impls
/// (matching `changed_traits`) and test references (matching
/// `changed_symbols`). Pulled out of `run` so it's testable without
/// spawning cargo-expand — any syn-parseable string serves as a
/// fixture.
pub(crate) fn find_in_expanded(
    expanded: &str,
    changed_traits: &BTreeSet<String>,
    changed_symbols: &BTreeSet<String>,
) -> Vec<Finding> {
    let Ok(ast) = syn::parse_file(expanded) else {
        eprintln!(
            "cargo-impact: cargo-expand output didn't parse as a syn::File; skipping. \
             This is usually a stability bug in the expansion; report with the expanded \
             output attached."
        );
        return Vec::new();
    };
    let mut visitor = ExpandedVisitor {
        changed_traits,
        changed_symbols,
        impl_hits: Vec::new(),
        test_hits: Vec::new(),
    };
    visitor.visit_file(&ast);

    let mut findings = Vec::with_capacity(visitor.impl_hits.len() + visitor.test_hits.len());

    for (trait_name, impl_for) in visitor.impl_hits {
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
        findings.push(Finding::new("", Tier::Likely, 0.75, kind, evidence));
    }

    for (test_name, matched) in visitor.test_hits {
        let matched_vec: Vec<String> = matched.into_iter().collect();
        let evidence = format!(
            "test body references {} after macro expansion (syn-only source walk missed \
             it — likely a fn-like macro like `sqlx::query!` or `include_str!` that \
             expands to code naming the changed symbol)",
            matched_vec.join(", ")
        );
        let kind = FindingKind::TestReference {
            test: Location {
                file: std::path::PathBuf::from("<expanded>"),
                symbol: test_name.clone(),
            },
            matched_symbols: matched_vec,
        };
        findings.push(
            Finding::new("", Tier::Likely, 0.75, kind, evidence)
                .with_suggested_action(format!("cargo nextest run -E 'test({test_name})'")),
        );
    }

    findings
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
    // Strategy: try `--lib` first because lib+bin crates should expand
    // the library side (traits, types, derive impls typically live
    // there). If that fails because the crate is binary-only (no
    // `[lib]` target), retry without `--lib` so cargo's default target
    // selection picks the bin. Other failure modes (compile errors,
    // missing dependencies) surface from the second attempt's stderr.
    match spawn_cargo_expand(root, &["--lib"]) {
        Ok(out) => Ok(out),
        Err(e) if is_no_library_error(&e.to_string()) => {
            eprintln!(
                "cargo-impact: `cargo expand --lib` found no library target; \
                 retrying without --lib for binary-only crate."
            );
            spawn_cargo_expand(root, &[])
        }
        Err(e) => Err(e),
    }
}

/// Detect cargo-expand's "no library targets" error so we can fall
/// back to binary expansion. Pattern-match on both the cargo-expand
/// message and the underlying cargo message — phrasing drifts between
/// versions, so we match on a stable substring.
fn is_no_library_error(stderr: &str) -> bool {
    let haystack = stderr.to_lowercase();
    haystack.contains("no library targets")
        || haystack.contains("no lib target")
        || haystack.contains("does not have a library")
}

fn spawn_cargo_expand(root: &Path, extra_args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("cargo");
    cmd.arg("expand")
        .args(extra_args)
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
// Visitor — merges trait-impl and test-ref detection over the single
// expanded stream. Kept local (rather than composed from traits.rs +
// tests_scan.rs visitors) because `cargo expand` merges the whole
// crate into one file; we can't cheaply map hits back to per-file
// paths, so we don't try.
// ---------------------------------------------------------------------------

struct ExpandedVisitor<'a> {
    changed_traits: &'a BTreeSet<String>,
    changed_symbols: &'a BTreeSet<String>,
    impl_hits: Vec<(String, String)>,
    test_hits: Vec<(String, BTreeSet<String>)>,
}

impl<'ast> Visit<'ast> for ExpandedVisitor<'_> {
    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        if let Some((_, trait_path, _)) = &node.trait_
            && let Some(trait_name) = last_ident(trait_path)
            && self.changed_traits.contains(&trait_name)
        {
            let impl_for = type_to_string(&node.self_ty);
            self.impl_hits.push((trait_name, impl_for));
        }
        syn::visit::visit_item_impl(self, node);
    }

    fn visit_item_fn(&mut self, f: &'ast ItemFn) {
        if !self.changed_symbols.is_empty() && is_test_fn(&f.attrs) {
            let body = f.block.to_token_stream().to_string();
            let matched: BTreeSet<String> = self
                .changed_symbols
                .iter()
                .filter(|sym| tokens_contain_ident(&body, sym))
                .cloned()
                .collect();
            if !matched.is_empty() {
                self.test_hits.push((f.sig.ident.to_string(), matched));
            }
        }
        syn::visit::visit_item_fn(self, f);
    }
}

fn last_ident(path: &SynPath) -> Option<String> {
    path.segments.last().map(|s| s.ident.to_string())
}

fn type_to_string(ty: &Type) -> String {
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
        let hits = find_in_expanded(src, &BTreeSet::new(), &BTreeSet::new());
        assert!(hits.is_empty());
    }

    #[test]
    fn matches_derived_impl_on_changed_trait() {
        let src = "struct S; impl Greeter for S { fn hi(&self) {} }";
        let hits = find_in_expanded(src, &changed(&["Greeter"]), &BTreeSet::new());
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
        let hits = find_in_expanded(src, &changed(&["Greeter"]), &BTreeSet::new());
        assert!(
            hits[0].evidence.contains("revealed by macro expansion"),
            "evidence should mark the finding as expansion-derived: {}",
            hits[0].evidence
        );
    }

    #[test]
    fn ignores_impls_on_unchanged_traits() {
        let src = "impl Unrelated for S { }";
        let hits = find_in_expanded(src, &changed(&["Greeter"]), &BTreeSet::new());
        assert!(hits.is_empty());
    }

    #[test]
    fn matches_impl_via_last_path_segment() {
        // Expanded output often carries fully-qualified paths;
        // match on the trailing segment only, same as traits.rs.
        let src = "impl ::serde::Serialize for S { }";
        let hits = find_in_expanded(src, &changed(&["Serialize"]), &BTreeSet::new());
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn multiple_matches_in_one_stream_all_emitted() {
        let src = "
            impl A for X { }
            impl A for Y { }
            impl B for Z { }
        ";
        let hits = find_in_expanded(src, &changed(&["A", "B"]), &BTreeSet::new());
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn unparseable_input_returns_empty_without_panicking() {
        let hits = find_in_expanded(
            "this is {{ not syn parseable",
            &changed(&["X"]),
            &changed(&["Y"]),
        );
        assert!(hits.is_empty());
    }

    #[test]
    fn disabled_flag_short_circuits_before_calling_cargo() {
        // If the tool weren't installed this would still return Ok(empty)
        // because `enabled = false` short-circuits. Proves the flag gate.
        let findings = run(
            Path::new("/nonexistent"),
            &changed(&["X"]),
            &BTreeSet::new(),
            false,
        )
        .unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn empty_inputs_short_circuit_before_spawning() {
        let findings = run(
            Path::new("/nonexistent"),
            &BTreeSet::new(),
            &BTreeSet::new(),
            true,
        )
        .unwrap();
        assert!(findings.is_empty());
    }

    // --- Expanded test-reference findings ---

    #[test]
    fn detects_test_referencing_changed_symbol_in_expanded_body() {
        // Simulates a test that, after cargo expand, references `User`
        // — the kind of code `sqlx::query!("SELECT * FROM users")`
        // expands into. The raw source wouldn't have carried this
        // reference.
        let src = r#"
            #[test]
            fn query_test() {
                let _: User = User::default();
            }
        "#;
        let hits = find_in_expanded(src, &BTreeSet::new(), &changed(&["User"]));
        assert_eq!(hits.len(), 1);
        let FindingKind::TestReference {
            test,
            matched_symbols,
        } = &hits[0].kind
        else {
            panic!("wrong kind: {:?}", hits[0].kind);
        };
        assert_eq!(test.symbol, "query_test");
        assert_eq!(test.file, std::path::PathBuf::from("<expanded>"));
        assert_eq!(matched_symbols, &vec!["User".to_string()]);
        assert_eq!(hits[0].tier, Tier::Likely);
        assert!((hits[0].confidence - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn expanded_test_ref_evidence_calls_out_expansion_source() {
        let src = "#[test] fn t() { User::new(); }";
        let hits = find_in_expanded(src, &BTreeSet::new(), &changed(&["User"]));
        assert!(
            hits[0].evidence.contains("after macro expansion"),
            "evidence should mark the finding as expansion-derived: {}",
            hits[0].evidence
        );
    }

    #[test]
    fn expanded_test_ref_emits_nextest_filter_suggestion() {
        let src = "#[test] fn login_case() { login(); }";
        let hits = find_in_expanded(src, &BTreeSet::new(), &changed(&["login"]));
        assert!(
            hits[0]
                .suggested_action
                .as_deref()
                .is_some_and(|s| s.contains("test(login_case)")),
            "expected nextest filter suggestion, got {:?}",
            hits[0].suggested_action
        );
    }

    #[test]
    fn non_test_fns_are_not_emitted_as_test_refs() {
        let src = "fn helper() { let _ = User::default(); }";
        let hits = find_in_expanded(src, &BTreeSet::new(), &changed(&["User"]));
        assert!(hits.is_empty());
    }

    #[test]
    fn expanded_test_ref_respects_word_boundaries() {
        // `user_profile` must not match the changed symbol `user`.
        let src = "#[test] fn t() { let user_profile = 1; let _ = user_profile; }";
        let hits = find_in_expanded(src, &BTreeSet::new(), &changed(&["user"]));
        assert!(hits.is_empty(), "unexpected hits: {hits:?}");
    }

    // --- Binary-only crate fallback ---

    #[test]
    fn is_no_library_error_matches_known_phrasings() {
        // cargo-expand's message (current phrasing).
        assert!(is_no_library_error(
            "error: no library targets found in package `foo`"
        ));
        // cargo's own variant used in some versions.
        assert!(is_no_library_error(
            "error: no lib target found in package `foo`"
        ));
        // Another phrasing surfaced in older cargo/cargo-expand combos.
        assert!(is_no_library_error(
            "error: the package `foo` does not have a library"
        ));
    }

    #[test]
    fn is_no_library_error_is_case_insensitive() {
        assert!(is_no_library_error(
            "ERROR: No Library Targets Found in package `foo`"
        ));
    }

    #[test]
    fn is_no_library_error_rejects_unrelated_errors() {
        assert!(!is_no_library_error(
            "error[E0382]: borrow of moved value `x`"
        ));
        assert!(!is_no_library_error("error: could not compile `foo`"));
        assert!(!is_no_library_error(""));
    }

    #[test]
    fn impl_and_test_findings_emitted_together_from_same_stream() {
        let src = r#"
            struct User;
            impl Greeter for User { fn hi(&self) {} }
            #[test]
            fn uses_user() {
                let _ = User;
            }
        "#;
        let hits = find_in_expanded(src, &changed(&["Greeter"]), &changed(&["User"]));
        assert_eq!(hits.len(), 2);
        let kinds: Vec<_> = hits
            .iter()
            .map(|h| match &h.kind {
                FindingKind::TraitImpl { .. } => "impl",
                FindingKind::TestReference { .. } => "test",
                _ => "other",
            })
            .collect();
        assert!(kinds.contains(&"impl"));
        assert!(kinds.contains(&"test"));
    }
}
