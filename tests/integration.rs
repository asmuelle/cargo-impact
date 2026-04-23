//! End-to-end integration test: exercises the full `cargo-impact` binary
//! against a seeded git fixture, parses the JSON output, and asserts the
//! expected findings flow through from git diff to structured report.
//!
//! Runs the *release* binary that cargo stamps into
//! `CARGO_BIN_EXE_cargo-impact` for integration tests (see the
//! [cargo book](https://doc.rust-lang.org/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-crates)).
//! This catches regressions that per-module unit tests can't — argv
//! stripping, feature resolution, orchestrator ordering, JSON envelope
//! stability, and the overall exit-code contract.

use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

/// Location of the built binary. Cargo sets this env var at compile time
/// for every integration test in this crate.
fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_cargo-impact")
}

/// Run `git` in `dir` and panic if it fails — integration-test hygiene.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

/// Seed a single-crate git repo with an initial commit, then overwrite
/// files per `modifications` so the working tree holds the "after" state.
/// Returns the temp dir so callers can run `cargo-impact` against it.
fn seed_repo(initial: &[(&str, &str)], modifications: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "t@t"]);
    git(root, &["config", "user.name", "t"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    // Windows defaults `core.autocrlf = true`, which mutates the index
    // version of committed files and can make our diff-vs-WT comparison
    // observe phantom differences (or miss real ones) on that platform.
    // Hold line endings verbatim across init/add/commit.
    git(root, &["config", "core.autocrlf", "false"]);
    // `-B` (create-or-reset) instead of `-b`: handles the case where
    // `git init`'s default branch is already `main` (git 2.28+ with
    // `init.defaultBranch = main`, common on CI runners).
    git(root, &["checkout", "-q", "-B", "main"]);

    for (rel, body) in initial {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, body).unwrap();
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "init"]);

    for (rel, body) in modifications {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, body).unwrap();
    }

    dir
}

/// Run `cargo-impact` against a fixture root, capturing stdout. Returns
/// (stdout, exit_code). Never panics on non-zero — tests assert
/// explicitly. On failure the stderr is eprintln'd so it surfaces in
/// nextest's per-test output when the caller's assertion fires
/// (especially useful for Windows CI where we can't pull per-test
/// logs without repo-admin auth).
fn run_impact(root: &Path, extra_args: &[&str]) -> (String, i32) {
    let mut cmd = Command::new(binary());
    cmd.arg("--manifest-dir").arg(root);
    for a in extra_args {
        cmd.arg(a);
    }
    let output = cmd.output().expect("spawn cargo-impact");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let code = output.status.code().unwrap_or(-1);
    // Exit 0 = clean, 1 = --fail-on tripped (expected by one test). Any
    // other code means cargo-impact itself blew up — surface its stderr.
    if code != 0 && code != 1 {
        eprintln!(
            "cargo-impact exited with code {code}\n\
             args: --manifest-dir <tmp> {}\n\
             stderr:\n{stderr}",
            extra_args.join(" ")
        );
    }
    (stdout, code)
}

fn manifest() -> &'static str {
    "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n"
}

// ---------------------------------------------------------------------------

#[test]
fn clean_workspace_reports_no_findings_and_exits_zero() {
    let body = "pub fn untouched() {}\n";
    let dir = seed_repo(
        &[("Cargo.toml", manifest()), ("src/lib.rs", body)],
        // No modifications = clean working tree.
        &[],
    );
    let (stdout, code) = run_impact(dir.path(), &["--format", "json"]);
    assert_eq!(code, 0, "clean workspace exit code; stdout:\n{stdout}");

    let report: Value = serde_json::from_str(&stdout).expect("parse JSON");
    assert_eq!(report["summary"]["total"], 0);
    assert!(report["findings"].as_array().unwrap().is_empty());
}

#[test]
fn trait_signature_change_emits_high_severity_findings() {
    let dir = seed_repo(
        &[
            ("Cargo.toml", manifest()),
            (
                "src/lib.rs",
                // One trait, one impl, one test that references the impl.
                "pub trait Greeter { fn hi(&self) -> u32; }\n\
                 pub struct Friend;\n\
                 impl Greeter for Friend { fn hi(&self) -> u32 { 1 } }\n\
                 \n\
                 #[cfg(test)]\n\
                 mod tests {\n\
                 use super::*;\n\
                 #[test] fn greets() { let _ = Friend.hi(); }\n\
                 }\n",
            ),
        ],
        // Modify the trait's required-method signature (return type flip).
        &[(
            "src/lib.rs",
            "pub trait Greeter { fn hi(&self) -> String; }\n\
             pub struct Friend;\n\
             impl Greeter for Friend { fn hi(&self) -> String { String::new() } }\n\
             \n\
             #[cfg(test)]\n\
             mod tests {\n\
             use super::*;\n\
             #[test] fn greets() { let _ = Friend.hi(); }\n\
             }\n",
        )],
    );
    let (stdout, code) = run_impact(dir.path(), &["--format", "json"]);
    assert_eq!(code, 0, "no --fail-on; stdout:\n{stdout}");

    let report: Value = serde_json::from_str(&stdout).expect("parse JSON");
    let findings = report["findings"].as_array().expect("findings array");
    assert!(
        !findings.is_empty(),
        "expected findings for a trait signature change; got none"
    );

    let kinds: Vec<&str> = findings.iter().filter_map(|f| f["kind"].as_str()).collect();
    assert!(
        kinds.contains(&"trait_definition_change"),
        "expected trait_definition_change finding; kinds = {kinds:?}"
    );
    assert!(
        kinds.contains(&"trait_impl"),
        "expected trait_impl finding; kinds = {kinds:?}"
    );
    // Severity bucket for a required-method sig change is High.
    assert!(report["summary"]["by_severity"]["high"].as_u64().unwrap() >= 1);
}

#[test]
fn fail_on_high_exits_nonzero_when_high_severity_finding_present() {
    let dir = seed_repo(
        &[
            ("Cargo.toml", manifest()),
            (
                "src/lib.rs",
                "pub trait Greeter { fn hi(&self); }\n\
                 pub struct F;\n\
                 impl Greeter for F { fn hi(&self) {} }\n",
            ),
        ],
        &[(
            "src/lib.rs",
            // Required-method sig change → High / Likely.
            "pub trait Greeter { fn hi(&self, n: u32); }\n\
             pub struct F;\n\
             impl Greeter for F { fn hi(&self, n: u32) { let _ = n; } }\n",
        )],
    );
    let (_stdout, code) = run_impact(dir.path(), &["--format", "json", "--fail-on", "high"]);
    assert_eq!(code, 1, "--fail-on high should trip on trait sig change");
}

#[test]
fn derive_of_changed_trait_is_flagged() {
    let dir = seed_repo(
        &[
            ("Cargo.toml", manifest()),
            (
                "src/lib.rs",
                "pub trait Bundle {}\n\
                 pub struct User;\n",
            ),
        ],
        &[(
            "src/lib.rs",
            // Trait gains a method (required, since no default) AND a
            // user-defined struct acquires `#[derive(Bundle)]`.
            "pub trait Bundle { fn count(&self) -> u32; }\n\
             #[derive(Bundle)]\n\
             pub struct User;\n",
        )],
    );
    let (stdout, _code) = run_impact(dir.path(), &["--format", "json"]);
    let report: Value = serde_json::from_str(&stdout).expect("parse JSON");
    let findings = report["findings"].as_array().unwrap();
    let kinds: Vec<&str> = findings.iter().filter_map(|f| f["kind"].as_str()).collect();
    assert!(
        kinds.contains(&"derived_trait_impl"),
        "expected derived_trait_impl finding; kinds = {kinds:?}"
    );
}

#[test]
fn test_flag_emits_nextest_filter_expression() {
    let dir = seed_repo(
        &[
            ("Cargo.toml", manifest()),
            (
                "src/lib.rs",
                "pub fn engine() -> u32 { 0 }\n\
                 #[cfg(test)] mod tests {\n\
                 use super::*;\n\
                 #[test] fn uses_engine() { let _ = engine(); }\n\
                 }\n",
            ),
        ],
        &[(
            "src/lib.rs",
            "pub fn engine() -> u32 { 1 }\n\
             #[cfg(test)] mod tests {\n\
             use super::*;\n\
             #[test] fn uses_engine() { let _ = engine(); }\n\
             }\n",
        )],
    );
    let (stdout, code) = run_impact(dir.path(), &["--test"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("test(uses_engine)"),
        "expected nextest filter to include the affected test; got {stdout:?}"
    );
}

#[test]
fn mcp_version_tool_responds_to_tools_call_over_stdio() {
    // Spawn the MCP server and pipe a single `tools/call impact_version`
    // request. Small JSON-RPC smoke test that proves the subcommand
    // dispatch + protocol handler + tool invocation all work end to end.
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new(binary())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcp server");

    let stdin = child.stdin.as_mut().expect("stdin handle");
    let req = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"impact_version","arguments":{}}}"#;
    writeln!(stdin, "{req}").expect("write");
    // Closing stdin signals EOF so the server's line loop terminates.
    drop(child.stdin.take());

    let out = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "mcp server exited non-zero ({:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status.code()
    );

    // Response is one JSON line; parse it.
    let line = stdout
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_else(|| {
            panic!("no response on mcp stdout\nstdout:\n{stdout}\nstderr:\n{stderr}")
        });
    let resp: Value = serde_json::from_str(line)
        .unwrap_or_else(|e| panic!("parse response `{line}`: {e}\nstderr:\n{stderr}"));
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        !text.is_empty() && text.chars().any(|c| c.is_ascii_digit()),
        "expected a version string, got {text:?}"
    );
}

#[test]
fn json_output_schema_is_stable() {
    // Agents and CI scripts consume this shape — treat it as a contract.
    // If these fields ever need to change, update the schema document in
    // README §8 first, then update this assertion.
    let dir = seed_repo(
        &[
            ("Cargo.toml", manifest()),
            ("src/lib.rs", "pub fn a() {}\n"),
        ],
        &[("src/lib.rs", "pub fn a() { let _ = 1; }\n")],
    );
    let (stdout, _code) = run_impact(dir.path(), &["--format", "json"]);
    let report: Value = serde_json::from_str(&stdout).expect("parse JSON");

    for field in [
        "version",
        "changed_files",
        "candidate_symbols",
        "findings",
        "summary",
    ] {
        assert!(
            !report[field].is_null(),
            "JSON envelope missing required field `{field}`; got:\n{stdout}"
        );
    }
    for field in ["total", "by_severity", "by_tier"] {
        assert!(
            !report["summary"][field].is_null(),
            "summary missing `{field}`"
        );
    }
}
