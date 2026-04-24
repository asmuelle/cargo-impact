//! `cargo-impact.toml` — project-level flag defaults.
//!
//! Lives at the workspace root. Every field in the `[defaults]` table
//! maps to a CLI flag on [`crate::ImpactArgs`]. Precedence (lowest →
//! highest): hardcoded defaults → config file → CLI flags.
//!
//! Without this file, CI users alias shell commands as a workaround.
//! With it, a team can commit `cargo-impact.toml` to the repo once
//! and every collaborator's `cargo impact` invocation picks up the
//! same `--fail-on`, `--semver-checks`, feature matrix, etc.
//!
//! Minimal schema — v0.3-alpha
//! ---------------------------
//! ```toml
//! [defaults]
//! confidence_min = 0.6
//! fail_on = "high"            # "high" | "medium" | "low"
//! semver_checks = true
//! rust_analyzer = false
//! features = ["tokio", "rt"]
//! all_features = false
//! no_default_features = false
//! budget = 32000
//! ```
//!
//! Intentionally omitted fields (always CLI-driven, never config):
//! * `test` — mode-of-use, not a default preference
//! * `context` — same
//! * `format` — per-invocation output choice
//! * `since` — per-invocation git ref
//! * `manifest_dir` — path is per-run, not team-wide
//!
//! Load failures are non-fatal: unreadable / malformed / invalid-type
//! config file emits a stderr notice and continues with the CLI values
//! (which include clap's hardcoded defaults). The goal is to help, not
//! to block.

use crate::{FailOn, ImpactArgs};
use serde::Deserialize;
use std::path::Path;

const CONFIG_FILENAME: &str = "cargo-impact.toml";

/// Parsed `cargo-impact.toml`. Every field is optional — absence means
/// "let the CLI flag's default win."
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub defaults: Defaults,
}

/// Per-flag overrides. Field names mirror [`ImpactArgs`] so serde maps
/// them directly.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Defaults {
    pub confidence_min: Option<f64>,
    /// Accepts `"high"`, `"medium"`, or `"low"` as strings; parsed
    /// into [`FailOn`] when applied.
    pub fail_on: Option<String>,
    pub semver_checks: Option<bool>,
    pub rust_analyzer: Option<bool>,
    pub features: Option<Vec<String>>,
    pub all_features: Option<bool>,
    pub no_default_features: Option<bool>,
    pub budget: Option<usize>,
}

impl ConfigFile {
    /// Load `{root}/cargo-impact.toml` if present. Missing file → empty
    /// `ConfigFile`. Read/parse errors log to stderr and return empty.
    pub fn load(root: &Path) -> Self {
        let path = root.join(CONFIG_FILENAME);
        let Ok(src) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match Self::parse(&src) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!(
                    "cargo-impact: {} is present but could not be parsed: {e}. \
                     Continuing with CLI-only defaults.",
                    path.display()
                );
                Self::default()
            }
        }
    }

    pub fn parse(src: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(src)
    }
}

/// Apply config-file defaults to `args` in place. CLI-provided values
/// win over file values; file values win over clap's hardcoded defaults.
///
/// Detecting "CLI value was actually passed" is impossible to do
/// perfectly without inspecting clap internals. Compromise: we treat
/// each CLI field's *default value* as "user didn't pass it" and only
/// override in that case. This covers the common workflow where users
/// keep a shared `cargo-impact.toml` and occasionally override one flag.
pub fn apply_config(defaults: &Defaults, args: &mut ImpactArgs) {
    if let Some(v) = defaults.confidence_min
        && args.confidence_min == 0.0
    {
        args.confidence_min = v;
    }
    if args.fail_on.is_none()
        && let Some(s) = &defaults.fail_on
    {
        match s.to_lowercase().as_str() {
            "high" => args.fail_on = Some(FailOn::High),
            "medium" => args.fail_on = Some(FailOn::Medium),
            "low" => args.fail_on = Some(FailOn::Low),
            other => eprintln!(
                "cargo-impact: cargo-impact.toml: invalid `fail_on = \"{other}\"` \
                     (expected `high`, `medium`, or `low`); leaving unset."
            ),
        }
    }
    if let Some(v) = defaults.semver_checks
        && !args.semver_checks
    {
        args.semver_checks = v;
    }
    if let Some(v) = defaults.rust_analyzer
        && !args.rust_analyzer
    {
        args.rust_analyzer = v;
    }
    if let Some(v) = &defaults.features
        && args.features.is_empty()
    {
        args.features.clone_from(v);
    }
    if let Some(v) = defaults.all_features
        && !args.all_features
    {
        args.all_features = v;
    }
    if let Some(v) = defaults.no_default_features
        && !args.no_default_features
    {
        args.no_default_features = v;
    }
    if let Some(v) = defaults.budget
        && args.budget == 0
    {
        args.budget = v;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_args() -> ImpactArgs {
        // Simulate `cargo impact` with zero CLI flags (all clap defaults).
        ImpactArgs {
            test: false,
            format: crate::Format::Text,
            since: "HEAD".into(),
            manifest_dir: None,
            confidence_min: 0.0,
            fail_on: None,
            semver_checks: false,
            rust_analyzer: false,
            features: Vec::new(),
            all_features: false,
            no_default_features: false,
            budget: 0,
            context: false,
            feature_powerset: false,
            macro_expand: false,
        }
    }

    #[test]
    fn parse_empty_config_yields_empty_defaults() {
        let cfg = ConfigFile::parse("").unwrap();
        assert!(cfg.defaults.confidence_min.is_none());
        assert!(cfg.defaults.fail_on.is_none());
    }

    #[test]
    fn parse_full_defaults_block() {
        let src = r#"
            [defaults]
            confidence_min = 0.6
            fail_on = "high"
            semver_checks = true
            rust_analyzer = false
            features = ["tokio", "rt"]
            budget = 32000
        "#;
        let cfg = ConfigFile::parse(src).unwrap();
        assert_eq!(cfg.defaults.confidence_min, Some(0.6));
        assert_eq!(cfg.defaults.fail_on.as_deref(), Some("high"));
        assert_eq!(cfg.defaults.semver_checks, Some(true));
        assert_eq!(
            cfg.defaults.features.as_deref(),
            Some(&["tokio".to_string(), "rt".to_string()][..])
        );
        assert_eq!(cfg.defaults.budget, Some(32000));
    }

    #[test]
    fn apply_sets_flags_when_cli_left_them_default() {
        let mut args = fresh_args();
        let defaults = Defaults {
            confidence_min: Some(0.6),
            fail_on: Some("high".into()),
            semver_checks: Some(true),
            budget: Some(8000),
            ..Defaults::default()
        };
        apply_config(&defaults, &mut args);
        assert_eq!(args.confidence_min, 0.6);
        assert!(matches!(args.fail_on, Some(FailOn::High)));
        assert!(args.semver_checks);
        assert_eq!(args.budget, 8000);
    }

    #[test]
    fn apply_does_not_override_explicit_cli_values() {
        let mut args = fresh_args();
        // Simulate the user having passed --confidence-min=0.9 and
        // --fail-on=low on the CLI.
        args.confidence_min = 0.9;
        args.fail_on = Some(FailOn::Low);
        args.budget = 1000;

        let defaults = Defaults {
            confidence_min: Some(0.6),
            fail_on: Some("high".into()),
            budget: Some(8000),
            ..Defaults::default()
        };
        apply_config(&defaults, &mut args);
        // CLI wins on all three.
        assert_eq!(args.confidence_min, 0.9);
        assert!(matches!(args.fail_on, Some(FailOn::Low)));
        assert_eq!(args.budget, 1000);
    }

    #[test]
    fn invalid_fail_on_string_leaves_args_untouched_and_logs() {
        let mut args = fresh_args();
        let defaults = Defaults {
            fail_on: Some("catastrophic".into()),
            ..Defaults::default()
        };
        apply_config(&defaults, &mut args);
        assert!(args.fail_on.is_none());
    }

    #[test]
    fn load_missing_file_returns_empty_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = ConfigFile::load(dir.path());
        assert!(cfg.defaults.confidence_min.is_none());
    }

    #[test]
    fn load_reads_file_from_root() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILENAME),
            "[defaults]\nconfidence_min = 0.75\nbudget = 16000\n",
        )
        .unwrap();
        let cfg = ConfigFile::load(dir.path());
        assert_eq!(cfg.defaults.confidence_min, Some(0.75));
        assert_eq!(cfg.defaults.budget, Some(16000));
    }

    #[test]
    fn load_malformed_file_logs_and_returns_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILENAME),
            "this is not valid toml !!! [[[",
        )
        .unwrap();
        // Doesn't panic; returns empty defaults.
        let cfg = ConfigFile::load(dir.path());
        assert!(cfg.defaults.confidence_min.is_none());
    }
}
