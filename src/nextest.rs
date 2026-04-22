use crate::tests_scan::AffectedTest;

/// Build a `cargo-nextest` filter expression matching all affected tests,
/// e.g. `test(auth_login) + test(api_smoke)`. Returns an empty string when
/// no tests are affected so callers can cheaply detect the no-op case.
pub fn filter_expression(tests: &[AffectedTest]) -> String {
    if tests.is_empty() {
        return String::new();
    }
    tests
        .iter()
        .map(|t| format!("test({})", t.name))
        .collect::<Vec<_>>()
        .join(" + ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mk(name: &str) -> AffectedTest {
        AffectedTest {
            name: name.into(),
            file: PathBuf::from("tests/t.rs"),
            matched_symbols: vec![],
        }
    }

    #[test]
    fn empty_input_yields_empty_filter() {
        assert_eq!(filter_expression(&[]), "");
    }

    #[test]
    fn single_test() {
        assert_eq!(filter_expression(&[mk("foo")]), "test(foo)");
    }

    #[test]
    fn multiple_tests_joined_with_or() {
        let out = filter_expression(&[mk("a"), mk("b"), mk("c")]);
        assert_eq!(out, "test(a) + test(b) + test(c)");
    }
}
