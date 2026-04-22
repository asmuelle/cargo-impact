//! `cargo-impact` — blast-radius analysis for Rust workspaces.
//!
//! This crate is the v0.1 MVP. Precision is intentionally crude: any change
//! to a Rust file is assumed to affect every top-level item in that file, and
//! a test is "affected" if its body syntactically references any such name.
//! Confidence tiers, macro expansion, and `rust-analyzer` integration land in
//! v0.2; see the project README (§11) for the precision roadmap.
//!
//! # Programmatic use
//!
//! ```
//! use cargo_impact::{nextest_filter, AffectedTest};
//! use std::path::PathBuf;
//!
//! let tests = [
//!     AffectedTest {
//!         name: "auth_roundtrip".into(),
//!         file: PathBuf::from("tests/auth.rs"),
//!         matched_symbols: vec!["login".into()],
//!     },
//!     AffectedTest {
//!         name: "smoke".into(),
//!         file: PathBuf::from("tests/smoke.rs"),
//!         matched_symbols: vec!["login".into()],
//!     },
//! ];
//! assert_eq!(nextest_filter(&tests), "test(auth_roundtrip) + test(smoke)");
//! ```

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::BTreeSet;
use std::path::PathBuf;

mod git;
mod nextest;
mod report;
mod symbols;
mod tests_scan;

pub use nextest::filter_expression as nextest_filter;
pub use symbols::top_level_symbol_names;
pub use tests_scan::{find_affected_tests, AffectedTest};

/// Command-line arguments for the `cargo impact` subcommand.
#[derive(Parser, Debug)]
#[command(
    name = "cargo-impact",
    bin_name = "cargo-impact",
    version,
    about = "Blast-radius analysis for Rust workspaces",
    long_about = None,
)]
pub struct ImpactArgs {
    /// Emit a `cargo-nextest` filter expression instead of the human report.
    #[arg(long)]
    pub test: bool,

    /// Git ref to diff against. Uncommitted (staged + unstaged) changes are
    /// always included regardless of this value.
    #[arg(long, default_value = "HEAD")]
    pub since: String,

    /// Repository root. Defaults to the current working directory.
    #[arg(long)]
    pub manifest_dir: Option<PathBuf>,
}

/// Run the impact analysis and print the configured output.
pub fn run(args: &ImpactArgs) -> Result<()> {
    let root = match &args.manifest_dir {
        Some(p) => p.clone(),
        None => std::env::current_dir().context("reading current directory")?,
    };

    let changed = git::changed_rust_files(&root, &args.since)?;
    if changed.is_empty() {
        if args.test {
            // Empty filter — callers can detect "nothing to run" and skip.
            println!();
        } else {
            println!(
                "cargo-impact: no Rust files changed relative to {}",
                args.since
            );
        }
        return Ok(());
    }

    let mut symbols: BTreeSet<String> = BTreeSet::new();
    for rel in &changed {
        let abs = root.join(rel);
        match symbols::top_level_symbol_names(&abs) {
            Ok(names) => symbols.extend(names),
            Err(e) => eprintln!("cargo-impact: skipping {}: {e:#}", rel.display()),
        }
    }

    let affected = tests_scan::find_affected_tests(&root, &symbols)?;

    if args.test {
        println!("{}", nextest::filter_expression(&affected));
    } else {
        report::print_text_report(&changed, &symbols, &affected);
    }
    Ok(())
}
