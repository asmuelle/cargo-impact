use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Return the set of changed Rust source files, including committed changes
/// relative to `since` plus all staged, unstaged, and untracked files.
pub fn changed_rust_files(root: &Path, since: &str) -> Result<Vec<PathBuf>> {
    let prefix = git_prefix(root)?;
    let committed = git_name_only(root, &["diff", "--name-only", since])?;
    let staged = git_name_only(root, &["diff", "--name-only", "--cached"])?;
    let unstaged = git_name_only(root, &["diff", "--name-only"])?;
    let untracked = git_name_only(
        root,
        &["ls-files", "--others", "--exclude-standard", "--full-name"],
    )?;

    let mut files: Vec<PathBuf> = committed
        .into_iter()
        .chain(staged)
        .chain(unstaged)
        .chain(untracked)
        .filter_map(|rel| worktree_to_root_relative(&rel, &prefix))
        .collect();
    files.sort();
    files.dedup();

    let out: Vec<PathBuf> = files
        .into_iter()
        .filter(|rel| rel.extension().and_then(|s| s.to_str()) == Some("rs"))
        .collect();
    Ok(out)
}

/// Show `rel` at `rev`, where `rel` is relative to the analysis root passed
/// to cargo-impact. Git tree paths are always worktree-root-relative, so
/// member-directory analyses need the root prefix re-added before `git show`.
pub(crate) fn show_file_at(root: &Path, rev: &str, rel: &Path) -> Result<Option<String>> {
    let spec = format!("{rev}:{}", tree_path(root, rel)?);
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("show")
        .arg(&spec)
        .output()
        .with_context(|| format!("git show {spec}"))?;
    if output.status.success() {
        Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
    } else {
        Ok(None)
    }
}

fn tree_path(root: &Path, rel: &Path) -> Result<String> {
    let prefix = git_prefix(root)?;
    let path = prefix.join(rel);
    Ok(path.to_string_lossy().replace('\\', "/"))
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

fn git_prefix(root: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-prefix"])
        .output()
        .context("invoking git rev-parse --show-prefix")?;
    if !output.status.success() {
        bail!(
            "git rev-parse --show-prefix failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let prefix = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(PathBuf::from(prefix))
}

fn worktree_to_root_relative(path: &Path, prefix: &Path) -> Option<PathBuf> {
    if prefix.as_os_str().is_empty() {
        return Some(path.to_path_buf());
    }
    path.strip_prefix(prefix).ok().map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["config", "user.email", "t@t"]);
        git(dir.path(), &["config", "user.name", "t"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        git(dir.path(), &["config", "core.autocrlf", "false"]);
        dir
    }

    #[test]
    fn member_root_paths_are_returned_relative_to_member() {
        let dir = repo();
        let member = dir.path().join("member");
        fs::create_dir_all(member.join("src")).unwrap();
        fs::write(member.join("src/lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);

        fs::write(member.join("src/lib.rs"), "pub fn f() -> u32 { 2 }\n").unwrap();

        let files = changed_rust_files(&member, "HEAD").unwrap();
        assert_eq!(files, vec![PathBuf::from("src/lib.rs")]);
    }

    #[test]
    fn deleted_rust_files_are_kept() {
        let dir = repo();
        fs::write(dir.path().join("lib.rs"), "pub fn f() {}\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);

        fs::remove_file(dir.path().join("lib.rs")).unwrap();

        let files = changed_rust_files(dir.path(), "HEAD").unwrap();
        assert_eq!(files, vec![PathBuf::from("lib.rs")]);
    }

    #[test]
    fn untracked_rust_files_are_kept() {
        let dir = repo();
        fs::write(dir.path().join("lib.rs"), "pub fn f() {}\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);

        fs::write(dir.path().join("new.rs"), "pub fn new() {}\n").unwrap();

        let files = changed_rust_files(dir.path(), "HEAD").unwrap();
        assert_eq!(files, vec![PathBuf::from("new.rs")]);
    }

    #[test]
    fn member_root_untracked_paths_are_returned_relative_to_member() {
        let dir = repo();
        let member = dir.path().join("member");
        fs::create_dir_all(member.join("src")).unwrap();
        fs::write(member.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);

        fs::write(member.join("src/new.rs"), "pub fn new() {}\n").unwrap();

        let files = changed_rust_files(&member, "HEAD").unwrap();
        assert_eq!(files, vec![PathBuf::from("src/new.rs")]);
    }

    #[test]
    fn show_file_at_readds_member_prefix() {
        let dir = repo();
        let member = dir.path().join("member");
        fs::create_dir_all(member.join("src")).unwrap();
        fs::write(member.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);

        let src = show_file_at(&member, "HEAD", Path::new("src/lib.rs"))
            .unwrap()
            .unwrap();
        assert!(src.contains("pub fn f"));
    }
}
