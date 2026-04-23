use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::Path;
use syn::Item;

/// Categorization of a top-level Rust item. Trait ripple and dyn-dispatch
/// analysis filter on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Fn,
    Struct,
    Enum,
    Trait,
    Const,
    Static,
    TypeAlias,
    Union,
    Mod,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopLevelSymbol {
    pub name: String,
    pub kind: SymbolKind,
}

/// Extract every top-level item from a Rust source file, tagged with its kind.
///
/// v0.2 precision remains file-level: any change to the file is assumed to
/// affect every item defined within. Span-accurate hunk mapping arrives later.
pub fn top_level_symbols(file: &Path) -> Result<Vec<TopLevelSymbol>> {
    let src = fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let ast =
        crate::cfg::parse_and_filter(&src).ok_or_else(|| anyhow!("parsing {}", file.display()))?;
    let mut out = Vec::new();
    collect_items(&ast.items, &mut out);
    Ok(out)
}

fn collect_items(items: &[Item], out: &mut Vec<TopLevelSymbol>) {
    for item in items {
        let sym = match item {
            Item::Fn(f) => Some((f.sig.ident.to_string(), SymbolKind::Fn)),
            Item::Struct(s) => Some((s.ident.to_string(), SymbolKind::Struct)),
            Item::Enum(e) => Some((e.ident.to_string(), SymbolKind::Enum)),
            Item::Trait(t) => Some((t.ident.to_string(), SymbolKind::Trait)),
            Item::Const(c) => Some((c.ident.to_string(), SymbolKind::Const)),
            Item::Static(s) => Some((s.ident.to_string(), SymbolKind::Static)),
            Item::Type(t) => Some((t.ident.to_string(), SymbolKind::TypeAlias)),
            Item::Union(u) => Some((u.ident.to_string(), SymbolKind::Union)),
            Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    collect_items(inner, out);
                }
                Some((m.ident.to_string(), SymbolKind::Mod))
            }
            _ => None,
        };
        if let Some((name, kind)) = sym {
            out.push(TopLevelSymbol { name, kind });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(body: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(".rs")
            .tempfile()
            .expect("tempfile");
        f.write_all(body.as_bytes()).expect("write");
        f
    }

    #[test]
    fn tags_each_item_with_kind() {
        let f = write_temp(
            "pub fn foo() {}\n\
             struct Bar;\n\
             enum Baz { A, B }\n\
             trait Quux {}\n",
        );
        let syms = top_level_symbols(f.path()).unwrap();
        let by_name: std::collections::HashMap<_, _> =
            syms.iter().map(|s| (s.name.as_str(), s.kind)).collect();
        assert_eq!(by_name["foo"], SymbolKind::Fn);
        assert_eq!(by_name["Bar"], SymbolKind::Struct);
        assert_eq!(by_name["Baz"], SymbolKind::Enum);
        assert_eq!(by_name["Quux"], SymbolKind::Trait);
    }

    #[test]
    fn descends_into_inline_modules() {
        let f =
            write_temp("mod outer {\n    pub fn inner_fn() {}\n    pub struct InnerStruct;\n}\n");
        let syms = top_level_symbols(f.path()).unwrap();
        let names: Vec<_> = syms.iter().map(|s| s.name.clone()).collect();
        assert!(names.iter().any(|n| n == "outer"));
        assert!(names.iter().any(|n| n == "inner_fn"));
        assert!(names.iter().any(|n| n == "InnerStruct"));
    }

    #[test]
    fn rejects_unparseable_source() {
        let f = write_temp("this is not valid rust !!!");
        let err = top_level_symbols(f.path()).unwrap_err();
        assert!(format!("{err:#}").contains("parsing"));
    }
}
