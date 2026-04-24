//! Per-reference context classification for RA-resolved references.
//!
//! Rust-analyzer tells us *that* a reference exists at `(file, line)`.
//! This module tells us *where* within the referencing code it sits —
//! inside a test fn, inside an impl block, or plain caller code —
//! which changes the severity class of the finding. All three are
//! still `Proven`-tier (RA resolved the name); we're refining the
//! impact class, not the confidence.
//!
//! Classification rules, in precedence order:
//!
//! 1. **TestFn** — the reference's enclosing fn carries `#[test]` /
//!    `#[tokio::test]` / `#[rstest]` / similar, OR any enclosing
//!    module carries `#[cfg(test)]`, OR an enclosing module is named
//!    `tests`. Severity downgraded to `Low` — test-only breakage can
//!    be updated alongside the change and doesn't propagate downstream.
//!
//! 2. **ImplBlock** — the reference sits inside an `impl` block
//!    (inherent or trait-impl). Severity upgraded to `High` — if the
//!    referenced symbol is a trait or trait-associated type, an impl
//!    breakage propagates to every downstream consumer.
//!
//! 3. **Caller** — anywhere else. Severity stays at the default
//!    `Medium` from `ResolvedReference`.
//!
//! Parse is on-demand and uncached (called per-file, not per-ref); if
//! a file has N references we still parse once and walk N times. For
//! pathological cases we could add a small LRU, but RA runs are
//! already dominated by LSP overhead.

use crate::finding::SeverityClass;
use std::path::Path;
use syn::spanned::Spanned;

/// Three-way classification of where an RA reference sits.
///
/// Equality for tests; `Copy` because the enum is two bits of state
/// and we pass it through by value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceContext {
    /// The reference is inside a `#[test]` fn or a test module.
    TestFn,
    /// The reference is inside an `impl` block (inherent or trait-impl).
    ImplBlock,
    /// Plain caller code — top-level fn, module-level const, etc.
    Caller,
}

impl ReferenceContext {
    /// Refined severity for an RA-resolved reference. The default from
    /// `FindingKind::ResolvedReference::default_severity()` is `Medium`;
    /// this adjusts it up (impl sites break harder) or down (test-only
    /// references matter less for downstream risk assessment).
    pub fn refined_severity(self) -> SeverityClass {
        match self {
            Self::TestFn => SeverityClass::Low,
            Self::ImplBlock => SeverityClass::High,
            Self::Caller => SeverityClass::Medium,
        }
    }
}

/// Classify a single `(file, line)` reference. Reads the file, parses
/// it with syn, and walks the AST until it finds the smallest
/// enclosing container at `line_1based`.
///
/// `line_1based` matches the user-facing "line 42" convention. RA's
/// `textDocument/references` returns 0-based; convert at the call site.
///
/// Returns `Caller` on any failure (read error, parse error, no
/// enclosing fn/impl). Never errors — severity refinement is
/// best-effort; a miss just means the default severity is kept.
pub fn classify(file_abs: &Path, line_1based: u32) -> ReferenceContext {
    let Ok(src) = std::fs::read_to_string(file_abs) else {
        return ReferenceContext::Caller;
    };
    let Ok(ast) = syn::parse_file(&src) else {
        return ReferenceContext::Caller;
    };
    classify_in_file(&ast, line_1based)
}

fn classify_in_file(ast: &syn::File, line_1based: u32) -> ReferenceContext {
    let mut walker = Walker {
        line: line_1based,
        in_test_mod: false,
        best: ReferenceContext::Caller,
    };
    walker.visit_items(&ast.items);
    walker.best
}

struct Walker {
    line: u32,
    in_test_mod: bool,
    best: ReferenceContext,
}

impl Walker {
    fn visit_items(&mut self, items: &[syn::Item]) {
        for item in items {
            self.visit_item(item);
        }
    }

    fn visit_item(&mut self, item: &syn::Item) {
        let span = item.span();
        let start = span.start().line as u32;
        let end = span.end().line as u32;
        if self.line < start || self.line > end {
            return;
        }
        match item {
            // Nested fns / blocks carry their own attributes but
            // rarely enclose references at a finer granularity worth
            // classifying. The current best wins unless a more
            // specific enclosure upgrades it below.
            syn::Item::Fn(f) if has_test_attr(&f.attrs) || self.in_test_mod => {
                self.best = ReferenceContext::TestFn;
            }
            syn::Item::Fn(_) => {}
            syn::Item::Impl(i) => {
                // TestFn has precedence over ImplBlock: a reference
                // inside an impl that lives in a test module is still
                // test code, and a #[test] inside an impl block is
                // still a test. Only bump to ImplBlock when neither
                // context applies.
                if self.in_test_mod || matches!(self.best, ReferenceContext::TestFn) {
                    self.best = ReferenceContext::TestFn;
                } else {
                    self.best = ReferenceContext::ImplBlock;
                }
                // Descend into impl items — a fn inside an impl may
                // itself be `#[test]` (rare, but possible via
                // mockall-style patterns).
                for ii in &i.items {
                    if let syn::ImplItem::Fn(f) = ii {
                        let span = f.span();
                        let start = span.start().line as u32;
                        let end = span.end().line as u32;
                        if self.line >= start && self.line <= end && has_test_attr(&f.attrs) {
                            self.best = ReferenceContext::TestFn;
                        }
                    }
                }
            }
            syn::Item::Mod(m) => {
                let was_test = self.in_test_mod;
                if is_test_module(m) {
                    self.in_test_mod = true;
                }
                if let Some((_, inner)) = &m.content {
                    self.visit_items(inner);
                }
                self.in_test_mod = was_test;
            }
            _ => {}
        }
    }
}

/// Attribute is one of the common test markers. We match by the
/// attribute's last path segment to handle both `#[test]` and
/// `#[tokio::test]` / `#[rstest::rstest]` without enumerating crates.
fn has_test_attr(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        let Some(last) = a.path().segments.last() else {
            return false;
        };
        matches!(
            last.ident.to_string().as_str(),
            "test" | "tokio_test" | "rstest" | "proptest" | "bench"
        )
    })
}

/// A module is a "test module" if it carries `#[cfg(test)]` or is
/// named `tests` (by Rust convention). Either is sufficient — the two
/// often overlap but aren't equivalent.
fn is_test_module(m: &syn::ItemMod) -> bool {
    if m.ident == "tests" {
        return true;
    }
    m.attrs.iter().any(|a| {
        if !a.path().is_ident("cfg") {
            return false;
        }
        // #[cfg(test)] has a single meta-item with ident `test`.
        a.parse_args::<syn::Ident>()
            .map(|id| id == "test")
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> syn::File {
        syn::parse_str(src).expect("parse")
    }

    #[test]
    fn plain_top_level_fn_is_caller() {
        let ast = parse("fn hello() {\n    let x = 1;\n    foo(x);\n}\n");
        assert_eq!(classify_in_file(&ast, 3), ReferenceContext::Caller);
    }

    #[test]
    fn test_attribute_on_fn_flags_test_fn() {
        let ast = parse("#[test]\nfn t_smoke() {\n    foo();\n}\n");
        assert_eq!(classify_in_file(&ast, 3), ReferenceContext::TestFn);
    }

    #[test]
    fn tokio_test_attribute_flags_test_fn() {
        let ast = parse("#[tokio::test]\nasync fn t_async() {\n    foo();\n}\n");
        assert_eq!(classify_in_file(&ast, 3), ReferenceContext::TestFn);
    }

    #[test]
    fn rstest_attribute_flags_test_fn() {
        let ast = parse("#[rstest]\nfn t_param() {\n    foo();\n}\n");
        assert_eq!(classify_in_file(&ast, 3), ReferenceContext::TestFn);
    }

    #[test]
    fn fn_inside_tests_module_is_test_fn() {
        let ast = parse("mod tests {\n    fn helper() {\n        foo();\n    }\n}\n");
        assert_eq!(classify_in_file(&ast, 3), ReferenceContext::TestFn);
    }

    #[test]
    fn fn_inside_cfg_test_module_is_test_fn() {
        let ast = parse("#[cfg(test)]\nmod inner {\n    fn helper() {\n        foo();\n    }\n}\n");
        assert_eq!(classify_in_file(&ast, 4), ReferenceContext::TestFn);
    }

    #[test]
    fn impl_block_method_is_impl_block() {
        let ast = parse("struct W;\nimpl W {\n    fn hi(&self) {\n        foo();\n    }\n}\n");
        assert_eq!(classify_in_file(&ast, 4), ReferenceContext::ImplBlock);
    }

    #[test]
    fn trait_impl_method_is_impl_block() {
        let ast = parse(
            "trait T { fn hi(&self); }\nstruct W;\nimpl T for W {\n    fn hi(&self) {\n        foo();\n    }\n}\n",
        );
        assert_eq!(classify_in_file(&ast, 5), ReferenceContext::ImplBlock);
    }

    #[test]
    fn test_fn_inside_impl_beats_impl_block() {
        // If a #[test] fn lives inside an impl block, TestFn wins —
        // the impl enclosure is syntactic but the functional intent
        // is a test.
        let ast = parse(
            "struct W;\nimpl W {\n    #[test]\n    fn t_inner() {\n        foo();\n    }\n}\n",
        );
        assert_eq!(classify_in_file(&ast, 5), ReferenceContext::TestFn);
    }

    #[test]
    fn nested_cfg_test_mod_propagates_into_inner_impl() {
        let ast = parse(
            "#[cfg(test)]\nmod tests {\n    struct W;\n    impl W {\n        fn helper(&self) {\n            foo();\n        }\n    }\n}\n",
        );
        // Reference is inside a test mod AND an impl — test wins per
        // precedence.
        assert_eq!(classify_in_file(&ast, 6), ReferenceContext::TestFn);
    }

    #[test]
    fn line_outside_any_container_stays_caller() {
        let ast = parse("fn a() {}\nfn b() {}\n");
        // Line 100 is past the file; walker never matches.
        assert_eq!(classify_in_file(&ast, 100), ReferenceContext::Caller);
    }

    #[test]
    fn refined_severity_mapping() {
        assert_eq!(
            ReferenceContext::TestFn.refined_severity(),
            SeverityClass::Low
        );
        assert_eq!(
            ReferenceContext::ImplBlock.refined_severity(),
            SeverityClass::High
        );
        assert_eq!(
            ReferenceContext::Caller.refined_severity(),
            SeverityClass::Medium
        );
    }

    #[test]
    fn classify_on_missing_file_returns_caller() {
        let nonexistent = Path::new("/definitely/does/not/exist-xyz.rs");
        assert_eq!(classify(nonexistent, 1), ReferenceContext::Caller);
    }
}
