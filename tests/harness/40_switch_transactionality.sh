#!/usr/bin/env bash
# 40_switch_transactionality.sh — Recover failed view switches through disk materialization.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"
source "$HARNESS_DIR/merge_helpers.sh"

begin_section "Failed materialization rolls back the published view"
make_temp_repo switch-transaction
init_repo

printf 'cache/\n' > .atomicignore
printf 'source view\n' > source.txt
require_success "add source and ignore rules" atomic add .atomicignore source.txt
require_success "record source and ignore rules" record_change "add source file and ignore rules"
mkdir -p cache
printf 'source cache\n' > cache/state.txt
require_capture "read source view" current_view
SOURCE_VIEW="$COMMAND_OUTPUT"

require_success "create and switch to target" new_view target --draft --parent "$SOURCE_VIEW" --switch
mkdir -p nested
printf 'target view\n' > nested/target.txt
require_success "add target file" atomic add nested/target.txt
require_success "record target file" record_change "add target file"

require_success "switch to source" switch_view "$SOURCE_VIEW"
assert_file_not_exists "target-only file is absent on source" nested/target.txt
assert_file_content "source file is intact before failed switch" source.txt "source view"
assert_file_content "source ignored workspace is present before failure" cache/state.txt "source cache"

if fault_output="$(env ATOMIC_TEST_FAIL_SWITCH_AFTER_PUBLISH=1 "$ATOMIC_BIN" view switch target --force 2>&1)"; then
    _fail "switch fails after publishing but before materialization" "failpoint unexpectedly succeeded: $fault_output"
    _skip "publication rollback and later recovery checks" "failpoint did not fire"
    print_summary || true
    exit 1
else
    if [[ "$fault_output" == *"injected switch failure after view publication"* ]]; then
        _pass "switch fails after publishing but before materialization"
    else
        _fail "switch fails after publishing but before materialization" "wrong failure: $fault_output"
        _skip "publication rollback and later recovery checks" "requested failpoint did not fire"
        print_summary || true
        exit 1
    fi
fi

# The failed command exits with a durable recovery marker. The next command
# opens the repository writable and must roll the whole transition back.
assert_file_exists "failed switch leaves durable recovery marker" \
    .atomic/deferred-tree-alignment.pending
require_success "next writable open performs switch recovery" atomic add source.txt
assert_equal_value() {
    local desc="$1" expected="$2" actual="$3"
    if [[ "$expected" == "$actual" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "expected '$expected', got '$actual'"
    fi
}
assert_current_view "reopen restores source as current view" "$SOURCE_VIEW"
assert_file_not_exists "successful recovery clears pending marker" \
    .atomic/deferred-tree-alignment.pending
assert_file_content "source tracked content survives rollback" source.txt "source view"
assert_file_content "source ignored workspace is restored by rollback" cache/state.txt "source cache"
require_success "switch succeeds after obstruction is removed" \
    atomic view switch target --force
assert_current_view "target becomes current after successful switch" target
assert_file_content "target file materializes after successful retry" nested/target.txt "target view"
assert_file_content "inherited source file remains present" source.txt "source view"
assert_file_not_exists "successful switch leaves no recovery marker" \
    .atomic/deferred-tree-alignment.pending

begin_section "Recovery removes a partially materialized target view"
require_success "switch back to source" switch_view "$SOURCE_VIEW"
assert_file_not_exists "target-only file is removed before mixed-state test" nested/target.txt

if fault_output="$(env ATOMIC_TEST_FAIL_SWITCH_AFTER_MATERIALIZE=1 "$ATOMIC_BIN" view switch target --force 2>&1)"; then
    _fail "switch fails after target files are materialized" "failpoint unexpectedly succeeded: $fault_output"
    _skip "partial-materialization recovery checks" "failpoint did not fire"
    print_summary || true
    exit 1
else
    if [[ "$fault_output" == *"injected switch failure after materialization"* ]]; then
        _pass "switch fails after target files are materialized"
    else
        _fail "switch fails after target files are materialized" "wrong failure: $fault_output"
        _skip "partial-materialization recovery checks" "requested failpoint did not fire"
        print_summary || true
        exit 1
    fi
fi
assert_file_exists "partially materialized target file exists before recovery" nested/target.txt
assert_file_exists "post-materialization failure retains recovery marker" \
    .atomic/deferred-tree-alignment.pending

require_success "writable reopen recovers mixed working copy" atomic add source.txt
assert_current_view "mixed-state recovery restores source current view" "$SOURCE_VIEW"
assert_file_not_exists "mixed-state recovery removes target-only tracked file" nested/target.txt
assert_file_content "mixed-state recovery restores source content" source.txt "source view"
assert_file_content "mixed-state recovery restores ignored workspace" cache/state.txt "source cache"
assert_file_not_exists "mixed-state recovery clears pending marker" \
    .atomic/deferred-tree-alignment.pending

print_summary
