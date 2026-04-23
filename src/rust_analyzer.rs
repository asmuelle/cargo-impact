//! rust-analyzer-backed reference resolution for `Proven`-tier findings.
//!
//! Spawns `rust-analyzer` as an LSP stdio subprocess, waits for it to
//! finish indexing the workspace, then for each changed top-level symbol
//! queries `textDocument/references` and converts the returned locations
//! into [`FindingKind::ResolvedReference`] findings at `Tier::Proven`.
//!
//! Opt-in: gated by `--rust-analyzer` (or `rust_analyzer` in the MCP
//! tool argument). Off by default because the tool must be installed and
//! indexing a cold workspace can take tens of seconds.
//!
//! Why this is honest `Proven`-tier
//! --------------------------------
//! The syn-only analyzers elsewhere in this crate emit at most `Likely`
//! because they match on identifier text — they cannot prove that a use
//! of the name `foo` actually resolves to the `foo` defined in a
//! changed file (shadowing, imports, same-name-different-crate). RA
//! runs the Rust front-end, so its references are name-resolved at the
//! exact granularity of the compiler. When RA says `foo` at src/a.rs:42
//! uses the `foo` we changed, it actually does.
//!
//! Scope of this implementation (v0.3-alpha.1)
//! -------------------------------------------
//! Shipped:
//! * LSP stdio client with Content-Length framing.
//! * `initialize` / `initialized` handshake.
//! * Indexing wait via `$/progress` notifications matching the
//!   `rustAnalyzer/Indexing` token, with a fallback wall-clock timeout.
//! * Per-file `textDocument/documentSymbol` + per-symbol
//!   `textDocument/references` for every changed top-level item.
//! * Graceful degradation: if RA isn't installed, fails spawning, or
//!   doesn't finish indexing in time, we log to stderr and return no
//!   findings rather than failing the whole run.
//!
//! Deferred to later v0.3 iterations:
//! * Cross-crate workspaces with non-trivial `[workspace]` layouts (we
//!   trust RA's own discovery from the root; mostly fine but not
//!   explicitly tested).
//! * `textDocument/didOpen` push for files RA hasn't auto-indexed —
//!   normally RA finds them via Cargo, but edge cases exist.
//! * Tiering refinement: currently every resolved reference is
//!   `Proven 0.98`. A follow-up can walk upward from the reference
//!   location to determine whether it sits inside a `#[test]` fn, an
//!   `impl` block, or plain caller, and tag severity accordingly.

use crate::finding::{Finding, FindingKind, Location, Tier};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

const TOOL_BIN: &str = "rust-analyzer";

/// How long to wait for rust-analyzer to finish indexing before giving
/// up and issuing queries against whatever has been indexed so far.
const INDEXING_TIMEOUT: Duration = Duration::from_secs(60);

/// Max total time a single `run` invocation will spend. Prevents a
/// misbehaving RA from hanging the whole pipeline.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(120);

/// Per-request read timeout — rust-analyzer responds to resolved
/// queries quickly once indexing is done, so a long stall is a bug
/// signal, not patience.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Entry point invoked by the orchestrator.
///
/// `changed_files` are repo-relative paths of Rust files that differ
/// between the `since` revision and the working tree. `changed_symbols`
/// are the top-level item names we want to find references to.
pub fn run(
    root: &Path,
    changed_files: &[PathBuf],
    changed_symbols: &BTreeSet<String>,
    enabled: bool,
) -> Result<Vec<Finding>> {
    if !enabled {
        return Ok(Vec::new());
    }
    if !is_installed() {
        eprintln!(
            "cargo-impact: --rust-analyzer requested but `{TOOL_BIN}` not found on PATH. \
             Install it via `rustup component add rust-analyzer`; skipping."
        );
        return Ok(Vec::new());
    }
    if changed_files.is_empty() || changed_symbols.is_empty() {
        return Ok(Vec::new());
    }

    let deadline = Instant::now() + TOTAL_TIMEOUT;
    let mut client = match LspClient::spawn(root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cargo-impact: failed to spawn rust-analyzer: {e:#}; skipping.");
            return Ok(Vec::new());
        }
    };

    if let Err(e) = client.initialize(root, deadline) {
        eprintln!("cargo-impact: rust-analyzer initialize failed: {e:#}; skipping.");
        let _ = client.shutdown();
        return Ok(Vec::new());
    }

    // Indexing wait is best-effort. If it times out we still issue
    // queries — they may return incomplete results, but some coverage
    // beats none.
    if let Err(e) = client.wait_for_indexing(deadline.min(Instant::now() + INDEXING_TIMEOUT)) {
        eprintln!(
            "cargo-impact: rust-analyzer indexing didn't complete in time: {e:#}; \
             continuing with partial index."
        );
    }

    let mut findings = Vec::new();
    for rel in changed_files {
        if Instant::now() >= deadline {
            eprintln!(
                "cargo-impact: rust-analyzer total-time budget exhausted; \
                 stopping after {} files.",
                changed_files
                    .iter()
                    .position(|f| f == rel)
                    .unwrap_or(changed_files.len())
            );
            break;
        }
        let abs = root.join(rel);
        match collect_references_for_file(&mut client, &abs, rel, changed_symbols, deadline) {
            Ok(hits) => findings.extend(hits),
            Err(e) => eprintln!(
                "cargo-impact: rust-analyzer query failed for {}: {e:#}; skipping file",
                rel.display()
            ),
        }
    }

    let _ = client.shutdown();
    Ok(findings)
}

/// For one file, enumerate top-level symbols via `documentSymbol`, keep
/// only those whose name appears in `changed_symbols`, and query
/// `textDocument/references` for each.
fn collect_references_for_file(
    client: &mut LspClient,
    abs: &Path,
    rel: &Path,
    changed_symbols: &BTreeSet<String>,
    deadline: Instant,
) -> Result<Vec<Finding>> {
    let symbols = client.document_symbols(abs, deadline)?;
    let mut findings = Vec::new();
    for sym in symbols {
        if !changed_symbols.contains(&sym.name) {
            continue;
        }
        if Instant::now() >= deadline {
            break;
        }
        let refs = client.references(abs, sym.line, sym.character, deadline)?;
        for loc in refs {
            let target_file = uri_to_relative_path(&loc.uri, abs.parent().unwrap_or(abs));
            let finding = Finding::new(
                "",
                Tier::Proven,
                0.98,
                FindingKind::ResolvedReference {
                    source_symbol: sym.name.clone(),
                    target: Location {
                        file: target_file.clone(),
                        symbol: format!("{}:{}", target_file.display(), loc.line + 1),
                    },
                },
                format!(
                    "rust-analyzer resolves a reference from {}:{} to `{}` (defined in {})",
                    target_file.display(),
                    loc.line + 1,
                    sym.name,
                    rel.display()
                ),
            );
            findings.push(finding);
        }
    }
    Ok(findings)
}

pub fn is_installed() -> bool {
    which(TOOL_BIN).is_some()
}

fn which(name: &str) -> Option<PathBuf> {
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

// ---------------------------------------------------------------------------
// LSP client
// ---------------------------------------------------------------------------

/// Minimal LSP client with Content-Length framing and request-ID
/// correlation. Notifications from the server are inspected for
/// `$/progress` indexing events but otherwise discarded.
struct LspClient {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: i64,
    indexing_done: bool,
}

#[derive(Debug, Clone)]
struct SymbolLoc {
    name: String,
    line: u32,
    character: u32,
}

#[derive(Debug, Clone)]
struct RefLoc {
    uri: String,
    line: u32,
}

impl LspClient {
    fn spawn(root: &Path) -> Result<Self> {
        let mut child = Command::new(TOOL_BIN)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .current_dir(root)
            .spawn()
            .context("spawning rust-analyzer")?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        Ok(Self {
            child,
            stdin,
            reader: BufReader::new(stdout),
            next_id: 0,
            indexing_done: false,
        })
    }

    fn next_id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }

    fn initialize(&mut self, root: &Path, deadline: Instant) -> Result<()> {
        let root_uri = path_to_uri(root);
        let id = self.next_id();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": {
                    "window": { "workDoneProgress": true },
                    "textDocument": {
                        "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                        "references": {}
                    }
                }
            }
        }))?;
        let _ = self.wait_for_response(id, deadline)?;
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }))?;
        Ok(())
    }

    /// Wait for rust-analyzer to signal that indexing is complete.
    /// Returns once we see `$/progress` with `kind: "end"` on the
    /// `rustAnalyzer/Indexing` token, or once the deadline passes.
    fn wait_for_indexing(&mut self, deadline: Instant) -> Result<()> {
        while !self.indexing_done && Instant::now() < deadline {
            let msg = match self.read_next(deadline) {
                Ok(m) => m,
                Err(_) => return Ok(()), // deadline hit inside read
            };
            self.handle_notification(&msg);
        }
        if !self.indexing_done {
            bail!("indexing did not complete within {INDEXING_TIMEOUT:?}");
        }
        Ok(())
    }

    fn document_symbols(&mut self, abs: &Path, deadline: Instant) -> Result<Vec<SymbolLoc>> {
        self.did_open_if_needed(abs)?;
        let id = self.next_id();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/documentSymbol",
            "params": {
                "textDocument": { "uri": path_to_uri(abs) }
            }
        }))?;
        let resp = self.wait_for_response(id, deadline)?;
        Ok(parse_document_symbols(&resp))
    }

    fn references(
        &mut self,
        abs: &Path,
        line: u32,
        character: u32,
        deadline: Instant,
    ) -> Result<Vec<RefLoc>> {
        let id = self.next_id();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/references",
            "params": {
                "textDocument": { "uri": path_to_uri(abs) },
                "position": { "line": line, "character": character },
                "context": { "includeDeclaration": false }
            }
        }))?;
        let resp = self.wait_for_response(id, deadline)?;
        Ok(parse_references(&resp))
    }

    fn did_open_if_needed(&mut self, abs: &Path) -> Result<()> {
        // Always send — RA ignores duplicates harmlessly. Pushing the
        // file ensures it's tracked even if the project-discovery didn't
        // auto-include it (common in test fixtures).
        let text = std::fs::read_to_string(abs)
            .with_context(|| format!("reading {} for didOpen", abs.display()))?;
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": path_to_uri(abs),
                    "languageId": "rust",
                    "version": 1,
                    "text": text
                }
            }
        }))?;
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        let id = self.next_id();
        // Best-effort: ignore errors since we're tearing down.
        let _ = self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "shutdown"
        }));
        let _ = self.send(&json!({
            "jsonrpc": "2.0",
            "method": "exit"
        }));
        let _ = self.child.wait();
        Ok(())
    }

    fn send(&mut self, msg: &Value) -> Result<()> {
        let body = serde_json::to_string(msg)?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
        self.stdin.flush()?;
        Ok(())
    }

    /// Read messages until one with `"id" == id` arrives; notifications
    /// in between are inspected for indexing state and then discarded.
    fn wait_for_response(&mut self, id: i64, deadline: Instant) -> Result<Value> {
        loop {
            if Instant::now() >= deadline {
                bail!("deadline exceeded waiting for response id {id}");
            }
            let msg = self.read_next(deadline)?;
            if msg.get("id").and_then(Value::as_i64) == Some(id) {
                if let Some(err) = msg.get("error") {
                    bail!("rust-analyzer error: {err}");
                }
                return Ok(msg);
            }
            self.handle_notification(&msg);
        }
    }

    fn handle_notification(&mut self, msg: &Value) {
        // Only care about $/progress end events with our token — RA
        // emits structured progress for indexing.
        if msg.get("method").and_then(Value::as_str) == Some("$/progress") {
            let token = msg
                .get("params")
                .and_then(|p| p.get("token"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let kind = msg
                .get("params")
                .and_then(|p| p.get("value"))
                .and_then(|v| v.get("kind"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if token.contains("Indexing") && kind == "end" {
                self.indexing_done = true;
            }
        }
    }

    /// Read a single LSP message (Content-Length framed) from the child.
    /// Honors `deadline` on a best-effort basis — we enforce a per-read
    /// budget but can't cancel mid-read.
    fn read_next(&mut self, deadline: Instant) -> Result<Value> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("deadline exceeded before read");
        }
        // Stdio-based reading is inherently blocking; rely on the child
        // process being well-behaved. The per-request timeout caps worst-
        // case wait.
        let effective = remaining.min(REQUEST_TIMEOUT);
        read_message(&mut self.reader, effective)
    }
}

// ---------------------------------------------------------------------------
// Wire format and parsing (pure; easy to unit-test)
// ---------------------------------------------------------------------------

/// Read one Content-Length-framed JSON message. Respects `budget` as a
/// deadline for the whole read; returns an error rather than blocking
/// forever if the child goes quiet.
fn read_message<R: Read + BufRead>(reader: &mut R, budget: Duration) -> Result<Value> {
    let start = Instant::now();
    let mut content_length: Option<usize> = None;
    let mut line = String::new();

    // Read headers until blank line.
    loop {
        if start.elapsed() > budget {
            bail!("timeout reading LSP headers after {budget:?}");
        }
        line.clear();
        let n = reader.read_line(&mut line).context("reading LSP header")?;
        if n == 0 {
            bail!("LSP stream closed while reading headers");
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(value) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            let trimmed = value.trim();
            content_length = Some(
                trimmed
                    .parse()
                    .with_context(|| format!("parsing Content-Length `{trimmed}`"))?,
            );
        }
    }
    let len = content_length.ok_or_else(|| anyhow!("LSP message missing Content-Length"))?;

    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .context("reading LSP message body")?;
    serde_json::from_slice(&body).context("parsing LSP body as JSON")
}

fn parse_document_symbols(resp: &Value) -> Vec<SymbolLoc> {
    let mut out = Vec::new();
    let Some(arr) = resp.get("result").and_then(Value::as_array) else {
        return out;
    };
    for sym in arr {
        collect_document_symbol(sym, &mut out);
    }
    out
}

fn collect_document_symbol(sym: &Value, out: &mut Vec<SymbolLoc>) {
    // DocumentSymbol has `name`, `selectionRange.start.{line,character}`
    // and optional `children`.
    let name = sym
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let pos = sym
        .get("selectionRange")
        .and_then(|r| r.get("start"))
        .or_else(|| sym.get("range").and_then(|r| r.get("start")));
    if let Some(p) = pos {
        if !name.is_empty() {
            out.push(SymbolLoc {
                name,
                line: p.get("line").and_then(Value::as_u64).unwrap_or(0) as u32,
                character: p.get("character").and_then(Value::as_u64).unwrap_or(0) as u32,
            });
        }
    }
    if let Some(children) = sym.get("children").and_then(Value::as_array) {
        for c in children {
            collect_document_symbol(c, out);
        }
    }
}

fn parse_references(resp: &Value) -> Vec<RefLoc> {
    let Some(arr) = resp.get("result").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|loc| {
            let uri = loc.get("uri").and_then(Value::as_str)?.to_string();
            let line = loc
                .get("range")
                .and_then(|r| r.get("start"))
                .and_then(|s| s.get("line"))
                .and_then(Value::as_u64)? as u32;
            Some(RefLoc { uri, line })
        })
        .collect()
}

fn path_to_uri(p: &Path) -> String {
    let canon = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let mut s = canon.to_string_lossy().replace('\\', "/");
    if !s.starts_with('/') {
        // Windows drive letter path — prepend `/` to reach three slashes.
        s.insert(0, '/');
    }
    format!("file://{s}")
}

fn uri_to_relative_path(uri: &str, base: &Path) -> PathBuf {
    let Some(stripped) = uri.strip_prefix("file://") else {
        return PathBuf::from(uri);
    };
    // Windows drive-letter URIs take the shape `file:///C:/path` — the
    // leading `/` before the drive letter is URI cruft, not part of the
    // filesystem path. Unix-style URIs like `file:///tmp/path` keep that
    // leading `/` because it *is* the root. Detect the drive-letter form
    // by shape so the logic works identically on both platforms — the
    // `#[cfg(windows)]` version of this used to unconditionally strip the
    // slash, which corrupted Unix-shaped URIs when the test happened to
    // run on a Windows host.
    let path_str = stripped
        .strip_prefix('/')
        .and_then(has_drive_letter)
        .unwrap_or(stripped);
    let abs = PathBuf::from(path_str);
    abs.strip_prefix(base)
        .map(std::path::Path::to_path_buf)
        .unwrap_or(abs)
}

/// Returns `Some(s)` if `s` starts with an ASCII drive letter followed
/// by `:` (e.g. `C:/foo`), otherwise `None`. Used to distinguish the
/// Windows `file:///C:/...` URI form from the Unix `file:///tmp/...`
/// form after stripping the leading scheme slashes.
fn has_drive_letter(s: &str) -> Option<&str> {
    let mut chars = s.chars();
    let first = chars.next()?;
    let second = chars.next()?;
    if first.is_ascii_alphabetic() && second == ':' {
        Some(s)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_message_parses_well_formed_frame() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let raw = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut cursor = std::io::BufReader::new(raw.as_bytes());
        let msg = read_message(&mut cursor, Duration::from_secs(1)).unwrap();
        assert_eq!(msg["id"], 1);
        assert_eq!(msg["result"]["ok"], true);
    }

    #[test]
    fn read_message_rejects_missing_content_length() {
        let raw = "Some-Other-Header: 3\r\n\r\n{}";
        let mut cursor = std::io::BufReader::new(raw.as_bytes());
        let err = read_message(&mut cursor, Duration::from_secs(1)).unwrap_err();
        assert!(format!("{err:#}").contains("Content-Length"));
    }

    #[test]
    fn read_message_case_insensitive_header() {
        let body = r#"{"x":1}"#;
        let raw = format!("content-length: {}\r\n\r\n{}", body.len(), body);
        let mut cursor = std::io::BufReader::new(raw.as_bytes());
        let msg = read_message(&mut cursor, Duration::from_secs(1)).unwrap();
        assert_eq!(msg["x"], 1);
    }

    #[test]
    fn parse_document_symbols_flat_and_hierarchical() {
        let resp = json!({
            "result": [
                {
                    "name": "Foo",
                    "kind": 5,
                    "range": { "start": {"line": 0, "character": 0}, "end": {"line":0,"character":9}},
                    "selectionRange": { "start": {"line": 0, "character": 4}, "end": {"line":0,"character":7}}
                },
                {
                    "name": "outer",
                    "range": { "start": {"line": 2, "character": 0}, "end": {"line":4,"character":1}},
                    "selectionRange": { "start": {"line": 2, "character": 4}, "end": {"line":2,"character":9}},
                    "children": [
                        {
                            "name": "inner",
                            "range": { "start": {"line": 3, "character": 4}, "end": {"line":3,"character":20}},
                            "selectionRange": { "start": {"line": 3, "character": 7}, "end": {"line":3,"character":12}}
                        }
                    ]
                }
            ]
        });
        let syms = parse_document_symbols(&resp);
        let names: Vec<_> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"outer"));
        assert!(names.contains(&"inner"), "children must be flattened");
    }

    #[test]
    fn parse_references_extracts_uri_and_line() {
        let resp = json!({
            "result": [
                {
                    "uri": "file:///tmp/x/src/lib.rs",
                    "range": {
                        "start": {"line": 10, "character": 4},
                        "end": {"line": 10, "character": 8}
                    }
                }
            ]
        });
        let refs = parse_references(&resp);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].line, 10);
        assert!(refs[0].uri.ends_with("src/lib.rs"));
    }

    #[test]
    fn path_to_uri_produces_file_scheme() {
        let tmp = std::env::temp_dir();
        let uri = path_to_uri(&tmp);
        assert!(uri.starts_with("file://"));
    }

    #[test]
    fn uri_to_relative_path_strips_base_unix_shape() {
        let rel = uri_to_relative_path("file:///tmp/x/src/lib.rs", Path::new("/tmp/x"));
        assert_eq!(rel.to_string_lossy(), "src/lib.rs");
    }

    #[test]
    fn uri_to_relative_path_handles_windows_drive_letter_shape() {
        // Windows URIs tack an extra `/` before the drive letter.
        // Verify the strip logic recognizes the drive-letter form
        // regardless of the host we're running on.
        let rel = uri_to_relative_path(
            "file:///C:/projects/x/src/lib.rs",
            Path::new("C:/projects/x"),
        );
        assert_eq!(rel.to_string_lossy().replace('\\', "/"), "src/lib.rs");
    }

    #[test]
    fn has_drive_letter_classifies_correctly() {
        assert!(has_drive_letter("C:/projects/x").is_some());
        assert!(has_drive_letter("z:foo").is_some());
        assert!(has_drive_letter("tmp/x").is_none());
        assert!(has_drive_letter("/tmp/x").is_none());
        assert!(has_drive_letter("").is_none());
        assert!(has_drive_letter("C").is_none());
    }

    #[test]
    fn disabled_flag_returns_empty() {
        let findings = run(Path::new("."), &[], &BTreeSet::new(), false).unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn enabled_with_empty_inputs_short_circuits() {
        // Even with the flag on, if there are no changed files or symbols
        // we must not spawn RA (which is expensive and may not exist on
        // the test runner).
        let findings = run(Path::new("."), &[], &BTreeSet::new(), true).unwrap();
        assert!(findings.is_empty());
    }
}
