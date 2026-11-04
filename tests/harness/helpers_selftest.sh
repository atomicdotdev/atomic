#!/usr/bin/env bash
# Verify the shared harness reports failed commands instead of passing them.
set -euo pipefail

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
test_root="$(mktemp -d "${TMPDIR:-/tmp}/atomic-harness-helpers-XXXXXX")"
trap 'rm -rf "$test_root"' EXIT

printf '%s\n' \
    '#!/usr/bin/env bash' \
    'if [[ "${1:-}" == "view" && "${2:-}" == "list" ]]; then' \
    '    printf "dev [shared]\n  - * feature [draft]\n"' \
    '    if [[ "${3:-}" == "--all" ]]; then printf "  - empty [draft]\n"; fi' \
    '    exit 0' \
    'fi' \
    'echo "fixture command failed" >&2' \
    'exit 42' > "$test_root/atomic"
chmod +x "$test_root/atomic"
export ATOMIC_BIN="$test_root/atomic" HARNESS_DIR test_root

run_expect_failure() {
    local description="$1" script="$2" needle="$3" forbidden="${4:-}" output
    if output="$(bash -c "$script" 2>&1)"; then
        echo "FAIL: $description unexpectedly succeeded" >&2
        exit 1
    fi
    if [[ "$output" != *"$needle"* ]]; then
        echo "FAIL: $description did not report '$needle': $output" >&2
        exit 1
    fi
    if [[ -n "$forbidden" && "$output" == *"$forbidden"* ]]; then
        echo "FAIL: $description continued after its failed prerequisite: $output" >&2
        exit 1
    fi
    echo "PASS: $description"
}

run_expect_failure "failed command includes diagnostic" \
    'source "$HARNESS_DIR/helpers.sh"; assert_success "record" atomic record; print_summary' \
    'fixture command failed'
run_expect_failure "failed status is not clean" \
    'source "$HARNESS_DIR/helpers.sh"; assert_clean "clean"; print_summary' \
    'short status command failed'
run_expect_failure "failed status is not an absent path" \
    'source "$HARNESS_DIR/helpers.sh"; assert_status_no_entry "absent" file; print_summary' \
    'short status command failed'
run_expect_failure "failed command is not a successful output match" \
    'source "$HARNESS_DIR/helpers.sh"; assert_output_contains "match" "fixture" atomic record; print_summary' \
    'command failed'
run_expect_failure "failed command is not a successful negative output match" \
    'source "$HARNESS_DIR/helpers.sh"; assert_output_not_contains "no match" "absent" atomic record; print_summary' \
    'command failed'
run_expect_failure "expected-failure output assertion rejects success" \
    'source "$HARNESS_DIR/helpers.sh"; assert_failed_output_not_contains "must fail" "absent" printf ok; print_summary' \
    'expected command failure'
run_expect_failure "failed change command is reported for author" \
    'source "$HARNESS_DIR/helpers.sh"; assert_change_author "author" deadbeef Alice; print_summary' \
    'change command failed: fixture command failed'
run_expect_failure "failed change command is reported for message" \
    'source "$HARNESS_DIR/helpers.sh"; assert_change_message "message" deadbeef text; print_summary' \
    'change command failed: fixture command failed'
run_expect_failure "non-current view is not current" \
    'source "$HARNESS_DIR/helpers.sh"; assert_current_view "current" dev; print_summary' \
    "expected current view 'dev'"
run_expect_failure "early exit is reported as incomplete" \
    'source "$HARNESS_DIR/helpers.sh"; false' \
    'Remaining assertions were not run'
run_expect_failure "successful early exit is not a pass" \
    'export HARNESS_EXPECT_SUMMARY=1; source "$HARNESS_DIR/helpers.sh"; true' \
    'without its assertion summary'
run_expect_failure "failed prerequisite stops the suite" \
    'source "$HARNESS_DIR/helpers.sh"; require_success "record" atomic record; echo MUST_NOT_RUN' \
    'fixture command failed' 'MUST_NOT_RUN'
run_expect_failure "failed output capture stops the suite" \
    'source "$HARNESS_DIR/helpers.sh"; require_capture "record" atomic record; echo MUST_NOT_RUN' \
    'fixture command failed' 'MUST_NOT_RUN'

skip_status=0
HARNESS_EXPECT_SUMMARY=1 bash -c 'source "$HARNESS_DIR/helpers.sh"; skip_suite "fixture"' >/dev/null 2>&1 || skip_status=$?
if [[ "$skip_status" -ne 77 ]]; then
    echo "FAIL: skipped suite returned $skip_status instead of 77" >&2
    exit 1
fi
echo "PASS: explicit suite skip stays skipped"

partial_status=0
HARNESS_EXPECT_SUMMARY=1 bash -c 'source "$HARNESS_DIR/helpers.sh"; _pass "exercised"; _skip "not exercised" "fixture"; print_summary' >/dev/null 2>&1 || partial_status=$?
if [[ "$partial_status" -ne 78 ]]; then
    echo "FAIL: partially exercised suite returned $partial_status instead of 78" >&2
    exit 1
fi
echo "PASS: skipped assertions make a suite partial"

mkdir -p "$test_root/runner"
cp "$HARNESS_DIR/helpers.sh" "$HARNESS_DIR/run_all.sh" "$test_root/runner/"
printf '%s\n' 'source "$(dirname "$0")/helpers.sh"' '_pass "exercised"' 'print_summary' > "$test_root/runner/01_full.sh"
printf '%s\n' 'source "$(dirname "$0")/helpers.sh"' '_pass "exercised"' '_skip "missing" "fixture"' 'print_summary' > "$test_root/runner/02_partial.sh"
printf '%s\n' 'source "$(dirname "$0")/helpers.sh"' 'skip_suite "fixture"' > "$test_root/runner/03_skip.sh"
runner_status=0
runner_out="$(bash "$test_root/runner/run_all.sh" 02 2>&1)" || runner_status=$?
if [[ "$runner_status" -ne 78 || "$runner_out" != *"Suite PARTIAL"* || "$runner_out" == *"Suite PASSED"* ]]; then
    echo "FAIL: runner misreported partial suite (exit $runner_status): $runner_out" >&2
    exit 1
fi
runner_status=0
runner_out="$(bash "$test_root/runner/run_all.sh" 03 2>&1)" || runner_status=$?
if [[ "$runner_status" -ne 77 || "$runner_out" != *"Suite SKIPPED"* || "$runner_out" == *"Suite PASSED"* ]]; then
    echo "FAIL: runner misreported skipped suite (exit $runner_status): $runner_out" >&2
    exit 1
fi
runner_status=0
runner_out="$(bash "$test_root/runner/run_all.sh" 01 03 2>&1)" || runner_status=$?
if [[ "$runner_status" -ne 78 || "$runner_out" != *"1 passed"* || "$runner_out" != *"1 skipped"* ]]; then
    echo "FAIL: runner misreported mixed coverage (exit $runner_status): $runner_out" >&2
    exit 1
fi
echo "PASS: runner distinguishes full, partial, and skipped suites"

bash -c 'source "$HARNESS_DIR/helpers.sh"; assert_current_view "current" feature; print_summary' >/dev/null
bash -c 'source "$HARNESS_DIR/helpers.sh"; assert_failed_output_contains "expected error" "fixture" atomic record; assert_failed_output_not_contains "not internal" "absent" atomic record; print_summary' >/dev/null
bash -c 'source "$HARNESS_DIR/helpers.sh"; assert_view_exists "empty draft" empty; print_summary' >/dev/null
run_expect_failure "view names are matched exactly" \
    'source "$HARNESS_DIR/helpers.sh"; assert_view_exists "missing" empt; print_summary' \
    "view 'empt' not found"
bash -c 'source "$HARNESS_DIR/helpers.sh"; source "$HARNESS_DIR/merge_helpers.sh"; printf "present\n" > "$test_root/merge-file"; assert_occurrence_count "absent" "$test_root/merge-file" missing 0; print_summary' >/dev/null
run_expect_failure "missing file cannot pass a negative content assertion" \
    'source "$HARNESS_DIR/helpers.sh"; source "$HARNESS_DIR/merge_helpers.sh"; assert_file_not_contains "missing file" "$test_root/no-file" text; print_summary' \
    'file does not exist'
bash -c 'source "$HARNESS_DIR/helpers.sh"; cd "$test_root"; create_file empty ""; [[ -f empty && ! -s empty ]]; append_file empty ""; [[ ! -s empty ]]; overwrite_file empty "value"; overwrite_file empty ""; [[ ! -s empty ]]' >/dev/null
echo "PASS: current nested view and zero-byte file"
