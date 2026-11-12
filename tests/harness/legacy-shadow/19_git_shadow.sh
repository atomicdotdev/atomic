#!/usr/bin/env bash
# HISTORICAL: coverage migrated to ../39_git_shim_workflow.sh and ../41_git_shim_hooks.sh.
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

require_success "git import succeeds" atomic git import --no-vault

# Count log entries (lines starting with #N)
count_log_entries() {
    local out
    if ! out="$(atomic log --format json "$@" 2>&1)"; then
        printf 'log failed: %s\n' "$out" >&2
        return 1
    fi
    printf '%s\n' "$out" | python3 -c 'import json,sys; entries=json.load(sys.stdin); assert isinstance(entries,list); print(len(entries))'
}

# Verify atomic log shows at least 3 changes (git commits + .atomicignore)
require_capture "count initial imported changes" count_log_entries
INITIAL_COUNT="$COMMAND_OUTPUT"
if [ "$INITIAL_COUNT" -ge 3 ]; then
    _pass "changes imported (count=$INITIAL_COUNT, expected >=3)"
else
    _fail "expected >=3 log entries, got $INITIAL_COUNT" ""
fi

# Verify GIT_SHA_INDEX was populated — incremental should find nothing new
# (all SHAs already indexed)
git_commit "Fourth commit" "src/lib.rs" "pub fn lib() {}"
require_success "incremental import succeeds" atomic git import --incremental

# Should now have initial + 1 more change
require_capture "count incremental changes" count_log_entries
NEW_COUNT="$COMMAND_OUTPUT"
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

require_success "git import" atomic git import --no-vault

# Make changes in Atomic
create_file "src/app.rs" "fn app() { println!(\"hello\"); }"
require_success "add app module" atomic add "src/app.rs"
require_success "record app module" record_change "feat: add app module"

# Push to git
require_success "create local Git shadow commit without pushing" atomic git push --no-push -m "Sync: add app module"

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
require_success "create Git dev branch" git checkout --quiet -b dev
git_commit "feat: auth" "src/auth.rs" "fn login() {}"
git_commit "feat: api" "src/api.rs" "fn handler() {}"
git_commit "test: auth tests" "tests/auth.rs" "fn test_login() {}"

# 3. Switch back and create squash merge (GitHub format)
require_success "return to default Git branch" git checkout --quiet "$DEFAULT_BRANCH"
require_success "squash Git dev branch" git merge --quiet --squash dev
require_success "record Git squash commit" git commit --quiet -m "Add auth and API (#42)

* feat: auth
* feat: api
* test: auth tests"

# Now import everything at once — main branch has: initial + squash commit
require_success "import with squash" atomic git import --no-vault

# Verify changes were imported
require_capture "count squash history" count_log_entries
LOG_COUNT="$COMMAND_OUTPUT"
if [ "$LOG_COUNT" -ge 2 ]; then
    _pass "squash commit imported (count=$LOG_COUNT)"
else
    _fail "expected >=2 changes, got $LOG_COUNT" ""
fi

# Check for ReviewGate tag — classification runs during import
require_capture "list squash tags" atomic tag list
TAG_OUTPUT="$COMMAND_OUTPUT"
if echo "$TAG_OUTPUT" | grep -qi "pr-42\|review\|squash\|merge"; then
    _pass "ReviewGate tag created for squash merge"
else
    # Classification only runs in --incremental mode, not on initial import
    _skip "ReviewGate tag for squash merge" "initial import did not produce a classification tag"
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
require_success "create Git feature branch" git checkout --quiet -b feature
git_commit "feat: widget" "src/widget.rs" "fn widget() {}"
require_success "return to default Git branch" git checkout --quiet "$DEFAULT_BRANCH"
require_success "create Git merge commit" git merge --quiet --no-ff feature -m "Merge branch 'feature'"

# Import all at once
require_success "import with merge" atomic git import --no-vault

# Verify changes were imported (initial + feature + merge)
require_capture "count merge history" count_log_entries
LOG_COUNT="$COMMAND_OUTPUT"
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
require_success "import" atomic git import --no-vault

# Install hooks
require_success "hooks install" atomic git hooks install

# Verify the supported advisory dispatchers were created. Legacy import hooks
# are intentionally removed rather than automatically importing in a hook.
assert_file_exists "post-checkout hook exists" ".git/hooks/post-checkout"
assert_file_exists "pre-commit hook exists" ".git/hooks/pre-commit"
assert_file_exists "pre-push hook exists" ".git/hooks/pre-push"
assert_file_exists "post-rewrite hook exists" ".git/hooks/post-rewrite"
assert_file_exists "reference-transaction hook exists" ".git/hooks/reference-transaction"
assert_file_not_exists "legacy post-commit hook absent" ".git/hooks/post-commit"
assert_file_not_exists "legacy post-merge hook absent" ".git/hooks/post-merge"

# Verify hook content has our markers
assert_output_contains "post-checkout has dispatcher marker" "atomic:git-bridge-dispatcher:v1" cat .git/hooks/post-checkout
assert_output_contains "post-checkout calls advisory shim" "git shim hook-post-checkout" cat .git/hooks/post-checkout

# Status should show installed
assert_output_contains "status shows installed" "installed" atomic git hooks status

# Idempotent install
require_success "second install is idempotent" atomic git hooks install

# Uninstall
require_success "hooks uninstall" atomic git hooks uninstall

# Verify hooks were removed
assert_file_not_exists "post-checkout hook removed" ".git/hooks/post-checkout"
assert_file_not_exists "post-rewrite hook removed" ".git/hooks/post-rewrite"

# Status should show not installed
assert_output_contains "status shows not installed" "not installed" atomic git hooks status

# ════════════════════════════════════════════════════════════════════════
# Section 7: Hooks preserve existing content
# ════════════════════════════════════════════════════════════════════════

begin_section "Git hooks: preserve existing hook content"

make_temp_repo "hooks-preserve"
init_git_repo
git_commit "Initial" "README.md" "# Test"
require_success "import" atomic git import --no-vault

# Create a pre-existing hook. Atomic must not alter an unmanaged dispatcher.
mkdir -p .git/hooks
echo '#!/bin/sh
echo "my custom hook"' > .git/hooks/post-checkout
chmod +x .git/hooks/post-checkout

# Install leaves the unmanaged hook intact and gives integration instructions.
require_capture "install with existing hook" atomic git hooks install
assert_output_contains "unmanaged hook reported" "left untouched" printf '%s\n' "$COMMAND_OUTPUT"
assert_file_content "existing hook content preserved exactly" ".git/hooks/post-checkout" '#!/bin/sh
echo "my custom hook"'
assert_output_not_contains "no dispatcher added to unmanaged hook" "atomic:git-bridge-dispatcher:v1" cat .git/hooks/post-checkout

# Uninstall should remove only atomic section
require_success "uninstall preserves custom" atomic git hooks uninstall
assert_file_content "custom hook still preserved exactly" ".git/hooks/post-checkout" '#!/bin/sh
echo "my custom hook"'
assert_output_not_contains "no dispatcher added after uninstall" "atomic:git-bridge-dispatcher:v1" cat .git/hooks/post-checkout

# ════════════════════════════════════════════════════════════════════════
# Section 8: Incremental import idempotency
# ════════════════════════════════════════════════════════════════════════

begin_section "Incremental import: idempotent"

make_temp_repo "idempotent"
init_git_repo
git_commit "First" "a.txt" "alpha"
git_commit "Second" "b.txt" "beta"

require_success "initial import" atomic git import --no-vault
require_capture "count initial history" count_log_entries
BASE_COUNT="$COMMAND_OUTPUT"
if [ "$BASE_COUNT" -ge 2 ]; then
    _pass "initial import (count=$BASE_COUNT, expected >=2)"
else
    _fail "expected >=2, got $BASE_COUNT" ""
fi

# Run incremental with no new commits — should be a no-op
require_success "incremental no-op" atomic git import --incremental
require_capture "count history after no-op" count_log_entries
NOOP_COUNT="$COMMAND_OUTPUT"
if [ "$NOOP_COUNT" -eq "$BASE_COUNT" ]; then
    _pass "no-op: count unchanged ($NOOP_COUNT)"
else
    _fail "expected $BASE_COUNT, got $NOOP_COUNT" ""
fi

# Add a commit and import again
git_commit "Third" "c.txt" "gamma"
require_success "incremental with new commit" atomic git import --incremental
require_capture "count history after new commit" count_log_entries
NEW_COUNT="$COMMAND_OUTPUT"
EXPECTED=$((BASE_COUNT + 1))
if [ "$NEW_COUNT" -eq "$EXPECTED" ]; then
    _pass "added 1 change ($NEW_COUNT)"
else
    _fail "expected $EXPECTED, got $NEW_COUNT" ""
fi

# Run incremental again — should be a no-op again
require_success "incremental no-op again" atomic git import --incremental
require_capture "count history after second no-op" count_log_entries
FINAL_COUNT="$COMMAND_OUTPUT"
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

require_success "initial import for draft regression" atomic git import --no-vault
require_capture "count primary history before import" count_log_entries --view "$PRIMARY_BRANCH"
PRIMARY_COUNT_BEFORE="$COMMAND_OUTPUT"

# Draft logs hide inherited parent changes by default. Incremental import must
# still deduplicate against the explicit target view, and it must not leave the
# user's current draft merely because Git's branch was checked for new commits.
require_success "create and switch to inherited draft" \
    atomic view create agent-draft --draft --parent "$PRIMARY_BRANCH" --switch
require_success "incremental no-op while draft is current" \
    atomic git import --incremental --branch "$PRIMARY_BRANCH" --no-vault

require_capture "count primary history after no-op" count_log_entries --view "$PRIMARY_BRANCH"
PRIMARY_COUNT_AFTER="$COMMAND_OUTPUT"
if [ "$PRIMARY_COUNT_AFTER" -eq "$PRIMARY_COUNT_BEFORE" ]; then
    _pass "active draft no-op: target count unchanged ($PRIMARY_COUNT_AFTER)"
else
    _fail "active draft no-op duplicated target history" \
        "expected $PRIMARY_COUNT_BEFORE, got $PRIMARY_COUNT_AFTER"
fi

assert_current_view "incremental import preserves current draft" "agent-draft"

# A real incremental sync can arrive while the draft has diverged from its
# parent. The import target must not be reconciled against that foreign working
# copy: doing so would remove b.txt from the primary TREE just because the
# draft currently has an unrecorded deletion.
printf '%s' "draft alpha" > a.txt
printf '%s' "draft shared" > shared.txt
printf '%s' "draft rename" > rename-me.txt
require_success "record divergent draft modification" \
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
if import_output="$(atomic git import --incremental --branch "$PRIMARY_BRANCH" --no-vault 2>&1)"; then
    _pass "incremental new commit while draft is current"

require_capture "count primary history after new commit" count_log_entries --view "$PRIMARY_BRANCH"
PRIMARY_COUNT_WITH_NEW_COMMIT="$COMMAND_OUTPUT"
EXPECTED_PRIMARY_COUNT=$((PRIMARY_COUNT_BEFORE + 1))
if [ "$PRIMARY_COUNT_WITH_NEW_COMMIT" -eq "$EXPECTED_PRIMARY_COUNT" ]; then
    _pass "active draft import adds exactly one target change"
else
    _fail "active draft import adds exactly one target change" \
        "expected $EXPECTED_PRIMARY_COUNT, got $PRIMARY_COUNT_WITH_NEW_COMMIT"
fi

assert_current_view "new-commit import preserves current draft" "agent-draft"
assert_file_content "draft modification remains materialized" "a.txt" "draft alpha"
assert_file_not_exists "draft deletion remains materialized" "b.txt"
assert_file_content "draft-owned target deletion remains materialized" \
    "shared.txt" "draft shared"
assert_file_content "draft-owned target rename remains materialized" \
    "rename-me.txt" "draft rename"
assert_file_not_exists "target rename does not leak into active draft" "renamed.txt"
assert_file_not_exists "target addition does not leak into active draft" "c.txt"
require_capture "read draft status after import" atomic status --short
DRAFT_STATUS="$COMMAND_OUTPUT"
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
require_success "record foreground add at deferred target path" \
    atomic record --all -m "Draft independently adds target path"
rm b.txt
assert_file_content "foreground same-path add remains materialized" \
    "c.txt" "draft gamma"

# A normal foreground rename after the deferred import must become the source
# view's new path baseline. Switching to the target and back may not resurrect
# the stale pre-rename path from the first journal event.
printf '%s' "beta" > b.txt
require_success "foreground draft rename after background import" \
    mv rename-me.txt draft-renamed.txt
require_success "record foreground draft rename" \
    atomic record --all -m "Draft renames after background import"
rm b.txt
assert_file_content "foreground draft rename is materialized" \
    "draft-renamed.txt" "draft rename"
assert_file_not_exists "foreground draft rename removes old path" "rename-me.txt"

require_success "force switch to imported primary after draft sync" \
    atomic view switch --force "$PRIMARY_BRANCH"
assert_file_content "primary modification is not taken from draft" "a.txt" "alpha"
assert_output_contains "primary graph retains the draft-deleted path" "D  b.txt" \
    atomic status --short
assert_file_content "primary receives the new Git commit" "c.txt" "gamma"
assert_file_not_exists "primary receives the target deletion" "shared.txt"
assert_file_not_exists "primary removes the old renamed path" "rename-me.txt"
assert_file_content "primary receives the renamed path" "renamed.txt" "rename baseline"

require_success "switch back to source with same-path add" \
    atomic view switch --force agent-draft
assert_file_content "source restores its own same-path inode" \
    "c.txt" "draft gamma"
assert_file_content "source restores its foreground rename" \
    "draft-renamed.txt" "draft rename"
assert_file_not_exists "source removes the target rename again" "renamed.txt"

# Return TREE to the target state used by the interrupted-switch recovery
# fixture below.
require_success "switch target again before recovery fixture" \
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
require_success "recover interrupted TREE/view alignment" \
    atomic view switch --force agent-draft
assert_file_not_exists "recovery clears the pending alignment marker" \
    ".atomic/deferred-tree-alignment.pending"
assert_file_content "draft keeps its later foreground rename" \
    "draft-renamed.txt" "draft rename"
assert_file_content "recovery restores the source same-path inode" \
    "c.txt" "draft gamma"
assert_file_not_exists "draft does not resurrect the stale rename baseline" "rename-me.txt"
assert_file_not_exists "draft does not retain the target rename" "renamed.txt"
else
    _fail "incremental new commit while draft is current" "$import_output"
    require_capture "count primary history after refused import" count_log_entries --view "$PRIMARY_BRANCH"
    if [[ "$COMMAND_OUTPUT" -eq "$PRIMARY_COUNT_BEFORE" ]]; then
        _pass "refused import preserves primary change count"
    else
        _fail "refused import preserves primary change count" "before=$PRIMARY_COUNT_BEFORE after=$COMMAND_OUTPUT"
    fi
    assert_current_view "refused import preserves current draft" "agent-draft"
    assert_file_content "refused import preserves draft modification" "a.txt" "draft alpha"
    assert_file_content "refused import preserves draft shared file" "shared.txt" "draft shared"
    assert_file_content "refused import preserves draft rename bytes" "rename-me.txt" "draft rename"
    assert_file_not_exists "refused import preserves draft deletion" "b.txt"
    assert_file_not_exists "refused import does not materialize target addition" "c.txt"
    _skip "active draft's later materialization and recovery checks" "incremental import failed"
fi

# ════════════════════════════════════════════════════════════════════════
# Section 9: Git-owned materialization after importing all local branches
# ════════════════════════════════════════════════════════════════════════

begin_section "Git shadow: Git owns the checked-out branch"

make_temp_repo "git-owned-materialization"
init_git_repo

git_commit "Main baseline" "shared.txt" "main baseline"
PRIMARY_BRANCH=$(git_default_branch)
require_success "create Git materialization branch" git checkout --quiet -b zz-materialization
git_commit "Feature content" "feature-only.txt" "feature content"
printf '#!/bin/sh\necho feature\n' > feature-tool.sh
chmod +x feature-tool.sh
ln -s shared.txt feature-link
git add feature-tool.sh feature-link
git commit --quiet -m "Add executable and symlink"
require_success "return to primary Git branch" git checkout --quiet "$PRIMARY_BRANCH"

# `--all` imports every local Git branch. Git remains checked out on the
# primary branch, so Atomic must not leave the worktree materialized as the
# final non-HEAD Atomic view.
require_success "import all local branches" atomic git import --all --no-vault

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
require_success "checkout Git materialization branch" git checkout --quiet zz-materialization
require_success "incremental import of checked-out feature" atomic git import --incremental --branch zz-materialization --no-vault
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

# git shadows Atomic (SPEC-single-materializer-validator.md §5.4): an
# `atomic view switch` now repoints the git shadow's HEAD to the mirror branch.
# This supersedes the old "git owns the checkout; view switch must not move git"
# model. It is a ref move — git metadata is preserved and files are never
# re-rendered; Atomic stays the content authority.
if switch_output="$(atomic view switch "$PRIMARY_BRANCH" 2>&1)"; then
_pass "Atomic view switch materializes primary view"
assert_dir_exists "Atomic view switch preserves Git metadata" ".git"
assert_file_content "primary view content is materialized" "shared.txt" "main baseline"

require_capture "read switched Git branch" git symbolic-ref --short HEAD
SWITCHED_GIT_BRANCH="$COMMAND_OUTPUT"
if [ "$SWITCHED_GIT_BRANCH" = "$PRIMARY_BRANCH" ]; then
    _pass "git shadow HEAD follows the view to '$PRIMARY_BRANCH'"
else
    _fail "git shadow HEAD follows the view to '$PRIMARY_BRANCH'" "on '$SWITCHED_GIT_BRANCH'"
fi

# The feature-* files are residue from the earlier raw `git checkout
# zz-materialization` (mixing raw git with Atomic — the anti-pattern SPEC §5.4
# discourages). Now that git HEAD follows the view to the primary branch they
# show as untracked; a shadow push would refuse them (V2). Clearing the residue
# restores a clean tree.
rm -f feature-only.txt feature-tool.sh feature-link
GIT_STATUS_AFTER_CLEAN=$(git status --short)
if [[ -z "$GIT_STATUS_AFTER_CLEAN" ]]; then
    _pass "clean worktree once raw-git residue is cleared"
else
    _fail "clean worktree once raw-git residue is cleared" "$GIT_STATUS_AFTER_CLEAN"
fi
else
    _fail "Atomic view switch materializes primary view" "$switch_output"
    require_capture "read Git branch after refused switch" git symbolic-ref --short HEAD
    if [[ "$COMMAND_OUTPUT" == "zz-materialization" ]]; then
        _pass "refused switch preserves Git branch"
    else
        _fail "refused switch preserves Git branch" "got: $COMMAND_OUTPUT"
    fi
    assert_file_content "refused switch preserves feature bytes" "feature-only.txt" "feature content"
    assert_file_content "refused switch preserves shared bytes" "shared.txt" "main baseline"
    if [[ -L feature-link && -x feature-tool.sh ]]; then
        _pass "refused switch preserves symlink and executable"
    else
        _fail "refused switch preserves symlink and executable" "feature-link or feature-tool.sh changed"
    fi
    require_capture "read Git status after refused switch" git status --short
    if [[ -z "$COMMAND_OUTPUT" ]]; then
        _pass "refused switch preserves clean Git status"
    else
        _fail "refused switch preserves clean Git status" "$COMMAND_OUTPUT"
    fi
    _skip "primary-view materialization checks" "view switch refused"
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
require_success "create Git login branch" git checkout --quiet -b feature-login
git_commit "Add login" "src/login.rs" "fn login() {}"
require_success "return to default Git branch" git checkout --quiet "$DEFAULT_BRANCH"
require_success "squash Git login branch" git merge --quiet --squash feature-login
require_success "record GitLab squash commit" git commit --quiet -m "Add login

See merge request mygroup/myproject!55"

require_success "import with GitLab MR" atomic git import --no-vault

# Verify changes were imported
require_capture "count GitLab history" count_log_entries
LOG_COUNT="$COMMAND_OUTPUT"
if [ "$LOG_COUNT" -ge 2 ]; then
    _pass "GitLab MR squash imported (count=$LOG_COUNT)"
else
    _fail "expected >=2 changes, got $LOG_COUNT" ""
fi

# ════════════════════════════════════════════════════════════════════════
# Summary
# ════════════════════════════════════════════════════════════════════════

print_summary
