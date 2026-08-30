#!/usr/bin/env bash
# End-to-end benchmark: jsh vs bash/zsh using hyperfine
#
# Usage:
#   ./bench.sh              # build release and run all benchmarks
#   ./bench.sh --skip-build # skip cargo build

set -euo pipefail

JSH="./target/release/jsh"
SHELLS=()

# --- Build jsh in release mode ---
if [[ "${1:-}" != "--skip-build" ]]; then
    echo "==> Building jsh in release mode..."
    cargo build --release 2>&1
    echo ""
fi

if ! command -v hyperfine &>/dev/null; then
    echo "ERROR: hyperfine not found. Install with: cargo install hyperfine"
    exit 1
fi

if [[ ! -x "$JSH" ]]; then
    echo "ERROR: $JSH not found. Run: cargo build --release"
    exit 1
fi

# Detect available POSIX-compatible shells (fish excluded - different syntax)
for sh in bash zsh; do
    if command -v "$sh" &>/dev/null; then
        SHELLS+=("$sh")
    fi
done
echo "==> Shells: jsh ${SHELLS[*]}"
echo ""

# Helper for -c flag benchmarks
run_bench_c() {
    local name="$1"
    local cmd="$2"
    local cmds=()
    for sh in jsh "${SHELLS[@]}"; do
        local shell_bin
        if [[ "$sh" == "jsh" ]]; then
            shell_bin="$JSH"
        else
            shell_bin="$(command -v "$sh")"
        fi
        cmds+=(-n "$sh" "$shell_bin -c '$cmd'")
    done
    echo "--- $name ---"
    hyperfine --warmup 3 --min-runs 50 --shell=none "${cmds[@]}" 2>&1
    echo ""
}

# ==========================================================================
# Benchmarks
# ==========================================================================

echo "=========================================="
echo " jsh end-to-end benchmarks (hyperfine)"
echo "=========================================="
echo ""

# 1. Startup time (empty command)
run_bench_c "Startup time (true)" "true"

# 2. Simple echo
run_bench_c "Simple echo" "echo hello"

# 3. Variable assignment and expansion
run_bench_c "Variable expansion" "FOO=bar; echo \$FOO"

# 4. Pipeline
run_bench_c "Simple pipeline" "echo hello | cat | cat | cat"

# 5. Many sequential commands
run_bench_c "10 sequential echos" "echo a; echo b; echo c; echo d; echo e; echo f; echo g; echo h; echo i; echo j"

# 6. Subshell
run_bench_c "Subshell" "(echo hello)"

# 7. Background + wait
run_bench_c "Background and wait" "echo hello &"

# 8. Redirects
run_bench_c "Redirect to /dev/null" "echo hello > /dev/null"

# 9. Long pipeline
run_bench_c "Long pipeline" "echo hello | cat | cat | cat | cat | cat | cat | cat"

# 10. Script execution via file
TMPSCRIPT=$(mktemp /tmp/jsh_bench_XXXXXX.sh)
cat > "$TMPSCRIPT" <<'SCRIPT'
echo start
FOO=hello
echo $FOO
echo world
echo done
SCRIPT

echo "--- Script file execution ---"
cmds=()
for sh in jsh "${SHELLS[@]}"; do
    if [[ "$sh" == "jsh" ]]; then
        shell_bin=$JSH
    else
        shell_bin=$(command -v "$sh")
    fi
    cmds+=(-n "$sh" "$shell_bin $TMPSCRIPT")
done
hyperfine --warmup 3 --min-runs 50 --shell=none "${cmds[@]}" 2>&1
echo ""

rm -f "$TMPSCRIPT"

echo "=========================================="
echo " Done! See target/criterion/ for detailed"
echo " micro-benchmark reports (cargo bench)."
echo "=========================================="
