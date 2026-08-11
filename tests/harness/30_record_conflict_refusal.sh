#!/usr/bin/env bash
# chmod +x tests/harness/30_record_conflict_refusal.sh
#
# 30_record_conflict_refusal.sh — `record` refuses marker-laden files, and says
# so as a USER error.
#
# The refusal itself is invariant 4 of the merge rubric ("honest exit state":
# `record` must not silently bake an unresolved merge into history — see
# docs/MERGE-CONFLICT-RUBRIC.md §4.3). That behavior existed but nothing
# asserted it at the CLI level, and nothing asserted how it is *reported*.
#
# It was reported wrongly: `RecordError::ConflictMarkersPresent` had no arm in
# the record command's error mapping, so it fell through to the catch-all
# `other => CliError::Internal`, which renders "Internal error:", appends
# "This appears to be a bug. Please report it at <issues URL>", and exits 128.
# A user following the documented merge workflow was told they had hit a bug,
# and scripts could not tell "you have unresolved conflicts" from "the tool
# crashed".
#
# This suite pins both halves:
#   1. the refusal happens, and --allow-conflict-markers overrides it
#   2. it is classified as a user error (no bug-report hint, exit != 128)

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"
source "$HARNESS_DIR/merge_helpers.sh"

echo "${BOLD}record refuses conflict markers (and reports it as a user error)${RESET}"

# Build the canonical order conflict: two views insert different content at the
# same anchor, then one side is inserted into the other. Leaves f.txt carrying
# markers on the base view. (Rubric A4 x B1; same shape as merge_helpers'
# build_case, spelled out here so this suite stands alone.)
setup_conflict() {
    init_repo
    printf 'alpha\nbeta\ngamma\n' > f.txt
    add_files f.txt >/dev/null
    record_change "base" >/dev/null
    BASE_VIEW="$(current_view)"

    new_view feature >/dev/null
    switch_view feature >/dev/null
    printf 'alpha\nONE\nbeta\ngamma\n' > f.txt
    record_change "edit A" >/dev/null
    FEATURE_HASH="$(tip_hash feature)"

    switch_view "$BASE_VIEW" >/dev/null
    printf 'alpha\nTWO\nbeta\ngamma\n' > f.txt
    record_change "edit B" >/dev/null

    atomic insert "$FEATURE_HASH" >/dev/null 2>&1
}

# ── 1 · the refusal ─────────────────────────────────────────────────────────

begin_section "record refuses a file that still carries markers"

make_temp_repo record-refusal
setup_conflict

assert_markers      "conflict markers are present"      f.txt
assert_failure      "record fails while markers remain" atomic record -m "should be refused"
assert_output_contains "names the file and the line" \
    "f.txt still contains conflict markers at line 2" \
    atomic record -m "should be refused"

# The refusal must not have recorded anything.
assert_markers      "markers survive the refused record" f.txt

# ── 2 · it is a USER error, not an internal error ───────────────────────────
#
# These are the assertions that would have caught the misclassification.

begin_section "the refusal is classified as a user error"

assert_output_not_contains "not reported as an internal error" \
    "Internal error" atomic record -m "should be refused"
assert_output_not_contains "does not tell the user to file a bug" \
    "Please report it" atomic record -m "should be refused"

# Exit code: must be a user/data error, never 128 (which error.rs reserves for
# internal errors, i.e. bugs).
# NB: the harness runs under `set -e`, so the expected failure must not be a
# bare command — capture the status without letting it abort the suite.
RECORD_EXIT=0
atomic record -m "should be refused" >/dev/null 2>&1 || RECORD_EXIT=$?
if [[ "$RECORD_EXIT" -ne 0 && "$RECORD_EXIT" -ne 128 ]]; then
    _pass "exit code is a user error ($RECORD_EXIT), not 128"
else
    _fail "exit code is a user error, not 128" \
        "expected non-zero and != 128, got $RECORD_EXIT"
fi

# The hint should point at the fix, not at the issue tracker.
# NB: the leading '--' is dropped from the needle on purpose — assert_output_contains
# passes it straight to `grep -F`, which would parse '--allow…' as an option.
assert_output_contains "hint points at the resolution" \
    "allow-conflict-markers" atomic record -m "should be refused"

# ── 3 · the documented override still works ─────────────────────────────────

begin_section "--allow-conflict-markers overrides the refusal"

assert_success "record succeeds with --allow-conflict-markers" \
    atomic record -m "markers are legitimate here" --allow-conflict-markers

# ── 4 · resolving normally still works ──────────────────────────────────────

begin_section "resolving the markers lets record through"

make_temp_repo record-refusal
setup_conflict

grep -vE '^(>>>>>>>|=======|<<<<<<<)' f.txt > f.resolved && mv f.resolved f.txt
assert_no_markers "markers removed by hand"          f.txt
assert_success    "record succeeds after resolution" atomic record -m "resolve conflict"
assert_present    "side ONE survived"                f.txt "ONE"
assert_present    "side TWO survived"                f.txt "TWO"

print_summary
