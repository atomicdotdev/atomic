#!/usr/bin/env bash
# 31_init_lazy_untracked.sh — initialization records must not scan unrelated
# untracked files when their complete path set is already known.
#
# A FIFO makes this regression deterministic: the old implementation hashed
# every untracked entry and blocked opening the FIFO for reading. The optimized
# path never visits or reads it, so `atomic init` completes normally.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo "${BOLD}init skips unrelated untracked content${RESET}"

make_temp_repo init-lazy-untracked

if ! command -v mkfifo >/dev/null 2>&1; then
    skip_suite "mkfifo is unavailable"
fi

mkfifo unrelated.pipe

INIT_STATUS=0
atomic init >init.out 2>&1 &
INIT_PID=$!

# Keep the harness finite if this behavior regresses. Polling avoids relying on
# GNU `timeout`, which is not installed by default on macOS.
for _ in {1..100}; do
    if ! kill -0 "$INIT_PID" 2>/dev/null; then
        break
    fi
    sleep 0.1
done

if kill -0 "$INIT_PID" 2>/dev/null; then
    kill "$INIT_PID" 2>/dev/null || true
    wait "$INIT_PID" 2>/dev/null || true
    _fail "atomic init completes without reading the FIFO" \
        "initialization was still running after 10 seconds"
else
    wait "$INIT_PID" || INIT_STATUS=$?
    if [[ "$INIT_STATUS" -eq 0 ]]; then
        _pass "atomic init completes without reading the FIFO"
    else
        _fail "atomic init completes without reading the FIFO" \
            "atomic init exited $INIT_STATUS: $(head -5 init.out)"
    fi
fi

assert_dir_exists "repository metadata was initialized" .atomic
assert_dir_exists "vault was initialized" .vault
assert_output_contains "repository initialization remains a separate change" \
    "Initialize repository" atomic log
assert_output_contains "vault initialization remains a separate change" \
    "Initialize vault" atomic log

print_summary
