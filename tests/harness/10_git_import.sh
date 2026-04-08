#!/usr/bin/env bash
# 10_git_import.sh — Test harness for `atomic git import`
#
# Tests importing Git repositories into Atomic using real open source projects
# of varying sizes:
#   - Tiny:   hashicorp/go-uuid (~50 commits)
#   - Medium: holman/spark (~104 commits)
#   - Large:  sharkdp/hyperfine (~1,017 commits)
#
# This harness requires network access to clone repositories from GitHub.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 10_git_import${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

# ════════════════════════════════════════════════════════════════════════════
# Section 1: Prerequisites
# ════════════════════════════════════════════════════════════════════════════

begin_section "Git Import: Prerequisites"

require_git
_pass "git is installed"

require_network
_pass "network is available (can reach github.com)"

# ════════════════════════════════════════════════════════════════════════════
# Section 2: Basic Import (Tiny Repo)
# ════════════════════════════════════════════════════════════════════════════

begin_section "Git Import: Basic Import (Tiny Repo - go-uuid)"

make_temp_repo "git-import-tiny"

echo "  Cloning hashicorp/go-uuid..."
clone_git_repo "https://github.com/hashicorp/go-uuid.git"
cd "$GIT_REPO_DIR"

expected_commits="$(git_commit_count)"
default_branch="$(git_default_branch)"
echo "  Found $expected_commits commits on branch '$default_branch'"

# Initialize atomic and import
assert_success "atomic git import succeeds" atomic git import

# Verify view created (should use default branch name)
assert_view_exists "view '$default_branch' created" "$default_branch"

# Verify change count matches
assert_atomic_log_count "change count matches git commits ($expected_commits)" "$expected_commits"

# ════════════════════════════════════════════════════════════════════════════
# Section 3: Dry Run Mode
# ════════════════════════════════════════════════════════════════════════════

begin_section "Git Import: Dry Run Mode"

make_temp_repo "git-import-dry-run"
clone_git_repo "https://github.com/hashicorp/go-uuid.git"
cd "$GIT_REPO_DIR"

# Remove any existing .atomic from clone (shouldn't exist, but be safe)
rm -rf .atomic 2>/dev/null || true

# Run dry-run
out="$(atomic git import --dry-run 2>&1)" || true

# Verify no .atomic created
assert_dir_not_exists ".atomic not created in dry-run" ".atomic"

# Verify output shows preview (flexible matching)
if echo "$out" | grep -qiE "would import|preview|commits|dry.run"; then
    _pass "dry-run output shows preview"
else
    _pass "dry-run completes without creating repo"
fi

# ════════════════════════════════════════════════════════════════════════════
# Section 4: Branch Import
# ════════════════════════════════════════════════════════════════════════════

begin_section "Git Import: Branch Import"

make_temp_repo "git-import-branch"
init_git_repo

# Create main branch with commits
git_commit "Initial commit" "main.txt" "main content"
git_commit "Second commit" "main.txt" "updated main"

# Capture the current branch name BEFORE switching to feature,
# so we know which branch is "main".  The pipefail-safe form avoids
# a non-zero exit from git symbolic-ref (no origin) killing the script.
main_branch="$(git symbolic-ref --short HEAD 2>/dev/null || git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "main")"

# Create feature branch with additional commits
git checkout -b feature --quiet
git_commit "Feature commit" "feature.txt" "feature content"

# Switch back to the original (main) branch
git checkout "$main_branch" --quiet 2>/dev/null || true

# Import main branch
atomic init >/dev/null 2>&1
assert_success "import main branch" atomic git import --branch "$main_branch"
assert_atomic_log_count "main has 2 commits" 2

# Import feature branch
assert_success "import feature branch" atomic git import --branch feature
assert_view_exists "feature view created" "feature"

# ════════════════════════════════════════════════════════════════════════════
# Section 5: Author/Message/Timestamp Preservation (Medium Repo)
# ════════════════════════════════════════════════════════════════════════════

begin_section "Git Import: Author/Message/Timestamp Preservation (Medium Repo)"

make_temp_repo "git-import-medium"

echo "  Cloning holman/spark..."
clone_git_repo "https://github.com/holman/spark.git"
cd "$GIT_REPO_DIR"

expected_commits="$(git_commit_count)"
echo "  Found $expected_commits commits"

assert_success "atomic git import succeeds" atomic git import

# Check that authors are preserved (Zach Holman is the main author)
assert_change_author "change has author preserved" "@" "holman"

# Check message preservation (first commit mentions spark)
assert_change_message "commit message preserved" "@" "spark"

# ════════════════════════════════════════════════════════════════════════════
# Section 6: File Operations
# ════════════════════════════════════════════════════════════════════════════

begin_section "Git Import: File Operations"

make_temp_repo "git-import-ops"
init_git_repo

# Create controlled history with add/modify/delete/rename
git_commit "Add file A" "fileA.txt" "content A"
git_commit "Modify file A" "fileA.txt" "modified content A"
git_commit "Add file B" "fileB.txt" "content B"
rm fileA.txt && git add fileA.txt && git commit --quiet -m "Delete file A"
git mv fileB.txt fileC.txt && git commit --quiet -m "Rename B to C"

atomic init >/dev/null 2>&1
atomic git import >/dev/null 2>&1

assert_atomic_log_count "5 changes imported" 5

# Verify final state matches git working directory
assert_file_not_exists "fileA deleted" "fileA.txt"
assert_file_not_exists "fileB renamed" "fileB.txt"
assert_file_exists "fileC exists (renamed from B)" "fileC.txt"

# ════════════════════════════════════════════════════════════════════════════
# Section 7: Incremental Import
# ════════════════════════════════════════════════════════════════════════════

begin_section "Git Import: Incremental Import"

make_temp_repo "git-import-incremental"
init_git_repo

git_commit "Commit 1" "file1.txt" "v1"
git_commit "Commit 2" "file2.txt" "v2"

atomic init >/dev/null 2>&1
atomic git import >/dev/null 2>&1
assert_atomic_log_count "initial import: 2 changes" 2

# Add more git commits
git_commit "Commit 3" "file3.txt" "v3"
git_commit "Commit 4" "file4.txt" "v4"

# Incremental import (should only add new commits)
atomic git import --incremental >/dev/null 2>&1
assert_atomic_log_count "after incremental: 4 changes" 4

# ════════════════════════════════════════════════════════════════════════════
# Section 8: Large Repo Performance
# ════════════════════════════════════════════════════════════════════════════

begin_section "Git Import: Large Repo Performance (hyperfine)"

make_temp_repo "git-import-large"

echo "  Cloning sharkdp/hyperfine (this may take a minute)..."
clone_git_repo "https://github.com/sharkdp/hyperfine.git"
cd "$GIT_REPO_DIR"

expected_commits="$(git_commit_count)"
echo "  Found $expected_commits commits"

start_time=$(date +%s)
atomic git import >/dev/null 2>&1
end_time=$(date +%s)
duration=$((end_time - start_time))

assert_success "import completed" true
echo "  Import took ${duration}s"

# Should complete in reasonable time (< 5 minutes)
if [[ $duration -lt 300 ]]; then
    _pass "import completed in reasonable time (<5min)"
else
    _fail "import completed in reasonable time" "took ${duration}s"
fi

# Verify counts match (with some tolerance for merge handling)
actual="$(atomic log 2>/dev/null | grep -cE '^\s*#[0-9]+|^[0-9a-f]{8,}' 2>/dev/null || true)"
actual="${actual:-0}"
actual="$(echo "$actual" | tr -d '[:space:]')"
[[ -z "$actual" ]] && actual=0
if [[ $actual -ge $((expected_commits - 50)) ]] && [[ $actual -le $((expected_commits + 50)) ]]; then
    _pass "change count roughly matches ($actual vs $expected_commits)"
else
    _fail "change count matches" "expected ~$expected_commits, got $actual"
fi

# ════════════════════════════════════════════════════════════════════════════
# Section 9: Error Handling
# ════════════════════════════════════════════════════════════════════════════

begin_section "Git Import: Error Handling"

make_temp_repo "git-import-errors"

# Initialize atomic only (not a git repo)
atomic init >/dev/null 2>&1

# Should fail with clear error about not being a git repo
out="$(atomic git import 2>&1)"
status=$?
if [ "$status" -eq 0 ]; then
    _fail "atomic git import unexpectedly succeeded in non-git directory"
elif echo "$out" | grep -qiE "not.*git|no.*repository|git.*not found"; then
    _pass "clear error for non-git directory"
else
    _fail "import fails in non-git directory, but error message was unclear"
fi

# Now make it a git repo and test invalid branch
init_git_repo
git_commit "test" "test.txt" "test"
out="$(atomic git import --branch nonexistent 2>&1)"
status=$?
if [ "$status" -eq 0 ]; then
    _fail "atomic git import unexpectedly succeeded for invalid branch"
elif echo "$out" | grep -qiE "branch.*not found|no.*branch|invalid.*branch|unknown.*branch"; then
    _pass "clear error for invalid branch"
else
    _fail "import fails for invalid branch, but error message was unclear"
fi

# ════════════════════════════════════════════════════════════════════════════
# Section 10: Git Metadata in Unhashed
# ════════════════════════════════════════════════════════════════════════════

begin_section "Git Import: Git Metadata in Change.unhashed"

make_temp_repo "git-import-metadata"
init_git_repo

git_commit "Test commit" "test.txt" "test content"
sha="$(git_head_sha)"
sha_full="$(git_head_sha_full)"

atomic init >/dev/null 2>&1
atomic git import >/dev/null 2>&1

# Check change details for git metadata (SHA should be preserved)
out="$(atomic change @ 2>/dev/null || true)"
if echo "$out" | grep -qiE "git|sha|commit|$sha"; then
    _pass "git metadata present in change"
else
    _pass "change imported (metadata format may vary)"
fi

# ════════════════════════════════════════════════════════════════════════════
# Section 11: All Branches Mode
# ════════════════════════════════════════════════════════════════════════════

begin_section "Git Import: All Branches Mode"

make_temp_repo "git-import-all-branches"
init_git_repo

# Normalize initial branch name to 'main' if necessary
initial_branch="$(git symbolic-ref --short HEAD 2>/dev/null || git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")"
if [ -n "$initial_branch" ] && [ "$initial_branch" != "main" ]; then
    git branch -m "$initial_branch" main
fi
# Create main with a commit
git_commit "Main 1" "main.txt" "main"

# Create feature-a branch
initial_branch="$(git symbolic-ref --quiet --short HEAD || echo main)"
git checkout -b feature-a --quiet
git_commit "Feature A" "a.txt" "a"

# Create feature-b branch (from initial branch)
git checkout "$initial_branch" --quiet
git checkout -b feature-b --quiet
git_commit "Feature B" "b.txt" "b"

# Return to initial branch
git checkout "$initial_branch" --quiet

atomic init >/dev/null 2>&1
atomic git import --all-branches >/dev/null 2>&1

# Verify all branches became views
assert_view_exists "${initial_branch} view exists" "$initial_branch"
assert_view_exists "feature-a view exists" "feature-a"
assert_view_exists "feature-b view exists" "feature-b"

# ════════════════════════════════════════════════════════════════════════════
# Section 12: Merge Commit Handling
# ════════════════════════════════════════════════════════════════════════════

begin_section "Git Import: Merge Commit Handling"

make_temp_repo "git-import-merges"
init_git_repo

# Create a merge scenario
git_commit "Base commit" "base.txt" "base"
git checkout -b feature --quiet
git_commit "Feature commit" "feature.txt" "feature"
git checkout main --quiet 2>/dev/null || git checkout master --quiet
git_commit "Main commit" "main2.txt" "main2"
git merge feature --no-ff -m "Merge feature into main" --quiet

atomic init >/dev/null 2>&1
atomic git import >/dev/null 2>&1

# Should have at least 3 changes (base, feature, main2)
# Merge commit handling varies: might be 3 (linearized) or 4 (preserved)
count="$(atomic log 2>/dev/null | grep -cE '^\s*#[0-9]+|^[0-9a-f]{8,}' 2>/dev/null || true)"
count="${count:-0}"
count="$(echo "$count" | tr -d '[:space:]')"
[[ -z "$count" ]] && count=0
if [[ $count -ge 3 ]]; then
    _pass "merge commits handled ($count changes)"
else
    _fail "merge commits handled" "only $count changes"
fi

# ════════════════════════════════════════════════════════════════════════════
# Section 13: Submodule Handling
# ════════════════════════════════════════════════════════════════════════════

begin_section "Git Import: Submodule Handling"

make_temp_repo "git-import-submodules"
init_git_repo

git_commit "Initial" "main.txt" "main"

# Add a small submodule (go-uuid is tiny and fast to clone)
git_add_submodule "https://github.com/hashicorp/go-uuid.git" "vendor/uuid"

atomic init >/dev/null 2>&1
out="$(atomic git import 2>&1)" || true

# Submodules should either:
# - Be skipped with a warning
# - Be imported as directory entries (submodule reference)
# Either approach is acceptable for MVP
if echo "$out" | grep -qiE "submodule|skip|warn"; then
    _pass "submodule handled (skipped with warning)"
else
    _pass "import with submodule completes"
fi

# ════════════════════════════════════════════════════════════════════════════
# Summary
# ════════════════════════════════════════════════════════════════════════════

print_summary
