# CLAUDE.md

Orientation for AI assistants working in this repo. Loaded automatically
by Claude Code; readable by other agents that respect this convention.

## What this crate is

`cargo-impact` is a `cargo` subcommand that answers "which tests and
surfaces need re-verifying after *this* diff?". It reads a git diff,
classifies changed items, walks the workspace for references / impls /
framework surfaces / doc drift / FFI boundaries, and emits a
confidence-tiered finding list. v0.3.0 stable, on crates.io.

## Use the MCP tools before raw CLI parsing

Both tools in this repo's `.mcp.json` are auto-registered for your
session. Prefer them over shelling to Bash:

- **`impact_analyze`** — full blast-radius report (JSON). Takes the
  same args as the CLI (`since`, `features`, `confidence_min`,
  `semver_checks`, `rust_analyzer`, `budget`). Returns the structured
  envelope documented in README §8.
- **`impact_test_filter`** — returns a `cargo-nextest` filter
  expression you can paste into a nextest invocation.
- **`impact_surface`** — projects a report to runtime-surface findings
  (FFI, build.rs, trait impls, derive impls). Use when reasoning about
  what ships vs. what's only test-visible.
- **`impact_semver`** — forces `cargo-semver-checks` on and returns
  its findings. Slow (10–30s); don't call speculatively.
- **`impact_explain(finding_id)`** — drill into a single finding by
  its content-hashed ID. IDs are stable across runs, so you can
  store one from an earlier call and round-trip it later.
- **`build_context_pack`** (from cargo-context) — assembles a
  token-budgeted pack of relevant files. Pair it with
  `cargo impact --context | cargo context --files-from -` for the
  blast-radius-scoped flow.

Shell to Bash only when the MCP surface doesn't cover the query
(benchmarking, interactive debugging, cargo toolchain operations).

## Local-verify triple before any commit

Every commit to main gates on three checks in CI. Run them all
locally before pushing, on the pinned MSRV toolchain:

```bash
cargo +1.95 fmt --all --check
cargo +1.95 clippy --all-targets --all-features --locked -- -D warnings
cargo +1.95 test --all-features --locked
```

The test total is **163** (155 lib + 7 integration + 1 doctest).
Any added code should add tests; any green-to-green diff should keep
that total accurate in the commit message.

## Project conventions

- **Rust edition 2024, MSRV 1.95**. Don't bump either without a
  reason you can point to in a commit message.
- **Honest tiering.** The `Tier` enum has four values: `Proven`,
  `Likely`, `Possible`, `Unknown`. Only name-resolved findings
  (via `--rust-analyzer`) reach `Proven`. syn-only analyzers top
  out at `Likely` — don't paper over the distinction.
- **Content-hashed finding IDs** are stable across runs. Don't
  reintroduce sequential IDs; `impact_explain` relies on the
  cross-run consistency.
- **Graceful degradation everywhere.** If an optional tool
  (`rust-analyzer`, `cargo-semver-checks`) is missing, log a
  stderr notice and return an empty findings list — never fail
  the whole run.
- **Pre-commit hooks are not skipped.** The repo has
  `block-no-verify` configured; don't try `git commit --no-verify`.

## Commit style

Read `git log` before writing a message — this repo uses
multi-paragraph conventional-ish commits that lead with the *why*,
name the affected files/modules, and call out deferred scope
explicitly. Short `fix:` / `chore:` one-liners are fine for tiny
changes.

## Honest caveats baked into the design

Worth surfacing when users ask "why didn't cargo-impact flag X?":

- `cfg_attr(feature = "x", derive(…))` is invisible to our analyzer;
  over-counts slightly when users conditionally derive.
- Macro expansion is partial: derives are recognized (`src/derive.rs`),
  attribute/fn-like macros aren't. Full `cargo expand` integration is
  a v0.4+ item.
- `log-miss` records stay on disk only (`target/ai-tools-cache/`);
  we never phone home.

## Where to file things

- Bugs / feature requests: https://github.com/asmuelle/cargo-impact/issues
- Interop with cargo-context: `github.com/asmuelle/cargo-context`. Note
  the maintainer (also the cargo-impact maintainer) uses a
  silent-close-as-decision pattern — if an issue closes without comment,
  treat that as a "no, not now" signal unless told otherwise.

## Don't commit without

- A commit message that names the files/modules touched and the
  test-count delta if any
- `fmt` + `clippy -D warnings` + `test` all green on 1.95
- Updated `§11` roadmap markers if a ⏳ or ⚠ moved to ✅
