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
# Section 8b: Incremental import while an inherited draft is current
# ════════════════════════════════════════════════════════════════════════

begin_section "Incremental import: preserves an active draft"

make_temp_repo "idempotent-active-draft"
init_git_repo
git_commit "First" "a.txt" "alpha"
git_commit "Second" "b.txt" "beta"
git_commit "Shared draft baseline" "shared.txt" "parent shared"
git_commit "Rename baseline" "rename-me.txt" "rename baseline"
PRIMARY_BRANCH=$(git_default_branch)

assert_success "initial import for draft regression" atomic git import --no-vault
PRIMARY_COUNT_BEFORE=$(atomic log --view "$PRIMARY_BRANCH" 2>/dev/null | grep -c '^#' || true)

# Draft logs hide inherited parent changes by default. Incremental import must
# still deduplicate against the explicit target view, and it must not leave the
# user's current draft merely because Git's branch was checked for new commits.
assert_success "create and switch to inherited draft" \
    atomic view create agent-draft --draft --parent "$PRIMARY_BRANCH" --switch
assert_success "incremental no-op while draft is current" \
    atomic git import --incremental --branch "$PRIMARY_BRANCH" --no-vault

PRIMARY_COUNT_AFTER=$(atomic log --view "$PRIMARY_BRANCH" 2>/dev/null | grep -c '^#' || true)
if [ "$PRIMARY_COUNT_AFTER" -eq "$PRIMARY_COUNT_BEFORE" ]; then
    _pass "active draft no-op: target count unchanged ($PRIMARY_COUNT_AFTER)"
else
    _fail "active draft no-op duplicated target history" \
        "expected $PRIMARY_COUNT_BEFORE, got $PRIMARY_COUNT_AFTER"
fi

CURRENT_ATOMIC_VIEW=$(tr -d '[:space:]' < .atomic/current_view)
if [ "$CURRENT_ATOMIC_VIEW" = "agent-draft" ]; then
    _pass "incremental import preserves current draft"
else
    _fail "incremental import preserves current draft" \
        "expected agent-draft, got $CURRENT_ATOMIC_VIEW"
fi

# A real incremental sync can arrive while the draft has diverged from its
# parent. The import target must not be reconciled against that foreign working
# copy: doing so would remove b.txt from the primary TREE just because the
# draft currently has an unrecorded deletion.
printf '%s' "draft alpha" > a.txt
printf '%s' "draft shared" > shared.txt
printf '%s' "draft rename" > rename-me.txt
assert_success "record divergent draft modification" \
    atomic record --all -m "Draft modifies an inherited file"
rm b.txt

printf '%s' "gamma" > c.txt
git add c.txt
git rm --cached --quiet --force shared.txt
git mv rename-me.txt renamed.txt
printf '%s' "rename baseline" > renamed.txt
git add renamed.txt
git commit --quiet -m "Third: add c, delete shared path, and rename a file"
# Restore the active draft's physical path after creating the Git commit. The
# import must not rewrite its global TREE mapping while updating the parent.
mv renamed.txt rename-me.txt
rm c.txt
printf '%s' "draft rename" > rename-me.txt
assert_success "incremental new commit while draft is current" \
    atomic git import --incremental --branch "$PRIMARY_BRANCH" --no-vault

PRIMARY_COUNT_WITH_NEW_COMMIT=$(atomic log --view "$PRIMARY_BRANCH" 2>/dev/null | grep -c '^#' || true)
EXPECTED_PRIMARY_COUNT=$((PRIMARY_COUNT_BEFORE + 1))
if [ "$PRIMARY_COUNT_WITH_NEW_COMMIT" -eq "$EXPECTED_PRIMARY_COUNT" ]; then
    _pass "active draft import adds exactly one target change"
else
    _fail "active draft import adds exactly one target change" \
        "expected $EXPECTED_PRIMARY_COUNT, got $PRIMARY_COUNT_WITH_NEW_COMMIT"
fi

CURRENT_ATOMIC_VIEW=$(tr -d '[:space:]' < .atomic/current_view)
if [ "$CURRENT_ATOMIC_VIEW" = "agent-draft" ]; then
    _pass "new-commit import preserves current draft"
else
    _fail "new-commit import preserves current draft" \
        "expected agent-draft, got $CURRENT_ATOMIC_VIEW"
fi
assert_file_content "draft modification remains materialized" "a.txt" "draft alpha"
assert_file_not_exists "draft deletion remains materialized" "b.txt"
assert_file_content "draft-owned target deletion remains materialized" \
    "shared.txt" "draft shared"
assert_file_content "draft-owned target rename remains materialized" \
    "rename-me.txt" "draft rename"
assert_file_not_exists "target rename does not leak into active draft" "renamed.txt"
assert_file_not_exists "target addition does not leak into active draft" "c.txt"
DRAFT_STATUS=$(atomic status --short 2>/dev/null || true)
if echo "$DRAFT_STATUS" | grep -qE '^\?+ +shared\.txt$'; then
    _fail "target deletion keeps draft path tracked" "$DRAFT_STATUS"
else
    _pass "target deletion keeps draft path tracked"
fi
if echo "$DRAFT_STATUS" | grep -qE '^\?+ +rename-me\.txt$'; then
    _fail "target rename keeps draft path tracked" "$DRAFT_STATUS"
else
    _pass "target rename keeps draft path tracked"
fi

# The source view may independently add the exact path that the target added
# while it was in the background. Both inodes must join the same deferred path
# lifecycle so switching can remove one occupant before installing the other.
printf '%s' "beta" > b.txt
printf '%s' "draft gamma" > c.txt
assert_success "record foreground add at deferred target path" \
    atomic record --all -m "Draft independently adds target path"
rm b.txt
assert_file_content "foreground same-path add remains materialized" \
    "c.txt" "draft gamma"

# A normal foreground rename after the deferred import must become the source
# view's new path baseline. Switching to the target and back may not resurrect
# the stale pre-rename path from the first journal event.
printf '%s' "beta" > b.txt
assert_success "foreground draft rename after background import" \
    mv rename-me.txt draft-renamed.txt
assert_success "record foreground draft rename" \
    atomic record --all -m "Draft renames after background import"
rm b.txt
assert_file_content "foreground draft rename is materialized" \
    "draft-renamed.txt" "draft rename"
assert_file_not_exists "foreground draft rename removes old path" "rename-me.txt"

assert_success "force switch to imported primary after draft sync" \
    atomic view switch --force "$PRIMARY_BRANCH"
assert_file_content "primary modification is not taken from draft" "a.txt" "alpha"
assert_output_contains "primary graph retains the draft-deleted path" "D  b.txt" \
    atomic status --short
assert_file_content "primary receives the new Git commit" "c.txt" "gamma"
assert_file_not_exists "primary receives the target deletion" "shared.txt"
assert_file_not_exists "primary removes the old renamed path" "rename-me.txt"
assert_file_content "primary receives the renamed path" "renamed.txt" "rename baseline"

assert_success "switch back to source with same-path add" \
    atomic view switch --force agent-draft
assert_file_content "source restores its own same-path inode" \
    "c.txt" "draft gamma"
assert_file_content "source restores its foreground rename" \
    "draft-renamed.txt" "draft rename"
assert_file_not_exists "source removes the target rename again" "renamed.txt"

# Return TREE to the target state used by the interrupted-switch recovery
# fixture below.
assert_success "switch target again before recovery fixture" \
    atomic view switch --force "$PRIMARY_BRANCH"
assert_file_content "target restores its own same-path inode" "c.txt" "gamma"

# Simulate a process exit after TREE committed for the target but before the
# target pointer was published. At that point the working copy and persisted
# pointer still belong to the source draft, while TREE contains the target's
# paths. A writable reopen must align TREE back to the persisted source and
# clear the marker before the command proceeds.
printf '%s' "draft alpha" > a.txt
printf '%s' "draft shared" > shared.txt
printf '%s' "draft rename" > draft-renamed.txt
printf '%s' "draft gamma" > c.txt
rm -f renamed.txt
printf '%s\n' "agent-draft" > .atomic/current_view
printf '{"version":1,"source_view":"agent-draft","target_view":"%s"}\n' \
    "$PRIMARY_BRANCH" > .atomic/deferred-tree-alignment.pending
assert_success "recover interrupted TREE/view alignment" \
    atomic view switch --force agent-draft
assert_file_not_exists "recovery clears the pending alignment marker" \
    ".atomic/deferred-tree-alignment.pending"
assert_file_content "draft keeps its later foreground rename" \
    "draft-renamed.txt" "draft rename"
assert_file_content "recovery restores the source same-path inode" \
    "c.txt" "draft gamma"
assert_file_not_exists "draft does not resurrect the stale rename baseline" "rename-me.txt"
assert_file_not_exists "draft does not retain the target rename" "renamed.txt"

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
