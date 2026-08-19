#!/usr/bin/env bash
# 33_shadow_push_conflict_markers.sh — Shadow push must never commit markers.
#
# Regression for SPEC-shadow-push-conflict-markers.md: `atomic git push`
# (shadow materialization, incl. --no-push) committed working copies that still
# carried unresolved conflict markers, silently corrupting git branches. This
# suite pins the guard that shares `atomic record`'s detector:
#
#   1. A clean working copy shadow-pushes exactly as before (no regression).
#   2. A working copy with Atomic's numbered/hash-tagged markers:
#        - aborts the push (non-zero), names the file + line, creates NO commit,
#        - leaves a `shadow-conflict` entry in .atomic/hook-errors.log
#          (non-interactive/hook context),
#        - and `atomic record` refuses the same file (detector consistency).
#   3. `--allow-conflict-markers` is the explicit escape hatch and still commits.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 33_shadow_push_conflict_markers${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

begin_section "Prerequisites"
require_git

# ════════════════════════════════════════════════════════════════════════
# Setup: repo + a clean baseline shadow push
# ════════════════════════════════════════════════════════════════════════

begin_section "Clean shadow push (baseline, no regression)"

make_temp_repo "shadow-conflict"
init_git_repo
git_commit "Initial" "README.md" "# Project"
assert_success "git import seeds the view" atomic git import --no-vault

create_file "src/app.ts" "const x = 1;"
add_files "src/app.ts"
record_change "feat: app" >/dev/null

assert_success "clean working copy shadow-pushes" \
    atomic git push --no-push -m "sync clean"

CLEAN_COUNT="$(git_commit_count)"
_pass "baseline git commit count = $CLEAN_COUNT"

# ════════════════════════════════════════════════════════════════════════
# A materialized conflict must abort the shadow push
# ════════════════════════════════════════════════════════════════════════

begin_section "Shadow push aborts on unresolved conflict markers"

# Atomic's markers are numbered and change-hash-tagged (SPEC §4):
#   >>>>>>> N   /   ======= N [HASH]   /   <<<<<<< N
cat > src/app.ts <<'MARKERS'
const shared = 1;
>>>>>>> 1
const a = 2;
======= 1 [C2YTBAHQ]
const b = 3;
<<<<<<< 1
MARKERS

# Capture the aborted push once (output + rc + commit count around it).
set +e
PUSH_OUT="$(atomic git push --no-push -m "sync conflicted" 2>&1)"
PUSH_RC=$?
set -e
AFTER_COUNT="$(git_commit_count)"

if [ "$PUSH_RC" -ne 0 ]; then
    _pass "shadow push exits non-zero on markers"
else
    _fail "shadow push exits non-zero on markers" "expected non-zero, got 0"
fi

if echo "$PUSH_OUT" | grep -qa "src/app.ts"; then
    _pass "abort names the conflicted file"
else
    _fail "abort names the conflicted file" "output: $(echo "$PUSH_OUT" | head -5)"
fi

if echo "$PUSH_OUT" | grep -qa "line 2"; then
    _pass "abort names the marker line"
else
    _fail "abort names the marker line" "output: $(echo "$PUSH_OUT" | head -5)"
fi

if [ "$AFTER_COUNT" -eq "$CLEAN_COUNT" ]; then
    _pass "no git commit created on markers ($AFTER_COUNT)"
else
    _fail "no git commit created on markers" \
        "expected $CLEAN_COUNT, got $AFTER_COUNT"
fi

# ════════════════════════════════════════════════════════════════════════
# Hook audit trail + detector consistency with `record`
# ════════════════════════════════════════════════════════════════════════

begin_section "Hook audit log + record consistency"

if [ -f .atomic/hook-errors.log ] && grep -qa "shadow-validate:V1" .atomic/hook-errors.log; then
    _pass "hook-errors.log records a shadow-validate:V1 entry"
else
    _fail "hook-errors.log records a shadow-validate:V1 entry" \
        "log: $(cat .atomic/hook-errors.log 2>/dev/null | head -3)"
fi

# `atomic record` must refuse the SAME working copy (shared detector, SPEC §5.4).
assert_failure "record refuses the same conflict markers" \
    atomic record -m "should be rejected"

# ════════════════════════════════════════════════════════════════════════
# Explicit override still commits
# ════════════════════════════════════════════════════════════════════════

begin_section "Explicit --allow-conflict-markers override"

# Realistic path: the markers are accepted as legitimate content by recording
# them with record's own override, so the working copy now matches the view
# (V2 tree↔view coherence holds). The shadow-push override then bypasses the V1
# marker gate and commits.
assert_success "record accepts markers with its override" \
    atomic record --allow-conflict-markers -m "accept markers as content"
assert_success "override commits a marker-laden tree" \
    atomic git push --no-push --allow-conflict-markers -m "override"

OVERRIDE_COUNT="$(git_commit_count)"
if [ "$OVERRIDE_COUNT" -gt "$CLEAN_COUNT" ]; then
    _pass "override created a commit ($OVERRIDE_COUNT)"
else
    _fail "override created a commit" "expected > $CLEAN_COUNT, got $OVERRIDE_COUNT"
fi

print_summary
