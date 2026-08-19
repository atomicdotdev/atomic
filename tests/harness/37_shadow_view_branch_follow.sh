#!/usr/bin/env bash
# 37_shadow_view_branch_follow.sh — git shadow HEAD follows `atomic view switch`.
#
# SPEC-single-materializer-validator.md §5.4, Direction A ("git shadows Atomic"):
# Atomic is upstream, so switching the Atomic view points the downstream git
# shadow's HEAD at the mirror branch. It is a ref move (never a `git checkout`):
# the working copy Atomic just materialized is left untouched. Best-effort and
# gated to shadow-sync repos.
#
# Pins:
#   1. Switching to a view with no git branch yet CREATES the branch and moves
#      HEAD to it, without disturbing the materialized working copy.
#   2. Switching back moves HEAD back; already-on is an idempotent no-op.
#   3. In a NON-shadow repo (no import/excludes), view switch does NOT touch git.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 37_shadow_view_branch_follow${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

begin_section "Prerequisites"
require_git

git_head_branch() { git symbolic-ref --short HEAD 2>/dev/null; }

# ════════════════════════════════════════════════════════════════════════
# git shadow HEAD follows the Atomic view
# ════════════════════════════════════════════════════════════════════════

begin_section "Shadow HEAD follows view switch"

make_temp_repo "view-branch-follow"
init_git_repo
git_commit "Initial" "README.md" "# Project"
assert_success "git import establishes shadow sync" atomic git import --no-vault

MAIN="$(git_head_branch)"
_pass "shadow default branch is '$MAIN'"

# Record a change and create a draft view (no --switch yet).
create_file "src/a.ts" "const a = 1;"
add_files "src/a.ts"
record_change "feat: a" >/dev/null
assert_success "create draft view" atomic view create agent --draft --parent "$MAIN"

# Switching to the draft moves the git shadow onto a mirror branch (created).
assert_success "switch to draft view" atomic view switch agent --force
if [ "$(git_head_branch)" = "agent" ]; then
    _pass "git HEAD followed to branch 'agent'"
else
    _fail "git HEAD followed to branch 'agent'" "on '$(git_head_branch)'"
fi
assert_success "mirror branch 'agent' exists" git rev-parse --verify --quiet refs/heads/agent

# The materialized working copy is intact (ref move, not a checkout).
assert_file_exists "working copy intact after follow" "src/a.ts"

# Switching back moves HEAD back to the shared branch.
assert_success "switch back to shared view" atomic view switch "$MAIN" --force
if [ "$(git_head_branch)" = "$MAIN" ]; then
    _pass "git HEAD followed back to '$MAIN'"
else
    _fail "git HEAD followed back to '$MAIN'" "on '$(git_head_branch)'"
fi

# Idempotent: switching to the view we're already on is a no-op.
assert_success "switch to current view is a no-op" atomic view switch "$MAIN"
if [ "$(git_head_branch)" = "$MAIN" ]; then
    _pass "idempotent switch leaves HEAD on '$MAIN'"
else
    _fail "idempotent switch leaves HEAD on '$MAIN'" "on '$(git_head_branch)'"
fi

# ════════════════════════════════════════════════════════════════════════
# Non-shadow repo: view switch must NOT touch git
# ════════════════════════════════════════════════════════════════════════

begin_section "Non-shadow repo: view switch leaves git alone"

make_temp_repo "no-shadow"
init_git_repo
git_commit "Initial" "README.md" "# Plain"
BEFORE_BRANCH="$(git_head_branch)"

# Atomic repo WITHOUT git import (no shadow excludes → shadow sync not active).
init_repo
create_file "src/x.ts" "const x = 1;"
add_files "src/x.ts"
record_change "feat: x" >/dev/null
new_view "other" >/dev/null 2>&1 || atomic view create other >/dev/null 2>&1
assert_success "switch view in non-shadow repo" atomic view switch other --force

if [ "$(git_head_branch)" = "$BEFORE_BRANCH" ]; then
    _pass "git HEAD untouched in non-shadow repo ('$BEFORE_BRANCH')"
else
    _fail "git HEAD untouched in non-shadow repo" \
        "expected '$BEFORE_BRANCH', got '$(git_head_branch)'"
fi
assert_failure "no mirror branch created in non-shadow repo" \
    git rev-parse --verify --quiet refs/heads/other

print_summary
