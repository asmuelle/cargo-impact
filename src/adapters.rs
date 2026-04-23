//! Framework adapters — axum and clap reference implementations.
//!
//! Maps changed Rust symbols to the *runtime surfaces* they affect —
//! things a user of the compiled binary can observe. axum handlers
//! are HTTP routes; clap derives are CLI subcommands. Both fire as
//! [`FindingKind::RuntimeSurface`] so downstream consumers
//! (`impact_surface` MCP tool, severity filters, etc.) can
//! consolidate surface-impact reasoning independent of the framework.
//!
//! Scope — v0.3-alpha
//! ------------------
//! This is the first pass on each adapter. Priorities chosen for what
//! users will actually hit in real Rust projects:
//!
//! * **axum**: detect `Router::new().route("/path", method(handler))`
//!   and `Router::nest("/prefix", …)` chains. When the handler
//!   identifier (or any path segment in the method's token stream)
//!   references a changed symbol, emit a surface finding.
//! * **clap**: detect `#[derive(Parser)]` and `#[derive(Subcommand)]`
//!   on struct/enum items. Fire when the struct/enum name itself is
//!   in the changed-symbol set (the command surface it defines has
//!   changed by definition) OR when any of its named fields/variants
//!   references a changed symbol.
//!
//! What this module *doesn't* do yet (honest deferrals)
//! ----------------------------------------------------
//! * No adapter trait / plugin surface — both analyzers are inlined in
//!   this module. Once we have three adapters with similar shape the
//!   trait will be obvious. Premature abstraction otherwise.
//! * No actix / rocket / warp / tauri — axum is the most-requested
//!   web framework and clap is the most-requested CLI framework.
//!   Others land when demand or PRs arrive.
//! * No parameter-type inspection. We match handler identifiers and
//!   token-stream references, not the full type graph. A handler that
//!   takes `User` as extractor won't be flagged when `User`'s fields
//!   change unless the handler's body also names it — usually true
//!   in practice, occasionally missed.
//! * Macro-expanded forms (`#[tokio::main]`, `#[axum::debug_handler]`)
//!   are *not* expanded — we match on the attribute name at the syntax
//!   level. This is fine because the interesting content is the
//!   function body, which we still see.

use crate::finding::{Finding, FindingKind, Location, Tier};
use crate::tests_scan::workspace_rust_files;
use anyhow::Result;
use quote::ToTokens;
use std::collections::BTreeSet;
use std::path::Path;
use syn::visit::Visit;

/// Entry point: run every bundled adapter against `root`, returning
/// their combined findings. Mirrors `traits::find_trait_impls` in shape
/// so the orchestrator plumbing stays uniform.
pub fn find_runtime_surfaces(
    root: &Path,
    changed_symbols: &BTreeSet<String>,
) -> Result<Vec<Finding>> {
    if changed_symbols.is_empty() {
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
        let mut axum_visitor = AxumVisitor {
            changed: changed_symbols,
            file: &rel,
            hits: Vec::new(),
        };
        axum_visitor.visit_file(&ast);
        findings.extend(axum_visitor.hits);

        let mut clap_hits: Vec<(String, String)> = Vec::new();
        find_clap_surfaces(&ast.items, changed_symbols, &mut clap_hits);
        for (ident, command_kind) in clap_hits {
            findings.push(build_surface(
                "clap",
                &format!("{command_kind} `{ident}`"),
                &rel,
                format!(
                    "clap `#[derive({command_kind})]` on `{ident}` at {} — CLI surface \
                     touches a changed symbol",
                    rel.display()
                ),
            ));
        }
    }

    Ok(findings)
}

fn build_surface(framework: &str, identifier: &str, rel: &Path, evidence: String) -> Finding {
    Finding::new(
        "",
        Tier::Likely,
        0.75,
        FindingKind::RuntimeSurface {
            framework: framework.to_string(),
            identifier: identifier.to_string(),
            site: Location {
                file: rel.to_path_buf(),
                symbol: identifier.to_string(),
            },
        },
        evidence,
    )
}

// ---------------------------------------------------------------------------
// axum
// ---------------------------------------------------------------------------

struct AxumVisitor<'a> {
    changed: &'a BTreeSet<String>,
    file: &'a Path,
    hits: Vec<Finding>,
}

impl<'ast> Visit<'ast> for AxumVisitor<'_> {
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        let method = call.method.to_string();
        // `.route(path, handler)` and `.nest(path, router)` are the two
        // axum builder-method forms we care about. Everything else
        // (Router::new, layer, with_state, …) recurses normally below.
        if method == "route" || method == "nest" {
            self.inspect_router_chain(call);
        }
        syn::visit::visit_expr_method_call(self, call);
    }
}

impl AxumVisitor<'_> {
    fn inspect_router_chain(&mut self, call: &syn::ExprMethodCall) {
        let Some(path_arg) = call.args.first() else {
            return;
        };
        let route_path = string_literal_from(path_arg);
        let tokens = call.args.to_token_stream().to_string();

        // Match any changed-symbol identifier appearing in the method
        // args' token stream. `quote` separates tokens with whitespace,
        // so word-boundary split is reliable (no false positives on
        // `login` matching `login_helper`).
        let mentions: BTreeSet<&String> = self
            .changed
            .iter()
            .filter(|sym| token_contains_ident(&tokens, sym))
            .collect();
        if mentions.is_empty() {
            return;
        }

        let identifier = match route_path {
            Some(p) => format!("{} `{}`", call.method, p),
            None => call.method.to_string(),
        };
        let mentioned_list = mentions
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let evidence = format!(
            "axum `{}` at {} references changed symbols: {}",
            identifier,
            self.file.display(),
            mentioned_list
        );
        self.hits
            .push(build_surface("axum", &identifier, self.file, evidence));
    }
}

/// Extract the string value from a positional argument if it's a bare
/// string literal expression. Used for axum's `route("/path", …)`.
fn string_literal_from(expr: &syn::Expr) -> Option<String> {
    if let syn::Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Str(s),
        ..
    }) = expr
    {
        Some(s.value())
    } else {
        None
    }
}

/// Word-boundary identifier search over a `quote`-produced token
/// string. Same helper as `tests_scan::tokens_contain_ident` — kept
/// separate rather than shared because the test-scanner is not a
/// public surface and cross-importing internal helpers adds coupling.
fn token_contains_ident(haystack: &str, needle: &str) -> bool {
    haystack
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|tok| tok == needle)
}

// ---------------------------------------------------------------------------
// clap
// ---------------------------------------------------------------------------

fn find_clap_surfaces(
    items: &[syn::Item],
    changed: &BTreeSet<String>,
    out: &mut Vec<(String, String)>,
) {
    for item in items {
        match item {
            syn::Item::Struct(s) => {
                let derives = derive_names(&s.attrs);
                if let Some(kind) = clap_surface_kind(&derives) {
                    if changed.contains(&s.ident.to_string())
                        || any_field_mentions_changed(&s.fields, changed)
                    {
                        out.push((s.ident.to_string(), kind));
                    }
                }
            }
            syn::Item::Enum(e) => {
                let derives = derive_names(&e.attrs);
                if let Some(kind) = clap_surface_kind(&derives) {
                    let variant_mentions = e.variants.iter().any(|v| {
                        changed.contains(&v.ident.to_string())
                            || any_field_mentions_changed(&v.fields, changed)
                    });
                    if changed.contains(&e.ident.to_string()) || variant_mentions {
                        out.push((e.ident.to_string(), kind));
                    }
                }
            }
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    find_clap_surfaces(inner, changed, out);
                }
            }
            _ => {}
        }
    }
}

/// Return the clap "surface kind" if this derive list includes one
/// of the command-defining derives. `Parser` for top-level commands,
/// `Subcommand` for enum-based subcommand dispatch, `Args` for arg
/// groups.
fn clap_surface_kind(derives: &[String]) -> Option<String> {
    for d in derives {
        match d.as_str() {
            "Parser" => return Some("Parser".into()),
            "Subcommand" => return Some("Subcommand".into()),
            "Args" => return Some("Args".into()),
            _ => {}
        }
    }
    None
}

fn derive_names(attrs: &[syn::Attribute]) -> Vec<String> {
    let mut out = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("derive") {
            continue;
        }
        let Ok(paths) = attr.parse_args_with(|input: syn::parse::ParseStream<'_>| {
            let punct: syn::punctuated::Punctuated<syn::Path, syn::Token![,]> =
                syn::punctuated::Punctuated::parse_terminated(input)?;
            Ok(punct.into_iter().collect::<Vec<_>>())
        }) else {
            continue;
        };
        for path in paths {
            if let Some(last) = path.segments.last() {
                out.push(last.ident.to_string());
            }
        }
    }
    out
}

fn any_field_mentions_changed(fields: &syn::Fields, changed: &BTreeSet<String>) -> bool {
    let tokens = fields.to_token_stream().to_string();
    changed.iter().any(|sym| token_contains_ident(&tokens, sym))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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

    fn syms(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    fn payloads(findings: &[Finding]) -> Vec<(String, String)> {
        findings
            .iter()
            .filter_map(|f| match &f.kind {
                FindingKind::RuntimeSurface {
                    framework,
                    identifier,
                    ..
                } => Some((framework.clone(), identifier.clone())),
                _ => None,
            })
            .collect()
    }

    // --- axum ---

    #[test]
    fn axum_route_with_changed_handler_fires() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"
                use axum::{Router, routing::get};
                fn build() -> Router {
                    Router::new().route("/api/user", get(get_user))
                }
                async fn get_user() -> &'static str { "hi" }
            "#,
        )]);
        let hits = find_runtime_surfaces(dir.path(), &syms(&["get_user"])).unwrap();
        let pairs = payloads(&hits);
        assert!(
            pairs
                .iter()
                .any(|(fw, id)| fw == "axum" && id.contains("/api/user")),
            "expected axum route for /api/user; got {pairs:?}"
        );
    }

    #[test]
    fn axum_nest_fires_when_nested_router_changed() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"
                use axum::Router;
                fn build(api: Router) -> Router {
                    Router::new().nest("/v1", api)
                }
            "#,
        )]);
        // `api` isn't the changed symbol, but if the token stream of the
        // nest call references a changed ident, we fire. Use a
        // realistic example: calling a helper by name.
        let dir2 = setup(&[(
            "src/lib.rs",
            r#"
                use axum::Router;
                fn build() -> Router {
                    Router::new().nest("/v1", build_api())
                }
                fn build_api() -> Router { Router::new() }
            "#,
        )]);
        let _ = dir; // keep the pattern-demo repo around for readability
        let hits = find_runtime_surfaces(dir2.path(), &syms(&["build_api"])).unwrap();
        let pairs = payloads(&hits);
        assert!(pairs.iter().any(|(fw, _)| fw == "axum"));
    }

    #[test]
    fn axum_ignores_routes_referencing_unrelated_symbols() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"
                use axum::{Router, routing::get};
                fn build() -> Router {
                    Router::new().route("/health", get(health))
                }
                async fn health() -> &'static str { "ok" }
            "#,
        )]);
        let hits = find_runtime_surfaces(dir.path(), &syms(&["get_user"])).unwrap();
        assert!(hits.is_empty(), "no route mentions get_user; got {hits:?}");
    }

    #[test]
    fn axum_avoids_substring_false_positives() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"
                use axum::{Router, routing::get};
                fn build() -> Router {
                    Router::new().route("/x", get(get_user_helper))
                }
                async fn get_user_helper() -> &'static str { "hi" }
            "#,
        )]);
        let hits = find_runtime_surfaces(dir.path(), &syms(&["get_user"])).unwrap();
        assert!(hits.is_empty(), "get_user must not match get_user_helper");
    }

    // --- clap ---

    #[test]
    fn clap_parser_on_struct_fires_when_struct_name_changed() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"
                #[derive(clap::Parser)]
                pub struct Cli {
                    #[arg(long)]
                    pub verbose: bool,
                }
            "#,
        )]);
        let hits = find_runtime_surfaces(dir.path(), &syms(&["Cli"])).unwrap();
        let pairs = payloads(&hits);
        assert!(
            pairs
                .iter()
                .any(|(fw, id)| fw == "clap" && id.contains("Cli")),
            "expected clap Parser for Cli; got {pairs:?}"
        );
    }

    #[test]
    fn clap_subcommand_enum_fires_when_variant_field_changed() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"
                #[derive(clap::Subcommand)]
                pub enum Cmd {
                    Run { target: TargetConfig },
                }
                pub struct TargetConfig;
            "#,
        )]);
        let hits = find_runtime_surfaces(dir.path(), &syms(&["TargetConfig"])).unwrap();
        let pairs = payloads(&hits);
        assert!(
            pairs
                .iter()
                .any(|(fw, id)| fw == "clap" && id.contains("Cmd")),
            "expected clap Subcommand for Cmd; got {pairs:?}"
        );
    }

    #[test]
    fn clap_args_derive_covered() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"
                #[derive(clap::Args)]
                pub struct ServeOpts {
                    #[arg(long)]
                    pub port: u16,
                }
            "#,
        )]);
        let hits = find_runtime_surfaces(dir.path(), &syms(&["ServeOpts"])).unwrap();
        assert!(hits.iter().any(|f| matches!(&f.kind, FindingKind::RuntimeSurface { framework, .. } if framework == "clap")));
    }

    #[test]
    fn clap_ignores_structs_without_clap_derives() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"
                #[derive(Debug, Clone)]
                pub struct Cli {
                    pub verbose: bool,
                }
            "#,
        )]);
        let hits = find_runtime_surfaces(dir.path(), &syms(&["Cli"])).unwrap();
        assert!(
            hits.is_empty(),
            "plain derives shouldn't fire; got {hits:?}"
        );
    }

    #[test]
    fn empty_changed_set_returns_empty() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"
                #[derive(clap::Parser)]
                pub struct Cli { #[arg(long)] pub x: u32 }
            "#,
        )]);
        let hits = find_runtime_surfaces(dir.path(), &BTreeSet::new()).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn finding_severity_is_high() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"
                #[derive(clap::Parser)]
                pub struct Cli { pub verbose: bool }
            "#,
        )]);
        let hits = find_runtime_surfaces(dir.path(), &syms(&["Cli"])).unwrap();
        assert_eq!(hits[0].severity, crate::finding::SeverityClass::High);
        assert_eq!(hits[0].tier, Tier::Likely);
    }
}
