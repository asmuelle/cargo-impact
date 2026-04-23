//! rust-analyzer integration scaffolding.
//!
//! The README §11 v0.3 milestone is "resolved call-graph analysis so
//! findings can legitimately reach the `Proven` tier." That requires
//! real name resolution — which rust-analyzer already computes for every
//! IDE feature it ships. Our plan is to spawn `rust-analyzer` as an LSP
//! subprocess, feed it `textDocument/references` requests for each
//! changed symbol, and turn the responses into `Proven`-tier findings.
//!
//! What this module *currently* does
//! ---------------------------------
//! * Exposes a `--rust-analyzer` opt-in flag (wired in `lib.rs`).
//! * Detects whether `rust-analyzer` is on `PATH`.
//! * When the flag is set and the tool is missing, warns once on stderr
//!   and returns an empty finding list (matches the `semver-checks`
//!   opt-in pattern — non-fatal, no noise by default).
//! * When the flag is set and the tool *is* available, prints a
//!   one-line notice explaining that the full integration is still
//!   under construction and returns empty.
//!
//! What it *does not* do yet (planned for a follow-up v0.3 commit)
//! ---------------------------------------------------------------
//! 1. Spawn `rust-analyzer` with `--stdio` and perform the LSP
//!    initialize handshake.
//! 2. Send `textDocument/didOpen` for every file in the project (walk
//!    via `cargo metadata` for a precise source list).
//! 3. Poll for indexing completion — rust-analyzer's `$/progress`
//!    notifications surface `rustAnalyzer/Indexing` begin / end pairs.
//! 4. For each changed symbol whose definition lives in a changed
//!    file, resolve its location (a `textDocument/documentSymbol`
//!    call against the file suffices for top-level items) and issue
//!    `textDocument/references` with `includeDeclaration = false`.
//! 5. Map every returned reference into a `TestReference`,
//!    `TraitImpl`, or similar finding **at `Proven` tier** since the
//!    edge is now name-resolved by the compiler front-end.
//! 6. Merge with the existing syn-only findings, deduping — if RA
//!    reports the same site the syn analyzer flagged, upgrade the
//!    finding's tier from `Likely` to `Proven` rather than emitting
//!    two copies.
//!
//! Why ship the stub before the implementation
//! -------------------------------------------
//! Two reasons. First, the API shape (opt-in flag, graceful skip
//! behavior) stays stable across the real implementation landing so
//! downstream consumers can start wiring `--rust-analyzer` into CI
//! today. Second, honest absence — the v0.2 tool claims no `Proven`
//! findings, and having `--rust-analyzer` as a visible surface
//! (even stubbed) keeps the honest-tiering story observable rather
//! than a paragraph buried in the changelog.

use crate::finding::Finding;
use anyhow::Result;
use std::path::Path;

const TOOL_BIN: &str = "rust-analyzer";

/// Entry point — mirrors `semver_checks::run` so the orchestrator can
/// call both opt-ins uniformly.
///
/// Current behavior: detect the tool, emit a notice, return empty. See
/// the module doc for the planned full integration steps.
pub fn run(_root: &Path, _since: &str, enabled: bool) -> Result<Vec<Finding>> {
    if !enabled {
        return Ok(Vec::new());
    }

    if !is_installed() {
        eprintln!(
            "cargo-impact: --rust-analyzer requested but `{TOOL_BIN}` not found on PATH. \
             Install it via `rustup component add rust-analyzer` (or from your IDE \
             tooling); skipping."
        );
        return Ok(Vec::new());
    }

    // Tool is present. Full LSP integration is not yet implemented —
    // tell the user so they don't assume silent success means silent
    // "nothing to find."
    eprintln!(
        "cargo-impact: rust-analyzer found on PATH, but the LSP integration that \
         promotes findings to the `Proven` tier is still under construction in the \
         v0.3 line. This run produces no RA-backed findings. Track progress in the \
         project README §11."
    );
    Ok(Vec::new())
}

/// Public so tests and the `--help` surface can describe the dependency
/// consistently across the two opt-in analyzers.
pub fn is_installed() -> bool {
    which(TOOL_BIN).is_some()
}

/// Same bespoke `which` used by `semver_checks` — keep the two in sync
/// rather than adding a shared util module for something this small.
fn which(name: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_flag_returns_empty_without_touching_path() {
        let findings = run(Path::new("."), "HEAD", false).unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn enabled_without_tool_returns_empty_findings() {
        // We can't reliably flip PATH in a test, but the contract is: even
        // if the tool isn't installed, `run` must succeed with an empty
        // vec — no panic, no error. If the tool *is* installed on this
        // machine, the stub branch also returns empty, so the same
        // assertion holds in both environments.
        let findings = run(Path::new("."), "HEAD", true).unwrap();
        assert!(findings.is_empty());
    }
}
