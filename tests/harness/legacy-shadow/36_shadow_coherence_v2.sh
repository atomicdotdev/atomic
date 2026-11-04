#!/usr/bin/env bash
# HISTORICAL: coverage migrated to ../40_git_shim_safety.sh.
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
require_success "git import" atomic git import --no-vault

create_file "src/a.ts" "const a = 1;"
require_success "add src/a.ts" atomic add "src/a.ts"
require_success "record src/a.ts" record_change "feat: a"
require_success "coherent local shadow commit" atomic git push --no-push -m "baseline"
require_capture "read baseline Git commit count" git rev-list --count HEAD
BASE_COUNT="$COMMAND_OUTPUT"
require_capture "read baseline Git HEAD" git rev-parse HEAD
BASE_HEAD="$COMMAND_OUTPUT"

begin_section "Out-of-band drift is rejected by V2"

# Edit a tracked file on disk WITHOUT recording it: the working copy now
# diverges from the view's recorded content.
overwrite_file "src/a.ts" "const a = 999; // edited on disk, never recorded"
snapshot_dir="$(mktemp -d "${TMPDIR:-/tmp}/atomic-shadow-drift-XXXXXX")"
_HARNESS_TMPDIRS+=("$snapshot_dir")
cp src/a.ts "$snapshot_dir/a.ts"
require_capture "read Atomic history before drift refusal" atomic log --format json
history_before="$COMMAND_OUTPUT"

set +e
PUSH_OUT="$(atomic git push --no-push -m "drift" 2>&1)"
PUSH_RC=$?
set -e
require_capture "read Git count after drift refusal" git rev-list --count HEAD
AFTER_COUNT="$COMMAND_OUTPUT"
assert_success "drift refusal preserves working bytes" cmp -s "$snapshot_dir/a.ts" src/a.ts
require_capture "read Git HEAD after drift refusal" git rev-parse HEAD
if [[ "$COMMAND_OUTPUT" == "$BASE_HEAD" ]]; then _pass "drift refusal preserves Git HEAD"; else _fail "drift refusal preserves Git HEAD" "HEAD changed"; fi
require_capture "read Atomic history after drift refusal" atomic log --format json
if [[ "$COMMAND_OUTPUT" == "$history_before" ]]; then _pass "drift refusal preserves Atomic history"; else _fail "drift refusal preserves Atomic history" "history changed"; fi

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

require_success "add src/a.ts" atomic add "src/a.ts"
require_success "record edited src/a.ts" record_change "feat: a (edited)"
require_success "local shadow commit succeeds once coherent" atomic git push --no-push -m "recorded edit"

require_capture "read final Git commit count" git rev-list --count HEAD
FINAL_COUNT="$COMMAND_OUTPUT"
if [ "$FINAL_COUNT" -eq "$((BASE_COUNT + 1))" ]; then
    _pass "exactly one commit after recording ($FINAL_COUNT)"
else
    _fail "exactly one commit after recording" "expected $((BASE_COUNT + 1)), got $FINAL_COUNT"
fi

print_summary
