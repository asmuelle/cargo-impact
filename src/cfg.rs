//! Feature-flag awareness for static analysis.
//!
//! Rust lets code hide behind `#[cfg(feature = "x")]`. Without tracking
//! which features are active, an analyzer sees every gated branch
//! simultaneously and emits findings for code that doesn't even exist in
//! the user's build. This module fixes that.
//!
//! Shape
//! -----
//! * [`FeatureSet`] enumerates either an exact set of active features or
//!   the `Permissive` sentinel that treats every cfg as active (v0.1
//!   behavior; used when the user passes no feature flags and there is
//!   no `Cargo.toml` to read defaults from).
//! * [`resolve_features`] reads `{manifest_dir}/Cargo.toml`, merges in
//!   user-supplied `--features` / `--all-features` / `--no-default-features`,
//!   and transitively expands the graph.
//! * [`with_features`] installs a `FeatureSet` into a thread-local for the
//!   duration of a closure. Analyzers switch from `syn::parse_file(s)` to
//!   [`parse_and_filter`], which strips items whose cfg gate evaluates to
//!   `false` against the installed set.
//!
//! Coarseness — v0.2 scope
//! -----------------------
//! Only `cfg(feature = "…")`, `cfg(not(…))`, `cfg(all(…))` and
//! `cfg(any(…))` are evaluated against the feature set. Target gates
//! (`target_os`, `target_arch`, `target_family`, …) and bare cfg names
//! other than `test` (e.g. `debug_assertions`, `miri`) are treated as
//! *active*. A false positive is strictly better than a false negative:
//! we'd rather warn about code that's gated off for the current target
//! than miss impact on code that actually ships. `cfg(test)` is always
//! true so `#[cfg(test)] mod tests` stays visible to the test scanner.

use anyhow::{Context, Result};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::Path;
use syn::{Attribute, File, Item, Meta};

/// Active-feature view for cfg evaluation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum FeatureSet {
    /// Every `cfg(...)` attribute evaluates to `true`. Used when no
    /// `Cargo.toml` was found and no features were supplied on the CLI.
    #[default]
    Permissive,
    /// Exactly these features are active; everything else is off.
    Exact(BTreeSet<String>),
}

impl FeatureSet {
    pub fn is_feature_active(&self, name: &str) -> bool {
        match self {
            Self::Permissive => true,
            Self::Exact(active) => active.contains(name),
        }
    }

    pub fn is_permissive(&self) -> bool {
        matches!(self, Self::Permissive)
    }
}

thread_local! {
    // Analyzers read this via parse_and_filter(). Orchestrator pushes a
    // resolved FeatureSet via `with_features` before running analyzers,
    // and the push/pop contract guarantees the previous value is restored
    // even if the closure panics (via the Drop impl of the guard).
    static ACTIVE: RefCell<FeatureSet> = const { RefCell::new(FeatureSet::Permissive) };
}

/// Run `f` with `features` installed as the thread-local active feature
/// set. The previous value is saved and restored on drop, so nested calls
/// and test interleavings compose cleanly.
pub fn with_features<R>(features: FeatureSet, f: impl FnOnce() -> R) -> R {
    let _guard = FeatureGuard::push(features);
    f()
}

struct FeatureGuard {
    previous: FeatureSet,
}

impl FeatureGuard {
    fn push(next: FeatureSet) -> Self {
        let previous = ACTIVE.with(|cell| std::mem::replace(&mut *cell.borrow_mut(), next));
        Self { previous }
    }
}

impl Drop for FeatureGuard {
    fn drop(&mut self) {
        // Take `self.previous` and install it back. `std::mem::take` leaves
        // `Permissive` behind in `self.previous`, which is fine — the guard
        // is about to be dropped anyway.
        let prev = std::mem::take(&mut self.previous);
        ACTIVE.with(|cell| *cell.borrow_mut() = prev);
    }
}

/// Parse Rust source and strip items whose cfg gates are inactive under
/// the thread-local [`FeatureSet`]. Returns `None` if parsing fails, so
/// callers can fall back to their own error handling (matching the
/// existing analyzer behavior on unparseable files).
pub fn parse_and_filter(src: &str) -> Option<File> {
    let mut ast = syn::parse_file(src).ok()?;
    ACTIVE.with(|cell| {
        let set = cell.borrow();
        if !set.is_permissive() {
            filter_item_vec(&mut ast.items, &set);
        }
    });
    Some(ast)
}

fn filter_item_vec(items: &mut Vec<Item>, features: &FeatureSet) {
    items.retain(|item| item_active(item, features));
    for item in items {
        if let Item::Mod(m) = item
            && let Some((_, inner)) = &mut m.content
        {
            filter_item_vec(inner, features);
        }
    }
}

fn item_active(item: &Item, features: &FeatureSet) -> bool {
    attrs_of(item)
        .iter()
        .all(|attr| cfg_matches(attr, features))
}

/// Evaluate a single attribute. Non-cfg attributes always pass — this
/// function is a filter, not a classifier.
pub fn cfg_matches(attr: &Attribute, features: &FeatureSet) -> bool {
    if !attr.path().is_ident("cfg") {
        return true;
    }
    match attr.parse_args::<Meta>() {
        Ok(meta) => eval_meta(&meta, features),
        // Unparseable cfg → be liberal (include the item) rather than
        // silently drop code the user may be actively editing.
        Err(_) => true,
    }
}

/// Read the currently-active feature set from the thread-local. Used
/// by analyzers that need to evaluate `#[cfg_attr(...)]` predicates
/// (which `parse_and_filter` deliberately doesn't touch — it filters
/// items, not attributes).
pub(crate) fn current_features() -> FeatureSet {
    ACTIVE.with(|cell| cell.borrow().clone())
}

/// Evaluate a cfg-style predicate Meta (e.g., the first argument of
/// `#[cfg_attr(predicate, ...)]`) against a feature set. Public to
/// `crate` so `derive.rs` can handle conditional derives without
/// duplicating the eval logic.
pub(crate) fn eval_cfg_meta(meta: &Meta, features: &FeatureSet) -> bool {
    eval_meta(meta, features)
}

fn eval_meta(meta: &Meta, features: &FeatureSet) -> bool {
    match meta {
        // Bare identifiers: cfg(test), cfg(debug_assertions), cfg(miri), ...
        Meta::Path(p) => {
            if let Some(ident) = p.get_ident()
                && ident == "test"
            {
                // Test items must stay visible to the test-scanner
                // regardless of the active feature set.
                return true;
            }
            // Everything else we don't model yet: liberal.
            true
        }
        // Name-value: cfg(feature = "foo"), cfg(target_os = "linux"), ...
        Meta::NameValue(nv) => {
            let name = nv
                .path
                .get_ident()
                .map(std::string::ToString::to_string)
                .unwrap_or_default();
            if name == "feature" {
                if let syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) = &nv.value
                {
                    return features.is_feature_active(&s.value());
                }
                return true;
            }
            // target_os / target_arch / target_family / ... → liberal.
            true
        }
        // Combinators: cfg(not(x)), cfg(all(a, b)), cfg(any(a, b)).
        Meta::List(list) => {
            let combinator = list
                .path
                .get_ident()
                .map(std::string::ToString::to_string)
                .unwrap_or_default();
            match combinator.as_str() {
                "not" => list
                    .parse_args::<Meta>()
                    .map_or(true, |inner| !eval_meta(&inner, features)),
                "all" => match list.parse_args_with(parse_meta_list) {
                    Ok(inners) => inners.iter().all(|m| eval_meta(m, features)),
                    Err(_) => true,
                },
                "any" => match list.parse_args_with(parse_meta_list) {
                    Ok(inners) => inners.iter().any(|m| eval_meta(m, features)),
                    Err(_) => true,
                },
                _ => true,
            }
        }
    }
}

fn parse_meta_list(input: syn::parse::ParseStream<'_>) -> syn::Result<Vec<Meta>> {
    let punct: syn::punctuated::Punctuated<Meta, syn::Token![,]> =
        syn::punctuated::Punctuated::parse_terminated(input)?;
    Ok(punct.into_iter().collect())
}

fn attrs_of(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(i) => &i.attrs,
        Item::Enum(i) => &i.attrs,
        Item::ExternCrate(i) => &i.attrs,
        Item::Fn(i) => &i.attrs,
        Item::ForeignMod(i) => &i.attrs,
        Item::Impl(i) => &i.attrs,
        Item::Macro(i) => &i.attrs,
        Item::Mod(i) => &i.attrs,
        Item::Static(i) => &i.attrs,
        Item::Struct(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        Item::TraitAlias(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        Item::Union(i) => &i.attrs,
        Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

/// Resolve the active feature set for a run. Reads `{manifest_dir}/Cargo.toml`
/// when present; honors `--no-default-features`, unions with `user_features`,
/// and transitively expands the graph so `foo = ["bar"]` propagates.
///
/// Fallback: if the manifest can't be read and the user passed no feature
/// flags, returns [`FeatureSet::Permissive`] — v0.1 behavior.
pub fn resolve_features(
    manifest_dir: &Path,
    user_features: &[String],
    no_default: bool,
    all_features: bool,
) -> Result<FeatureSet> {
    // clap's `value_delimiter = ','` already splits the flag value, but users
    // can also reach this API programmatically — normalize once up front so
    // every downstream branch sees the same shape.
    let user_features: Vec<String> = user_features
        .iter()
        .flat_map(|s| s.split(','))
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect();

    let manifest_path = manifest_dir.join("Cargo.toml");
    let src = match std::fs::read_to_string(&manifest_path) {
        Ok(s) => s,
        Err(_) => {
            if !all_features && user_features.is_empty() {
                return Ok(FeatureSet::Permissive);
            }
            // No manifest but the user supplied features explicitly — honor
            // them as-is without transitive expansion.
            let active: BTreeSet<String> = user_features.into_iter().collect();
            return Ok(FeatureSet::Exact(active));
        }
    };

    let parsed: toml::Value = toml::from_str(&src).context("parsing Cargo.toml")?;
    let features_table = parsed.get("features").and_then(|v| v.as_table());

    if all_features {
        let mut active = BTreeSet::new();
        if let Some(table) = features_table {
            for key in table.keys() {
                if key != "default" {
                    active.insert(key.clone());
                }
            }
        }
        return Ok(FeatureSet::Exact(active));
    }

    let mut active = BTreeSet::new();
    if !no_default
        && let Some(table) = features_table
        && let Some(defaults) = table.get("default").and_then(|v| v.as_array())
    {
        for d in defaults.iter().filter_map(|v| v.as_str()) {
            if let Some(name) = same_crate_feature_name(d) {
                active.insert(name);
            }
        }
    }

    for f in user_features {
        active.insert(f);
    }

    expand_transitive(&mut active, features_table);
    Ok(FeatureSet::Exact(active))
}

fn expand_transitive(active: &mut BTreeSet<String>, table: Option<&toml::value::Table>) {
    let Some(table) = table else { return };
    loop {
        let before = active.len();
        let snapshot: Vec<String> = active.iter().cloned().collect();
        for feature in snapshot {
            let Some(implied) = table.get(&feature).and_then(|v| v.as_array()) else {
                continue;
            };
            for s in implied.iter().filter_map(|v| v.as_str()) {
                if let Some(name) = same_crate_feature_name(s) {
                    active.insert(name);
                }
            }
        }
        if active.len() == before {
            break;
        }
    }
}

/// Cargo feature-string syntax lets users write several things in one
/// place:
/// * `"feature_name"` — another feature in the same crate
/// * `"dep_name"` — activate the optional dep's auto feature
/// * `"dep:dep_name"` — activate the optional dep without exposing a feature flag
/// * `"crate/feature"` — activate `feature` on dependency `crate`
/// * `"crate?/feature"` — same, but only if `crate` is itself enabled
///
/// For cfg evaluation we only care about same-crate feature names. Return
/// `None` for anything that refers to a different crate or a `dep:`
/// activation.
fn same_crate_feature_name(s: &str) -> Option<String> {
    if s.starts_with("dep:") || s.contains('/') {
        return None;
    }
    Some(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn set(items: &[&str]) -> FeatureSet {
        FeatureSet::Exact(items.iter().map(|s| (*s).to_string()).collect())
    }

    // --- FeatureSet basics ---

    #[test]
    fn permissive_treats_every_cfg_as_active() {
        assert!(FeatureSet::Permissive.is_feature_active("anything"));
    }

    #[test]
    fn exact_respects_membership() {
        let s = set(&["foo"]);
        assert!(s.is_feature_active("foo"));
        assert!(!s.is_feature_active("bar"));
    }

    // --- cfg evaluation ---

    fn eval(src: &str, set: &FeatureSet) -> bool {
        let ast: File = syn::parse_str(src).expect("parse");
        let item = ast.items.into_iter().next().expect("one item");
        attrs_of(&item).iter().all(|a| cfg_matches(a, set))
    }

    #[test]
    fn feature_cfg_matches_when_active() {
        assert!(eval(
            "#[cfg(feature = \"tokio\")] fn f() {}",
            &set(&["tokio"])
        ));
    }

    #[test]
    fn feature_cfg_rejects_when_inactive() {
        assert!(!eval(
            "#[cfg(feature = \"tokio\")] fn f() {}",
            &set(&["async-std"])
        ));
    }

    #[test]
    fn cfg_test_is_always_active() {
        // So the test-scanner keeps seeing `#[cfg(test)] mod tests`.
        assert!(eval("#[cfg(test)] fn f() {}", &set(&[])));
    }

    #[test]
    fn target_cfgs_stay_permissive() {
        // We don't model target triples yet — be liberal.
        assert!(eval("#[cfg(target_os = \"linux\")] fn f() {}", &set(&[])));
    }

    #[test]
    fn not_negates() {
        assert!(eval(
            "#[cfg(not(feature = \"foo\"))] fn f() {}",
            &set(&["bar"])
        ));
        assert!(!eval(
            "#[cfg(not(feature = \"foo\"))] fn f() {}",
            &set(&["foo"])
        ));
    }

    #[test]
    fn all_requires_every_inner() {
        assert!(eval(
            "#[cfg(all(feature = \"a\", feature = \"b\"))] fn f() {}",
            &set(&["a", "b"])
        ));
        assert!(!eval(
            "#[cfg(all(feature = \"a\", feature = \"b\"))] fn f() {}",
            &set(&["a"])
        ));
    }

    #[test]
    fn any_requires_at_least_one_inner() {
        assert!(eval(
            "#[cfg(any(feature = \"a\", feature = \"b\"))] fn f() {}",
            &set(&["b"])
        ));
        assert!(!eval(
            "#[cfg(any(feature = \"a\", feature = \"b\"))] fn f() {}",
            &set(&["c"])
        ));
    }

    #[test]
    fn nested_combinators() {
        assert!(eval(
            "#[cfg(all(not(feature = \"off\"), any(feature = \"a\", feature = \"b\")))] fn f() {}",
            &set(&["a"])
        ));
        assert!(!eval(
            "#[cfg(all(not(feature = \"off\"), feature = \"a\"))] fn f() {}",
            &set(&["off", "a"])
        ));
    }

    // --- AST filtering ---

    #[test]
    fn parse_and_filter_strips_inactive_items_under_with_features() {
        let src = "\
            fn always() {}\n\
            #[cfg(feature = \"tokio\")] fn only_tokio() {}\n\
            #[cfg(feature = \"async-std\")] fn only_astd() {}\n\
        ";
        let ast = with_features(set(&["tokio"]), || parse_and_filter(src).unwrap());
        let names: Vec<_> = ast
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Fn(f) => Some(f.sig.ident.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["always", "only_tokio"]);
    }

    #[test]
    fn permissive_default_keeps_all_items() {
        let src = "\
            fn a() {}\n\
            #[cfg(feature = \"tokio\")] fn b() {}\n\
        ";
        let ast = parse_and_filter(src).unwrap();
        assert_eq!(ast.items.len(), 2);
    }

    #[test]
    fn with_features_restores_previous_on_drop() {
        with_features(set(&["foo"]), || {
            ACTIVE.with(|c| assert!(c.borrow().is_feature_active("foo")));
        });
        // Previous = Permissive.
        ACTIVE.with(|c| assert!(c.borrow().is_permissive()));
    }

    #[test]
    fn filter_recurses_into_inline_modules() {
        let src = "\
            mod outer {\n\
                fn keep() {}\n\
                #[cfg(feature = \"off\")] fn drop_me() {}\n\
            }\n\
        ";
        let ast = with_features(set(&[]), || parse_and_filter(src).unwrap());
        let Some(Item::Mod(m)) = ast.items.first() else {
            panic!("expected mod");
        };
        let inner = &m.content.as_ref().unwrap().1;
        assert_eq!(inner.len(), 1);
        matches!(&inner[0], Item::Fn(f) if f.sig.ident == "keep");
    }

    // --- Cargo.toml feature resolution ---

    fn write_manifest(body: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("Cargo.toml"), body).unwrap();
        dir
    }

    #[test]
    fn resolve_uses_default_features_when_allowed() {
        let dir = write_manifest(
            "\
            [package]\nname=\"x\"\nversion=\"0.1.0\"\nedition=\"2021\"\n\
            [features]\ndefault = [\"a\"]\na = []\nb = []\n",
        );
        let f = resolve_features(dir.path(), &[], false, false).unwrap();
        assert!(f.is_feature_active("a"));
        assert!(!f.is_feature_active("b"));
    }

    #[test]
    fn no_default_features_skips_defaults() {
        let dir = write_manifest(
            "\
            [package]\nname=\"x\"\nversion=\"0.1.0\"\nedition=\"2021\"\n\
            [features]\ndefault = [\"a\"]\na = []\n",
        );
        let f = resolve_features(dir.path(), &[], true, false).unwrap();
        assert!(!f.is_feature_active("a"));
    }

    #[test]
    fn user_features_union_with_defaults() {
        let dir = write_manifest(
            "\
            [package]\nname=\"x\"\nversion=\"0.1.0\"\nedition=\"2021\"\n\
            [features]\ndefault = [\"a\"]\na = []\nb = []\n",
        );
        let f = resolve_features(dir.path(), &["b".into()], false, false).unwrap();
        assert!(f.is_feature_active("a"));
        assert!(f.is_feature_active("b"));
    }

    #[test]
    fn transitive_expansion_propagates() {
        let dir = write_manifest(
            "\
            [package]\nname=\"x\"\nversion=\"0.1.0\"\nedition=\"2021\"\n\
            [features]\ndefault=[]\n\
            tokio = [\"async-runtime\"]\n\
            async-runtime = [\"io\"]\n\
            io = []\n",
        );
        let f = resolve_features(dir.path(), &["tokio".into()], false, false).unwrap();
        assert!(f.is_feature_active("tokio"));
        assert!(f.is_feature_active("async-runtime"));
        assert!(f.is_feature_active("io"));
    }

    #[test]
    fn all_features_activates_every_non_default_entry() {
        let dir = write_manifest(
            "\
            [package]\nname=\"x\"\nversion=\"0.1.0\"\nedition=\"2021\"\n\
            [features]\ndefault=[\"a\"]\na=[]\nb=[]\nc=[]\n",
        );
        let f = resolve_features(dir.path(), &[], false, true).unwrap();
        assert!(f.is_feature_active("a"));
        assert!(f.is_feature_active("b"));
        assert!(f.is_feature_active("c"));
        // "default" itself is a meta-feature, not a real flag to activate.
        assert!(!f.is_feature_active("default"));
    }

    #[test]
    fn missing_manifest_falls_back_to_permissive() {
        let dir = TempDir::new().unwrap();
        let f = resolve_features(dir.path(), &[], false, false).unwrap();
        assert!(f.is_permissive());
    }

    #[test]
    fn missing_manifest_with_explicit_features_uses_exact() {
        let dir = TempDir::new().unwrap();
        let f = resolve_features(dir.path(), &["x".into()], false, false).unwrap();
        assert!(!f.is_permissive());
        assert!(f.is_feature_active("x"));
    }

    #[test]
    fn cross_crate_and_dep_prefixed_features_are_skipped() {
        let dir = write_manifest(
            "\
            [package]\nname=\"x\"\nversion=\"0.1.0\"\nedition=\"2021\"\n\
            [features]\ndefault=[]\n\
            full = [\"dep:serde\", \"tokio/rt\", \"self_feature\"]\n\
            self_feature = []\n",
        );
        let f = resolve_features(dir.path(), &["full".into()], false, false).unwrap();
        assert!(f.is_feature_active("full"));
        assert!(f.is_feature_active("self_feature"));
        assert!(!f.is_feature_active("serde"));
        assert!(!f.is_feature_active("tokio"));
    }

    #[test]
    fn comma_separated_user_feature_list_is_split() {
        let dir = TempDir::new().unwrap();
        let f = resolve_features(dir.path(), &["foo,bar,baz".into()], false, false).unwrap();
        assert!(f.is_feature_active("foo"));
        assert!(f.is_feature_active("bar"));
        assert!(f.is_feature_active("baz"));
    }
}
