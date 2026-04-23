//! `cargo impact log-miss` — ground-truth collection for heuristic tuning.
//!
//! When the analyzer tells a user "run test X" and the user then
//! discovers test Y also needed to run (a miss), they record it with:
//!
//! ```bash
//! cargo impact log-miss --finding-id f-abcd1234 --what-broke "missed test tests::api_smoke"
//! ```
//!
//! Each invocation appends one JSON line to
//! `target/ai-tools-cache/impact/misses.jsonl`. That file is a free
//! dataset for tuning our tier confidences and kind-specific
//! heuristics later — impossible to retrofit, trivial to add now.
//!
//! Privacy note
//! ------------
//! The log-miss records stay on disk in the user's `target/` directory.
//! We never phone home. Aggregating across installations is deliberately
//! outside v0.3's scope — the file is yours, append-only, readable by
//! any tool that consumes JSONL.

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// CLI arguments for the `log-miss` subcommand. Dispatched from
/// `main.rs` after the `mcp` check.
#[derive(Parser, Debug)]
#[command(
    name = "cargo-impact log-miss",
    about = "Record a missed finding for heuristic tuning"
)]
pub struct LogMissArgs {
    /// The content-hashed finding ID from a prior `cargo impact` run.
    /// Pass "none" if the miss was a finding we didn't emit at all.
    #[arg(long)]
    pub finding_id: String,

    /// Free-form description of what the user expected us to flag.
    /// One line is ideal; multi-line content is preserved as-is but
    /// encoded as a JSON string in the record.
    #[arg(long)]
    pub what_broke: String,

    /// Workspace root. Defaults to the current working directory —
    /// the log is written under `{root}/target/ai-tools-cache/impact/`.
    #[arg(long)]
    pub manifest_dir: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct MissRecord<'a> {
    /// Seconds since Unix epoch. Intentionally `u64` and integer-valued
    /// so jsonl parsers that don't handle floats cleanly still work.
    timestamp: u64,
    /// Crate version that produced the referenced finding (helps us
    /// know which heuristic was active at the time of the miss).
    tool_version: &'a str,
    finding_id: &'a str,
    what_broke: &'a str,
    /// Git commit SHA if we can resolve it, else "unknown" — useful
    /// for aligning a miss with the actual diff that was analyzed.
    git_head: String,
}

/// Append a miss record to `target/ai-tools-cache/impact/misses.jsonl`.
/// Called from `main.rs` subcommand dispatch.
pub fn run(args: &LogMissArgs) -> Result<()> {
    let root = match &args.manifest_dir {
        Some(p) => p.clone(),
        None => std::env::current_dir().context("reading current directory")?,
    };

    let dir = root.join("target").join("ai-tools-cache").join("impact");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let path = dir.join("misses.jsonl");
    let record = MissRecord {
        timestamp: current_unix_secs(),
        tool_version: env!("CARGO_PKG_VERSION"),
        finding_id: &args.finding_id,
        what_broke: &args.what_broke,
        git_head: git_head(&root).unwrap_or_else(|| "unknown".to_string()),
    };
    let line = serde_json::to_string(&record)?;

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    writeln!(file, "{line}").with_context(|| format!("writing {}", path.display()))?;
    file.flush()?;

    eprintln!(
        "cargo-impact: logged miss to {}\n\
         ({} records total — free dataset for future heuristic tuning)",
        path.display(),
        count_lines(&path).unwrap_or(0)
    );
    Ok(())
}

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn git_head(root: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn count_lines(path: &std::path::Path) -> std::io::Result<usize> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    Ok(std::io::BufReader::new(file).lines().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_cache_directory_and_writes_record() {
        let dir = tempfile::TempDir::new().unwrap();
        let args = LogMissArgs {
            finding_id: "f-abcd1234".into(),
            what_broke: "missed test api_smoke".into(),
            manifest_dir: Some(dir.path().to_path_buf()),
        };
        run(&args).unwrap();
        let log = dir.path().join("target/ai-tools-cache/impact/misses.jsonl");
        assert!(log.exists(), "expected log file at {}", log.display());
        let content = std::fs::read_to_string(&log).unwrap();
        assert!(content.contains("f-abcd1234"));
        assert!(content.contains("missed test api_smoke"));
        // JSONL means exactly one newline at the end of each record.
        assert!(content.ends_with('\n'));
    }

    #[test]
    fn appends_multiple_records() {
        let dir = tempfile::TempDir::new().unwrap();
        for (id, msg) in [("f-1", "first"), ("f-2", "second"), ("f-3", "third")] {
            run(&LogMissArgs {
                finding_id: id.into(),
                what_broke: msg.into(),
                manifest_dir: Some(dir.path().to_path_buf()),
            })
            .unwrap();
        }
        let log = dir.path().join("target/ai-tools-cache/impact/misses.jsonl");
        let content = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("first"));
        assert!(lines[1].contains("second"));
        assert!(lines[2].contains("third"));
    }

    #[test]
    fn each_line_is_valid_standalone_json() {
        let dir = tempfile::TempDir::new().unwrap();
        run(&LogMissArgs {
            finding_id: "f-check".into(),
            what_broke: "contains \"quotes\" and a\nnewline".into(),
            manifest_dir: Some(dir.path().to_path_buf()),
        })
        .unwrap();
        let log = dir.path().join("target/ai-tools-cache/impact/misses.jsonl");
        let content = std::fs::read_to_string(&log).unwrap();
        for line in content.lines().filter(|l| !l.is_empty()) {
            let _: serde_json::Value =
                serde_json::from_str(line).expect("each jsonl row must parse as JSON");
        }
    }
}
