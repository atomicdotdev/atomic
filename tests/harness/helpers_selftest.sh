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

bash -c 'source "$HARNESS_DIR/helpers.sh"; assert_current_view "current" feature; print_summary' >/dev/null
bash -c 'source "$HARNESS_DIR/helpers.sh"; assert_view_exists "empty draft" empty; print_summary' >/dev/null
run_expect_failure "view names are matched exactly" \
    'source "$HARNESS_DIR/helpers.sh"; assert_view_exists "missing" empt; print_summary' \
    "view 'empt' not found"
bash -c 'source "$HARNESS_DIR/helpers.sh"; cd "$test_root"; create_file empty ""; [[ -f empty && ! -s empty ]]; append_file empty ""; [[ ! -s empty ]]; overwrite_file empty "value"; overwrite_file empty ""; [[ ! -s empty ]]' >/dev/null
echo "PASS: current nested view and zero-byte file"
