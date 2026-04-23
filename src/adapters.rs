//! Framework adapters — axum, clap, actix-web, rocket.
//!
//! Maps changed Rust symbols to the *runtime surfaces* they affect —
//! things a user of the compiled binary can observe. Web handlers
//! are HTTP routes; clap derives are CLI subcommands. All fire as
//! [`FindingKind::RuntimeSurface`] so downstream consumers
//! (`impact_surface` MCP tool, severity filters, etc.) can
//! consolidate surface-impact reasoning independent of the framework.
//!
//! Current coverage
//! ----------------
//! * **axum** (`Router::new().route/.nest` method chains)
//! * **clap** (`#[derive(Parser | Subcommand | Args)]`)
//! * **actix-web** (HTTP-verb attribute macros + `.service` /
//!   `.route` / `.scope` method chains on `App`)
//! * **rocket** (HTTP-verb attribute macros + `rocket::build().mount`)
//!
//! The two web-framework adapters share a pass over top-level fns
//! looking for HTTP-verb attribute macros (`#[get]`, `#[post]`,
//! `#[put]`, `#[delete]`, `#[patch]`, `#[head]`, `#[options]`) —
//! disambiguated via `use` statements at the top of the file so
//! a `#[get("/")]` lights up as `actix-web` when `use actix_web::…`
//! is present, `rocket` when `use rocket::…` is present, and the
//! neutral `"http-handler"` family when neither is.
//!
//! What this module *doesn't* do yet (honest deferrals)
//! ----------------------------------------------------
//! * No adapter trait / plugin surface — four concrete analyzers,
//!   all inlined. Once a fifth adapter with a meaningfully different
//!   shape shows up, that's when a trait stops being premature.
//! * No warp / tauri / dioxus / leptos — axum + actix + rocket cover
//!   the dominant HTTP-handler ecosystem; the rest are on-demand.
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
        let framework = detect_web_framework(&ast);

        let mut axum_visitor = AxumVisitor {
            changed: changed_symbols,
            file: &rel,
            hits: Vec::new(),
        };
        axum_visitor.visit_file(&ast);
        findings.extend(axum_visitor.hits);

        let mut actix_visitor = ActixMethodVisitor {
            changed: changed_symbols,
            file: &rel,
            hits: Vec::new(),
            active: framework == WebFramework::Actix,
        };
        actix_visitor.visit_file(&ast);
        findings.extend(actix_visitor.hits);

        let mut rocket_visitor = RocketMethodVisitor {
            changed: changed_symbols,
            file: &rel,
            hits: Vec::new(),
            active: framework == WebFramework::Rocket,
        };
        rocket_visitor.visit_file(&ast);
        findings.extend(rocket_visitor.hits);

        // HTTP-verb attribute macros (`#[get("/")]` etc.) shared by
        // actix + rocket. Framework disambiguation via the earlier
        // import scan; when neither framework is in scope we fall
        // back to the neutral `"http-handler"` family so the finding
        // is still emitted but doesn't lie about provenance.
        for (fn_ident, verb) in http_verb_handlers(&ast) {
            let body_refs = find_changed_refs_in_fn(&ast, &fn_ident, changed_symbols);
            if body_refs.is_empty() {
                continue;
            }
            let framework_name = match framework {
                WebFramework::Actix => "actix-web",
                WebFramework::Rocket => "rocket",
                WebFramework::None => "http-handler",
            };
            let identifier = format!("{verb} `{fn_ident}`");
            let evidence = format!(
                "{framework_name} `#[{verb}]` handler `{fn_ident}` in {} references \
                 changed symbols: {}",
                rel.display(),
                body_refs.iter().cloned().collect::<Vec<_>>().join(", ")
            );
            findings.push(build_surface(framework_name, &identifier, &rel, evidence));
        }

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

// ---------------------------------------------------------------------------
// HTTP-verb attribute macros — shared between actix-web and rocket
// ---------------------------------------------------------------------------

const HTTP_VERBS: &[&str] = &["get", "post", "put", "delete", "patch", "head", "options"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebFramework {
    Actix,
    Rocket,
    None,
}

/// Scan `use` statements and any qualified paths at file scope to guess
/// which web framework this file is using. Returns `None` when the
/// answer is ambiguous (both crates imported, or neither) — callers
/// then tag HTTP-handler findings with the neutral `"http-handler"`
/// family rather than picking a side.
fn detect_web_framework(ast: &syn::File) -> WebFramework {
    let mut has_actix = false;
    let mut has_rocket = false;
    for item in &ast.items {
        if let syn::Item::Use(u) = item {
            let tokens = u.tree.to_token_stream().to_string();
            // `use actix_web::…`, `use ::actix_web::…`, `actix_web::get as get` etc.
            if tokens.contains("actix_web") {
                has_actix = true;
            }
            if tokens.contains("rocket") {
                has_rocket = true;
            }
        }
    }
    match (has_actix, has_rocket) {
        (true, false) => WebFramework::Actix,
        (false, true) => WebFramework::Rocket,
        _ => WebFramework::None,
    }
}

/// Walk top-level fns for `#[<verb>(...)]` attributes where `<verb>` is
/// an HTTP method. Returns `(fn_ident, verb)` pairs. Matches on the
/// attribute path's *last segment* so both bare `#[get(...)]` (post
/// `use actix_web::get;`) and qualified `#[actix_web::get(...)]` forms
/// resolve the same way.
fn http_verb_handlers(ast: &syn::File) -> Vec<(String, String)> {
    let mut out = Vec::new();
    walk_http_verbs(&ast.items, &mut out);
    out
}

fn walk_http_verbs(items: &[syn::Item], out: &mut Vec<(String, String)>) {
    for item in items {
        match item {
            syn::Item::Fn(f) => {
                for attr in &f.attrs {
                    let Some(last) = attr.path().segments.last() else {
                        continue;
                    };
                    let name = last.ident.to_string();
                    if HTTP_VERBS.contains(&name.as_str()) {
                        out.push((f.sig.ident.to_string(), name));
                        break; // one verb per handler is the norm
                    }
                }
            }
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    walk_http_verbs(inner, out);
                }
            }
            _ => {}
        }
    }
}

/// Find changed symbols referenced inside the named fn's body. Used
/// after `http_verb_handlers` to decide whether the handler is
/// actually affected by the diff. The existing `AxumVisitor` does
/// something similar via per-call-site token match; here we resolve
/// the function body once and filter the changed set against its
/// token stream.
fn find_changed_refs_in_fn(
    ast: &syn::File,
    fn_name: &str,
    changed: &BTreeSet<String>,
) -> BTreeSet<String> {
    fn walk(
        items: &[syn::Item],
        fn_name: &str,
        changed: &BTreeSet<String>,
    ) -> Option<BTreeSet<String>> {
        for item in items {
            match item {
                syn::Item::Fn(f) if f.sig.ident == fn_name => {
                    let body = f.block.to_token_stream().to_string();
                    // Also include the fn signature so parameter-type
                    // references count (handlers typically take
                    // extractors whose types are user-defined).
                    let sig = f.sig.to_token_stream().to_string();
                    let haystack = format!("{sig}\n{body}");
                    let hits: BTreeSet<String> = changed
                        .iter()
                        .filter(|s| token_contains_ident(&haystack, s))
                        .cloned()
                        .collect();
                    return Some(hits);
                }
                syn::Item::Mod(m) => {
                    if let Some((_, inner)) = &m.content
                        && let Some(r) = walk(inner, fn_name, changed)
                    {
                        return Some(r);
                    }
                }
                _ => {}
            }
        }
        None
    }
    walk(&ast.items, fn_name, changed).unwrap_or_default()
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
// actix-web
// ---------------------------------------------------------------------------
//
// Detects the builder-chain forms on `App`:
//   App::new()
//       .route("/path", web::get().to(handler))
//       .service(web::resource("/users").route(web::post().to(create_user)))
//       .service(web::scope("/api").service(get_users))
//
// We match on method names (`route`, `service`, `scope`) and fire when
// the call args' token stream contains a changed symbol — identical
// logic to the axum visitor, separate struct so future per-framework
// refinement (e.g. route-path extraction per framework's string-lit
// conventions) has a clean home.

struct ActixMethodVisitor<'a> {
    changed: &'a BTreeSet<String>,
    file: &'a Path,
    hits: Vec<Finding>,
    /// Only emit when the file has `use actix_web…` — else the `.route`
    /// / `.service` method names would match too many unrelated builder
    /// patterns.
    active: bool,
}

impl<'ast> Visit<'ast> for ActixMethodVisitor<'_> {
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if self.active {
            let method = call.method.to_string();
            if matches!(method.as_str(), "route" | "service" | "scope") {
                self.inspect(call, &method);
            }
        }
        syn::visit::visit_expr_method_call(self, call);
    }
}

impl ActixMethodVisitor<'_> {
    fn inspect(&mut self, call: &syn::ExprMethodCall, method: &str) {
        let tokens = call.args.to_token_stream().to_string();
        let mentions: BTreeSet<&String> = self
            .changed
            .iter()
            .filter(|sym| token_contains_ident(&tokens, sym))
            .collect();
        if mentions.is_empty() {
            return;
        }
        let route_path = call.args.first().and_then(string_literal_from);
        let identifier = match route_path {
            Some(p) => format!("{method} `{p}`"),
            None => method.to_string(),
        };
        let mentioned_list = mentions
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let evidence = format!(
            "actix-web `{identifier}` at {} references changed symbols: {mentioned_list}",
            self.file.display()
        );
        self.hits
            .push(build_surface("actix-web", &identifier, self.file, evidence));
    }
}

// ---------------------------------------------------------------------------
// rocket
// ---------------------------------------------------------------------------
//
// Detects the `rocket::build().mount("/api", routes![...])` pattern.
// The per-handler `#[get("/")]` attribute macros are covered by the
// shared HTTP-verb pass above — this visitor catches the wire-up
// call where handlers are mounted into the router.

struct RocketMethodVisitor<'a> {
    changed: &'a BTreeSet<String>,
    file: &'a Path,
    hits: Vec<Finding>,
    active: bool,
}

impl<'ast> Visit<'ast> for RocketMethodVisitor<'_> {
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if self.active && call.method == "mount" {
            self.inspect_mount(call);
        }
        syn::visit::visit_expr_method_call(self, call);
    }
}

impl RocketMethodVisitor<'_> {
    fn inspect_mount(&mut self, call: &syn::ExprMethodCall) {
        // `.mount("/path", routes![a, b, c])` — the second arg carries
        // the handler identifiers. Token-match across the whole call
        // args slice since `routes![...]` is a macro invocation whose
        // contents survive as tokens.
        let tokens = call.args.to_token_stream().to_string();
        let mentions: BTreeSet<&String> = self
            .changed
            .iter()
            .filter(|sym| token_contains_ident(&tokens, sym))
            .collect();
        if mentions.is_empty() {
            return;
        }
        let route_path = call.args.first().and_then(string_literal_from);
        let identifier = match route_path {
            Some(p) => format!("mount `{p}`"),
            None => "mount".to_string(),
        };
        let mentioned_list = mentions
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let evidence = format!(
            "rocket `{identifier}` at {} references changed symbols: {mentioned_list}",
            self.file.display()
        );
        self.hits
            .push(build_surface("rocket", &identifier, self.file, evidence));
    }
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
                if let Some(kind) = clap_surface_kind(&derives)
                    && (changed.contains(&s.ident.to_string())
                        || any_field_mentions_changed(&s.fields, changed))
                {
                    out.push((s.ident.to_string(), kind));
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

    // --- framework disambiguation ---

    #[test]
    fn framework_detected_via_use_statement() {
        let ast: syn::File = syn::parse_str("use actix_web::{App, web}; fn main() {}").unwrap();
        assert_eq!(detect_web_framework(&ast), WebFramework::Actix);

        let ast: syn::File = syn::parse_str("use rocket::{get, routes}; fn main() {}").unwrap();
        assert_eq!(detect_web_framework(&ast), WebFramework::Rocket);

        let ast: syn::File = syn::parse_str("fn main() {}").unwrap();
        assert_eq!(detect_web_framework(&ast), WebFramework::None);

        // Ambiguous — both imports → None, so the HTTP-verb finding
        // gets tagged `http-handler` rather than lying about provenance.
        let ast: syn::File =
            syn::parse_str("use actix_web::get; use rocket::post; fn main() {}").unwrap();
        assert_eq!(detect_web_framework(&ast), WebFramework::None);
    }

    // --- actix-web ---

    #[test]
    fn actix_http_verb_handler_with_changed_body_fires() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"
                use actix_web::get;

                #[get("/user/{id}")]
                async fn show_user() -> String {
                    let u = load_user();
                    u
                }
                fn load_user() -> String { String::new() }
            "#,
        )]);
        let hits = find_runtime_surfaces(dir.path(), &syms(&["load_user"])).unwrap();
        let pairs: Vec<_> = hits
            .iter()
            .filter_map(|f| match &f.kind {
                FindingKind::RuntimeSurface {
                    framework,
                    identifier,
                    ..
                } => Some((framework.clone(), identifier.clone())),
                _ => None,
            })
            .collect();
        assert!(
            pairs.iter().any(|(fw, id)| fw == "actix-web"
                && id.contains("get")
                && id.contains("show_user")),
            "expected actix-web get handler; got {pairs:?}"
        );
    }

    #[test]
    fn actix_service_chain_with_changed_handler_fires() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"
                use actix_web::{App, web};

                fn build() -> App<()> {
                    App::new().service(web::resource("/api/v1/users").to(list_users))
                }

                async fn list_users() -> &'static str { "hi" }
            "#,
        )]);
        let hits = find_runtime_surfaces(dir.path(), &syms(&["list_users"])).unwrap();
        assert!(
            hits.iter().any(|f| matches!(
                &f.kind,
                FindingKind::RuntimeSurface { framework, .. } if framework == "actix-web"
            )),
            "expected actix-web service finding; got {hits:?}"
        );
    }

    #[test]
    fn actix_handler_without_changed_body_refs_is_ignored() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"
                use actix_web::get;

                #[get("/health")]
                async fn health() -> &'static str { "ok" }
            "#,
        )]);
        // `health` handler touches nothing that's in the changed set.
        let hits = find_runtime_surfaces(dir.path(), &syms(&["load_user"])).unwrap();
        assert!(
            hits.is_empty(),
            "unaffected handler must stay quiet; got {hits:?}"
        );
    }

    // --- rocket ---

    #[test]
    fn rocket_http_verb_handler_with_changed_body_fires() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"
                use rocket::get;

                #[get("/user/<id>")]
                fn show_user(id: u32) -> String {
                    render_user(id)
                }
                fn render_user(id: u32) -> String { id.to_string() }
            "#,
        )]);
        let hits = find_runtime_surfaces(dir.path(), &syms(&["render_user"])).unwrap();
        assert!(
            hits.iter().any(|f| matches!(
                &f.kind,
                FindingKind::RuntimeSurface { framework, .. } if framework == "rocket"
            )),
            "expected rocket get handler; got {hits:?}"
        );
    }

    #[test]
    fn rocket_mount_routes_macro_fires() {
        let dir = setup(&[(
            "src/lib.rs",
            r#"
                use rocket::{build, routes};

                fn launch() {
                    rocket::build().mount("/api", routes![show_user, list_users]);
                }
                fn show_user() {}
                fn list_users() {}
            "#,
        )]);
        let hits = find_runtime_surfaces(dir.path(), &syms(&["list_users"])).unwrap();
        assert!(
            hits.iter().any(|f| matches!(
                &f.kind,
                FindingKind::RuntimeSurface { framework, identifier, .. }
                    if framework == "rocket" && identifier.contains("/api")
            )),
            "expected rocket mount finding for /api; got {hits:?}"
        );
    }

    // --- http-handler fallback ---

    #[test]
    fn http_verb_without_framework_import_uses_neutral_family() {
        // Neither actix nor rocket in scope — attribute macro is still
        // recognizable, but we don't claim provenance.
        let dir = setup(&[(
            "src/lib.rs",
            r#"
                #[get("/user")]
                fn show_user() -> String { load_user() }
                fn load_user() -> String { String::new() }
            "#,
        )]);
        let hits = find_runtime_surfaces(dir.path(), &syms(&["load_user"])).unwrap();
        assert!(
            hits.iter().any(|f| matches!(
                &f.kind,
                FindingKind::RuntimeSurface { framework, .. } if framework == "http-handler"
            )),
            "expected neutral http-handler framework tag; got {hits:?}"
        );
    }
}
