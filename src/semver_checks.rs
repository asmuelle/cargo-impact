//! Orchestrates [`cargo-semver-checks`](https://github.com/obi1kenobi/cargo-semver-checks)
//! for public-API breakage detection.
//!
//! Opt-in via the `--semver-checks` CLI flag. The tool builds rustdoc JSON
//! twice (baseline + current) so invocations typically take 10-30 seconds
//! on non-trivial crates — paying that cost on every run would hurt the
//! sub-second interactive target from README §9.
//!
//! Behavior matrix
//! ---------------
//! | State                             | Outcome                              |
//! | --------------------------------- | ------------------------------------ |
//! | flag not set                      | skip silently                        |
//! | flag set, tool not on PATH        | warn once on stderr, skip            |
//! | flag set, tool runs, exit 0       | no finding (clean, no noise)         |
//! | flag set, tool runs, exit != 0    | one `SemverCheck` finding, HIGH tier |
//! | flag set, tool runs, spawn error  | warn, skip (best-effort)             |
//!
//! Parsing is deliberately coarse: we capture the tool's stderr verbatim in
//! the `details` field of a single finding rather than attempting to parse
//! cargo-semver-checks's internal report shape. The JSON output format of
//! cargo-semver-checks is still evolving, and a robust v0.3 pass that
//! emits one finding per lint violation is better done when the schema is
//! stable than on guesswork today.

use crate::finding::{Finding, FindingKind, Tier};
use anyhow::{Context, Result};
use std::path::Path;
use std::process::{Command, Output};

/// Return the sub-command name used to invoke cargo-semver-checks. Cargo
/// auto-discovers `cargo-<NAME>` binaries on `PATH`, so this is what we look
/// up when detecting availability.
const TOOL_BIN: &str = "cargo-semver-checks";

/// Run cargo-semver-checks against the workspace rooted at `root`, comparing
/// the working-tree state to `since`. Returns any emitted findings.
///
/// `enabled` gates the whole thing — callers pass in `args.semver_checks`.
pub fn run(root: &Path, since: &str, enabled: bool) -> Result<Vec<Finding>> {
    if !enabled {
        return Ok(Vec::new());
    }
    if !is_installed() {
        eprintln!(
            "cargo-impact: --semver-checks requested but `{TOOL_BIN}` not found on PATH. \
             Install it with `cargo install cargo-semver-checks`; skipping."
        );
        return Ok(Vec::new());
    }

    let output = match invoke(root, since) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("cargo-impact: cargo-semver-checks failed to start: {e:#}");
            return Ok(Vec::new());
        }
    };

    Ok(parse_output(output.status.success(), &combined(&output)))
}

/// Classify the outcome of a cargo-semver-checks invocation into findings.
/// Extracted as a pure function so unit tests don't need to spawn the real
/// binary — tests construct `(success, combined_output)` pairs and assert
/// the resulting findings.
pub fn parse_output(success: bool, combined: &str) -> Vec<Finding> {
    if success {
        return Vec::new();
    }
    let details = combined.trim().to_string();
    let evidence = first_failing_lint(&details).map_or_else(
        || "cargo-semver-checks reports breaking public-API changes".to_string(),
        |lint| format!("cargo-semver-checks: {lint}"),
    );
    let kind = FindingKind::SemverCheck {
        level: "breaking".to_string(),
        details,
    };
    vec![
        Finding::new("", Tier::Likely, 0.95, kind, evidence).with_suggested_action(
            "cargo semver-checks check-release  # for full detail".to_string(),
        ),
    ]
}

/// Check whether `cargo-semver-checks` is installed. Uses the bin name cargo
/// would discover — we don't need to run it, just find it on PATH.
pub fn is_installed() -> bool {
    which(TOOL_BIN).is_some()
}

/// Minimal `which` clone: search `$PATH` for `name`, returning the first
/// executable match. Avoids pulling in a dep for a 10-line utility.
fn which(name: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        // Windows falls back to extensions listed in PATHEXT.
        #[cfg(windows)]
        if let Some(pathext) = std::env::var_os("PATHEXT") {
            for ext in std::env::split_paths(&pathext) {
                let with_ext =
                    candidate.with_extension(ext.to_string_lossy().trim_start_matches('.'));
                if with_ext.is_file() {
                    return Some(with_ext);
                }
            }
        }
    }
    None
}

fn invoke(root: &Path, since: &str) -> Result<Output> {
    Command::new("cargo")
        .arg("semver-checks")
        .arg("check-release")
        .arg("--baseline-rev")
        .arg(since)
        .current_dir(root)
        .output()
        .context("spawning cargo semver-checks")
}

/// Concatenate stdout + stderr for presentation. cargo-semver-checks prints
/// the detailed lint report to stderr in its default text mode, so we must
/// capture both to give the user the full picture.
fn combined(output: &Output) -> String {
    let mut s = String::new();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.is_empty() {
        s.push_str(&stdout);
    }
    if !stdout.is_empty() && !stderr.is_empty() {
        s.push('\n');
    }
    if !stderr.is_empty() {
        s.push_str(&stderr);
    }
    s
}

/// Extract the first "FAIL " line from combined output, if any. cargo-semver-
/// checks prefixes each breaking lint with "FAIL" in its text report; giving
/// the user the first one as the evidence summary is much more actionable
/// than a generic "breaking changes detected".
fn first_failing_lint(combined: &str) -> Option<String> {
    for line in combined.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("FAIL ") {
            return Some(rest.trim().to_string());
        }
        // cargo-semver-checks 0.30+ uses a different prefix; tolerate both.
        if let Some(rest) = trimmed.strip_prefix("--- failure ") {
            return Some(rest.trim_end_matches(": ---").trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::SeverityClass;

    #[test]
    fn success_produces_no_findings() {
        let findings = parse_output(true, "whatever was on stdout");
        assert!(findings.is_empty());
    }

    #[test]
    fn failure_yields_single_high_finding() {
        let findings = parse_output(false, "some scary looking cargo output");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, SeverityClass::High);
        assert_eq!(findings[0].tier, Tier::Likely);
        assert_eq!(findings[0].confidence, 0.95);
        match &findings[0].kind {
            FindingKind::SemverCheck { level, details } => {
                assert_eq!(level, "breaking");
                assert!(details.contains("cargo output"));
            }
            other => panic!("expected SemverCheck, got {other:?}"),
        }
    }

    #[test]
    fn first_failing_lint_extracted_as_evidence_when_present() {
        let output = "\
Building baseline … done
FAIL function_parameter_count_changed: `foo` now takes 3 params (was 2)
        at src/lib.rs:42
Other status noise
";
        let findings = parse_output(false, output);
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0]
                .evidence
                .contains("function_parameter_count_changed"),
            "evidence should surface the first failing lint; got {:?}",
            findings[0].evidence
        );
    }

    #[test]
    fn handles_alternative_failure_prefix_format() {
        let output = "--- failure enum_variant_added: ---\n body details";
        let findings = parse_output(false, output);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].evidence.contains("enum_variant_added"));
    }

    #[test]
    fn suggested_action_points_users_at_raw_tool() {
        let findings = parse_output(false, "FAIL anything");
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0]
                .suggested_action
                .as_deref()
                .is_some_and(|s| s.contains("cargo semver-checks"))
        );
    }

    #[test]
    fn run_returns_empty_when_disabled_regardless_of_tool_presence() {
        let findings = run(Path::new("."), "HEAD", false).unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn which_finds_a_ubiquitous_binary() {
        // `git` is always on PATH in our CI matrix (checkout action provides it)
        // and locally for dev. If this ever flakes, the test environment
        // itself is broken in a way worth noticing.
        assert!(which("git").is_some());
        assert!(which("this-binary-does-not-exist-i-promise-xyz").is_none());
    }
}
