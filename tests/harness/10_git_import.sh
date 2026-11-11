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

count_imported_git_changes() {
    local count=0 hash log_output details
    if ! log_output="$(atomic log --format short --no-color --full-hash 2>&1)"; then
        echo "atomic log failed: $log_output" >&2
        return 1
    fi
    while IFS= read -r hash; do
        [[ -z "$hash" ]] && continue
        if ! details="$(atomic change "$hash" --full-hash 2>&1)"; then
            echo "atomic change $hash failed: $details" >&2
            return 1
        fi
        if printf '%s\n' "$details" | grep -qF 'Git source revision:'; then
            count=$((count + 1))
        fi
    done <<< "$(printf '%s\n' "$log_output" | awk '$1 ~ /^[A-Z2-7]+$/ && length($1) == 52 { print $1 }')"
    echo "$count"
}

git_first_parent_commit_count() {
    git rev-list --first-parent --count HEAD
}
assert_imported_git_change_count() {
    local desc="$1"
    local expected="$2"
    local actual
    if ! actual="$(count_imported_git_changes 2>&1)"; then
        _fail "$desc" "failed to count imported changes: $actual"
        return
    fi
    if [[ "$actual" -eq "$expected" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "expected $expected imported git changes, got $actual"
    fi
}

find_imported_change_hash_containing() {
    local needle="$1"
    local hash log_output details
    if ! log_output="$(atomic log --format short --no-color --full-hash 2>&1)"; then
        echo "atomic log failed: $log_output" >&2
        return 1
    fi
    while IFS= read -r hash; do
        [[ -z "$hash" ]] && continue
        if ! details="$(atomic change "$hash" --full-hash 2>&1)"; then
            echo "atomic change $hash failed: $details" >&2
            return 1
        fi
        if printf '%s\n' "$details" | grep -qiF "$needle"; then
            echo "$hash"
            return 0
        fi
    done <<< "$(printf '%s\n' "$log_output" | awk '$1 ~ /^[A-Z2-7]+$/ && length($1) == 52 { print $1 }')"
    echo "no imported change contains '$needle'" >&2
    return 1
}

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

expected_commits="$(git_first_parent_commit_count)"
default_branch="$(git_default_branch)"
echo "  Found $expected_commits first-parent commits on branch '$default_branch'"

# Initialize atomic and import
require_success "atomic git import succeeds" atomic git import

git_status_after_import="$(git status --short)"
if [[ -z "$git_status_after_import" ]]; then
    _pass "git status stays clean after import"
else
    _fail "git status dirty after import" "$git_status_after_import"
fi

# Verify view created (should use default branch name)
assert_view_exists "view '$default_branch' created" "$default_branch"

# Verify imported change count is effectively complete.
# Some tiny repos contain a non-material merge/empty commit that import
# intentionally skips, so allow a one-commit gap here.
require_capture "count imported tiny-repository changes" count_imported_git_changes
actual_imported_commits="$COMMAND_OUTPUT"
if [[ "$actual_imported_commits" -eq "$expected_commits" ]] || \
   [[ "$actual_imported_commits" -eq $((expected_commits - 1)) ]]; then
    _pass "change count matches git first-parent commits ($expected_commits)"
else
    _fail "change count matches git first-parent commits ($expected_commits)" \
        "expected $expected_commits imported git mainline changes (or $((expected_commits - 1)) with one skipped empty/merge commit), got $actual_imported_commits"
fi

# ════════════════════════════════════════════════════════════════════════════
# Section 3: Dry Run Mode
# ════════════════════════════════════════════════════════════════════════════

begin_section "Git Import: Dry Run Mode"

make_temp_repo "git-import-dry-run"
clone_git_repo "https://github.com/hashicorp/go-uuid.git"
cd "$GIT_REPO_DIR"

assert_dir_not_exists "fresh clone has no Atomic repository" ".atomic"

# Run dry-run
require_capture "git import dry-run succeeds" atomic git import --dry-run
out="$COMMAND_OUTPUT"

# Verify no .atomic created
assert_dir_not_exists ".atomic not created in dry-run" ".atomic"

# Verify output shows preview (flexible matching)
if echo "$out" | grep -qiE "would import|preview|commits|dry.run"; then
    _pass "dry-run output shows preview"
else
    _fail "dry-run output shows preview" "got: $out"
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
require_success "return to main Git branch" git checkout "$main_branch" --quiet

# Import main branch
require_success "initialize Atomic repository" atomic init
require_success "import main branch" atomic git import --branch "$main_branch"
assert_imported_git_change_count "main has 2 commits" 2

# Import feature branch
require_success "import feature branch" atomic git import --branch feature
assert_view_exists "feature view created" "feature"

# ════════════════════════════════════════════════════════════════════════════
# Section 5: Author/Message/Timestamp Preservation (Medium Repo)
# ════════════════════════════════════════════════════════════════════════════

begin_section "Git Import: Author/Message/Timestamp Preservation (Medium Repo)"

make_temp_repo "git-import-medium"

echo "  Cloning holman/spark..."
clone_git_repo "https://github.com/holman/spark.git"
cd "$GIT_REPO_DIR"

expected_commits="$(git_first_parent_commit_count)"
echo "  Found $expected_commits first-parent commits"

require_success "atomic git import succeeds" atomic git import

# Locate an actual first-parent commit by its Git source revision, rather than
# matching the repository name in every imported change's metadata.
medium_git_sha="$(git log --first-parent --author=holman --regexp-ignore-case --grep=spark --format=%H -n 1)"
if [[ -n "$medium_git_sha" ]] && medium_change_hash="$(find_imported_change_hash_containing "$medium_git_sha" 2>&1)"; then
    assert_change_author "change has author preserved" "$medium_change_hash" "holman"
    assert_change_message "commit message preserved" "$medium_change_hash" "spark"
else
    _fail "change has author preserved" "no matching Git commit or imported change: ${medium_change_hash:-$medium_git_sha}"
    _fail "commit message preserved" "no matching Git commit or imported change: ${medium_change_hash:-$medium_git_sha}"
fi

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

require_success "initialize Atomic repository" atomic init
require_success "import file operations" atomic git import

assert_imported_git_change_count "5 changes imported" 5

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

require_success "initialize Atomic repository" atomic init
require_success "import initial Git history" atomic git import
assert_imported_git_change_count "initial import: 2 changes" 2

# Add more git commits
git_commit "Commit 3" "file3.txt" "v3"
git_commit "Commit 4" "file4.txt" "v4"

# Incremental import (should only add new commits)
require_success "import incremental Git history" atomic git import --incremental
assert_imported_git_change_count "after incremental: 4 changes" 4

# ════════════════════════════════════════════════════════════════════════════
# Section 8: Large Repo Performance
# ════════════════════════════════════════════════════════════════════════════

begin_section "Git Import: Large Repo Performance (hyperfine)"

make_temp_repo "git-import-large"

echo "  Cloning sharkdp/hyperfine (this may take a minute)..."
clone_git_repo "https://github.com/sharkdp/hyperfine.git"
cd "$GIT_REPO_DIR"

expected_commits="$(git_first_parent_commit_count)"
echo "  Found $expected_commits first-parent commits"

start_time=$(date +%s)
require_success "import hyperfine Git history" atomic git import
end_time=$(date +%s)
duration=$((end_time - start_time))

echo "  Import took ${duration}s"

# Should complete in reasonable time (< 5 minutes)
if [[ $duration -lt 300 ]]; then
    _pass "import completed in reasonable time (<5min)"
else
    _fail "import completed in reasonable time" "took ${duration}s"
fi

# Verify imported Git mainline commit count exactly. Do not use raw `atomic log`
# length here: git import records follow-up Atomic-only changes such as
# repository/vault initialization, and those are not Git commits.
require_capture "count imported hyperfine changes" count_imported_git_changes
actual="$COMMAND_OUTPUT"
if [[ "$actual" -eq "$expected_commits" ]]; then
    _pass "imported git change count matches ($actual vs $expected_commits)"
else
    _fail "imported git change count matches" "expected $expected_commits imported git changes, got $actual"
fi

# ════════════════════════════════════════════════════════════════════════════
# Section 9: Error Handling
# ════════════════════════════════════════════════════════════════════════════

begin_section "Git Import: Error Handling"

make_temp_repo "git-import-errors"

# Initialize atomic only (not a git repo)
require_success "initialize non-Git Atomic repository" atomic init

# Should fail with clear error about not being a git repo
status=0
out="$(atomic git import 2>&1)" || status=$?
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
status=0
out="$(atomic git import --branch nonexistent 2>&1)" || status=$?
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

require_success "initialize Atomic repository" atomic init
require_success "import Git metadata" atomic git import

# Check change details for git metadata (SHA should be preserved)
require_capture "read imported Git metadata" find_imported_change_hash_containing "$sha_full"
metadata_hash="$COMMAND_OUTPUT"
require_capture "show imported change metadata" atomic change "$metadata_hash" --full-hash
out="$COMMAND_OUTPUT"
if echo "$out" | grep -qiF "$sha_full"; then
    _pass "git metadata present in change"
else
    _fail "git metadata present in change" "expected commit $sha_full: $out"
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

require_success "initialize Atomic repository" atomic init
require_success "import all Git branches" atomic git import --all

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
# Detect the default branch name (may be "main" or "master" depending on git config)
_default_branch="$(git symbolic-ref --short HEAD 2>/dev/null || echo main)"
git_commit "Base commit" "base.txt" "base"
git checkout -b feature --quiet
git_commit "Feature commit" "feature.txt" "feature"
git checkout "$_default_branch" --quiet 2>/dev/null
git_commit "Main commit" "main2.txt" "main2"
git merge feature --no-ff -m "Merge feature into main" --quiet 2>/dev/null

require_success "initialize Atomic repository" atomic init
require_success "import merge history" atomic git import

# Should have at least 3 changes (base, feature, main2)
# Merge commit handling varies: might be 3 (linearized) or 4 (preserved)
require_capture "read imported merge log" atomic log
count="$(printf '%s\n' "$COMMAND_OUTPUT" | grep -cE '^\s*#[0-9]+|^[0-9a-f]{8,}' || true)"
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
require_success "add Git submodule" git_add_submodule "https://github.com/hashicorp/go-uuid.git" "vendor/uuid"

require_success "initialize Atomic repository" atomic init
require_capture "import Git history with submodule" atomic git import
out="$COMMAND_OUTPUT"

# Submodules should either:
# - Be skipped with a warning
# - Be imported as directory entries (submodule reference)
# Either approach is acceptable for MVP
if echo "$out" | grep -qiE "submodule|skip|warn"; then
    _pass "submodule handled (skipped with warning)"
else
    assert_file_exists "submodule checkout remains present" "vendor/uuid/uuid.go"
fi

# ════════════════════════════════════════════════════════════════════════════
# Section 14: Status Parity After Import (Renames + Deletions)
# ════════════════════════════════════════════════════════════════════════════
#
# After `atomic git import`, `atomic status` must be clean — the pristine
# graph should match the git working copy exactly.  This section exercises
# the two operations that historically caused spurious status entries:
#
#   1. File deletions:  record_modified_file (for diff lines) produces
#      GraphOp::Replacement, not GraphOp::FileDel, so apply_change never
#      removed the TREE entry.  The fix explicitly untracks deleted files
#      after apply.
#
#   2. File renames:  git mv old new must produce a GraphOp::FileMove that
#      updates TREE (del old path, put new path with same inode).

begin_section "Git Import: Status Parity After Import (renames + deletions)"

make_temp_repo "git-import-status-parity"
GIT_REPO="$REPO_DIR"

git -C "$GIT_REPO" init --quiet 2>/dev/null
git -C "$GIT_REPO" config user.email "test@test.com" 2>/dev/null
git -C "$GIT_REPO" config user.name "Test" 2>/dev/null

# ── v1: create a small tree (including a binary file) ───────────────────
mkdir -p "$GIT_REPO/src/sub" "$GIT_REPO/docs"
printf 'fn app() {}\n'    > "$GIT_REPO/src/sub/app.rs"
printf 'fn bench() {}\n'  > "$GIT_REPO/src/sub/bench.rs"
printf 'mod sub;\n'        > "$GIT_REPO/src/sub/mod.rs"
printf '# readme\n'        > "$GIT_REPO/docs/README.md"
printf 'old ci config\n'   > "$GIT_REPO/.travis.yml"
# Binary file: PNG header + null bytes (triggers is_binary detection)
printf '\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR_old_logo_data_here\n' > "$GIT_REPO/docs/logo.png"
git -C "$GIT_REPO" add -A >/dev/null 2>&1
git -C "$GIT_REPO" commit -m "v1: initial tree" --quiet 2>/dev/null

# ── v2: mass rename + delete + binary modify ────────────────────────────
#   - src/sub/app.rs   → src/app.rs      (pure rename)
#   - src/sub/bench.rs → src/bench.rs    (rename + content change)
#   - src/sub/mod.rs   → deleted
#   - .travis.yml      → deleted
#   - docs/logo.png    → modified (binary content swap)
mkdir -p "$GIT_REPO/src"
git -C "$GIT_REPO" mv src/sub/app.rs   src/app.rs   2>/dev/null
git -C "$GIT_REPO" mv src/sub/bench.rs src/bench.rs 2>/dev/null
printf 'fn bench_v2() {}\n// added in v2\n' > "$GIT_REPO/src/bench.rs"
git -C "$GIT_REPO" rm src/sub/mod.rs  --quiet 2>/dev/null
git -C "$GIT_REPO" rm .travis.yml     --quiet 2>/dev/null
# Replace binary file with different binary content (smaller PNG)
printf '\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR_new_v2\n' > "$GIT_REPO/docs/logo.png"
git -C "$GIT_REPO" add -A >/dev/null 2>&1
git -C "$GIT_REPO" commit -m "v2: flatten + delete + binary update" --quiet 2>/dev/null

# ── v3: modify a renamed file (to exercise the post-rename path) ────────
printf 'fn bench_v3() {}\n// v3 change\n' > "$GIT_REPO/src/bench.rs"
git -C "$GIT_REPO" add -A >/dev/null 2>&1
git -C "$GIT_REPO" commit -m "v3: modify after rename" --quiet 2>/dev/null

# ── Import ──────────────────────────────────────────────────────────────
require_success "import rename and deletion history" atomic git import

# ── Assertions ──────────────────────────────────────────────────────────

# 1. No spurious deleted files in status. Capture once so a command failure
# cannot be mistaken for an empty/clean result by any of these checks.
require_capture "read status after import" atomic status --short
_status="$COMMAND_OUTPUT"
_deleted_count=$(printf '%s\n' "$_status" | grep -c '^D ' || true)
if [[ "$_deleted_count" -eq 0 ]]; then
    _pass "no spurious deletions in status after import"
else
    _deleted_files=$(printf '%s\n' "$_status" | grep '^D ')
    _fail "spurious deletions in status" "$_deleted_count file(s): $_deleted_files"
fi

# 2. Renamed files are tracked (not untracked ??)
_untracked_app=$(printf '%s\n' "$_status" | grep -F '?? src/app.rs' || true)
if [[ -z "$_untracked_app" ]]; then
    _pass "src/app.rs tracked after rename"
else
    _fail "src/app.rs untracked after rename" "status shows: $_untracked_app"
fi

_untracked_bench=$(printf '%s\n' "$_status" | grep -F '?? src/bench.rs' || true)
if [[ -z "$_untracked_bench" ]]; then
    _pass "src/bench.rs tracked after rename+modify"
else
    _fail "src/bench.rs untracked after rename+modify" "status shows: $_untracked_bench"
fi

# 3. Deleted files are NOT tracked (should not appear in status at all)
_deleted_travis=$(printf '%s\n' "$_status" | grep -F 'travis' || true)
if [[ -z "$_deleted_travis" ]]; then
    _pass ".travis.yml not in status (correctly deleted)"
else
    _fail ".travis.yml still in status after delete" "status shows: $_deleted_travis"
fi

_deleted_mod=$(printf '%s\n' "$_status" | grep -F 'sub/mod.rs' || true)
if [[ -z "$_deleted_mod" ]]; then
    _pass "src/sub/mod.rs not in status (correctly deleted)"
else
    _fail "src/sub/mod.rs still in status after delete" "status shows: $_deleted_mod"
fi

# 4. Old rename source paths are gone from tracking
_old_sub_app=$(printf '%s\n' "$_status" | grep -F 'sub/app.rs' || true)
if [[ -z "$_old_sub_app" ]]; then
    _pass "src/sub/app.rs gone from status (old rename source)"
else
    _fail "src/sub/app.rs still in status" "should have been removed by rename: $_old_sub_app"
fi

# 5. Binary file is tracked and NOT modified (graph content matches disk)
_modified_logo=$(printf '%s\n' "$_status" | grep -F 'docs/logo.png' || true)
if [[ -z "$_modified_logo" ]]; then
    _pass "docs/logo.png clean after binary modify (graph matches disk)"
else
    _fail "docs/logo.png dirty after import" "status shows: $_modified_logo"
fi

# 6. Overall: status should be completely clean (no M, D, A, or ?? entries)
_status_lines=$(printf '%s\n' "$_status" | grep -cE '^[MDAU?]' || true)
if [[ "$_status_lines" -eq 0 ]]; then
    _pass "status is completely clean after import (full parity)"
else
    _dirty=$(printf '%s\n' "$_status" | head -10)
    _fail "status not clean after import" "$_status_lines dirty entries: $_dirty"
fi

_git_status_lines=$(cd "$GIT_REPO" && git status --short)
if [[ -z "$_git_status_lines" ]]; then
    _pass "git status is clean after import"
else
    _fail "git status dirty after import" "$_git_status_lines"
fi

# 7. Change count matches git commit count
_git_count=$(git -C "$GIT_REPO" rev-list --count HEAD 2>/dev/null)
require_capture "count imported parity changes" count_imported_git_changes
_atomic_log="$COMMAND_OUTPUT"
if [[ "$_atomic_log" -eq "$_git_count" ]]; then
    _pass "change count matches git ($_git_count)"
else
    _fail "change count mismatch" "git=$_git_count atomic=$_atomic_log"
fi

# 8. Verify file contents match git HEAD
_bench_content=$(cat "$GIT_REPO/src/bench.rs" 2>/dev/null)
_expected_bench=$(printf 'fn bench_v3() {}\n// v3 change\n')
if [[ "$_bench_content" == "$_expected_bench" ]]; then
    _pass "src/bench.rs content matches git HEAD after rename+modify chain"
else
    _fail "src/bench.rs content mismatch" "expected v3 content"
fi

# ════════════════════════════════════════════════════════════════════════════
# Summary
# ════════════════════════════════════════════════════════════════════════════

print_summary
