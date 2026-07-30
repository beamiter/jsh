#!/usr/bin/env bash
# Phase 14c — vs-nushell wallclock benchmark.
#
# Runs equivalent structured-data pipelines through jsh and nushell and
# reports relative wallclock time. Skips nu cases if `nu` isn't installed.
#
# Usage:
#   ./bench_nu.sh              # build release and run all benches
#   ./bench_nu.sh --skip-build # reuse existing target/release/jsh

set -euo pipefail

JSH="./target/release/jsh"

if [[ "${1:-}" != "--skip-build" ]]; then
    echo "==> Building jsh in release mode..."
    cargo build --release 2>&1 | tail -5
    echo ""
fi

if ! command -v hyperfine &>/dev/null; then
    echo "ERROR: hyperfine not found. Install with: cargo install hyperfine"
    exit 1
fi

HAVE_NU=0
if command -v nu &>/dev/null; then
    HAVE_NU=1
    echo "==> nushell: $(nu --version)"
else
    echo "==> nushell: not installed — running jsh-only baseline"
fi
echo "==> jsh:      $($JSH --version 2>/dev/null || echo 'no --version')"
echo ""

# Generate a synthetic JSON list-of-records for the larger cases.
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

ROWS=10000
python3 - "$TMPDIR/big.json" "$ROWS" <<'PY'
import json, sys, random
path, n = sys.argv[1], int(sys.argv[2])
random.seed(42)
rows = [
    {"id": i, "group": i % 7, "score": random.random() * 100, "name": f"row-{i:05d}"}
    for i in range(n)
]
with open(path, "w") as f:
    json.dump(rows, f)
PY
BIG_JSON="$TMPDIR/big.json"

run_pair() {
    local title="$1"
    local jsh_script="$2"
    local nu_script="$3"

    echo "--- $title ---"
    local cmds=(-n jsh "$JSH -c '$jsh_script'")
    if [[ "$HAVE_NU" == "1" ]]; then
        cmds+=(-n nu "nu -c \"$nu_script\"")
    fi
    hyperfine --warmup 2 --min-runs 10 --shell=none "${cmds[@]}" 2>&1 || true
    echo ""
}

echo "============================================"
echo " jsh vs nushell — structured pipeline bench"
echo " (10k rows, JSON in $BIG_JSON)"
echo "============================================"
echo ""

# 1. Parse + count
run_pair "from-json | length" \
    "cat $BIG_JSON | from-json | length" \
    "open --raw $BIG_JSON | from json | length"

# 2. Filter
run_pair "from-json | where group == 3 | length" \
    "cat $BIG_JSON | from-json | where group == 3 | length" \
    "open --raw $BIG_JSON | from json | where group == 3 | length"

# 3. Map (each closure body)
run_pair "from-json | each {|r| \$r.score * 2} | math sum" \
    "cat $BIG_JSON | from-json | each {|r| \$r.score * 2} | math sum" \
    "open --raw $BIG_JSON | from json | each {|r| \$r.score * 2} | math sum"

# 4. Group-by aggregate
run_pair "group-by group" \
    "cat $BIG_JSON | from-json | group-by group | columns | length" \
    "open --raw $BIG_JSON | from json | group-by group | columns | length"

# 5. Sort-by
run_pair "sort-by score | take 10" \
    "cat $BIG_JSON | from-json | sort-by score | take 10 | length" \
    "open --raw $BIG_JSON | from json | sort-by score | first 10 | length"

# 6. Roundtrip JSON
run_pair "JSON in/out roundtrip" \
    "cat $BIG_JSON | from-json | to-json | from-json | length" \
    "open --raw $BIG_JSON | from json | to json | from json | length"

echo "============================================"
echo " Done. Lower mean time is faster."
echo "============================================"
