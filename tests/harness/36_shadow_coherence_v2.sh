#!/usr/bin/env bash
# 36_shadow_coherence_v2.sh — Validator Rule V2 (tree ↔ view coherence).
#
# SPEC-single-materializer-validator.md §6.2 / Phase 4: the staged tree must
# correspond to what the current view materializes. A working copy that has
# drifted from the recorded view (e.g. an out-of-band edit that was never
# recorded) must be rejected before commit — not silently baked into a shadow
# commit whose Atomic-State trailer would then lie.
#
# This suite pins:
#   1. A normal record → push is coherent and commits (no false positive).
#   2. An out-of-band disk edit (not recorded) is rejected by V2: no commit,
#      names the path, logs shadow-validate:V2, git left untouched.
#   3. Recording that edit restores coherence, and the push then commits.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 36_shadow_coherence_v2${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

begin_section "Prerequisites"
require_git

begin_section "Coherent record → push commits (no false positive)"

make_temp_repo "v2-coherence"
init_git_repo
git_commit "Initial" "README.md" "# Project"
assert_success "git import" atomic git import --no-vault

create_file "src/a.ts" "const a = 1;"
add_files "src/a.ts"
record_change "feat: a" >/dev/null
assert_success "coherent shadow push commits" atomic git push --no-push -m "baseline"
BASE_COUNT="$(git_commit_count)"

begin_section "Out-of-band drift is rejected by V2"

# Edit a tracked file on disk WITHOUT recording it: the working copy now
# diverges from the view's recorded content.
overwrite_file "src/a.ts" "const a = 999; // edited on disk, never recorded"

set +e
PUSH_OUT="$(atomic git push --no-push -m "drift" 2>&1)"
PUSH_RC=$?
set -e
AFTER_COUNT="$(git_commit_count)"

if [ "$PUSH_RC" -ne 0 ]; then
    _pass "shadow push aborts (V2) on out-of-band drift"
else
    _fail "shadow push aborts (V2) on out-of-band drift" "expected non-zero"
fi

if echo "$PUSH_OUT" | grep -qa "src/a.ts"; then
    _pass "abort names the drifted file"
else
    _fail "abort names the drifted file" "output: $(echo "$PUSH_OUT" | head -5)"
fi

if [ "$AFTER_COUNT" -eq "$BASE_COUNT" ]; then
    _pass "no commit created on drift ($AFTER_COUNT)"
else
    _fail "no commit created on drift" "expected $BASE_COUNT, got $AFTER_COUNT"
fi

if [ -f .atomic/hook-errors.log ] && grep -qa "shadow-validate:V2" .atomic/hook-errors.log; then
    _pass "hook-errors.log records a shadow-validate:V2 entry"
else
    _fail "hook-errors.log records a shadow-validate:V2 entry" \
        "log: $(cat .atomic/hook-errors.log 2>/dev/null | head -3)"
fi

begin_section "Recording the edit restores coherence"

add_files "src/a.ts"
record_change "feat: a (edited)" >/dev/null
assert_success "push commits once coherent" atomic git push --no-push -m "recorded edit"

FINAL_COUNT="$(git_commit_count)"
if [ "$FINAL_COUNT" -eq "$((BASE_COUNT + 1))" ]; then
    _pass "exactly one commit after recording ($FINAL_COUNT)"
else
    _fail "exactly one commit after recording" "expected $((BASE_COUNT + 1)), got $FINAL_COUNT"
fi

print_summary
