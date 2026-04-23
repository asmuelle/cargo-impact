//! `.impactignore` path filtering.
//!
//! A gitignore-shaped file at the workspace root. Each non-empty,
//! non-comment line is a pattern; a finding whose target path matches
//! any pattern is dropped before being emitted. This lets users
//! carve out paths whose blast radius is noise — vendored code,
//! auto-generated files, third-party shims, etc.
//!
//! Supported shape (v0.3-alpha — deliberately a subset of gitignore)
//! ----------------------------------------------------------------
//! * Blank lines and lines starting with `#` are ignored.
//! * Leading/trailing whitespace is trimmed.
//! * A pattern matches if either:
//!     - The target's string form contains the pattern literally, or
//!     - The pattern contains `*` as a glob wildcard, matched against
//!       any single path component.
//!
//! Deliberately *not* supported yet:
//! * Negation (`!pattern`) — can be added in a v0.4 pass when we see
//!   real usage needing it.
//! * `**` recursive wildcards — `*` matching a component is the
//!   dominant case; recursive matches add complexity we don't need.
//! * Absolute vs relative distinctions — all patterns match against
//!   the finding's path.to_string_lossy() repo-relative form.
//!
//! The parser is paranoid about failures: if the file is missing,
//! empty, or unparseable, the resulting matcher is `empty()` (matches
//! nothing). A malformed `.impactignore` never kills a run.

use std::path::Path;

const IGNORE_FILENAME: &str = ".impactignore";

/// Path-pattern matcher built from an `.impactignore` file.
#[derive(Debug, Clone, Default)]
pub struct IgnoreSet {
    patterns: Vec<Pattern>,
}

#[derive(Debug, Clone)]
enum Pattern {
    /// Match if the target string contains this needle verbatim.
    Literal(String),
    /// Match if any path component equals this value (treating `*` as a
    /// single-component wildcard via [`glob_matches`]).
    Glob(String),
}

impl IgnoreSet {
    /// Empty matcher — matches nothing. Equivalent to "no ignore file".
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load `{root}/.impactignore` if present. Missing file → empty
    /// matcher. Unreadable or malformed file → empty matcher with a
    /// stderr warning.
    pub fn load(root: &Path) -> Self {
        let path = root.join(IGNORE_FILENAME);
        let Ok(src) = std::fs::read_to_string(&path) else {
            return Self::empty();
        };
        Self::parse(&src)
    }

    /// Parse the contents of an `.impactignore` file into a matcher.
    /// Exposed for unit tests; prefer [`load`] in production.
    pub fn parse(src: &str) -> Self {
        let patterns = src
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| {
                if l.contains('*') {
                    Pattern::Glob(l.to_string())
                } else {
                    Pattern::Literal(l.to_string())
                }
            })
            .collect();
        Self { patterns }
    }

    /// True when the path matches any pattern in this set. Always `false`
    /// for the empty matcher (hot path — no allocations in that case).
    pub fn is_ignored(&self, path: &Path) -> bool {
        if self.patterns.is_empty() {
            return false;
        }
        let s = path.to_string_lossy();
        let s_normalized = s.replace('\\', "/");
        self.patterns.iter().any(|p| match p {
            Pattern::Literal(needle) => s_normalized.contains(needle),
            Pattern::Glob(pat) => {
                s_normalized
                .split('/')
                .any(|component| glob_matches(pat, component))
                // Also match the full string — lets users write
                // `target/*` and have it fire against `target/x.rs`
                // in addition to a bare `target` component match.
                || glob_matches(pat, &s_normalized)
            }
        })
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.patterns.len()
    }
}

/// Match `pattern` containing `*` wildcards against `input`. Each `*`
/// matches any run of characters including the empty string. No `?`,
/// no character classes, no recursion — deliberately minimal.
fn glob_matches(pattern: &str, input: &str) -> bool {
    // Backtracking implementation sized for the patterns we expect
    // (a handful of wildcards per line). For thousands of patterns or
    // highly ambiguous ones a Thompson-style NFA would be better —
    // revisit if profiling shows this being a hot spot.
    let p: Vec<char> = pattern.chars().collect();
    let i: Vec<char> = input.chars().collect();
    glob_inner(&p, &i, 0, 0)
}

fn glob_inner(p: &[char], i: &[char], pi: usize, ii: usize) -> bool {
    if pi == p.len() {
        return ii == i.len();
    }
    if p[pi] == '*' {
        // Try matching `*` against 0, 1, 2, ... more characters of `i`.
        for skip in ii..=i.len() {
            if glob_inner(p, i, pi + 1, skip) {
                return true;
            }
        }
        false
    } else if ii < i.len() && p[pi] == i[ii] {
        glob_inner(p, i, pi + 1, ii + 1)
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn empty_matcher_never_matches() {
        let m = IgnoreSet::empty();
        assert!(!m.is_ignored(Path::new("src/lib.rs")));
        assert!(!m.is_ignored(Path::new("anything/at/all")));
        assert!(m.is_empty());
    }

    #[test]
    fn literal_pattern_matches_substring() {
        let m = IgnoreSet::parse("vendor\n");
        assert!(m.is_ignored(Path::new("vendor/foo.rs")));
        assert!(m.is_ignored(Path::new("src/vendor/bar.rs")));
        assert!(!m.is_ignored(Path::new("src/lib.rs")));
    }

    #[test]
    fn glob_matches_single_component() {
        let m = IgnoreSet::parse("*.generated.rs\n");
        assert!(m.is_ignored(Path::new("src/api.generated.rs")));
        assert!(m.is_ignored(Path::new("target/api.generated.rs")));
        assert!(!m.is_ignored(Path::new("src/api.rs")));
    }

    #[test]
    fn comments_and_blank_lines_ignored() {
        let src = "\
# this is a comment
\n\
   # leading whitespace comment
\n\
vendor\n\
\n\
# another comment\n\
";
        let m = IgnoreSet::parse(src);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn missing_file_yields_empty_matcher() {
        let dir = tempfile::TempDir::new().unwrap();
        let m = IgnoreSet::load(dir.path());
        assert!(m.is_empty());
    }

    #[test]
    fn load_reads_file_at_root() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(".impactignore"),
            "vendor\ntarget\n*.generated.rs\n",
        )
        .unwrap();
        let m = IgnoreSet::load(dir.path());
        assert_eq!(m.len(), 3);
        assert!(m.is_ignored(Path::new("vendor/lib.rs")));
        assert!(m.is_ignored(Path::new("target/debug/build.rs")));
        assert!(m.is_ignored(Path::new("api.generated.rs")));
    }

    #[test]
    fn windows_style_paths_are_normalized() {
        let m = IgnoreSet::parse("vendor\n");
        // PathBuf from a Windows-style string round-trips as such, and
        // we normalize backslashes to forward slashes before matching.
        let p = PathBuf::from("src\\vendor\\mod.rs");
        assert!(m.is_ignored(&p));
    }

    #[test]
    fn glob_matches_helper_covers_wildcard_semantics() {
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("*.rs", "lib.rs"));
        assert!(glob_matches("a*b", "axxxb"));
        assert!(glob_matches("a*b", "ab"));
        assert!(!glob_matches("a*b", "ac"));
        assert!(glob_matches("prefix*", "prefix_and_more"));
        assert!(!glob_matches("exact", "exactly"));
    }
}
