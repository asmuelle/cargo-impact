# Changelog

All notable changes to `cargo-impact` are recorded here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), with
one section per shipped version. Dates in ISO-8601.

Versioning: [SemVer](https://semver.org/). Pre-1.0 minor bumps may
carry breaking changes — the project's output formats (JSON envelope,
SARIF, MCP tool schemas) are stable across patch releases within a
minor but may evolve at minor boundaries; breaking changes are called
out explicitly here.

## [0.4.0] — 2026-04-24

### Highlights

v0.4 is the production-CI release: SARIF on code scanning, sticky PR
comments, SLO-regression gate, plus precision improvements that move
findings closer to `Proven` without trading away honest tiering.

### Added — Core

- `--format sarif` emits SARIF v2.1.0 that GitHub code scanning and
  every security-scanner UI already renders inline on PR diffs.
  Findings carry `partialFingerprints` (content-hashed IDs) so dismissals
  propagate across runs.
- `--format pr-comment` renders markdown optimized for GitHub PR
  sticky comments: severity-badge headers, collapsed `<details>` per
  severity, HIGH expanded by default.
- Deterministic output: two runs over the same diff produce
  byte-identical output across every format. Covered by a
  determinism gate in the integration test suite.
- Criterion benchmark suite (`benches/pipeline.rs`) covering the
  render layer across all 5 formats plus full-pipeline measurements
  for the clean-workspace short-circuit and a trait-change fixture.
- SLO-regression CI gate (`scripts/bench-gate.sh` +
  `.github/workflows/bench.yml`) that compares each PR's bench p50s
  against `benches/baseline.json` × 2.5x (absorbs GH-runner variance,
  catches real 2x+ regressions). Re-baseline via `BENCH_GATE_MODE=update`.
- GitHub Actions composite action ([`asmuelle/cargo-impact-action`](https://github.com/asmuelle/cargo-impact-action))
  with sane defaults: runs on PR, uploads SARIF, posts sticky comment.
  Minimum usage is ≤10 lines of YAML.

### Added — Precision (v0.4 stretch)

- `actix-web` + `rocket` framework adapters. Method-chain visitors
  (`.route` / `.service` / `.scope` / `.mount`) plus a shared
  HTTP-verb attribute-macro pass (`#[get]`, `#[post]`, …) with
  framework disambiguation via use-statement scan.
- Per-reference severity refinement for RA `ResolvedReference`
  findings. Each reference is classified by its enclosing container:
  test fn → `Low`, impl block → `High`, caller → `Medium`. Syn-based
  classifier in `src/ref_context.rs`; confidence stays at `Proven 0.98`.
- syn/RA finding dedup: when a syn analyzer flags a site `Likely` and
  a Proven RA `ResolvedReference` covers the same `(name, file)` pair,
  the syn finding is dropped. Reports no longer double-count and the
  tier summary reflects actual unique coverage.
- `--feature-powerset` (depth-1 scope): runs the analyzer under
  baseline, `--no-default-features`, and `--all-features` in sequence,
  merging findings by content-hashed ID. Findings visible only under
  a non-baseline set are annotated in evidence with the feature set
  that revealed them.
- `--macro-expand` (MVP scope): shells to `cargo expand --lib` and
  walks the expanded AST for trait impls synthesized by derive /
  attribute macros (serde, clap, thiserror, tokio). Graceful no-op
  when `cargo-expand` is absent from PATH.

### Added — MCP surface

- `impact_analyze` emits `notifications/message` progress events at
  analyzer stage boundaries (`symbols`, `analyzers`, `semver_checks`,
  `rust_analyzer`, `macro_expand`, `done`) so long runs give live
  feedback instead of a 30-second silence. No protocol break —
  clients that ignore unknown notifications behave identically.
- `AnalyzeArgs` grew `feature_powerset` and `macro_expand` JSON
  fields matching the CLI flags.

### Added — Library API

- New `analyze_with_progress<F: FnMut(&ProgressEvent<'_>)>(args, cb)`
  entry point for callers that want live stage updates. `analyze()`
  is now a one-liner wrapper that passes a no-op callback.
- `ProgressEvent { stage, current, total, detail }` struct public.

### Changed

- `analyze_inner` signature now takes a progress callback. Not
  breaking for library callers — the public `analyze()` entry point
  preserves its signature.
- Rustdoc link for `FindingKind::tag` qualified to `[`Self::tag`]`
  — fixes `RUSTDOCFLAGS=-D warnings` on stable rustdoc.

### Dependencies

- New: `proc-macro2` as an explicit dep with `features = ["span-locations"]`
  (transitively required via syn, but the feature needs enabling for
  line-mapping in `ref_context.rs`).

### Tests

Test count grew from 174 (v0.3.0) to **224** (212 lib + 11 integration
+ 1 doc). Notable additions:

- Framework adapter coverage for actix/rocket and HTTP-verb handlers.
- `dedup` module unit tests for shadow rules and pass-through edge cases.
- `ref_context` classifier covering test / impl / caller contexts.
- MCP progress notification schema assertion and end-to-end streaming
  test.
- `--feature-powerset` end-to-end with a feature-gated fixture.
- `--macro-expand` graceful-degradation test (Unix-only) with
  scrubbed PATH to simulate missing tool.

## [0.3.0] — 2026-04

First stable with agent-native surface:

- Hand-rolled MCP server (`cargo impact mcp`) with all six tools.
- rust-analyzer LSP client for `Proven`-tier `ResolvedReference` findings.
- Content-hashed finding IDs stable across runs.
- `cargo-semver-checks` integration (opt-in via `--semver-checks`).
- Framework adapters for axum and clap.
- `--context` bridge to cargo-context.
- Rust edition 2024, MSRV 1.95.

## [0.2.1] — 2026-04

- Trait ripple differentiation (required / default / new method) via
  per-method HEAD-vs-WT classification.
- `dyn Trait` dispatch edges at `Likely 0.75`.
- `--format={json,markdown}`.
- `--features` / `--all-features` / `--no-default-features` with
  cfg-aware AST filtering.
- `--confidence-min` and `--fail-on={high,medium,low}` for CI.
- Documentation drift via intra-doc links + keyword fallback.
- Diff-aware candidate symbols, FFI signature tracking, `build.rs`
  change detection.

## [0.1.0] — 2026-03

Initial release:

- `cargo impact` parses `git diff` with syn, extracts candidate symbols.
- Walks the workspace for test functions referencing changed symbols.
- Emits a `cargo-nextest` filter expression via `--test`.
- Text output.

[0.4.0]: https://github.com/asmuelle/cargo-impact/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/asmuelle/cargo-impact/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/asmuelle/cargo-impact/compare/v0.1.0...v0.2.1
[0.1.0]: https://github.com/asmuelle/cargo-impact/releases/tag/v0.1.0
