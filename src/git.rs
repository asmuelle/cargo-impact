use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Return the set of changed Rust source files, including committed changes
/// relative to `since` plus all staged and unstaged modifications.
pub fn changed_rust_files(root: &Path, since: &str) -> Result<Vec<PathBuf>> {
    let committed = git_name_only(root, &["diff", "--name-only", since])?;
    let staged = git_name_only(root, &["diff", "--name-only", "--cached"])?;
    let unstaged = git_name_only(root, &["diff", "--name-only"])?;

    let mut files: Vec<PathBuf> = committed
        .into_iter()
        .chain(staged)
        .chain(unstaged)
        .collect();
    files.sort();
    files.dedup();

    let out: Vec<PathBuf> = files
        .into_iter()
        .filter(|rel| {
            let abs = root.join(rel);
            abs.extension().and_then(|s| s.to_str()) == Some("rs") && abs.is_file()
        })
        .collect();
    Ok(out)
}

fn git_name_only(root: &Path, args: &[&str]) -> Result<Vec<PathBuf>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .with_context(|| format!("invoking git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect())
}
