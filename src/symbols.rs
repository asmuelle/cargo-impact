use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use syn::{File, Item};

/// Extract the names of all top-level items defined in a Rust source file.
///
/// This is v0.1 MVP precision: any change to the file is assumed to affect
/// every top-level item defined within. Span-accurate hunk-to-item mapping
/// arrives in v0.2.
pub fn top_level_symbol_names(file: &Path) -> Result<Vec<String>> {
    let src = fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let ast: File = syn::parse_file(&src).with_context(|| format!("parsing {}", file.display()))?;
    let mut names = Vec::new();
    collect_item_names(&ast.items, &mut names);
    Ok(names)
}

fn collect_item_names(items: &[Item], out: &mut Vec<String>) {
    for item in items {
        match item {
            Item::Fn(f) => out.push(f.sig.ident.to_string()),
            Item::Struct(s) => out.push(s.ident.to_string()),
            Item::Enum(e) => out.push(e.ident.to_string()),
            Item::Trait(t) => out.push(t.ident.to_string()),
            Item::Const(c) => out.push(c.ident.to_string()),
            Item::Static(s) => out.push(s.ident.to_string()),
            Item::Type(t) => out.push(t.ident.to_string()),
            Item::Union(u) => out.push(u.ident.to_string()),
            Item::Mod(m) => {
                out.push(m.ident.to_string());
                if let Some((_, inner)) = &m.content {
                    collect_item_names(inner, out);
                }
            }
            // impl blocks, use statements, extern blocks, and macros don't
            // introduce a single named symbol we can track here.
            _ => {}
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
    fn extracts_top_level_fn_struct_enum_trait() {
        let f = write_temp(
            "pub fn foo() {}\n\
             struct Bar;\n\
             enum Baz { A, B }\n\
             trait Quux {}\n",
        );
        let names = top_level_symbol_names(f.path()).unwrap();
        for expected in ["foo", "Bar", "Baz", "Quux"] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing {expected} in {names:?}"
            );
        }
    }

    #[test]
    fn descends_into_inline_modules() {
        let f =
            write_temp("mod outer {\n    pub fn inner_fn() {}\n    pub struct InnerStruct;\n}\n");
        let names = top_level_symbol_names(f.path()).unwrap();
        assert!(names.iter().any(|n| n == "outer"));
        assert!(names.iter().any(|n| n == "inner_fn"));
        assert!(names.iter().any(|n| n == "InnerStruct"));
    }

    #[test]
    fn rejects_unparseable_source() {
        let f = write_temp("this is not valid rust !!!");
        let err = top_level_symbol_names(f.path()).unwrap_err();
        assert!(format!("{err:#}").contains("parsing"));
    }
}
