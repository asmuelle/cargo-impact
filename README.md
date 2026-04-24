# cargo-impact
*Predictive Regression Analysis & Verification Mapping for Rust*

[![CI](https://github.com/asmuelle/cargo-impact/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/asmuelle/cargo-impact/actions/workflows/ci.yml)
[![Security](https://github.com/asmuelle/cargo-impact/actions/workflows/security.yml/badge.svg?branch=main)](https://github.com/asmuelle/cargo-impact/actions/workflows/security.yml)
[![Spec](https://github.com/asmuelle/cargo-impact/actions/workflows/spec.yml/badge.svg?branch=main)](https://github.com/asmuelle/cargo-impact/actions/workflows/spec.yml)
[![Release](https://github.com/asmuelle/cargo-impact/actions/workflows/release.yml/badge.svg)](https://github.com/asmuelle/cargo-impact/actions/workflows/release.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

> **Status:** v0.3.0 (stable) on [crates.io](https://crates.io/crates/cargo-impact) — `cargo install cargo-impact`. This README is both the living design spec and the user manual; sections describing yet-unshipped behavior are explicitly called out (§11 has the full shipped-vs-deferred breakdown).

## Contents

1. [Core Philosophy](#1-core-philosophy)
2. [Quickstart (Intended UX)](#2-quickstart-intended-ux)
3. [Technical Architecture](#3-technical-architecture)
4. [CLI Interface (UX)](#4-cli-interface-ux)
5. [Vibe Coding Workflow Integration](#5-vibe-coding-workflow-integration)
6. [Summary Table: Context vs. Impact](#6-summary-table-context-vs-impact)
7. [Integration with `cargo-context`](#7-integration-with-cargo-context)
8. [MCP Server Surface](#8-mcp-server-surface)
9. [Performance Targets](#9-performance-targets)
10. [Non-Goals](#10-non-goals)
11. [Implementation Roadmap](#11-implementation-roadmap)
12. [License](#license)

---

## 1. Core Philosophy
`cargo-impact` moves the developer from "Running all tests and hoping for the best" to **Surgical Verification**. It treats a code change as a "stone thrown into a pond" and calculates exactly which ripples hit which shores (tests, docs, APIs).

It answers the critical question: *"I changed X; what is the minimum set of things I must check, and how confident can I be in each one?"* Every finding is labeled with a confidence tier — static analysis is never certain, and the tool is honest about that.

---

## 2. Quickstart

### Install

Pick whichever path fits your environment:

```bash
# 1. crates.io (once published — see https://crates.io/crates/cargo-impact)
cargo install cargo-impact

# 2. Pinned from source by tag (works today, no crates.io dependency)
cargo install --git https://github.com/asmuelle/cargo-impact --tag v0.3.0

# 3. Prebuilt binary from the GitHub release page
#    https://github.com/asmuelle/cargo-impact/releases
#    Binaries for linux-x86_64, linux-aarch64, macos-x86_64,
#    macos-aarch64, and windows-x86_64 are attached to each release.
```

### In CI — [`cargo-impact-action`](https://github.com/asmuelle/cargo-impact-action)

Drop `cargo-impact` into a GitHub repo with ≤10 lines of YAML. The
action installs cargo-impact, runs against the PR diff, uploads a
SARIF report (code scanning renders findings inline on the diff),
and posts a sticky markdown PR comment.

```yaml
on: [pull_request]
permissions:
  contents: read
  pull-requests: write
  security-events: write
jobs:
  impact:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with: { fetch-depth: 0 }
      - uses: asmuelle/cargo-impact-action@v1
        with:
          fail-on: high      # omit to stay informational-only
```

See the [action's README](https://github.com/asmuelle/cargo-impact-action)
for all inputs/outputs and troubleshooting.

### First run

```bash
cd my-rust-project
cargo impact
```

Expected first-run output on a clean workspace:

```text
cargo-impact: no Rust files changed relative to HEAD
```

Make a change, re-run, and you'll get the severity-grouped report. `cargo impact --help` lists every flag.

### Typical AI loop
```bash
cargo impact --context \               # 1. Feed cargo-context just the
  | cargo context --files-from -       #    files inside the blast radius
# ... give the pack to your AI, have it generate a patch, apply it ...
cargo impact --format markdown         # 2. Get the verification checklist
# ... AI ticks items, flags what it cannot verify ...
cargo impact --test                    # 3. Run only the affected tests
```

(Without `cargo-context` installed, `cargo impact --context | xargs cat` is a
plain-text fallback — see §7 for the full integration story.)

For agent-native consumption, start the MCP server: `cargo impact mcp` speaks JSON-RPC 2.0 over stdio with all six tools from §8.

### Working *on* this repo with Claude Code

If you clone this repo to contribute and open it in Claude Code, you're set up automatically. The committed `.mcp.json` registers both `cargo-impact` and `cargo-context` as MCP servers; `.claude/settings.json` enables them via an explicit allowlist and pre-approves the relevant `cargo impact` / `cargo context` Bash patterns so agents don't hit permission prompts. `CLAUDE.md` at the repo root orients assistants to the project's conventions (MSRV 1.95, edition 2024, honest tiering, the three-check pre-commit gate).

Install both tools from crates.io first, then open the repo:

```bash
cargo install cargo-impact cargo-context
cd cargo-impact
claude   # or code . / zed . with Claude Code configured
```

Missing either binary degrades gracefully — the MCP runtime logs the unavailable server and skips its tools; nothing else breaks.

---

## 3. Technical Architecture

`cargo-impact` is an **orchestrator**, not a from-scratch analyzer. It composes existing best-in-class Rust tooling into a single blast-radius report, with every finding labeled by confidence tier rather than a binary "affected/not affected" flag.

### A. Analysis Backend (The Engine)
Static analysis of Rust requires *resolved names*, not just syntax. The backend is layered:

| Layer | Tool | Responsibility |
| :--- | :--- | :--- |
| Syntax | `syn` | Parse diff hunks → candidate symbols |
| Macro expansion | `cargo expand` / HIR | Expand derives, attribute, and `fn`-like macros before analysis (critical for `serde`, `axum`, `clap`, `tokio`) |
| Name resolution & call graph | `rust-analyzer` as library (`ra_ap_hir`, `ra_ap_ide`) | Resolve paths, find references, traverse calls across modules and crates |
| Public API | `rustdoc --output-format json` + `cargo-public-api` | Stable diff of the public surface |
| Semver impact | `cargo-semver-checks` | Classify public changes as additive vs. breaking |
| Cache | `target/impact-cache/` keyed by content hash + cargo fingerprint | Sub-second warm runs; incremental invalidation |

Why not `syn` alone: re-exports, trait method dispatch, generics, and macro-generated code all defeat syntax-only analysis. rust-analyzer gives IDE-grade precision on stable Rust.

### B. Blast Radius Detection
For each changed symbol, `cargo-impact` emits a finding with a **confidence tier** (see §3F):

*   **Direct references:** Resolved call-graph edges from the reverse-reference index → `Proven`.
*   **Trait ripple (differentiated):**
    *   Required method signature changed → `Proven` (all impls break at compile time; report as build consequence, not risk).
    *   Default method body changed → `Likely` for impls that *don't* override; `Proven` for impls that delegate via `super::`.
    *   New method added → `Proven` compile break unless defaulted.
    *   Trait bound changed → `Likely` for downstream generic code.
*   **Dynamic dispatch (`dyn Trait`):** All impls reachable via `dyn Trait` construction sites → `Likely`.
*   **FFI / `unsafe extern`:** Any change to `extern "C"` signatures or `#[no_mangle]` symbols → `Proven` HIGH (blast radius leaves Rust entirely).
*   **`build.rs` changes:** Treated as `Proven` HIGH by default — build scripts can invalidate downstream compilation in non-obvious ways.
*   **Feature-gated code:** If the changed symbol is behind `#[cfg(feature = "...")]`, run analysis per active feature set. `--features`, `--all-features`, and `--feature-powerset` supported.

### C. Surgical Testing
Does not re-implement test selection — orchestrates proven tools:

*   **Coverage-driven selection:** Delegates to [`cargo-difftests`](https://github.com/dnbln/cargo-difftests) for file-to-test mapping based on actual coverage traces.
*   **Call-graph augmentation:** Adds tests that *statically* reference changed symbols but weren't hit by the last coverage run (catches untested paths).
*   **Emits nextest filters:** Output is a [`cargo-nextest`](https://nexte.st) filter expression, e.g. `cargo nextest run -E 'test(test_auth_login) + test(test_api_handshake)'`. Falls back to `cargo test` filters when nextest is absent.
*   **Handles:** doctests in triple-backtick blocks inside doc comments, `#[cfg(test)] mod tests`, per-member integration tests in workspaces, `rstest` / `proptest` parameterization, `#[ignore]`, `serial_test`.

### D. Runtime Surface Mapping (The "Exposed" Layer)
Framework detection runs **after macro expansion** (handler attributes, derive-based routers are invisible pre-expansion). Pluggable adapters, not hardcoded framework logic:

*   **HTTP routers:** `axum` (`Router::route`, nested routers, `#[debug_handler]`), `actix-web` (`web::resource`, scopes), `rocket` (`#[get]`/`#[post]`), `warp`.
*   **CLI:** `clap` derive and builder APIs.
*   **Desktop/mobile:** `tauri` commands, `dioxus` routes.
*   **Public crate API:** Delegated to `cargo-semver-checks` + `cargo-public-api`.
*   **Adapter contract:** A small trait so third parties can register custom surface mappers for proprietary frameworks.

### E. Documentation Drift Detection
Precise, not keyword-based:

*   **Intra-doc links:** Parses rustdoc JSON for `[\`PaymentGateway\`]`-style references in doc comments and `/docs/*.md`. Exact symbol resolution, not substring match on `User`.
*   **Rustdoc example blocks:** Flags doctest examples that exercise changed symbols.
*   **Changelog heuristic:** If a `pub` item changed and `CHANGELOG.md` / `RELEASES.md` wasn't touched in the same diff → flag.

### F. Confidence Tiers
Every finding carries a tier. This replaces the fiction that static analysis is ever "99% sure."

| Tier | Score | Meaning | Example source |
| :--- | :--- | :--- | :--- |
| **Proven** | 0.95–1.00 | Resolved call-graph edge or rustdoc JSON symbol match | `fn a()` calls `fn b()` directly, both resolved |
| **Likely** | 0.60–0.94 | Trait impl via `dyn`, feature-gated caller, default-method non-overrider | Handler registered via `axum::Router::route` with expanded macro |
| **Possible** | 0.30–0.59 | Heuristic match, unexpanded macro residue, cross-crate without rustdoc | Identifier appears in doc comment without intra-doc link |
| **Unknown** | < 0.30 | Listed but not scored | Reflection via `Any`, runtime config, FFI callbacks, `OnceCell` mutation |

`--confidence-min=0.6` filters output for CI; default shows all tiers with the score attached.

---

## 4. CLI Interface (UX)

Flags below reflect the shipping v0.3.0 surface — `--context`, `--features`, and the `mcp` subcommand are all live. Flags still in flight: `--checklist` (the verification checklist is currently embedded inside `--format markdown` rather than a dedicated output), `--feature-powerset` (CI-grade matrix analysis, v0.4 scope). See §11 for the full roadmap.

```bash
# Analyze the current working tree against HEAD
cargo impact

# Emit a cargo-nextest filter expression for affected tests only
cargo impact --test
# Example output: test(auth_roundtrip) + test(api_smoke)

# Analyze against a specific revision (branch, tag, SHA)
cargo impact --since main

# AI-consumable formats
cargo impact --format text       # default — severity-grouped text with emoji icons
cargo impact --format markdown   # summary + per-severity sections + verification checklist
cargo impact --format json       # structured envelope; stable schema for agents

# CI gating
cargo impact --confidence-min 0.6   # hide Possible / Unknown findings
cargo impact --fail-on high         # exit 1 if any HIGH finding is emitted
cargo impact --fail-on medium       # exit 1 on HIGH or MEDIUM

# Feature-aware analysis — cfg(feature = "x") gates are evaluated against
# the resolved active set. Items whose gates don't match are stripped before
# every analyzer sees them.
cargo impact --features tokio,rt            # union with default features
cargo impact --features tokio --no-default-features
cargo impact --all-features                 # audit the full feature surface

# Opt-in public-API breakage detection (requires cargo-semver-checks on PATH;
# runs rustdoc twice internally, typically 10–30s).
cargo impact --semver-checks
```

### Sample report (text)

v0.2 is syn-only; no finding reaches the `Proven` tier — that is reserved for resolved call-graph analysis arriving with rust-analyzer in v0.3. Every score below is the *honest* ceiling for syntactic analysis.

```text
cargo-impact v0.2.0

Changed files (3):
  src/engine.rs
  src/ffi.rs
  build.rs

Candidate symbols (4):
  Greeter
  callback_t
  process_event
  UserProfile

🔴 HIGH (3)
  [f-0001] build.rs changed (build.rs) · Likely 0.90
  [f-0002] FFI callback_t modified in src/ffi.rs · Likely 0.95
  [f-0003] impl Greeter for Foo (src/engine.rs) · Likely 0.80

🟡 MEDIUM (2)
  [f-0004] test `api_smoke` (tests/integration.rs) references process_event · Likely 0.85
  [f-0005] dyn Greeter used in src/dispatch.rs · Likely 0.75

🔵 LOW (1)
  [f-0006] intra-doc link to UserProfile in docs/architecture.md:42 · Likely 0.90

⚪ UNKNOWN (0)
```

### Sample JSON envelope

Stable across releases; matches the MCP tool-call schema (§8) so the CLI and the future MCP server return identical shapes.

```json
{
  "version": "0.2.0",
  "changed_files": ["src/engine.rs", "src/ffi.rs", "build.rs"],
  "candidate_symbols": ["Greeter", "UserProfile", "callback_t", "process_event"],
  "findings": [
    {
      "id": "f-0001",
      "severity": "high",
      "tier": "likely",
      "confidence": 0.9,
      "kind": "build_script_changed",
      "file": "build.rs",
      "evidence": "build script `build.rs` changed — build scripts can invalidate downstream compilation in non-obvious ways (…)"
    },
    {
      "id": "f-0004",
      "severity": "medium",
      "tier": "likely",
      "confidence": 0.85,
      "kind": "test_reference",
      "test": { "file": "tests/integration.rs", "symbol": "api_smoke" },
      "matched_symbols": ["process_event"],
      "evidence": "test body references process_event (syntactic match, no name resolution)",
      "suggested_action": "cargo nextest run -E 'test(api_smoke)'"
    }
  ],
  "summary": {
    "total": 6,
    "by_severity": { "high": 3, "medium": 2, "low": 1 },
    "by_tier":     { "proven": 0, "likely": 6, "possible": 0, "unknown": 0 }
  }
}
```

### Markdown output

`--format markdown` produces a paste-to-AI-ready document: summary, per-severity sections, and a verification checklist with `- [ ]` items an agent can tick. Example shape:

```markdown
# cargo-impact v0.2.0 blast radius

## Summary
- **Changed files:** 3
- **Candidate symbols:** 4
- **Findings:** 6 (3 high, 2 medium, 1 low, 0 unknown)

## 🔴 HIGH (3)
- **[f-0001]** `build.rs` changed (build.rs) — *Likely 0.90* — build script changed …
- **[f-0002]** FFI `callback_t` modified in src/ffi.rs — *Likely 0.95* — blast radius leaves Rust …
…

## Verification checklist
- [ ] **HIGH** `build.rs` changed — *Likely 0.90*
- [ ] **HIGH** FFI `callback_t` modified in src/ffi.rs — *Likely 0.95*
- [ ] **MEDIUM** test `api_smoke` references process_event — *Likely 0.85*
…
```

---

## 5. Vibe Coding Workflow Integration

This completes the **Context → Code → Verify** loop:

1.  **Context:** `cargo context --fix | pbcopy` → AI generates a fix.
2.  **Apply:** Developer applies the AI's code.
3.  **Impact:** Developer runs `cargo impact`.
4.  **Verify:**
    *   `cargo impact` flags a specific integration test and one API endpoint (with confidence tiers).
    *   Developer runs `cargo impact --test` (5 seconds instead of 5 minutes).
    *   Developer runs `cargo impact --checklist --format=markdown | pbcopy` and pastes back to the AI: *"Here's the verification checklist — tick what you've addressed and tell me what you still need me to test manually."*

## 6. Summary Table: Context vs. Impact

| Feature | `cargo-context` (The Input) | `cargo-impact` (The Output) |
| :--- | :--- | :--- |
| **Goal** | Maximize AI understanding | Minimize human verification effort |
| **Focus** | What is the AI looking at? | What did the AI touch? |
| **Primary Tools** | `git diff` + `cargo metadata` + rustdoc JSON | `rust-analyzer` call graph + `cargo-difftests` + `cargo-semver-checks` |
| **Key Output** | A Markdown Context Pack | A confidence-tiered Blast Radius Report |
| **Vibe Shift** | No more copy-pasting files | No more "test all and pray" |

---

## 7. Integration with `cargo-context`

`cargo-impact` and `cargo-context` are designed as a **bidirectional pair**, not two isolated tools. They share cache, symbol index, and MCP surface.

### Forward flow: Context → Impact
The normal developer loop. AI edits files inside a context pack; `cargo-impact` analyzes the resulting diff.

```bash
cargo context --fix | pbcopy         # AI gets context, generates patch
# ... apply patch ...
cargo impact                         # analyze what the patch touched
```

### Reverse flow: Impact → Context ✅ shipped
`cargo-impact --context` emits a deduped, newline-delimited list of every file implicated in the blast radius (changed files + each finding's primary path). `cargo-context --files-from -` consumes that list directly and builds a context pack scoped to exactly those files:

```bash
cargo impact --context \
  | cargo context --files-from - \
  | pbcopy
# Pack contains only the blast-radius files, not the whole repo.
```

`cargo-context` applies its usual scrubber to each path (so `.env` etc. never leak raw secrets), skips missing paths with an accounting header, and prioritizes the scoped section at diff-level priority so it survives `--budget` pressure. Implementation: [cargo-context#5](https://github.com/asmuelle/cargo-context/issues/5).

### Scope-limited context packs ⏳ not scheduled
Passing the full `cargo-impact` JSON envelope (not just file paths) so `cargo-context` can prioritize by confidence tier, filter out findings already verified elsewhere, or emit per-finding mini-packs. A schema proposal is on record at [cargo-context#5](https://github.com/asmuelle/cargo-context/issues/5#issuecomment-4304409079), but the issue closed without acceptance and no concrete implementation is tracked on either side. Users who need this today can pipe `--format=json` into their own tooling; the envelope shape is stable within v0.3.

```bash
# If/when the cargo-context side lands:
cargo impact --format=json > .impact.json
cargo context --impact-scope=.impact.json   # per-finding packs
```

### Shared cache ⏳ deferred
The spec imagined both tools reading `target/ai-tools-cache/` with namespaced subdirectories (`context/`, `impact/`) so rust-analyzer's index is built once and shared. Neither tool ships this yet; each maintains its own cache. A real implementation needs the cache format versioned independently of both tools — tracked for a joint v0.4.

### Shared MCP server ⏳ deferred
Combined `cargo-ai-tools` MCP server that exposes both families under one process (see §8 for the cargo-impact side). Also v0.4+.

---

## 8. MCP Server Surface

`cargo-impact` ships as a CLI **and** an MCP server. Agents (Claude Code, Cursor, Zed, custom) connect over stdio and call tools directly — no copy-paste, no shell parsing.

```bash
cargo impact mcp           # start MCP server over stdio
cargo impact mcp --http    # or Streamable HTTP on localhost
```

### Tools exposed

| Tool | Purpose | Returns |
| :--- | :--- | :--- |
| `impact.analyze` | Run blast radius on current diff or commit range | Structured finding graph with tiers |
| `impact.test_filter` | Get a `cargo-nextest` filter expression for affected tests | String + rationale per test |
| `impact.checklist` | Generate the verification checklist | Markdown + JSON sibling |
| `impact.surface` | List affected runtime surfaces (routes, CLI subcommands, FFI) | Structured surface list |
| `impact.semver` | Classify public API changes | `{additive, breaking, none}` + reasons |
| `impact.explain` | Explain *why* a specific finding was flagged | Trace of the evidence chain |

### Tool schema example: `impact.analyze`

```json
{
  "name": "impact.analyze",
  "description": "Analyze the blast radius of a git diff in a Rust workspace.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "since": { "type": "string", "description": "Git ref, e.g. 'HEAD~1' or 'main'. Defaults to unstaged+staged." },
      "features": { "type": "array", "items": { "type": "string" } },
      "all_features": { "type": "boolean", "default": false },
      "confidence_min": { "type": "number", "minimum": 0, "maximum": 1 },
      "max_findings": { "type": "integer", "default": 200 }
    }
  }
}
```

### Response envelope
Every tool returns the same outer shape so agents can reason uniformly:

```json
{
  "findings": [
    {
      "id": "f-0001",
      "severity": "high|medium|low|unknown",
      "tier": "proven|likely|possible|unknown",
      "confidence": 0.98,
      "kind": "direct_call|trait_impl|dyn_dispatch|ffi|route|doc_drift|semver",
      "source": { "file": "src/core/engine.rs", "symbol": "process_event", "span": [41, 67] },
      "target": { "file": "tests/integration_tests.rs", "symbol": "api_smoke" },
      "evidence": "Resolved call-graph edge via ra_ap_ide::references",
      "suggested_action": "cargo nextest run -E 'test(api_smoke)'"
    }
  ],
  "summary": { "proven": 4, "likely": 7, "possible": 2, "unknown": 1 },
  "cache": { "hit": true, "build_time_ms": 142 }
}
```

### Why not just parse CLI output
Agents parsing pretty-printed CLI text is brittle and burns tokens. MCP tool calls return typed JSON, stream progress for long analyses, and let the agent ask `impact.explain(id="f-0007")` to drill into a specific finding without re-running the whole analysis.

---

## 9. Performance Targets

The "5 seconds, not 5 minutes" claim in §5 needs teeth. These are the SLOs the tool targets — not aspirational, but used as regression thresholds in the benchmark suite.

| Scenario | Target | Measured on |
| :--- | :--- | :--- |
| Warm run, <50 changed files, small workspace (<10 crates) | **< 500ms** | `cargo-impact` self-hosting benchmark |
| Warm run, typical workspace (10–30 crates) | **< 1.5s** | `ripgrep`, `zola`-sized repos |
| Warm run, large workspace (100+ crates) | **< 5s** | `rustc`, `deno`, internal monorepos |
| Cold run (first invocation, no cache) | **< 30s** for 100-crate workspace | includes RA index build |
| MCP response p95 | **< 200ms** after warm cache | per-tool-call latency |
| `--feature-powerset` on 8 features | **< 60s** | CI-only mode, not interactive |

### Cache strategy
*   **Keyed on:** `(file_content_hash, cargo_fingerprint, rustc_version, features_hash)`.
*   **Granularity:** Per-file symbol index, per-symbol call-graph edges. A one-line change in `src/utils.rs` invalidates only `utils`'s index and its dependents' edges — not the whole crate.
*   **Location:** `target/ai-tools-cache/impact/` (shared with `cargo-context`, see §7).
*   **Eviction:** LRU by access time; cap at 500MB by default, configurable via `CARGO_IMPACT_CACHE_SIZE`.
*   **Invalidation signal:** `cargo-impact` watches `Cargo.lock` and rustc version; bumps purge dependent caches.

### When performance degrades
If a run exceeds 2× the target for its scenario, `cargo-impact` emits a diagnostic:

```
⚠ cargo-impact: analysis took 4.2s (target < 1.5s for this workspace size).
  Likely cause: rust-analyzer index cold (cache dir recently cleared).
  Subsequent runs should be fast. Run `cargo impact --bench` to profile.
```

### Benchmark suite
`cargo impact --bench` runs a built-in benchmark against the current workspace, reports against the SLO table, and writes a JSON trace for regression tracking. Designed to be run in CI on the cargo-impact repo itself — no flaky wall-clock assertions, uses the cargo fingerprint to skip when inputs are unchanged.

---

## 10. Non-Goals

Scope discipline matters. `cargo-impact` is a **static impact oracle**, not a correctness checker. The following are explicitly out of scope and will not be added:

*   **Runtime behavior verification.** The tool does not execute code, trace syscalls, or observe actual runtime paths. A function can pass every affected test and still be broken in production; `cargo-impact` cannot detect that.
*   **Logic bug detection.** If the AI writes `a + b` where it meant `a - b` and the tests don't catch it, neither will `cargo-impact`. We tell you *what* to check, not *whether the logic is right*.
*   **Code review replacement.** A human (or reviewing agent) still reads the diff. The blast radius tells them where to focus, not what to conclude.
*   **Mutation testing.** That is [`cargo-mutants`](https://github.com/sourcefrog/cargo-mutants)'s job. If you want "would this test catch a bug if one existed," use that.
*   **Fuzzing / property testing integration.** Out of scope. Run `cargo-fuzz` / `proptest` separately and feed their failures back to the AI through whatever channel you already use.
*   **Type-level refactoring guidance.** We flag that `UserProfile` was modified; we do not advise on whether a different type design would have been better.
*   **Runtime tracing or profiling.** Not a flamegraph, not a tokio console, not a perf tool.
*   **IDE diagnostics.** `rust-analyzer` already does this. We consume its index; we do not compete with it.
*   **Formatting, linting, or style enforcement.** `rustfmt` and `clippy` exist.
*   **Dependency vulnerability scanning.** That is [`cargo-audit`](https://rustsec.org) / [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny).
*   **Build-time regression detection.** Changes that blow up compile time are real but invisible to us — use [`cargo-bloat`](https://github.com/RazrFalcon/cargo-bloat) or `-Z self-profile`.
*   **Cross-language impact.** A Rust change that breaks a Python FFI consumer is flagged at the `extern "C"` boundary (§3B) but we do not trace into the foreign language.
*   **Non-Rust workspaces.** We are `cargo-*`. Polyglot monorepos are out of scope; run `cargo-impact` against the Rust portion and compose with your own tooling for the rest.

### What we *will* integrate but not reinvent
*   Selective testing → `cargo-difftests`
*   Semver classification → `cargo-semver-checks`
*   Public-API diffing → `cargo-public-api`
*   Test execution → `cargo-nextest`
*   Name resolution → `rust-analyzer` (as library)

If an existing tool solves a subproblem well, we orchestrate it. If we find ourselves reimplementing one, that is a signal we are off-mission.

---

## 11. Implementation Roadmap

The spec is deliberately ambitious. These milestones are the cut points where the tool is genuinely useful to a real user, not a lab demo. Each milestone ships independently.

### v0.1 — "Surgical test filter" (the MVP) ✅ shipped
**Goal:** A Rust developer saves time on `cargo test` today, with zero AI integration.

*   ✅ `cargo impact` parses `git diff` with `syn` → candidate symbols
*   ⏭ `rust-analyzer` (as library) resolves direct call-graph references — deferred to v0.3; v0.2 uses syn-only token matching honestly tiered `Likely`
*   ✅ Emits a `cargo-nextest` filter expression via `--test`
*   ✅ Human-readable text output

**Deliberately deferred:** macros, traits, features, MCP, framework adapters, confidence tiers, public-API analysis. If v0.1 isn't a 2-week project, we're over-engineering.

**Success metric:** On the `ripgrep` workspace, typical edits trigger <10% of tests with zero false negatives across 50 seeded changes.

### v0.2 — "Honest blast radius" (in progress — core shipped)
**Goal:** The report earns the name. Confidence tiers, trait handling, and the first AI-consumable format.

*   ⚠ Macro expansion — `#[derive(...)]` now recognized (matched on last path segment, so `serde::Serialize` / `clap::Parser` / etc. all resolve). Full `cargo expand` / HIR pass for attribute and `fn`-like macros still deferred; nightly toolchain boundary
*   ✅ Trait ripple differentiation (§3B): required vs. default vs. new method — per-method HEAD-vs-WT classification shipping in addition to the blanket `TraitImpl` scan
*   ✅ `dyn Trait` dispatch edges as `Likely 0.75`
*   ✅ Confidence tiers (§3F) with numeric scores (`Proven` reserved for v0.3 RA integration)
*   ✅ `--format=json` and `--format=markdown`
*   ✅ `--features` / `--all-features` / `--no-default-features` — cfg-aware AST filtering against the resolved feature set
*   ✅ `cargo-semver-checks` integration (opt-in via `--semver-checks`)
*   ✅ `--confidence-min` and `--fail-on={high,medium,low}` for CI
*   ✅ Documentation drift via intra-doc links (plus length-gated keyword fallback)
*   ✅ Bonus: diff-aware candidate symbols (HEAD-vs-WT per-item comparison) + FFI signature tracking + `build.rs` change detection

**Success metric:** On the cargo-impact repo itself, 95% of `Proven`-tier findings correspond to tests that actually fail when the finding is seeded as a regression. *(v0.2 emits no `Proven` findings — the metric re-activates once rust-analyzer integration lands in v0.3.)*

### v0.3 — "Agent-native"
**Goal:** First-class AI integration. The tool is now consumed by agents, not just humans.

*   ✅ MCP server (`cargo impact mcp`) — all six §8 tools (`impact_analyze`, `impact_test_filter`, `impact_surface`, `impact_semver`, `impact_explain`, `impact_version`) ship in v0.3.0.
*   ✅ Rust-analyzer integration for the `Proven` tier — LSP stdio client with Content-Length framing, initialize handshake, indexing-progress wait, `documentSymbol` + `references` queries, emitting `ResolvedReference` findings at `Tier::Proven`.
*   ✅ Content-hashed finding IDs so `impact_explain` can round-trip by ID across runs.
*   ✅ `--context` bridge to `cargo-context` (forward-flow shipped via `cargo-context --files-from -`; JSON-envelope `--impact-scope` not scheduled — see §7)
*   ⏳ Framework adapters: `axum`, `clap` (reference implementations); documented adapter trait for third parties
*   ⏳ `cargo impact log-miss` for ground-truth collection
*   ⏳ Token budgeting on markdown output
*   ⏳ Configuration file (`cargo-impact.toml`) + `.impactignore`

**Success metric:** A Claude Code / Cursor session can complete a non-trivial Rust refactor using only MCP tool calls — no shell output parsing, no manual context assembly.

### v0.4 — "Production-grade"
**Goal:** A Rust developer can drop `cargo-impact` into their CI pipeline, have it gate PRs, and trust the numbers. Today the tool works on a laptop; v0.4 is when it works in CI.

What "production-grade" actually means, broken into concrete cuts.

#### v0.4 core — must ship

The minimum set that makes the CI-gate story real. If v0.4 ships with only these, it's still a meaningful release.

*   **`--format sarif`** — SARIF v2.1.0 output that GitHub code scanning, GitLab security, and every other security-scanner UI already knows how to render. Makes cargo-impact findings appear inline on PR diffs without any custom glue.
*   **GitHub Actions composite action** — `uses: asmuelle/cargo-impact-action@v1` with sane defaults (runs on PR, uploads SARIF, comments the markdown report). Target: a first-time user can gate their repo with ≤10 lines of YAML.
*   **PR-comment output mode** — `--format=pr-comment` renders the markdown optimized for GitHub PR comments (collapsed `<details>` per severity, severity-badge headers, `#<N>` cross-links to the SARIF upload). Complements the SARIF path for teams that don't run code scanning.
*   **Deterministic output** — strip timestamps, pin sort orders, fix format-version strings, make two runs against the same diff byte-identical. Required for any CI that diffs output across runs or caches by content hash.
*   **Benchmark suite + SLO regression gates** — `cargo bench`-based timing against a fixture matrix. Committed baselines in `benches/baseline.json`; CI workflow `.github/workflows/bench.yml` runs `scripts/bench-gate.sh` on every PR and fails if any bench's p50 exceeds baseline × 2.5 (threshold absorbs GH-runner variance, catches genuine 2x+ regressions). Re-baseline with `BENCH_GATE_MODE=update`.

**v0.4 core success metric:** A maintainer of a real open-source Rust project (not us) can add cargo-impact to their CI in under 15 minutes following the README, and a PR that breaks a trait contract surfaces as a blocking annotation on their code-scanning UI.

#### v0.4 stretch — ship if the core is solid

Higher-value precision improvements that depend on the core being landed first. Any of these would be individually a meaningful release; we ship whichever are ready.

*   ✅ **Macro expansion via `cargo expand`** — _shipped (MVP scope)._ `--macro-expand` shells to `cargo expand --lib`, parses the output as one merged `syn::File`, and emits additional `TraitImpl` findings for impls synthesized by derive / attribute macros (serde, clap, thiserror, tokio). Evidence on expansion-backed findings is suffixed with `(revealed by macro expansion)` so consumers can distinguish them from source-visible impls. Attribute-macro body re-analysis and proper source-map back to the unexpanded file remain deferred — the MVP catches the common `#[derive(Serialize)] → impl Serialize` case that syn-only analysis misses. Graceful no-op when `cargo-expand` is absent from PATH.
*   ✅ **Per-reference severity refinement** — _shipped._ Each `ResolvedReference` is classified by its enclosing container: test fn → `Low` (test-only), impl block → `High` (impl breakage propagates), caller → `Medium` (default). Syn-based classifier in `src/ref_context.rs`; RA still resolves the reference, we refine the severity.
*   ✅ **syn/RA finding dedup and tier upgrade** — _shipped._ When a syn analyzer flags a site `Likely` and RA confirms it at `Proven`, the syn finding is dropped so the report doesn't double-count. See `src/dedup.rs`.
*   ✅ **`--feature-powerset` (CI mode)** — _shipped (depth-1 scope)._ Runs the analyzer under baseline, `--no-default-features`, and `--all-features` in sequence, merging findings by content-hashed ID. Findings visible only under a non-baseline set are annotated in evidence with the feature set that revealed them. Full O(2^N) powerset across individual features is out of scope; the depth-1 view catches the common std/no_std and sync/async feature-gated blast radius without combinatorial blow-up.
*   ✅ **Streaming progress over MCP** — _shipped._ The MCP `impact_analyze` tool emits `notifications/message` events (`level: "info"`, `logger: "cargo-impact"`, `data: { stage, current, total, detail? }`) at each analyzer stage boundary, ending with a `stage: "done"` event before the final `result`. Clients that ignore unknown notifications see no change; clients that render log messages get live feedback during the `--rust-analyzer` / `--semver-checks` long tail. Underlying API: `analyze_with_progress()` in the library.
*   ✅ **More framework adapters** — _shipped: `actix-web` + `rocket`._ Method-chain visitors (`.route` / `.service` / `.scope` / `.mount`) plus a shared HTTP-verb attribute-macro pass with framework disambiguation via use-statement scan. `tauri` / `dioxus` / `leptos` remain on-demand.

#### v0.4 explicit non-goals

Called out here so nobody expects them landing in this milestone — each is a real user-facing request, each is deliberately scoped out.

*   **`no_std` / `wasm32` cross-target support** — genuinely useful, but requires target-triple-aware cfg evaluation and a second analysis pass. Moved to v0.5.
*   **Polyrepo / cross-workspace** — path dependencies and git dependencies spanning repos. Design work needed before scoping; Beyond-v0.4.
*   **`git bisect` driver using impact data** — neat, but narrow enough user base to wait for demand.
*   **`cargo-difftests` coverage integration** — external tool still pre-1.0; fall back to call-graph-only test selection until its shape stabilizes.
*   **IDE integration (VS Code / Zed)** — users can already get structured output via the MCP server. A dedicated editor extension is Beyond-v0.4.
*   **Shared `target/ai-tools-cache/` with cargo-context** — depends on maintainer alignment on cache format; treat as v0.5+ joint work.

### Beyond v0.4
Unscheduled, prioritized by demand rather than by us:

*   IDE integration (VS Code extension, Zed LSP plugin)
*   `git bisect` driver using impact data to narrow the search
*   Historical blast-radius mining (find under-tested hot spots across a year of history)
*   Cross-workspace impact for polyrepo setups with path dependencies
*   `--impact-scope` JSON-envelope consumer on the cargo-context side (pending cargo-context maintainer scheduling; schema proposal on record at [cargo-context#5](https://github.com/asmuelle/cargo-context/issues/5#issuecomment-4304409079))

### What could kill each milestone
Honest risk log, not hand-wave:

| Milestone | Biggest risk | Mitigation |
| :--- | :--- | :--- |
| v0.1 | `ra_ap_*` API churn between rust-analyzer releases | Pin RA version per release; fall back to LSP protocol if library API breaks |
| v0.2 | Macro expansion is too slow or too incomplete on real workspaces | Downgrade confidence on unexpanded macros rather than fail; document known-bad proc macros |
| v0.3 | MCP ecosystem fragments before stabilizing | Ship stdio + Streamable HTTP; keep CLI first-class so tool isn't MCP-dependent |
| v0.4 | SARIF shape evolves after we ship; GitHub Actions billing or permissions model changes for the composite action | Target SARIF v2.1.0 (well-established, used by every major scanner); keep `--format=json` as the stable ground truth so SARIF is a downstream renderer; composite action is a thin wrapper with no business logic. `cargo-difftests` specifically — fall back to call-graph-only test selection (less precise but still useful) if that tool doesn't stabilize. |

---

## License

Dual-licensed under either of

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
* MIT License ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option. Per Rust ecosystem convention.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
