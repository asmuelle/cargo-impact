use crate::tests_scan::AffectedTest;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Print a human-readable impact report to stdout.
pub fn print_text_report(
    changed_files: &[PathBuf],
    symbols: &BTreeSet<String>,
    affected: &[AffectedTest],
) {
    println!("cargo-impact v{}", env!("CARGO_PKG_VERSION"));
    println!();

    println!("Changed files ({}):", changed_files.len());
    for f in changed_files {
        println!("  {}", f.display());
    }
    println!();

    println!("Candidate symbols ({}):", symbols.len());
    for s in symbols {
        println!("  {s}");
    }
    println!();

    println!("Affected tests ({}):", affected.len());
    for t in affected {
        println!(
            "  {name} ({file}) — matches: {matches}",
            name = t.name,
            file = t.file.display(),
            matches = t.matched_symbols.join(", "),
        );
    }
    println!();

    if affected.is_empty() {
        println!("No tests matched. Run `cargo impact --test` to get an empty filter.");
    } else {
        println!("Run `cargo impact --test` to get a cargo-nextest filter expression.");
    }
}
