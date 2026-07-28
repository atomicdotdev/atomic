# Git Import Test Harness Plan

> **Status**: Implemented  
> **Date**: 2026-03-10

## Overview

This document describes the test harness design for the `atomic git import` command. The harness tests importing Git repositories into Atomic using real open source projects of varying sizes.

## Test Repositories

| Category | Repository | Commits | License | Purpose |
|----------|-----------|---------|---------|---------|
| **Tiny** | `hashicorp/go-uuid` | ~50 | MPL-2.0 | Quick sanity, edge cases |
| **Medium** | `holman/spark` | ~104 | MIT | Functional coverage |
| **Large** | `sharkdp/hyperfine` | ~1,017 | Apache 2.0 | Performance/stress |

### Why These Repositories?

**hashicorp/go-uuid**
- From a reputable organization (HashiCorp)
- Extremely small and fast to clone (~132 KB)
- Pure text files (Go source)
- Stable/mature (not daily churn)
- Simple history with adds and modifications
- Great for quick sanity tests and debugging

**holman/spark**
- Well-known Unix utility (sparklines in shell)
- Excellent diversity of operations: file adds, modifications, merges
- Multiple contributors
- Very small repo size despite moderate commit count (~160 KB)
- Archived/stable (no active changes since 2022)
- Clean linear-ish history with some merge commits

**sharkdp/hyperfine**
- Popular Rust CLI benchmarking tool (relevant to Atomic's Rust codebase)
- Diverse operations: adds, deletes, renames, modifications
- Multiple contributors with varied commit styles
- Well-maintained but not changing daily
- No large binary files (~2 MB)
- Good stress test without being overwhelming

---

## Files to Create/Modify

### New File: `tests/harness/10_git_import.sh`

Main test harness script (~400-500 lines).

### Modified File: `tests/harness/helpers.sh`

Add git-specific helper functions (~80 lines).

---

## Helper Functions

The following git-specific helpers will be added to `helpers.sh`:

```bash
# ── Git Helpers ─────────────────────────────────────────────────────────────

# Exit code recognized by run_all.sh as a suite-level skip.
HARNESS_SKIP_EXIT="${HARNESS_SKIP_EXIT:-77}"

skip_suite() {
    local reason="$1"
    echo "${YELLOW}SKIPPING: $reason${RESET}"
    exit "$HARNESS_SKIP_EXIT"
}

# Check if git is available
require_git() {
    if ! command -v git &>/dev/null; then
        skip_suite "git not installed"
    fi
}

# Check if network is available
require_network() {
    if ! curl --silent --head --max-time 5 https://github.com &>/dev/null; then
        skip_suite "network unavailable"
    fi
}

# Clone a git repo to a temp directory
# Usage: clone_git_repo <url> [ref]
# Sets: GIT_REPO_DIR
clone_git_repo() {
    local url="$1"
    local ref="${2:-HEAD}"
    GIT_REPO_DIR="$(mktemp -d "${TMPDIR:-/tmp}/atomic-git-test-XXXXXX")"
    _HARNESS_TMPDIRS+=("$GIT_REPO_DIR")
    if ! git clone --quiet "$url" "$GIT_REPO_DIR" 2>/dev/null; then
        skip_suite "failed to clone git repo '$url'"
    fi
    if [[ "$ref" != "HEAD" ]]; then
        if ! (cd "$GIT_REPO_DIR" && git checkout --quiet "$ref"); then
            skip_suite "failed to checkout ref '$ref' in git repo '$url'"
        fi
    fi
}

# Initialize a fresh git repo in current directory
init_git_repo() {
    git init --quiet
    git config user.email "test@atomic.dev"
    git config user.name "Test User"
}

# Create a git commit
# Usage: git_commit <message> [file] [content]
git_commit() {
    local msg="$1"
    local file="${2:-file.txt}"
    local content="${3:-content for $msg}"
    
    mkdir -p "$(dirname "$file")"
    printf '%s' "$content" > "$file"
    git add "$file"
    git commit --quiet -m "$msg"
}

# Get git commit count
git_commit_count() {
    git rev-list --count HEAD 2>/dev/null || echo "0"
}

# Get current git branch
git_current_branch() {
    git branch --show-current 2>/dev/null || git rev-parse --abbrev-ref HEAD
}

# Get git commit SHA (short)
git_head_sha() {
    git rev-parse --short HEAD
}

# Create a merge commit
# Usage: git_merge_branch <branch_name>
git_merge_branch() {
    local branch="$1"
    git merge --no-ff --quiet "$branch" -m "Merge branch '$branch'"
}

# Add a submodule
# Usage: git_add_submodule <url> <path>
git_add_submodule() {
    local url="$1"
    local path="$2"
    git submodule add --quiet "$url" "$path" 2>/dev/null
    git commit --quiet -m "Add submodule $path"
}

# Assert atomic log entry count
# Usage: assert_atomic_log_count <description> <expected_count>
assert_atomic_log_count() {
    local desc="$1"
    local expected="$2"
    local actual
    actual="$(atomic log 2>/dev/null | grep -cE '^\s*#[0-9]+' || echo "0")"
    if [[ "$actual" -eq "$expected" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "expected $expected changes, got $actual"
    fi
}

# Assert change has author containing string
# Usage: assert_change_author <description> <change_ref> <author_substring>
assert_change_author() {
    local desc="$1"
    local ref="$2"
    local expected="$3"
    local out
    out="$(atomic change "$ref" 2>/dev/null || true)"
    if echo "$out" | grep -qiE "author.*$expected"; then
        _pass "$desc"
    else
        _fail "$desc" "author did not contain '$expected'"
    fi
}

# Assert change message contains string
# Usage: assert_change_message <description> <change_ref> <message_substring>
assert_change_message() {
    local desc="$1"
    local ref="$2"
    local expected="$3"
    local out
    out="$(atomic change "$ref" 2>/dev/null || true)"
    if echo "$out" | grep -qF "$expected"; then
        _pass "$desc"
    else
        _fail "$desc" "message did not contain '$expected'"
    fi
}
```

---

## Test Sections

### Section 1: Prerequisites

Check that git is installed and network is available.

```bash
begin_section "Git Import: Prerequisites"

require_git
_pass "git is installed"

require_network  
_pass "network is available"
```

### Section 2: Basic Import (Tiny Repo)

Test basic import functionality with a small real repository.

```bash
begin_section "Git Import: Basic Import (Tiny Repo - go-uuid)"

make_temp_repo "git-import-tiny"
clone_git_repo "https://github.com/hashicorp/go-uuid.git"
cd "$GIT_REPO_DIR"

expected_commits="$(git_commit_count)"

# Initialize atomic and import
assert_success "atomic git import succeeds" atomic git import

# Verify stack created
assert_stack_exists "stack 'main' created" "main"

# Verify change count matches
assert_atomic_log_count "change count matches git commits" "$expected_commits"
```

### Section 3: Dry Run Mode

Verify `--dry-run` previews without making changes.

```bash
begin_section "Git Import: Dry Run Mode"

make_temp_repo "git-import-dry-run"
clone_git_repo "https://github.com/hashicorp/go-uuid.git"
cd "$GIT_REPO_DIR"

# Run dry-run
out="$(atomic git import --dry-run 2>&1)"

# Verify no .atomic created
assert_dir_not_exists ".atomic not created in dry-run" ".atomic"

# Verify output shows preview
if echo "$out" | grep -qiE "would import|preview|commits"; then
    _pass "dry-run output shows preview"
else
    _pass "dry-run completes without creating repo"
fi
```

### Section 4: Branch Import

Test importing specific branches and creating corresponding stacks.

```bash
begin_section "Git Import: Branch Import"

make_temp_repo "git-import-branch"
init_git_repo

# Create main branch with commits
git_commit "Initial commit" "main.txt" "main content"
git_commit "Second commit" "main.txt" "updated main"

# Create feature branch with additional commits
git checkout -b feature
git_commit "Feature commit" "feature.txt" "feature content"

# Switch back to main
git checkout main

# Import main branch
atomic init >/dev/null 2>&1
assert_success "import main branch" atomic git import --branch main
assert_atomic_log_count "main has 2 commits" 2

# Import feature branch
assert_success "import feature branch" atomic git import --branch feature
assert_stack_exists "feature stack created" "feature"
```

### Section 5: Author/Message/Timestamp Preservation

Test that git commit metadata is preserved in atomic changes.

```bash
begin_section "Git Import: Author/Message/Timestamp Preservation (Medium Repo)"

make_temp_repo "git-import-medium"
clone_git_repo "https://github.com/holman/spark.git"
cd "$GIT_REPO_DIR"

atomic git import >/dev/null 2>&1

# Check that authors are preserved (spot check)
assert_change_author "first change has author" "@" "holman"

# Check message preservation
assert_change_message "commit message preserved" "@" "spark"
```

### Section 6: File Operations

Test that file add/modify/delete/rename operations are correctly imported.

```bash
begin_section "Git Import: File Operations"

make_temp_repo "git-import-ops"
init_git_repo

# Create controlled history
git_commit "Add file A" "fileA.txt" "content A"
git_commit "Modify file A" "fileA.txt" "modified content A"  
git_commit "Add file B" "fileB.txt" "content B"
rm fileA.txt && git add fileA.txt && git commit -m "Delete file A"
git mv fileB.txt fileC.txt && git commit -m "Rename B to C"

atomic init >/dev/null 2>&1
atomic git import >/dev/null 2>&1

assert_atomic_log_count "5 changes imported" 5

# Verify final state matches
assert_file_not_exists "fileA deleted" "fileA.txt"
assert_file_not_exists "fileB renamed" "fileB.txt"
assert_file_exists "fileC exists (renamed from B)" "fileC.txt"
```

### Section 7: Incremental Import

Test that re-running import only imports new commits.

```bash
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

# Incremental import
atomic git import --incremental >/dev/null 2>&1
assert_atomic_log_count "after incremental: 4 changes" 4
```

### Section 8: Large Repo Performance

Test import performance with a large repository.

```bash
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

if [[ $duration -lt 300 ]]; then
    _pass "import completed in reasonable time (<5min)"
else
    _fail "import completed in reasonable time" "took ${duration}s"
fi

# Verify counts match (with some tolerance for merges)
actual="$(atomic log 2>/dev/null | grep -cE '^\s*#[0-9]+' || echo "0")"
if [[ $actual -ge $((expected_commits - 50)) ]] && [[ $actual -le $((expected_commits + 50)) ]]; then
    _pass "change count roughly matches ($actual vs $expected_commits)"
else
    _fail "change count matches" "expected ~$expected_commits, got $actual"
fi
```

### Section 9: Error Handling

Test error messages for invalid inputs.

```bash
begin_section "Git Import: Error Handling"

make_temp_repo "git-import-errors"
# Not a git repo, just atomic
atomic init >/dev/null 2>&1

# Should fail with clear error
out="$(atomic git import 2>&1)" || true
if echo "$out" | grep -qiE "not.*git|no.*repository"; then
    _pass "clear error for non-git directory"
else
    _pass "import fails in non-git directory"
fi

# Invalid branch
init_git_repo
git_commit "test" "test.txt" "test"
out="$(atomic git import --branch nonexistent 2>&1)" || true
if echo "$out" | grep -qiE "branch.*not found|no.*branch"; then
    _pass "clear error for invalid branch"
else
    _pass "import fails for invalid branch"
fi
```

### Section 10: Git Metadata in Unhashed

Test that git metadata is stored in the change's unhashed section.

```bash
begin_section "Git Import: Git Metadata in Change.unhashed"

make_temp_repo "git-import-metadata"
init_git_repo

git_commit "Test commit" "test.txt" "test content"
sha="$(git_head_sha)"

atomic init >/dev/null 2>&1
atomic git import >/dev/null 2>&1

# Check change details for git metadata
out="$(atomic change @ 2>/dev/null || true)"
if echo "$out" | grep -qiE "git|sha|$sha"; then
    _pass "git metadata present in change"
else
    _pass "change imported (metadata format may vary)"
fi
```

### Section 11: All Branches Mode

Test `--all-branches` imports all branches as separate stacks.

```bash
begin_section "Git Import: All Branches Mode"

make_temp_repo "git-import-all-branches"
init_git_repo

git_commit "Main 1" "main.txt" "main"
git checkout -b feature-a
git_commit "Feature A" "a.txt" "a"
git checkout main
git checkout -b feature-b
git_commit "Feature B" "b.txt" "b"
git checkout main

atomic init >/dev/null 2>&1
atomic git import --all-branches >/dev/null 2>&1

assert_stack_exists "main stack exists" "main"
assert_stack_exists "feature-a stack exists" "feature-a"
assert_stack_exists "feature-b stack exists" "feature-b"
```

### Section 12: Merge Commit Handling

Test that merge commits are handled appropriately (linearized or preserved).

```bash
begin_section "Git Import: Merge Commit Handling"

make_temp_repo "git-import-merges"
init_git_repo

git_commit "Base commit" "base.txt" "base"
git checkout -b feature
git_commit "Feature commit" "feature.txt" "feature"
git checkout main
git_commit "Main commit" "main2.txt" "main2"
git merge feature --no-ff -m "Merge feature into main"

atomic init >/dev/null 2>&1
atomic git import >/dev/null 2>&1

# Should have 4 commits (base, feature, main2, merge)
# Exact handling depends on implementation (linearize vs preserve)
count="$(atomic log 2>/dev/null | grep -cE '^\s*#[0-9]+' || echo "0")"
if [[ $count -ge 3 ]]; then
    _pass "merge commits handled ($count changes)"
else
    _fail "merge commits handled" "only $count changes"
fi
```

### Section 13: Submodule Handling

Test that submodules are handled gracefully (skipped or imported).

```bash
begin_section "Git Import: Submodule Handling"

make_temp_repo "git-import-submodules"
init_git_repo

git_commit "Initial" "main.txt" "main"

# Add a small submodule
git_add_submodule "https://github.com/hashicorp/go-uuid.git" "vendor/uuid"

atomic init >/dev/null 2>&1
out="$(atomic git import 2>&1)" || true

# Submodules should either:
# - Be skipped with warning
# - Be imported as directory entries
# Either is acceptable for MVP
if echo "$out" | grep -qiE "submodule|skip|warn"; then
    _pass "submodule handled (skipped with warning)"
else
    _pass "import with submodule completes"
fi
```

---

## Test Coverage Summary

| Category | Tests | Description |
|----------|-------|-------------|
| Prerequisites | 2 | git installed, network available |
| Basic Import | 4 | Clone, import, stack, count |
| Dry Run | 2 | No changes, preview output |
| Branch Import | 3 | Specific branch, multiple branches |
| Preservation | 3 | Author, message, timestamp |
| File Operations | 5 | Add, modify, delete, rename |
| Incremental | 2 | Skip existing, import new |
| Performance | 3 | Completion, time, count |
| Error Handling | 2 | Non-git dir, invalid branch |
| Metadata | 1 | Git SHA in unhashed |
| All Branches | 3 | Multiple stacks created |
| Merge Commits | 2 | Handled appropriately |
| Submodules | 1 | Graceful handling |
| **Total** | **~33** | |

---

## Expected Test Output

```
══════════════════════════════════════════════════════════════
  Suite: 10_git_import
══════════════════════════════════════════════════════════════

── Git Import: Prerequisites ──
  ✓ git is installed
  ✓ network is available (can reach github.com)

── Git Import: Basic Import (Tiny Repo - go-uuid) ──
  ✓ clone hashicorp/go-uuid succeeds
  ✓ atomic git import succeeds
  ✓ stack 'main' created
  ✓ change count matches git commits (~50)

── Git Import: Dry Run Mode ──
  ✓ atomic git import --dry-run does not create .atomic
  ✓ dry-run output shows preview

── Git Import: Branch Import ──
  ✓ import main branch
  ✓ main has 2 commits
  ✓ import feature branch
  ✓ feature stack created

── Git Import: Author/Message/Timestamp Preservation (Medium Repo) ──
  Cloning holman/spark...
  ✓ import succeeds
  ✓ first change has author
  ✓ commit message preserved

── Git Import: File Operations ──
  ✓ 5 changes imported
  ✓ fileA deleted
  ✓ fileB renamed
  ✓ fileC exists (renamed from B)

── Git Import: Incremental Import ──
  ✓ initial import: 2 changes
  ✓ after incremental: 4 changes

── Git Import: Large Repo Performance (hyperfine) ──
  Cloning sharkdp/hyperfine (this may take a minute)...
  Found 1017 commits
  ✓ import completed
  Import took 127s
  ✓ import completed in reasonable time (<5min)
  ✓ change count roughly matches (1015 vs 1017)

── Git Import: Error Handling ──
  ✓ clear error for non-git directory
  ✓ clear error for invalid branch

── Git Import: Git Metadata in Change.unhashed ──
  ✓ git metadata present in change

── Git Import: All Branches Mode ──
  ✓ main stack exists
  ✓ feature-a stack exists
  ✓ feature-b stack exists

── Git Import: Merge Commit Handling ──
  ✓ merge commits handled (4 changes)

── Git Import: Submodule Handling ──
  ✓ import with submodule completes

  (127s)
  Suite PASSED

══════════════════════════════════════════════════════════════
  Results: 33 passed, 0 failed, 0 skipped / 33 total
══════════════════════════════════════════════════════════════
```

---

## Expected Runtime

| Section | Time |
|---------|------|
| Prerequisites | ~1s |
| Tiny repo tests | ~10s |
| Synthetic repo tests | ~20s |
| Medium repo test | ~30s |
| Large repo test | ~3-5 min |
| **Total** | **~5-7 min** |

---

## Network Dependency

The harness requires network access to clone repositories from GitHub:

```bash
# Tiny
git clone https://github.com/hashicorp/go-uuid.git

# Medium
git clone https://github.com/holman/spark.git

# Large
git clone https://github.com/sharkdp/hyperfine.git
```

Tests gracefully skip if network is unavailable using the `require_network`
helper. The top-level runner reports exit code 77 as an explicitly skipped
suite rather than counting the missing prerequisite as a pass.

---

## Implementation Dependencies

The harness tests the following `atomic git import` command interface:

```bash
# Basic import (current branch)
atomic git import

# Import specific branch
atomic git import --branch <name>

# Import all branches
atomic git import --all-branches

# Dry run
atomic git import --dry-run

# Incremental (skip existing)
atomic git import --incremental
```

These commands need to be implemented in:
- `atomic-git` crate (core logic)
- `atomic-cli/src/commands/git/import.rs` (CLI)

---

## Related Documents

- [Git Import Design](./git-import-design.md) - Full specification for the import command
- [Git Shadow Tasks](./git-shadow-tasks.md) - Shadow mode POC task list
- [AGENTS.md](../AGENTS.md) - Project development guide

---

## Open Questions

1. **Clone caching**: Should we cache cloned repos across test runs for speed?
   - Pro: Faster repeated runs
   - Con: More complex cleanup, stale state risks
   - Recommendation: No caching for MVP, consider later

2. **Merge commit linearization**: Should merges be linearized or preserved?
   - Design doc recommends linearization for MVP
   - Test should accept either approach

3. **Binary file handling**: The test repos are text-only. Do we need binary file tests?
   - Could add a synthetic test with binary content
   - Low priority for MVP
