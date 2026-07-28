#!/usr/bin/env bash
# run_all.sh — Main test harness runner for Atomic CLI.
#
# Executes every test suite in order and produces a combined summary.
#
# Usage:
#   ./tests/harness/run_all.sh              # run everything
#   ./tests/harness/run_all.sh 01 03        # run only suites 01 and 03
#   ATOMIC_BIN=/path/to/atomic ./tests/harness/run_all.sh  # custom binary
#
# Environment variables:
#   ATOMIC_BIN   — path to the atomic binary (auto-detected if unset)
#   VERBOSE      — set to 1 for full output even on success
#   STOP_ON_FAIL — set to 1 to abort after the first failing suite

set -euo pipefail

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HARNESS_DIR/../.." && pwd)"
HARNESS_SKIP_EXIT=77
export HARNESS_SKIP_EXIT

# ── Colours ─────────────────────────────────────────────────────────────────

if [[ -t 1 ]]; then
    RED=$'\033[0;31m'
    GREEN=$'\033[0;32m'
    YELLOW=$'\033[0;33m'
    CYAN=$'\033[0;36m'
    BOLD=$'\033[1m'
    RESET=$'\033[0m'
else
    RED="" GREEN="" YELLOW="" CYAN="" BOLD="" RESET=""
fi

# ── Build binary if needed ──────────────────────────────────────────────────

if [[ -z "${ATOMIC_BIN:-}" ]]; then
    if [[ -x "$REPO_ROOT/target/debug/atomic" ]]; then
        ATOMIC_BIN="$REPO_ROOT/target/debug/atomic"
    elif [[ -x "$REPO_ROOT/target/release/atomic" ]]; then
        ATOMIC_BIN="$REPO_ROOT/target/release/atomic"
    else
        echo "${YELLOW}Building atomic binary …${RESET}"
        (cd "$REPO_ROOT" && cargo build -p atomic-cli --quiet 2>/dev/null) || {
            echo "${RED}FATAL: could not build atomic binary${RESET}" >&2
            exit 1
        }
        ATOMIC_BIN="$REPO_ROOT/target/debug/atomic"
    fi
fi
export ATOMIC_BIN

echo ""
echo "${BOLD}╔═══════════════════════════════════════════════════════════╗${RESET}"
echo "${BOLD}║       Atomic CLI — Integration Test Harness              ║${RESET}"
echo "${BOLD}╚═══════════════════════════════════════════════════════════╝${RESET}"
echo ""
echo "  Binary:  ${CYAN}${ATOMIC_BIN}${RESET}"
echo "  Harness: ${CYAN}${HARNESS_DIR}${RESET}"
echo ""

# ── Discover suites ─────────────────────────────────────────────────────────

# Test suites are shell scripts matching NN_name.sh in the harness directory.
# They are sorted by the numeric prefix so they run in a deterministic order.

ALL_SUITES=()
while IFS= read -r script; do
    ALL_SUITES+=("$script")
done < <(find "$HARNESS_DIR" -maxdepth 1 -name '[0-9][0-9]_*.sh' -type f | sort)

if [[ ${#ALL_SUITES[@]} -eq 0 ]]; then
    echo "${RED}No test suites found in $HARNESS_DIR${RESET}"
    exit 1
fi

# ── Filter suites (optional) ───────────────────────────────────────────────

# If positional arguments are given, treat them as prefix filters.
#   ./run_all.sh 01 03   → only run 01_*.sh and 03_*.sh
SELECTED_SUITES=()
if [[ $# -gt 0 ]]; then
    for arg in "$@"; do
        for suite in "${ALL_SUITES[@]}"; do
            base="$(basename "$suite")"
            if [[ "$base" == "${arg}"* ]]; then
                SELECTED_SUITES+=("$suite")
            fi
        done
    done
    if [[ ${#SELECTED_SUITES[@]} -eq 0 ]]; then
        echo "${RED}No suites matched the filter(s): $*${RESET}"
        echo "Available suites:"
        for s in "${ALL_SUITES[@]}"; do
            echo "  $(basename "$s")"
        done
        exit 1
    fi
else
    SELECTED_SUITES=("${ALL_SUITES[@]}")
fi

echo "  Suites:  ${#SELECTED_SUITES[@]} selected"
for s in "${SELECTED_SUITES[@]}"; do
    echo "           • $(basename "$s")"
done
echo ""

# ── Run suites ──────────────────────────────────────────────────────────────

TOTAL_SUITES=0
PASSED_SUITES=0
SKIPPED_SUITES=0
FAILED_SUITES=0
SKIPPED_SUITE_NAMES=()
FAILED_SUITE_NAMES=()

TOTAL_TESTS=0
TOTAL_PASSED=0
TOTAL_FAILED=0
TOTAL_SKIPPED=0

for suite in "${SELECTED_SUITES[@]}"; do
    suite_name="$(basename "$suite" .sh)"
    TOTAL_SUITES=$((TOTAL_SUITES + 1))

    echo "${BOLD}${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    echo "${BOLD}  Suite: ${suite_name}${RESET}"
    echo "${BOLD}${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"

    suite_start="$(date +%s)"

    # Run the suite.  Capture exit code but don't abort run_all.
    set +e
    bash "$suite"
    suite_exit=$?
    set -e

    suite_end="$(date +%s)"
    suite_duration=$((suite_end - suite_start))

    echo ""
    echo "  ${CYAN}(${suite_duration}s)${RESET}"

    if [[ $suite_exit -eq 0 ]]; then
        PASSED_SUITES=$((PASSED_SUITES + 1))
        echo "  ${GREEN}Suite PASSED${RESET}"
    elif [[ $suite_exit -eq $HARNESS_SKIP_EXIT ]]; then
        SKIPPED_SUITES=$((SKIPPED_SUITES + 1))
        SKIPPED_SUITE_NAMES+=("$suite_name")
        echo "  ${YELLOW}Suite SKIPPED${RESET}"
    else
        FAILED_SUITES=$((FAILED_SUITES + 1))
        FAILED_SUITE_NAMES+=("$suite_name")
        echo "  ${RED}Suite FAILED (exit $suite_exit)${RESET}"

        if [[ "${STOP_ON_FAIL:-0}" == "1" ]]; then
            echo ""
            echo "${RED}STOP_ON_FAIL is set — aborting remaining suites.${RESET}"
            break
        fi
    fi

    echo ""
done

# ── Combined summary ────────────────────────────────────────────────────────

echo ""
echo "${BOLD}╔═══════════════════════════════════════════════════════════╗${RESET}"
echo "${BOLD}║                   Combined Results                       ║${RESET}"
echo "${BOLD}╠═══════════════════════════════════════════════════════════╣${RESET}"
echo "${BOLD}║${RESET}                                                           ${BOLD}║${RESET}"
echo "${BOLD}║${RESET}  Suites: ${GREEN}${PASSED_SUITES} passed${RESET}  ${YELLOW}${SKIPPED_SUITES} skipped${RESET}  ${RED}${FAILED_SUITES} failed${RESET} / ${TOTAL_SUITES} total  ${BOLD}║${RESET}"
echo "${BOLD}║${RESET}                                                           ${BOLD}║${RESET}"

if [[ ${#SKIPPED_SUITE_NAMES[@]} -gt 0 ]]; then
    echo "${BOLD}║${RESET}  ${YELLOW}Skipped suites:${RESET}                                         ${BOLD}║${RESET}"
    for name in "${SKIPPED_SUITE_NAMES[@]}"; do
        printf "${BOLD}║${RESET}    ${YELLOW}• %-51s${RESET} ${BOLD}║${RESET}\n" "$name"
    done
    echo "${BOLD}║${RESET}                                                           ${BOLD}║${RESET}"
fi

if [[ ${#FAILED_SUITE_NAMES[@]} -gt 0 ]]; then
    echo "${BOLD}║${RESET}  ${RED}Failed suites:${RESET}                                          ${BOLD}║${RESET}"
    for name in "${FAILED_SUITE_NAMES[@]}"; do
        printf "${BOLD}║${RESET}    ${RED}• %-51s${RESET} ${BOLD}║${RESET}\n" "$name"
    done
    echo "${BOLD}║${RESET}                                                           ${BOLD}║${RESET}"
fi

echo "${BOLD}╚═══════════════════════════════════════════════════════════╝${RESET}"
echo ""

# ── Exit code ───────────────────────────────────────────────────────────────

if [[ $FAILED_SUITES -gt 0 ]]; then
    exit 1
fi
exit 0
