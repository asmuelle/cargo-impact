//! Debug breadcrumbs for best-effort cleanup paths.
//!
//! Several sites in the crate intentionally ignore errors (cache
//! persistence, child-process teardown) because failure there must never
//! fail the run. To keep those failures diagnosable, they emit a stderr
//! breadcrumb when the `CARGO_IMPACT_DEBUG` environment variable is set.

/// True when the `CARGO_IMPACT_DEBUG` environment variable is set.
pub(crate) fn enabled() -> bool {
    std::env::var_os("CARGO_IMPACT_DEBUG").is_some()
}

/// Print a breadcrumb to stderr when `CARGO_IMPACT_DEBUG` is set.
///
/// Keeps the same `cargo-impact:` prefix as the crate's user-facing
/// notices, tagged `[debug]` so the two are distinguishable.
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if crate::debug::enabled() {
            eprintln!("cargo-impact[debug]: {}", format_args!($($arg)*));
        }
    };
}

pub(crate) use debug_log;

#[cfg(test)]
mod tests {
    // `enabled()` reads the process environment; mutating env vars in
    // tests is racy across threads, so just assert it answers based on
    // the current environment without panicking.
    #[test]
    fn enabled_reflects_environment() {
        let expected = std::env::var_os("CARGO_IMPACT_DEBUG").is_some();
        assert_eq!(super::enabled(), expected);
    }
}
