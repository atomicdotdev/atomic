#!/usr/bin/env bash
# 38_shadow_drift_v3.sh — Validator Rule V3 (git ↔ Atomic state lineage).
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
    atomic log --view "$MAIN" -f json 2>/dev/null \
        | grep -o '"state"[[:space:]]*:[[:space:]]*"[^"]*"' \
        | head -1 \
        | sed -E 's/.*"([^"]+)"$/\1/'
}

begin_section "Fast-forward push commits (no false positive)"

make_temp_repo "drift-v3"
init_git_repo
git_commit "Initial" "README.md" "# Project"
assert_success "git import" atomic git import --no-vault
MAIN="$(git symbolic-ref --short HEAD)"

create_file "src/a.ts" "const a = 1;"
add_files "src/a.ts"
record_change "feat: a" >/dev/null
assert_success "baseline push (fast-forward)" atomic git push --no-push -m "A"
BASE_COUNT="$(git_commit_count)"

begin_section "Drift (state not in lineage) is rejected by V3"

# Record another change (not yet pushed).
create_file "src/b.ts" "const b = 2;"
add_files "src/b.ts"
record_change "feat: b" >/dev/null

# Fabricate drift: a git commit on the branch advertising an Atomic-State that
# is NOT in the view's history (mutate the tip state's last char). find_last_
# pushed_state will pick this up as the branch's last published state.
TIP_STATE="$(view_tip_state)"
# Mutate a MIDDLE character (full data bits) rather than the last (which carries
# base32 padding bits) so the result is still a valid merkle, just a different
# one that is not in the view's history.
IDX=10
ORIG_CHAR="${TIP_STATE:$IDX:1}"
REPL="Q"; [ "$ORIG_CHAR" = "Q" ] && REPL="Z"
FAKE_STATE="${TIP_STATE:0:$IDX}$REPL${TIP_STATE:$((IDX + 1))}"
git commit --allow-empty -q -m "Simulated drift

Atomic-View: $MAIN
Atomic-State: $FAKE_STATE"

DRIFT_TIP_COUNT="$(git_commit_count)"

set +e
PUSH_OUT="$(atomic git push --no-push -m "onto drift" 2>&1)"
PUSH_RC=$?
set -e
AFTER_COUNT="$(git_commit_count)"

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

# Reconcile by dropping the drift commit so the branch tip is a coherent shadow
# state again.
git reset --hard HEAD~1 >/dev/null 2>&1
assert_success "push succeeds after reconcile" atomic git push --no-push -m "recorded b"
FINAL_COUNT="$(git_commit_count)"
if [ "$FINAL_COUNT" -gt "$BASE_COUNT" ]; then
    _pass "reconciled push created a commit ($FINAL_COUNT)"
else
    _fail "reconciled push created a commit" "expected > $BASE_COUNT, got $FINAL_COUNT"
fi

print_summary
