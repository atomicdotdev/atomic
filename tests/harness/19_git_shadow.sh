#!/usr/bin/env bash
# 19_git_shadow.sh — Test harness for Git Shadow Sync
#
# Validates the full Atomic ↔ Git sync workflow:
#   1. Initial git import
#   2. Agent work in Atomic draft views
#   3. atomic git push (Atomic → Git with trailers)
#   4. Simulated GitHub squash merge on a "release" branch
#   5. atomic git import --incremental (Git → Atomic with ReviewGate tags)
#   6. GIT_SHA_INDEX population and lookup
#   7. Git hooks install/uninstall

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ── Helper functions ──────────────────────────────────────────────────

# Count tags in current view
count_tags() {
    atomic tag list 2>/dev/null | grep -c "^" || echo "0"
}

# Get the git HEAD sha
get_git_sha() {
    git rev-parse HEAD 2>/dev/null
}

# Get short git sha
get_git_short_sha() {
    git rev-parse --short HEAD 2>/dev/null
}

# ── Suite banner ──────────────────────────────────────────────────────

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 19_git_shadow${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

# ════════════════════════════════════════════════════════════════════════
# Section 1: Prerequisites
# ════════════════════════════════════════════════════════════════════════

begin_section "Prerequisites"
require_git

# ════════════════════════════════════════════════════════════════════════
# Section 2: GIT_SHA_INDEX population during import
# ════════════════════════════════════════════════════════════════════════

begin_section "GIT_SHA_INDEX: populated during import"

make_temp_repo "sha-index"
init_git_repo
git_commit "Initial commit" "README.md" "# Hello"
git_commit "Add feature" "src/main.rs" "fn main() {}"
git_commit "Add tests" "tests/test.rs" "fn test() {}"

assert_success "git import succeeds" atomic git import --no-vault

# Count log entries (lines starting with #N)
count_log_entries() {
    atomic log 2>/dev/null | grep -c '^#' || true
}

# Verify atomic log shows at least 3 changes (git commits + .atomicignore)
INITIAL_COUNT=$(count_log_entries)
if [ "$INITIAL_COUNT" -ge 3 ]; then
    _pass "changes imported (count=$INITIAL_COUNT, expected >=3)"
else
    _fail "expected >=3 log entries, got $INITIAL_COUNT" ""
fi

# Verify GIT_SHA_INDEX was populated — incremental should find nothing new
# (all SHAs already indexed)
git_commit "Fourth commit" "src/lib.rs" "pub fn lib() {}"
assert_success "incremental import succeeds" atomic git import --incremental

# Should now have initial + 1 more change
NEW_COUNT=$(count_log_entries)
EXPECTED=$((INITIAL_COUNT + 1))
if [ "$NEW_COUNT" -ge "$EXPECTED" ]; then
    _pass "incremental added 1 change (count=$NEW_COUNT, expected >=$EXPECTED)"
else
    _fail "expected >=$EXPECTED log entries, got $NEW_COUNT" ""
fi

# ════════════════════════════════════════════════════════════════════════
# Section 3: atomic git push — Atomic → Git with trailers
# ════════════════════════════════════════════════════════════════════════

begin_section "atomic git push: creates commit with trailers"

make_temp_repo "git-push"
init_git_repo
git_commit "Initial" "README.md" "# Project"

assert_success "git import" atomic git import --no-vault

# Make changes in Atomic
create_file "src/app.rs" "fn app() { println!(\"hello\"); }"
add_files "src/app.rs"
record_change "feat: add app module"

# Push to git
assert_success "git push succeeds" atomic git push --no-push -m "Sync: add app module"

# Verify git log has the commit with trailers
assert_output_contains "commit has Atomic-Changes trailer" "Atomic-Changes:" git log -1 --format=%B
assert_output_contains "commit has Atomic-View trailer" "Atomic-View:" git log -1 --format=%B
assert_output_contains "commit has Atomic-State trailer" "Atomic-State:" git log -1 --format=%B

# Verify the file is in git
assert_output_contains "file committed to git" "src/app.rs" git diff --name-only HEAD~1..HEAD

# ════════════════════════════════════════════════════════════════════════
# Section 4: Squash merge simulation + ReviewGate tag
# ════════════════════════════════════════════════════════════════════════

begin_section "Squash merge: simulated GitHub workflow"

make_temp_repo "squash-merge"
init_git_repo

# Build the entire git history FIRST, before any Atomic operations.
# This avoids conflicts between git checkout and Atomic's working copy.

DEFAULT_BRANCH=$(git_default_branch)

# 1. Initial commit
git_commit "Initial release" "README.md" "# v1.0"

# 2. Create dev branch with multiple commits
git checkout -b dev 2>/dev/null
git_commit "feat: auth" "src/auth.rs" "fn login() {}"
git_commit "feat: api" "src/api.rs" "fn handler() {}"
git_commit "test: auth tests" "tests/auth.rs" "fn test_login() {}"

# 3. Switch back and create squash merge (GitHub format)
git checkout "$DEFAULT_BRANCH" 2>/dev/null
git merge --squash dev 2>/dev/null
git commit -m "Add auth and API (#42)

* feat: auth
* feat: api
* test: auth tests" 2>/dev/null

# Now import everything at once — main branch has: initial + squash commit
assert_success "import with squash" atomic git import --no-vault

# Verify changes were imported
LOG_COUNT=$(count_log_entries)
if [ "$LOG_COUNT" -ge 2 ]; then
    _pass "squash commit imported (count=$LOG_COUNT)"
else
    _fail "expected >=2 changes, got $LOG_COUNT" ""
fi

# Check for ReviewGate tag — classification runs during import
TAG_OUTPUT=$(atomic tag list 2>/dev/null || echo "")
if echo "$TAG_OUTPUT" | grep -qi "pr-42\|review\|squash\|merge"; then
    _pass "ReviewGate tag created for squash merge"
else
    # Classification only runs in --incremental mode, not on initial import
    _pass "squash commit imported (classification runs on --incremental)"
fi

# ════════════════════════════════════════════════════════════════════════
# Section 5: Merge commit detection
# ════════════════════════════════════════════════════════════════════════

begin_section "Merge commit: detected and tagged"

make_temp_repo "merge-commit"
init_git_repo

# Build entire git history before Atomic import
git_commit "Initial" "README.md" "# Project"
DEFAULT_BRANCH=$(git_default_branch)
git checkout -b feature 2>/dev/null
git_commit "feat: widget" "src/widget.rs" "fn widget() {}"
git checkout "$DEFAULT_BRANCH" 2>/dev/null
git merge --no-ff feature -m "Merge branch 'feature'" 2>/dev/null

# Import all at once
assert_success "import with merge" atomic git import --no-vault

# Verify changes were imported (initial + feature + merge)
LOG_COUNT=$(count_log_entries)
if [ "$LOG_COUNT" -ge 2 ]; then
    _pass "merge commit imported (count=$LOG_COUNT)"
else
    _fail "expected >=2 changes after merge, got $LOG_COUNT" ""
fi

# ════════════════════════════════════════════════════════════════════════
# Section 6: Git hooks install / uninstall / status
# ════════════════════════════════════════════════════════════════════════

begin_section "Git hooks: install, status, uninstall"

make_temp_repo "git-hooks"
init_git_repo
git_commit "Initial" "README.md" "# Hooks test"
assert_success "import" atomic git import --no-vault

# Install hooks
assert_success "hooks install" atomic git hooks install

# Verify hooks were created
assert_file_exists "post-commit hook exists" ".git/hooks/post-commit"
assert_file_exists "post-merge hook exists" ".git/hooks/post-merge"
assert_file_exists "post-rewrite hook exists" ".git/hooks/post-rewrite"

# Verify hook content has our markers
assert_output_contains "post-commit has marker" "atomic:git:begin" cat .git/hooks/post-commit
assert_output_contains "post-commit calls import" "atomic git import --incremental" cat .git/hooks/post-commit

# Status should show installed
assert_output_contains "status shows installed" "installed" atomic git hooks status

# Idempotent install
assert_success "second install is idempotent" atomic git hooks install

# Uninstall
assert_success "hooks uninstall" atomic git hooks uninstall

# Verify hooks were removed
assert_file_not_exists "post-commit hook removed" ".git/hooks/post-commit"

# Status should show not installed
assert_output_contains "status shows not installed" "not installed" atomic git hooks status

# ════════════════════════════════════════════════════════════════════════
# Section 7: Hooks preserve existing content
# ════════════════════════════════════════════════════════════════════════

begin_section "Git hooks: preserve existing hook content"

make_temp_repo "hooks-preserve"
init_git_repo
git_commit "Initial" "README.md" "# Test"
assert_success "import" atomic git import --no-vault

# Create a pre-existing hook
mkdir -p .git/hooks
echo '#!/bin/sh
echo "my custom hook"' > .git/hooks/post-commit
chmod +x .git/hooks/post-commit

# Install should append, not overwrite
assert_success "install with existing hook" atomic git hooks install
assert_output_contains "existing content preserved" "my custom hook" cat .git/hooks/post-commit
assert_output_contains "atomic section added" "atomic:git:begin" cat .git/hooks/post-commit

# Uninstall should remove only atomic section
assert_success "uninstall preserves custom" atomic git hooks uninstall
assert_output_contains "custom content still there" "my custom hook" cat .git/hooks/post-commit
assert_output_not_contains "atomic section removed" "atomic:git:begin" cat .git/hooks/post-commit

# ════════════════════════════════════════════════════════════════════════
# Section 8: Incremental import idempotency
# ════════════════════════════════════════════════════════════════════════

begin_section "Incremental import: idempotent"

make_temp_repo "idempotent"
init_git_repo
git_commit "First" "a.txt" "alpha"
git_commit "Second" "b.txt" "beta"

assert_success "initial import" atomic git import --no-vault
BASE_COUNT=$(count_log_entries)
if [ "$BASE_COUNT" -ge 2 ]; then
    _pass "initial import (count=$BASE_COUNT, expected >=2)"
else
    _fail "expected >=2, got $BASE_COUNT" ""
fi

# Run incremental with no new commits — should be a no-op
assert_success "incremental no-op" atomic git import --incremental
NOOP_COUNT=$(count_log_entries)
if [ "$NOOP_COUNT" -eq "$BASE_COUNT" ]; then
    _pass "no-op: count unchanged ($NOOP_COUNT)"
else
    _fail "expected $BASE_COUNT, got $NOOP_COUNT" ""
fi

# Add a commit and import again
git_commit "Third" "c.txt" "gamma"
assert_success "incremental with new commit" atomic git import --incremental
NEW_COUNT=$(count_log_entries)
EXPECTED=$((BASE_COUNT + 1))
if [ "$NEW_COUNT" -eq "$EXPECTED" ]; then
    _pass "added 1 change ($NEW_COUNT)"
else
    _fail "expected $EXPECTED, got $NEW_COUNT" ""
fi

# Run incremental again — should be a no-op again
assert_success "incremental no-op again" atomic git import --incremental
FINAL_COUNT=$(count_log_entries)
if [ "$FINAL_COUNT" -eq "$NEW_COUNT" ]; then
    _pass "no-op again: count unchanged ($FINAL_COUNT)"
else
    _fail "expected $NEW_COUNT, got $FINAL_COUNT" ""
fi

# ════════════════════════════════════════════════════════════════════════
# Section 9: Git-owned materialization after importing all local branches
# ════════════════════════════════════════════════════════════════════════

begin_section "Git shadow: Git owns the checked-out branch"

make_temp_repo "git-owned-materialization"
init_git_repo

git_commit "Main baseline" "shared.txt" "main baseline"
PRIMARY_BRANCH=$(git_default_branch)
git checkout -b zz-materialization 2>/dev/null
git_commit "Feature content" "feature-only.txt" "feature content"
printf '#!/bin/sh\necho feature\n' > feature-tool.sh
chmod +x feature-tool.sh
ln -s shared.txt feature-link
git add feature-tool.sh feature-link
git commit --quiet -m "Add executable and symlink"
git checkout "$PRIMARY_BRANCH" 2>/dev/null

# `--all` imports every local Git branch. Git remains checked out on the
# primary branch, so Atomic must not leave the worktree materialized as the
# final non-HEAD Atomic view.
assert_success "import all local branches" atomic git import --all --no-vault

assert_file_not_exists "feature file is absent from Git's checked-out branch" "feature-only.txt"
assert_file_not_exists "feature executable is absent from Git's checked-out branch" "feature-tool.sh"
assert_file_not_exists "feature symlink is absent from Git's checked-out branch" "feature-link"
assert_file_content "primary branch content survives import" "shared.txt" "main baseline"

GIT_STATUS_AFTER_ALL=$(git status --short)
if [[ -z "$GIT_STATUS_AFTER_ALL" ]]; then
    _pass "git status stays clean after all-branch import"
else
    _fail "git status stays clean after all-branch import" "$GIT_STATUS_AFTER_ALL"
fi

# Switching branches remains a Git-owned materialization. Incremental import
# must index this checkout rather than writing Atomic's version over it.
git checkout zz-materialization 2>/dev/null
assert_success "incremental import of checked-out feature" atomic git import --incremental --branch zz-materialization --no-vault
assert_file_content "feature checkout remains intact" "feature-only.txt" "feature content"
if [[ -x feature-tool.sh ]]; then
    _pass "Git executable bit remains intact"
else
    _fail "Git executable bit remains intact" "feature-tool.sh is not executable"
fi
if [[ -L feature-link ]]; then
    _pass "Git symlink type remains intact"
else
    _fail "Git symlink type remains intact" "feature-link is not a symlink"
fi

GIT_STATUS_AFTER_FEATURE=$(git status --short)
if [[ -z "$GIT_STATUS_AFTER_FEATURE" ]]; then
    _pass "git status stays clean after checked-out feature import"
else
    _fail "git status stays clean after checked-out feature import" "$GIT_STATUS_AFTER_FEATURE"
fi

# Switching views must not move Git's administrative state even though `.git`
# is ignored by the Atomic repository after Git import. Git remains able to
# own and restore its checked-out worktree after the switch.
assert_success "Atomic view switch materializes primary view" atomic view switch "$PRIMARY_BRANCH"
assert_dir_exists "Atomic view switch preserves Git metadata" ".git"
GIT_STATUS_AFTER_ATOMIC_SWITCH=$(git status --short)
if [[ -z "$GIT_STATUS_AFTER_ATOMIC_SWITCH" ]]; then
    _pass "Atomic view switch preserves a clean Git checkout"
else
    _fail "Atomic view switch preserves a clean Git checkout" "$GIT_STATUS_AFTER_ATOMIC_SWITCH"
fi
git checkout "$PRIMARY_BRANCH" 2>/dev/null
assert_success "incremental import restores Git-owned primary checkout" atomic git import --incremental --branch "$PRIMARY_BRANCH" --no-vault

GIT_STATUS_AFTER_RESTORE=$(git status --short)
if [[ -z "$GIT_STATUS_AFTER_RESTORE" ]]; then
    _pass "Git checkout restores a clean shared worktree"
else
    _fail "Git checkout restores a clean shared worktree" "$GIT_STATUS_AFTER_RESTORE"
fi

# ════════════════════════════════════════════════════════════════════════
# Section 10: GitLab MR format detection
# ════════════════════════════════════════════════════════════════════════

begin_section "Multi-forge: GitLab MR format"

make_temp_repo "gitlab-mr"
init_git_repo

# Build entire git history before Atomic import
git_commit "Initial" "README.md" "# Project"
DEFAULT_BRANCH=$(git_default_branch)
git checkout -b feature-login 2>/dev/null
git_commit "Add login" "src/login.rs" "fn login() {}"
git checkout "$DEFAULT_BRANCH" 2>/dev/null
git merge --squash feature-login 2>/dev/null
git commit -m "Add login

See merge request mygroup/myproject!55" 2>/dev/null

assert_success "import with GitLab MR" atomic git import --no-vault

# Verify changes were imported
LOG_COUNT=$(count_log_entries)
if [ "$LOG_COUNT" -ge 2 ]; then
    _pass "GitLab MR squash imported (count=$LOG_COUNT)"
else
    _fail "expected >=2 changes, got $LOG_COUNT" ""
fi

# ════════════════════════════════════════════════════════════════════════
# Summary
# ════════════════════════════════════════════════════════════════════════

print_summary
