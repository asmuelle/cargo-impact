//! Core finding types.
//!
//! A [`Finding`] is the unit of output: one thing the developer should verify.
//! Each one carries a confidence tier ([`Tier`]), a numeric score, a severity
//! class ([`SeverityClass`]), and a `kind`-specific payload explaining why it
//! was flagged. Serialized identically whether emitted as JSON or rendered
//! into the markdown/text reports.

use serde::Serialize;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Confidence tier per README §3F.
///
/// v0.2 ships without resolved call-graph analysis (rust-analyzer integration
/// arrives in v0.3), so no finding reaches `Proven` in this release — syn-only
/// analysis is honestly at most `Likely`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Proven,
    Likely,
    Possible,
    Unknown,
}

impl Tier {
    /// Rank for filtering (`--confidence-min` clamps by score; this is used
    /// only for stable ordering).
    pub fn rank(self) -> u8 {
        match self {
            Self::Proven => 3,
            Self::Likely => 2,
            Self::Possible => 1,
            Self::Unknown => 0,
        }
    }
}

/// Severity bucket used for `--fail-on` and human-facing grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SeverityClass {
    High,
    Medium,
    Low,
    Unknown,
}

impl SeverityClass {
    pub fn as_label(self) -> &'static str {
        match self {
            Self::High => "HIGH",
            Self::Medium => "MEDIUM",
            Self::Low => "LOW",
            Self::Unknown => "UNKNOWN",
        }
    }

    /// Emoji column used in text/markdown. Matches README §4.
    pub fn icon(self) -> &'static str {
        match self {
            Self::High => "🔴",
            Self::Medium => "🟡",
            Self::Low => "🔵",
            Self::Unknown => "⚪",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Location {
    pub file: PathBuf,
    pub symbol: String,
}

/// Reason a specific finding was emitted. Variants carry the analysis-kind
/// payload; cross-cutting fields (tier, confidence, severity) live on
/// [`Finding`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FindingKind {
    /// A test function whose body syntactically references a changed symbol.
    TestReference {
        test: Location,
        matched_symbols: Vec<String>,
    },
    /// An `impl TraitName for T` block in the workspace where `TraitName`
    /// was defined in a changed file.
    TraitImpl {
        trait_name: String,
        impl_for: String,
        impl_site: Location,
    },
    /// A `#[derive(TraitName)]` attribute on a struct, enum, or union where
    /// `TraitName` was defined in a changed file. Treated as an implicit
    /// impl site — the derive will expand to one at compile time — but
    /// distinguished from `TraitImpl` so consumers can filter.
    DerivedTraitImpl {
        trait_name: String,
        impl_for: String,
        derive_site: Location,
    },
    /// A `dyn TraitName` type reference for a trait whose definition changed.
    DynDispatch { trait_name: String, site: Location },
    /// An intra-doc link like `[`Symbol`]` in a markdown file or `///` comment
    /// referencing a changed symbol.
    DocDriftLink {
        symbol: String,
        doc: Location,
        line: u32,
    },
    /// A plain identifier match inside a doc comment or markdown file — weaker
    /// signal than an intra-doc link, emitted at `Possible` tier only.
    DocDriftKeyword {
        symbol: String,
        doc: Location,
        line: u32,
    },
    /// An `extern "C"` signature or `#[no_mangle]` function was added,
    /// removed, or modified. Signatures cross the Rust/native boundary —
    /// downstream consumers outside Rust cannot be analyzed by us, so these
    /// are always surfaced at `High` severity.
    FfiSignatureChange {
        symbol: String,
        file: PathBuf,
        /// `"added"`, `"removed"`, or `"modified"`.
        change: &'static str,
    },
    /// A `build.rs` script file changed. Build scripts can invalidate
    /// downstream compilation in non-obvious ways (env vars, rerun-if-*,
    /// generated code, linker flags).
    BuildScriptChanged { file: PathBuf },
    /// Outcome of a `cargo-semver-checks` run. `level` is one of
    /// `"breaking"` (the only currently-emitted value) or a finer-grained
    /// classification in a future release. `details` carries the tool's
    /// own output verbatim so consumers can surface it without a
    /// re-invocation.
    SemverCheck { level: String, details: String },
    /// A name-resolved reference to a changed symbol, emitted by the
    /// rust-analyzer LSP integration. These are the *only* findings that
    /// legitimately reach the `Proven` tier in this release — the syn-only
    /// analyzers (TestReference, TraitImpl, DerivedTraitImpl, etc.) top out
    /// at `Likely` because they can't prove name resolution without a
    /// compiler front-end.
    ResolvedReference {
        source_symbol: String,
        target: Location,
    },
    /// A runtime-surface handler (HTTP route, CLI subcommand, etc.)
    /// implicated by a changed symbol. Emitted by framework-specific
    /// adapters (axum, clap — see `src/adapters.rs`). `framework`
    /// names the adapter that produced it; `identifier` is the
    /// framework-specific surface identity (route path, subcommand
    /// name); `site` points at the Rust source defining the handler.
    RuntimeSurface {
        framework: String,
        identifier: String,
        site: Location,
    },
    /// A specific, per-method change inside a trait definition. Complements
    /// `TraitImpl` (which flags every impl of a changed trait at blanket
    /// precision) by explaining *what* about the trait changed — required
    /// vs default method, added/removed, signature vs body. Severity and
    /// confidence derive from `change` per README §3B.
    TraitDefinitionChange {
        trait_name: String,
        file: PathBuf,
        /// Specific method name when the change is method-scoped; `None`
        /// for trait-level changes (supertraits, generic bounds).
        method: Option<String>,
        /// Machine-readable classification; renderers map this to evidence
        /// text and severity.
        change: TraitChange,
    },
}

/// Per-method or trait-level change classification. One-to-one with the
/// bullets in README §3B. Confidence floor for anything requiring
/// resolution (actual impl bodies) stays at `Likely` in v0.2 — we cannot
/// prove which impls delegate vs override without rust-analyzer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TraitChange {
    /// A new method was added *without* a default body. Every impl that
    /// does not supply it will fail to compile.
    RequiredMethodAdded,
    /// A new method was added *with* a default body. Rarely breaking, but
    /// can shadow same-named methods on implementing types.
    DefaultMethodAdded,
    /// A method was removed. Breaks any caller that referenced it and any
    /// impl that still tries to define it.
    MethodRemoved,
    /// A required-method signature (args, return type, generics, where
    /// clause) changed. Impls with the old signature break at compile time.
    RequiredMethodSignatureChanged,
    /// Only the body of a default method changed. Runtime behavior shifts
    /// for impls that rely on the default; impls that override are
    /// unaffected. We cannot tell which is which without name resolution.
    DefaultMethodBodyChanged,
    /// The trait's supertrait list or generic bounds changed. Downstream
    /// generic code constrained by the trait may stop compiling.
    SupertraitOrBoundChanged,
}

impl TraitChange {
    /// Severity class per README §3B. Required-side changes and removals
    /// are compile breaks on downstream impls; default-body changes are
    /// runtime-only and narrower; bound changes sit in the middle.
    pub fn severity(self) -> SeverityClass {
        match self {
            Self::RequiredMethodAdded
            | Self::RequiredMethodSignatureChanged
            | Self::MethodRemoved => SeverityClass::High,
            Self::SupertraitOrBoundChanged => SeverityClass::Medium,
            Self::DefaultMethodAdded | Self::DefaultMethodBodyChanged => SeverityClass::Low,
        }
    }

    /// Confidence tier. All classifications stay at `Likely` or `Possible`
    /// in v0.2 — proving which impls actually delegate vs override needs
    /// resolved name lookup, which arrives with rust-analyzer in v0.3.
    pub fn tier(self) -> Tier {
        match self {
            Self::RequiredMethodAdded
            | Self::RequiredMethodSignatureChanged
            | Self::MethodRemoved
            | Self::SupertraitOrBoundChanged => Tier::Likely,
            Self::DefaultMethodAdded | Self::DefaultMethodBodyChanged => Tier::Possible,
        }
    }

    /// Numeric confidence score. Higher for changes that unambiguously
    /// break downstream compilation; lower for runtime-only or
    /// defaulted-only changes where impact depends on resolution.
    pub fn confidence(self) -> f64 {
        match self {
            Self::RequiredMethodAdded | Self::RequiredMethodSignatureChanged => 0.95,
            Self::MethodRemoved => 0.90,
            Self::SupertraitOrBoundChanged => 0.75,
            Self::DefaultMethodBodyChanged => 0.55,
            Self::DefaultMethodAdded => 0.40,
        }
    }

    /// Short human phrase for evidence/summary rendering. Callers are
    /// expected to prepend the trait and method names.
    pub fn phrase(self) -> &'static str {
        match self {
            Self::RequiredMethodAdded => "required method added",
            Self::DefaultMethodAdded => "default method added",
            Self::MethodRemoved => "method removed",
            Self::RequiredMethodSignatureChanged => "required method signature changed",
            Self::DefaultMethodBodyChanged => "default method body changed",
            Self::SupertraitOrBoundChanged => "supertraits or generic bounds changed",
        }
    }
}

impl FindingKind {
    /// Default severity for this kind — callers can override but rarely need to.
    pub fn default_severity(&self) -> SeverityClass {
        match self {
            Self::TraitImpl { .. }
            | Self::DerivedTraitImpl { .. }
            | Self::FfiSignatureChange { .. }
            | Self::BuildScriptChanged { .. }
            | Self::RuntimeSurface { .. } => SeverityClass::High,
            Self::TestReference { .. }
            | Self::DynDispatch { .. }
            | Self::ResolvedReference { .. } => SeverityClass::Medium,
            Self::DocDriftLink { .. } | Self::DocDriftKeyword { .. } => SeverityClass::Low,
            Self::SemverCheck { level, .. } => match level.as_str() {
                "breaking" => SeverityClass::High,
                "minor" | "patch" => SeverityClass::Medium,
                _ => SeverityClass::Unknown,
            },
            Self::TraitDefinitionChange { change, .. } => change.severity(),
        }
    }

    /// The primary file path this finding is about, for ignore-filtering
    /// and UI "go to file" affordances. Returns `None` for global findings
    /// that don't name a specific path (e.g. `SemverCheck`, which reports
    /// on the whole public API surface).
    pub fn primary_path(&self) -> Option<&Path> {
        match self {
            Self::TestReference { test, .. } => Some(test.file.as_path()),
            Self::TraitImpl { impl_site, .. } => Some(impl_site.file.as_path()),
            Self::DerivedTraitImpl { derive_site, .. } => Some(derive_site.file.as_path()),
            Self::DynDispatch { site, .. } => Some(site.file.as_path()),
            Self::DocDriftLink { doc, .. } => Some(doc.file.as_path()),
            Self::DocDriftKeyword { doc, .. } => Some(doc.file.as_path()),
            Self::FfiSignatureChange { file, .. } => Some(file.as_path()),
            Self::BuildScriptChanged { file, .. } => Some(file.as_path()),
            Self::ResolvedReference { target, .. } => Some(target.file.as_path()),
            Self::TraitDefinitionChange { file, .. } => Some(file.as_path()),
            Self::RuntimeSurface { site, .. } => Some(site.file.as_path()),
            Self::SemverCheck { .. } => None,
        }
    }

    /// Every possible value [`Self::tag`] can return — useful for schema
    /// generators (SARIF rules list, MCP tool descriptions) that need
    /// to enumerate kinds without having a runtime instance.
    pub fn all_tags() -> &'static [&'static str] {
        &[
            "test_reference",
            "trait_impl",
            "derived_trait_impl",
            "dyn_dispatch",
            "doc_drift_link",
            "doc_drift_keyword",
            "ffi_signature_change",
            "build_script_changed",
            "semver_check",
            "trait_definition_change",
            "resolved_reference",
            "runtime_surface",
        ]
    }

    /// Tag used for sorting/grouping and the JSON `kind` field's value.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::TestReference { .. } => "test_reference",
            Self::TraitImpl { .. } => "trait_impl",
            Self::DerivedTraitImpl { .. } => "derived_trait_impl",
            Self::DynDispatch { .. } => "dyn_dispatch",
            Self::DocDriftLink { .. } => "doc_drift_link",
            Self::DocDriftKeyword { .. } => "doc_drift_keyword",
            Self::FfiSignatureChange { .. } => "ffi_signature_change",
            Self::BuildScriptChanged { .. } => "build_script_changed",
            Self::SemverCheck { .. } => "semver_check",
            Self::TraitDefinitionChange { .. } => "trait_definition_change",
            Self::ResolvedReference { .. } => "resolved_reference",
            Self::RuntimeSurface { .. } => "runtime_surface",
        }
    }
}

/// Single unit of analysis output.
///
/// Construct via [`Finding::new`] so the severity/tier/confidence invariants
/// are enforced (confidence clamped to [0, 1]; severity default derived from
/// the kind unless overridden).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    pub id: String,
    pub severity: SeverityClass,
    pub tier: Tier,
    pub confidence: f64,
    #[serde(flatten)]
    pub kind: FindingKind,
    pub evidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_action: Option<String>,
}

impl Eq for Finding {}

impl Finding {
    pub fn new(
        id: impl Into<String>,
        tier: Tier,
        confidence: f64,
        kind: FindingKind,
        evidence: impl Into<String>,
    ) -> Self {
        let severity = kind.default_severity();
        Self {
            id: id.into(),
            severity,
            tier,
            confidence: confidence.clamp(0.0, 1.0),
            kind,
            evidence: evidence.into(),
            suggested_action: None,
        }
    }

    /// Stable, deterministic ID derived from the finding's content. Same
    /// finding across two runs produces the same ID — this is what lets
    /// `impact_explain` round-trip. Call after the finding's final
    /// kind/evidence are set but before the ID is assigned.
    ///
    /// Hash inputs: kind tag + evidence + the kind payload (formatted via
    /// `{:?}` on the serde-derived Debug). `DefaultHasher` is non-
    /// cryptographic but that's fine — we're deduping, not proving
    /// non-existence.
    pub fn content_id(&self) -> String {
        let mut hasher = DefaultHasher::new();
        self.kind.tag().hash(&mut hasher);
        self.evidence.hash(&mut hasher);
        // serde_json serialization is stable across runs for our data and
        // captures kind-specific fields (trait_name, file, etc.) without
        // needing per-variant hand-plumbing.
        if let Ok(payload) = serde_json::to_string(&self.kind) {
            payload.hash(&mut hasher);
        }
        format!("f-{:016x}", hasher.finish())
    }

    pub fn with_severity(mut self, severity: SeverityClass) -> Self {
        self.severity = severity;
        self
    }

    pub fn with_suggested_action(mut self, action: impl Into<String>) -> Self {
        self.suggested_action = Some(action.into());
        self
    }

    /// Delegates to [`FindingKind::primary_path`]. Convenience shortcut
    /// so callers don't have to reach through `.kind` for a near-ubiquitous
    /// operation.
    pub fn primary_path(&self) -> Option<&Path> {
        self.kind.primary_path()
    }
}

/// Counts by tier — exposed in the JSON envelope and the text footer.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TierSummary {
    pub proven: u32,
    pub likely: u32,
    pub possible: u32,
    pub unknown: u32,
}

impl TierSummary {
    pub fn from_findings(findings: &[Finding]) -> Self {
        let mut s = Self::default();
        for f in findings {
            match f.tier {
                Tier::Proven => s.proven += 1,
                Tier::Likely => s.likely += 1,
                Tier::Possible => s.possible += 1,
                Tier::Unknown => s.unknown += 1,
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_kind() -> FindingKind {
        FindingKind::TestReference {
            test: Location {
                file: PathBuf::from("tests/t.rs"),
                symbol: "smoke".into(),
            },
            matched_symbols: vec!["login".into()],
        }
    }

    #[test]
    fn confidence_clamped_to_unit_interval() {
        let f = Finding::new("f-0001", Tier::Likely, 1.5, sample_kind(), "e");
        assert_eq!(f.confidence, 1.0);
        let f = Finding::new("f-0001", Tier::Likely, -0.5, sample_kind(), "e");
        assert_eq!(f.confidence, 0.0);
    }

    #[test]
    fn default_severity_by_kind() {
        let f = Finding::new("x", Tier::Likely, 0.5, sample_kind(), "e");
        assert_eq!(f.severity, SeverityClass::Medium);
    }

    #[test]
    fn tier_summary_tallies_correctly() {
        let mk = |tier: Tier, id: &str| Finding::new(id, tier, 0.5, sample_kind(), "e");
        let findings = vec![
            mk(Tier::Likely, "a"),
            mk(Tier::Likely, "b"),
            mk(Tier::Possible, "c"),
            mk(Tier::Unknown, "d"),
        ];
        let s = TierSummary::from_findings(&findings);
        assert_eq!(s.proven, 0);
        assert_eq!(s.likely, 2);
        assert_eq!(s.possible, 1);
        assert_eq!(s.unknown, 1);
    }

    #[test]
    fn json_shape_uses_kind_tag() {
        let f = Finding::new("f-0001", Tier::Likely, 0.85, sample_kind(), "direct ref");
        let v: serde_json::Value = serde_json::to_value(&f).unwrap();
        assert_eq!(v["kind"], "test_reference");
        assert_eq!(v["tier"], "likely");
        assert_eq!(v["severity"], "medium");
        assert_eq!(v["confidence"], 0.85);
        assert!(v["test"].is_object());
    }
}
