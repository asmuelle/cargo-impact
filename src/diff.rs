//! HEAD-vs-working-tree item diff.
//!
//! For each changed file, parse both the `since` revision (default `HEAD`) and
//! the working-tree content, then compare item by item. This gives v0.2 a
//! significantly narrower "changed symbols" set than the v0.1 blanket
//! file-level approach — items whose tokens are identical in both versions
//! are excluded, and the downstream analyzers (tests_scan, traits,
//! dyn_dispatch, doc_drift) all benefit automatically.
//!
//! Graceful fallback: if the HEAD version can't be retrieved (new file,
//! first commit, detached state) or either side fails to parse, the caller
//! falls back to the v0.1 blanket behavior for that file. Precision is a
//! best-effort enhancement, never a hard requirement.

use crate::symbols::SymbolKind;
use anyhow::Result;
use quote::ToTokens;
use std::collections::BTreeMap;
use std::path::Path;
use syn::{File, Item};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemChange {
    Added,
    Removed,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedItem {
    pub name: String,
    pub kind: SymbolKind,
    pub change: ItemChange,
}

/// Compute the per-item diff for a single file between `since` and the
/// working tree. Returns `Ok(None)` when the HEAD version is unavailable
/// (new/untracked file, `since` not resolvable) so the caller can fall back
/// to blanket analysis; returns `Ok(Some(items))` on success.
pub fn diff_file(root: &Path, rel_file: &Path, since: &str) -> Result<Option<Vec<ChangedItem>>> {
    let wt_path = root.join(rel_file);
    let wt_src = std::fs::read_to_string(&wt_path).ok();

    let head_src = match crate::git::show_file_at(root, since, rel_file)? {
        Some(s) => s,
        None => {
            // File did not exist at `since` — everything in the WT is new.
            return Ok(wt_src.map(|src| all_as(&src, ItemChange::Added)));
        }
    };

    let Some(wt_src) = wt_src else {
        // File existed at `since` but no longer exists in the working tree.
        return Ok(Some(all_as(&head_src, ItemChange::Removed)));
    };

    let Some(head_ast) = crate::cfg::parse_and_filter(&head_src) else {
        return Ok(None);
    };
    let Some(wt_ast) = crate::cfg::parse_and_filter(&wt_src) else {
        return Ok(None);
    };

    Ok(Some(compare(&head_ast, &wt_ast)))
}

fn all_as(src: &str, change: ItemChange) -> Vec<ChangedItem> {
    let Some(ast) = crate::cfg::parse_and_filter(src) else {
        return Vec::new();
    };
    items_by_qualified_name(&ast)
        .into_iter()
        .map(|(_, (name, kind, _))| ChangedItem { name, kind, change })
        .collect()
}

fn compare(head: &File, wt: &File) -> Vec<ChangedItem> {
    let head_items = items_by_qualified_name(head);
    let wt_items = items_by_qualified_name(wt);

    let mut out = Vec::new();
    for (key, (name, kind, tokens)) in &wt_items {
        match head_items.get(key) {
            None => out.push(ChangedItem {
                name: name.clone(),
                kind: *kind,
                change: ItemChange::Added,
            }),
            Some((_, _, head_tokens)) if head_tokens != tokens => out.push(ChangedItem {
                name: name.clone(),
                kind: *kind,
                change: ItemChange::Modified,
            }),
            _ => {}
        }
    }
    for (key, (name, kind, _)) in &head_items {
        if !wt_items.contains_key(key) {
            out.push(ChangedItem {
                name: name.clone(),
                kind: *kind,
                change: ItemChange::Removed,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Build a qualified-name → (bare name, kind, token-stream) map for a parsed
/// file. Anonymous items (impls, use statements, extern blocks, macros) are
/// excluded — diff tracking is item-based and they have no single name to key
/// on. FFI extern "C" signature changes are caught by the dedicated `ffi`
/// module instead.
fn items_by_qualified_name(ast: &File) -> BTreeMap<String, (String, SymbolKind, String)> {
    let mut out = BTreeMap::new();
    collect(&ast.items, &mut Vec::new(), &mut out);
    out
}

fn collect(
    items: &[Item],
    modules: &mut Vec<String>,
    out: &mut BTreeMap<String, (String, SymbolKind, String)>,
) {
    for item in items {
        let entry = match item {
            Item::Fn(f) => Some((f.sig.ident.to_string(), SymbolKind::Fn, item_tokens(item))),
            Item::Struct(s) => Some((s.ident.to_string(), SymbolKind::Struct, item_tokens(item))),
            Item::Enum(e) => Some((e.ident.to_string(), SymbolKind::Enum, item_tokens(item))),
            Item::Trait(t) => Some((t.ident.to_string(), SymbolKind::Trait, item_tokens(item))),
            Item::Const(c) => Some((c.ident.to_string(), SymbolKind::Const, item_tokens(item))),
            Item::Static(s) => Some((s.ident.to_string(), SymbolKind::Static, item_tokens(item))),
            Item::Type(t) => Some((
                t.ident.to_string(),
                SymbolKind::TypeAlias,
                item_tokens(item),
            )),
            Item::Union(u) => Some((u.ident.to_string(), SymbolKind::Union, item_tokens(item))),
            Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    modules.push(m.ident.to_string());
                    collect(inner, modules, out);
                    modules.pop();
                }
                Some((m.ident.to_string(), SymbolKind::Mod, item_tokens(item)))
            }
            _ => None,
        };
        if let Some((name, kind, tokens)) = entry {
            let key = qualified_name(modules, &name);
            out.insert(key, (name, kind, tokens));
        }
    }
}

fn qualified_name(modules: &[String], name: &str) -> String {
    if modules.is_empty() {
        name.to_string()
    } else {
        format!("{}::{name}", modules.join("::"))
    }
}

fn item_tokens(item: &Item) -> String {
    item.to_token_stream().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    /// Initialize a git repo with a single committed file, then mutate the
    /// working tree. Returns (dir, rel_path).
    fn git_fixture(initial: &str, modified: Option<&str>) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // git init + identity + disable gpg so the commit goes through on CI.
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
            &["config", "commit.gpgsign", "false"],
            // Windows git defaults core.autocrlf = true, which rewrites
            // line endings in the index and breaks our diff assertions.
            &["config", "core.autocrlf", "false"],
        ] {
            let status = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        }

        let rel = std::path::PathBuf::from("src.rs");
        fs::write(root.join(&rel), initial).unwrap();

        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["add", "src.rs"])
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["commit", "-q", "-m", "init"])
            .status()
            .unwrap();
        assert!(status.success());

        if let Some(new) = modified {
            fs::write(root.join(&rel), new).unwrap();
        }

        (dir, rel)
    }

    fn names(items: &[ChangedItem]) -> Vec<(&str, ItemChange)> {
        items.iter().map(|i| (i.name.as_str(), i.change)).collect()
    }

    #[test]
    fn detects_added_item() {
        let (dir, rel) = git_fixture(
            "fn stable() {}\n",
            Some("fn stable() {}\nfn fresh() { 1 + 1; }\n"),
        );
        let items = diff_file(dir.path(), &rel, "HEAD").unwrap().unwrap();
        assert_eq!(names(&items), vec![("fresh", ItemChange::Added)]);
    }

    #[test]
    fn detects_removed_item() {
        let (dir, rel) = git_fixture("fn stable() {}\nfn gone() {}\n", Some("fn stable() {}\n"));
        let items = diff_file(dir.path(), &rel, "HEAD").unwrap().unwrap();
        assert_eq!(names(&items), vec![("gone", ItemChange::Removed)]);
    }

    #[test]
    fn detects_modified_item() {
        let (dir, rel) = git_fixture(
            "fn stable() {}\nfn changed() { 1; }\n",
            Some("fn stable() {}\nfn changed() { 2; }\n"),
        );
        let items = diff_file(dir.path(), &rel, "HEAD").unwrap().unwrap();
        assert_eq!(names(&items), vec![("changed", ItemChange::Modified)]);
    }

    #[test]
    fn same_named_items_in_different_modules_do_not_collide() {
        let (dir, rel) = git_fixture(
            "mod a { pub trait Greeter { fn hi(&self) -> u32; } }\n\
             mod b { pub trait Greeter { fn hi(&self) -> u32; } }\n",
            Some(
                "mod a { pub trait Greeter { fn hi(&self) -> String; } }\n\
                 mod b { pub trait Greeter { fn hi(&self) -> u32; } }\n",
            ),
        );
        let items = diff_file(dir.path(), &rel, "HEAD").unwrap().unwrap();
        assert!(
            names(&items)
                .iter()
                .any(|(name, change)| *name == "Greeter" && *change == ItemChange::Modified),
            "expected the changed nested trait to survive alongside same-named siblings: {items:?}"
        );
    }

    #[test]
    fn deleted_file_marks_all_previous_items_removed() {
        let (dir, rel) = git_fixture("fn gone() {}\nstruct Removed;\n", None);
        fs::remove_file(dir.path().join(&rel)).unwrap();

        let items = diff_file(dir.path(), &rel, "HEAD").unwrap().unwrap();
        let got = names(&items);
        assert!(got.contains(&("gone", ItemChange::Removed)));
        assert!(got.contains(&("Removed", ItemChange::Removed)));
    }

    #[test]
    fn identical_files_produce_empty_diff() {
        let body = "fn a() {}\nfn b() {}\n";
        let (dir, rel) = git_fixture(body, Some(body));
        let items = diff_file(dir.path(), &rel, "HEAD").unwrap().unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn new_file_with_no_head_version_marks_everything_added() {
        // Commit one file, then create a second file that never existed in HEAD.
        let (dir, _) = git_fixture("fn seed() {}\n", None);
        let new_rel = std::path::PathBuf::from("brand_new.rs");
        fs::write(dir.path().join(&new_rel), "fn hello() {}\nstruct S;\n").unwrap();

        let items = diff_file(dir.path(), &new_rel, "HEAD").unwrap().unwrap();
        let names: Vec<_> = names(&items);
        assert!(names.contains(&("hello", ItemChange::Added)));
        assert!(names.contains(&("S", ItemChange::Added)));
    }

    #[test]
    fn unreadable_working_tree_returns_none() {
        let (dir, _) = git_fixture("fn a() {}\n", None);
        let missing = std::path::PathBuf::from("does-not-exist.rs");
        let result = diff_file(dir.path(), &missing, "HEAD").unwrap();
        assert!(result.is_none(), "expected None for unreadable WT file");
    }

    #[test]
    fn unparseable_file_returns_none_so_caller_can_fall_back() {
        let (dir, rel) = git_fixture("fn a() {}\n", Some("!! not rust !!"));
        let result = diff_file(dir.path(), &rel, "HEAD").unwrap();
        assert!(
            result.is_none(),
            "expected None when WT is not valid Rust so caller falls back"
        );
    }
}
