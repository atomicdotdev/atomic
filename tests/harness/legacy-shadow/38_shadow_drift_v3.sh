#!/usr/bin/env bash
# HISTORICAL: coverage migrated to ../40_git_shim_safety.sh and ../42_git_shim_guard.sh.
#
# SPEC-single-materializer-validator.md §6.3 / Phase 5: a shadow push must refuse
# when the git branch's last published Atomic-State is NOT in the current view's
# recorded lineage (genuine drift — the branch was reset, or published from a
# state this view cannot reach). There is NO bypass flag: the remedy is
# reconcile-then-push (git reset to the last coherent shadow commit, or import).
#
# Pins:
#   1. A normal fast-forward push (last state IS in lineage) commits (no false
#      positive).
#   2. When the branch tip advertises a state absent from the view's history,
#      the push aborts (V3): no commit, reconcile hint, shadow-validate:V3.
#   3. Reconciling (git reset to drop the drift commit) lets the push succeed.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 38_shadow_drift_v3${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

begin_section "Prerequisites"
require_git

# Extract the current view's tip Atomic-State (base32) from the log.
view_tip_state() {
    local output
    output="$(atomic log --view "$MAIN" -f json 2>&1)" || return 1
    printf '%s\n' "$output" | python3 -c 'import json,sys; rows=json.load(sys.stdin); assert rows and len(rows[0]["state"])==52; print(rows[0]["state"])'
}

begin_section "Fast-forward push commits (no false positive)"

make_temp_repo "drift-v3"
init_git_repo
git_commit "Initial" "README.md" "# Project"
require_success "git import" atomic git import --no-vault
require_capture "read current Git branch" git symbolic-ref --short HEAD
MAIN="$COMMAND_OUTPUT"

create_file "src/a.ts" "const a = 1;"
require_success "add src/a.ts" atomic add "src/a.ts"
require_success "record src/a.ts" record_change "feat: a"
require_success "baseline local shadow commit" atomic git push --no-push -m "A"
require_capture "read baseline commit count" git rev-list --count HEAD
BASE_COUNT="$COMMAND_OUTPUT"

begin_section "Drift (state not in lineage) is rejected by V3"

# Record another change (not yet pushed).
create_file "src/b.ts" "const b = 2;"
require_success "add src/b.ts" atomic add "src/b.ts"
require_success "record src/b.ts" record_change "feat: b"
require_capture "read coherent Git HEAD" git rev-parse HEAD
COHERENT_HEAD="$COMMAND_OUTPUT"

# Fabricate drift: a git commit on the branch advertising an Atomic-State that
# is NOT in the view's history (mutate the tip state's last char). find_last_
# pushed_state will pick this up as the branch's last published state.
require_capture "read current view state" view_tip_state
TIP_STATE="$COMMAND_OUTPUT"
# Mutate a MIDDLE character (full data bits) rather than the last (which carries
# base32 padding bits) so the result is still a valid merkle, just a different
# one that is not in the view's history.
IDX=10
ORIG_CHAR="${TIP_STATE:$IDX:1}"
REPL="Q"; [ "$ORIG_CHAR" = "Q" ] && REPL="Z"
FAKE_STATE="${TIP_STATE:0:$IDX}$REPL${TIP_STATE:$((IDX + 1))}"
require_success "create empty drift fixture commit" git commit --allow-empty -q -m "Simulated drift

Atomic-View: $MAIN
Atomic-State: $FAKE_STATE"

require_capture "read drift commit count" git rev-list --count HEAD
DRIFT_TIP_COUNT="$COMMAND_OUTPUT"
require_capture "read drift Git HEAD" git rev-parse HEAD
DRIFT_HEAD="$COMMAND_OUTPUT"
require_capture "read Atomic history before refusal" atomic log --view "$MAIN" -f json
HISTORY_BEFORE="$COMMAND_OUTPUT"
snapshot_dir="$(mktemp -d "${TMPDIR:-/tmp}/atomic-shadow-drift-XXXXXX")"
_HARNESS_TMPDIRS+=("$snapshot_dir")
cp src/b.ts "$snapshot_dir/b.ts"

set +e
PUSH_OUT="$(atomic git push --no-push -m "onto drift" 2>&1)"
PUSH_RC=$?
set -e
require_capture "read Git count after refusal" git rev-list --count HEAD
AFTER_COUNT="$COMMAND_OUTPUT"
assert_success "drift refusal preserves source bytes" cmp -s "$snapshot_dir/b.ts" src/b.ts
require_capture "read Git HEAD after refusal" git rev-parse HEAD
if [[ "$COMMAND_OUTPUT" == "$DRIFT_HEAD" ]]; then _pass "drift refusal preserves Git HEAD"; else _fail "drift refusal preserves Git HEAD" "HEAD changed"; fi
require_capture "read Atomic history after refusal" atomic log --view "$MAIN" -f json
if [[ "$COMMAND_OUTPUT" == "$HISTORY_BEFORE" ]]; then _pass "drift refusal preserves Atomic history"; else _fail "drift refusal preserves Atomic history" "history changed"; fi

if [ "$PUSH_RC" -ne 0 ]; then
    _pass "shadow push aborts (V3) on drift"
else
    _fail "shadow push aborts (V3) on drift" "expected non-zero"
fi

if echo "$PUSH_OUT" | grep -qaiE "drift|lineage|reconcile"; then
    _pass "abort explains the drift + reconcile"
else
    _fail "abort explains the drift + reconcile" "output: $(echo "$PUSH_OUT" | head -5)"
fi

if [ "$AFTER_COUNT" -eq "$DRIFT_TIP_COUNT" ]; then
    _pass "no commit created on drift ($AFTER_COUNT)"
else
    _fail "no commit created on drift" "expected $DRIFT_TIP_COUNT, got $AFTER_COUNT"
fi

if [ -f .atomic/hook-errors.log ] && grep -qa "shadow-validate:V3" .atomic/hook-errors.log; then
    _pass "hook-errors.log records a shadow-validate:V3 entry"
else
    _fail "hook-errors.log records a shadow-validate:V3 entry" \
        "log: $(cat .atomic/hook-errors.log 2>/dev/null | head -3)"
fi

begin_section "Reconcile-then-push succeeds"

# The drift commit was empty, so compare-and-set the disposable fixture ref
# back to its prior commit without resetting the index or working bytes.
require_success "restore fixture branch with expected-old lease" git update-ref "refs/heads/$MAIN" "$COHERENT_HEAD" "$DRIFT_HEAD"
assert_success "fixture ref restoration preserves source bytes" cmp -s "$snapshot_dir/b.ts" src/b.ts
require_success "local shadow commit succeeds after reconcile" atomic git push --no-push -m "recorded b"
require_capture "read reconciled Git commit count" git rev-list --count HEAD
FINAL_COUNT="$COMMAND_OUTPUT"
if [ "$FINAL_COUNT" -gt "$BASE_COUNT" ]; then
    _pass "reconciled push created a commit ($FINAL_COUNT)"
else
    _fail "reconciled push created a commit" "expected > $BASE_COUNT, got $FINAL_COUNT"
fi

print_summary
