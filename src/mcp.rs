//! Model Context Protocol (MCP) server.
//!
//! Exposes cargo-impact's analyzer over stdio so AI agents can invoke it
//! as a first-class tool instead of parsing CLI output. Started via
//! `cargo impact mcp` (dispatched by `main.rs` before clap runs against
//! the analysis flags).
//!
//! Protocol
//! --------
//! MCP is JSON-RPC 2.0 over stdio with newline-delimited messages. This
//! implementation is deliberately hand-rolled — the protocol surface we
//! need is small and adding a binding crate (`rmcp`, `rust-mcp-sdk`, …)
//! would pull a transitive dep graph larger than the feature itself.
//!
//! Methods implemented
//! -------------------
//! * `initialize` — handshake; advertises the `tools` capability.
//! * `initialized` — one-way notification; we ack silently.
//! * `tools/list` — returns the three tools below.
//! * `tools/call` — dispatches to the named tool.
//! * `shutdown` / `exit` — graceful termination.
//!
//! Tools exposed (all six from README §8)
//! --------------------------------------
//! * `impact_analyze` — run the full blast-radius analysis. Accepts the
//!   common args (`since`, `confidence_min`, `features`, `all_features`,
//!   `no_default_features`, `semver_checks`, `rust_analyzer`,
//!   `manifest_dir`). Returns the same JSON envelope the CLI emits under
//!   `--format json`.
//! * `impact_test_filter` — shortcut for the `cargo-nextest` filter
//!   expression. Same input args, returns the filter string.
//! * `impact_surface` — project the report to runtime surface findings
//!   (FFI signatures, `build.rs` changes, trait impls, derive impls)
//!   with the full `impact_analyze` JSON shape minus non-surface kinds.
//! * `impact_semver` — project the report to `cargo-semver-checks`
//!   findings. Forces `semver_checks = true` so agents always get an
//!   answer even if the caller didn't pre-configure it.
//! * `impact_explain` — given a finding ID (content-hashed and stable
//!   across runs), re-run `analyze()` and return the matching finding's
//!   full detail. Lets agents drill into a specific signal without
//!   re-emitting the entire report.
//! * `impact_version` — smoke-test tool that returns the crate version.
//!   Agents call this first to verify the server is alive.

use crate::{analyze, render_report, AnalysisReport, Format, ImpactArgs};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

const PROTOCOL_VERSION: &str = "2024-11-05";

pub fn serve() -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let reader = stdin.lock();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg): std::result::Result<Value, _> = serde_json::from_str(&line) else {
            write_error(&mut stdout, Value::Null, -32700, "parse error")?;
            continue;
        };
        handle_message(&msg, &mut stdout)?;
    }
    Ok(())
}

fn handle_message(msg: &Value, out: &mut impl Write) -> Result<()> {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(json!({}));

    // Notifications (no `id` field) do not get a response per JSON-RPC 2.0.
    let is_notification = msg.get("id").is_none();

    match method {
        "initialize" => write_result(out, id, initialize_result()),
        "initialized" | "notifications/initialized" => Ok(()),
        "tools/list" => write_result(out, id, tools_list_result()),
        "tools/call" => match call_tool(&params) {
            Ok(value) => write_result(out, id, value),
            Err(err) => write_error(out, id, -32000, &format!("{err:#}")),
        },
        "shutdown" => {
            write_result(out, id, Value::Null)?;
            Ok(())
        }
        "exit" => {
            std::process::exit(0);
        }
        _ if is_notification => Ok(()),
        _ => write_error(out, id, -32601, &format!("method not found: {method}")),
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": "cargo-impact",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

fn tools_list_result() -> Value {
    json!({
        "tools": [
            {
                "name": "impact_analyze",
                "description":
                    "Run cargo-impact's blast-radius analysis on the current Rust \
                     workspace and return a JSON report of findings (changed files, \
                     candidate symbols, severity/tier-classified findings with \
                     evidence and suggested actions).",
                "inputSchema": input_schema_analyze()
            },
            {
                "name": "impact_test_filter",
                "description":
                    "Produce a cargo-nextest filter expression (`test(a) + test(b)`) \
                     covering only the tests that reference changed symbols. Empty \
                     when nothing would be affected.",
                "inputSchema": input_schema_analyze()
            },
            {
                "name": "impact_surface",
                "description":
                    "Project the blast radius to runtime-surface findings only: FFI \
                     signature changes, build.rs changes, hand-written trait impls, \
                     and derive-macro impls. Useful when an agent wants to reason \
                     about what ships to downstream consumers, not about internal \
                     test coverage.",
                "inputSchema": input_schema_analyze()
            },
            {
                "name": "impact_semver",
                "description":
                    "Run cargo-semver-checks (forcing it on regardless of whether \
                     the caller passed `semver_checks`) and return the resulting \
                     findings. Requires cargo-semver-checks on PATH; returns an \
                     empty findings list with a stderr note if missing.",
                "inputSchema": input_schema_analyze()
            },
            {
                "name": "impact_explain",
                "description":
                    "Look up a single finding by its content-hashed ID (as emitted \
                     by `impact_analyze`) and return its full detail — kind payload, \
                     evidence, suggested action, severity, tier, confidence. IDs are \
                     stable across runs, so agents can store the ID from one call \
                     and round-trip it in a later call.",
                "inputSchema": json!({
                    "type": "object",
                    "required": ["finding_id"],
                    "properties": {
                        "finding_id": {
                            "type": "string",
                            "description": "Content-hashed finding ID like `f-abcd1234...`."
                        },
                        "since": { "type": "string" },
                        "features": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "all_features": { "type": "boolean" },
                        "no_default_features": { "type": "boolean" },
                        "semver_checks": { "type": "boolean" },
                        "rust_analyzer": { "type": "boolean" },
                        "manifest_dir": { "type": "string" }
                    }
                })
            },
            {
                "name": "impact_version",
                "description": "Return the cargo-impact crate version. Useful as a \
                                connection smoke-test.",
                "inputSchema": json!({ "type": "object", "properties": {} })
            }
        ]
    })
}

fn input_schema_analyze() -> Value {
    json!({
        "type": "object",
        "properties": {
            "since": {
                "type": "string",
                "description": "Git ref to diff against (default HEAD)."
            },
            "confidence_min": {
                "type": "number",
                "minimum": 0,
                "maximum": 1,
                "description": "Drop findings whose confidence is below this threshold."
            },
            "features": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Active Cargo features for cfg evaluation."
            },
            "all_features": {
                "type": "boolean",
                "description": "Activate every feature declared in the manifest."
            },
            "no_default_features": {
                "type": "boolean",
                "description": "Skip the manifest's `default` feature list."
            },
            "semver_checks": {
                "type": "boolean",
                "description": "Run cargo-semver-checks (requires tool on PATH)."
            },
            "rust_analyzer": {
                "type": "boolean",
                "description": "Opt in to rust-analyzer-backed Proven-tier \
                                findings (stub in v0.3-alpha)."
            },
            "manifest_dir": {
                "type": "string",
                "description": "Override the workspace root; defaults to cwd."
            }
        }
    })
}

/// Parameters agents send to the analyze-like tools. Every field is
/// optional so a minimal call — `{"name": "impact_analyze", "arguments": {}}`
/// — runs with full defaults.
#[derive(Debug, Default, Deserialize, Serialize)]
struct AnalyzeArgs {
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    confidence_min: Option<f64>,
    #[serde(default)]
    features: Option<Vec<String>>,
    #[serde(default)]
    all_features: Option<bool>,
    #[serde(default)]
    no_default_features: Option<bool>,
    #[serde(default)]
    semver_checks: Option<bool>,
    #[serde(default)]
    rust_analyzer: Option<bool>,
    #[serde(default)]
    manifest_dir: Option<String>,
}

impl AnalyzeArgs {
    fn into_impact_args(self) -> ImpactArgs {
        ImpactArgs {
            test: false,
            format: Format::Json,
            since: self.since.unwrap_or_else(|| "HEAD".to_string()),
            manifest_dir: self.manifest_dir.map(std::path::PathBuf::from),
            confidence_min: self.confidence_min.unwrap_or(0.0),
            fail_on: None,
            semver_checks: self.semver_checks.unwrap_or(false),
            rust_analyzer: self.rust_analyzer.unwrap_or(false),
            features: self.features.unwrap_or_default(),
            all_features: self.all_features.unwrap_or(false),
            no_default_features: self.no_default_features.unwrap_or(false),
        }
    }
}

fn call_tool(params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing tool name"))?;
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    match name {
        "impact_version" => Ok(text_content(env!("CARGO_PKG_VERSION"))),
        "impact_analyze" => {
            let args: AnalyzeArgs = serde_json::from_value(arguments)?;
            let impact_args = args.into_impact_args();
            let report = analyze(&impact_args)?;
            Ok(text_content(&render_json_report(&impact_args, &report)?))
        }
        "impact_test_filter" => {
            let args: AnalyzeArgs = serde_json::from_value(arguments)?;
            let impact_args = args.into_impact_args();
            let report = analyze(&impact_args)?;
            let filter = crate::nextest_filter(&report.findings);
            Ok(text_content(&filter))
        }
        "impact_surface" => {
            let args: AnalyzeArgs = serde_json::from_value(arguments)?;
            let impact_args = args.into_impact_args();
            let mut report = analyze(&impact_args)?;
            report.findings.retain(|f| {
                matches!(
                    f.kind.tag(),
                    "ffi_signature_change"
                        | "build_script_changed"
                        | "trait_impl"
                        | "derived_trait_impl"
                )
            });
            Ok(text_content(&render_json_report(&impact_args, &report)?))
        }
        "impact_semver" => {
            let args: AnalyzeArgs = serde_json::from_value(arguments)?;
            let mut impact_args = args.into_impact_args();
            // Force-enable so agents always get a semver answer from this
            // tool, even if the call didn't explicitly set it.
            impact_args.semver_checks = true;
            let mut report = analyze(&impact_args)?;
            report.findings.retain(|f| f.kind.tag() == "semver_check");
            Ok(text_content(&render_json_report(&impact_args, &report)?))
        }
        "impact_explain" => {
            let finding_id = arguments
                .get("finding_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing finding_id"))?
                .to_string();
            let explain_args: AnalyzeArgs = serde_json::from_value(arguments)?;
            let impact_args = explain_args.into_impact_args();
            let report = analyze(&impact_args)?;
            match report.findings.into_iter().find(|f| f.id == finding_id) {
                Some(f) => Ok(text_content(&serde_json::to_string_pretty(&f)?)),
                None => anyhow::bail!(
                    "finding `{finding_id}` not present in current report. IDs are \
                     content-hashed and stable across runs, so absence here means \
                     the underlying code change no longer produces this finding."
                ),
            }
        }
        other => anyhow::bail!("unknown tool: {other}"),
    }
}

fn render_json_report(args: &ImpactArgs, report: &AnalysisReport) -> Result<String> {
    render_report(
        args.format,
        &report.changed_files,
        &report.candidate_symbols,
        &report.findings,
    )
}

fn text_content(body: &str) -> Value {
    json!({
        "content": [
            { "type": "text", "text": body }
        ]
    })
}

fn write_result(out: &mut impl Write, id: Value, result: Value) -> Result<()> {
    let env = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    });
    writeln!(out, "{env}")?;
    out.flush()?;
    Ok(())
}

fn write_error(out: &mut impl Write, id: Value, code: i32, message: &str) -> Result<()> {
    let env = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    });
    writeln!(out, "{env}")?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_one(input: Value) -> Value {
        let mut out: Vec<u8> = Vec::new();
        handle_message(&input, &mut out).expect("handle_message");
        let s = String::from_utf8(out).expect("utf8");
        // One response per call — split to the first non-empty line.
        let line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        serde_json::from_str(line).expect("parse response")
    }

    #[test]
    fn initialize_advertises_tools_capability() {
        let resp = run_one(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        }));
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert!(resp["result"]["capabilities"]["tools"].is_object());
        assert_eq!(resp["result"]["serverInfo"]["name"], "cargo-impact");
    }

    #[test]
    fn tools_list_returns_all_six_tools() {
        let resp = run_one(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        }));
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 6);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in [
            "impact_analyze",
            "impact_test_filter",
            "impact_surface",
            "impact_semver",
            "impact_explain",
            "impact_version",
        ] {
            assert!(
                names.contains(&expected),
                "tools/list missing `{expected}`; got {names:?}"
            );
        }
    }

    #[test]
    fn impact_explain_rejects_missing_finding_id() {
        let resp = run_one(json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "impact_explain", "arguments": {} }
        }));
        let msg = resp["error"]["message"].as_str().unwrap();
        assert!(
            msg.contains("finding_id"),
            "expected missing-id error; got: {msg:?}"
        );
    }

    #[test]
    fn impact_version_tool_returns_crate_version() {
        let resp = run_one(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "impact_version", "arguments": {} }
        }));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn unknown_method_returns_method_not_found_error() {
        let resp = run_one(json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "totally_fake"
        }));
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn unknown_tool_returns_internal_error() {
        let resp = run_one(json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": { "name": "bogus", "arguments": {} }
        }));
        assert!(resp["error"]["message"].as_str().unwrap().contains("bogus"));
    }

    #[test]
    fn analyze_args_defaults_populate_impact_args_sensibly() {
        let args = AnalyzeArgs::default().into_impact_args();
        assert_eq!(args.since, "HEAD");
        assert!(!args.semver_checks);
        assert!(!args.rust_analyzer);
        assert!(matches!(args.format, Format::Json));
    }

    #[test]
    fn notifications_without_id_produce_no_response() {
        let mut out: Vec<u8> = Vec::new();
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        handle_message(&notification, &mut out).expect("handle");
        assert!(
            out.is_empty(),
            "notifications must not elicit a response; got {out:?}"
        );
    }
}
