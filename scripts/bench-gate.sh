#!/usr/bin/env bash
# SLO-regression gate for cargo-impact benchmarks.
#
# Runs the criterion bench suite, parses each bench's p50 (median) from
# `target/criterion/<id>/new/estimates.json`, and compares against the
# committed baseline in `benches/baseline.json`. Fails the script if any
# current p50 exceeds baseline * BENCH_GATE_MULT (default 2.5).
#
# Why 2.5x default: GitHub ubuntu-latest runners show 20-40% per-run
# variance from cold caches and noisy-neighbor contention on shared
# hosts. A narrower multiplier trips on noise, not regressions. 2.5x
# catches real 2x+ slowdowns which is the signal we want.
#
# Env vars:
#   BENCH_GATE_MULT   — multiplier (default 2.5). Float accepted.
#   BENCH_GATE_MODE   — "check" (default) gates; "update" rewrites
#                        benches/baseline.json from the current run
#                        and exits 0 without gating. Use after a
#                        deliberate rebaseline.
#   BENCH_GATE_ONLY   — run only benchmarks whose id matches this
#                        substring (passed to criterion as a filter).
#                        Useful for local iteration.
#   BENCH_GATE_SKIP_RUN — "1" skips cargo bench and uses whatever
#                        estimates already sit under target/criterion.
#                        For fast local iteration after a prior run.
#
# Exit codes:
#   0  all benches within multiplier × baseline (or MODE=update)
#   1  one or more benches regressed past the gate
#   2  setup/parse error (missing baseline, jq/cargo absent, etc.)
#
# Dependencies: cargo, jq. Runs at repo root; call from any cwd.

set -euo pipefail

# Force C locale so awk's printf renders 0.12, not 0,12. Matters for
# CI log readability and for any downstream tooling that might try to
# parse the ratio column.
export LC_ALL=C

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

baseline="benches/baseline.json"
mult="${BENCH_GATE_MULT:-2.5}"
mode="${BENCH_GATE_MODE:-check}"
only="${BENCH_GATE_ONLY:-}"

if ! command -v jq >/dev/null 2>&1; then
    echo "bench-gate: jq is required but not found on PATH" >&2
    exit 2
fi
if ! command -v cargo >/dev/null 2>&1; then
    echo "bench-gate: cargo is required but not found on PATH" >&2
    exit 2
fi
if [[ ! -f "$baseline" ]]; then
    echo "bench-gate: baseline file missing: $baseline" >&2
    exit 2
fi

if [[ "${BENCH_GATE_SKIP_RUN:-0}" == "1" ]]; then
    echo "bench-gate: skipping cargo bench (BENCH_GATE_SKIP_RUN=1)"
else
    echo "bench-gate: running cargo bench (this takes ~1-2 minutes)..."
    bench_args=()
    if [[ -n "$only" ]]; then
        bench_args+=("$only")
    fi
    cargo bench --bench pipeline --all-features -- "${bench_args[@]}" >&2
fi

# Parse each baselined bench id out of baseline.json, locate the
# matching estimates.json from the current run, extract the median
# point estimate, compare.
ids=$(jq -r '.benchmarks | keys[]' "$baseline")

failed=0
declare -a rows
rows+=("bench_id|baseline_ns|threshold_ns|current_ns|ratio|status")

if [[ "$mode" == "update" ]]; then
    tmp=$(mktemp)
    jq '.benchmarks = {}' "$baseline" > "$tmp"
fi

while IFS= read -r id; do
    [[ -z "$id" ]] && continue
    est="target/criterion/${id}/new/estimates.json"
    if [[ ! -f "$est" ]]; then
        echo "bench-gate: missing estimates for '$id' at $est" >&2
        echo "bench-gate: (did the bench run? filter in effect: '${only:-none}')" >&2
        failed=1
        rows+=("$id|?|?|MISSING|?|ERROR")
        continue
    fi
    current=$(jq -r '.median.point_estimate' "$est")
    baseline_ns=$(jq -r ".benchmarks[\"$id\"].p50_ns" "$baseline")

    if [[ "$mode" == "update" ]]; then
        # Round current up to 3 significant digits to avoid overfitting
        # to a single run. awk produces a plain integer.
        rounded=$(awk -v v="$current" 'BEGIN{ printf "%.0f", v + 0.5 }')
        jq --arg k "$id" --argjson v "$rounded" \
            '.benchmarks[$k] = { p50_ns: $v }' "$tmp" > "${tmp}.next"
        mv "${tmp}.next" "$tmp"
        rows+=("$id|$baseline_ns|(update)|$current|-|UPDATED")
        continue
    fi

    threshold=$(awk -v b="$baseline_ns" -v m="$mult" 'BEGIN{ printf "%.0f", b * m }')
    ratio=$(awk -v c="$current" -v b="$baseline_ns" 'BEGIN{ if (b == 0) print "inf"; else printf "%.2f", c / b }')

    if awk -v c="$current" -v t="$threshold" 'BEGIN{ exit !(c > t) }'; then
        rows+=("$id|$baseline_ns|$threshold|$current|${ratio}x|FAIL")
        failed=1
    else
        rows+=("$id|$baseline_ns|$threshold|$current|${ratio}x|ok")
    fi
done <<< "$ids"

if [[ "$mode" == "update" ]]; then
    mv "$tmp" "$baseline"
    echo "bench-gate: baseline updated from current run."
fi

echo
echo "bench-gate: results (multiplier=${mult}x, mode=${mode})"
printf '%s\n' "${rows[@]}" | column -s'|' -t

if [[ $failed -ne 0 && "$mode" != "update" ]]; then
    echo
    echo "bench-gate: FAIL — one or more benches regressed beyond ${mult}x baseline."
    echo "bench-gate: if this is an intended change, run with BENCH_GATE_MODE=update"
    echo "bench-gate: and commit the regenerated benches/baseline.json."
    exit 1
fi

echo
echo "bench-gate: PASS"
exit 0
