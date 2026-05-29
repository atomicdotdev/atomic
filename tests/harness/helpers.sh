#!/usr/bin/env bash
# helpers.sh — Shared utilities for the Atomic CLI test harness.
#
# Source this file at the top of every test script:
#
#   HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
#   source "$HARNESS_DIR/helpers.sh"
#
# It provides:
#   - Coloured pass / fail / skip output
#   - Temporary directory management (auto-cleaned on exit)
#   - Helper functions for common atomic operations
#   - Assertion primitives

set -euo pipefail

# ── Colours ─────────────────────────────────────────────────────────────────

if [[ -t 1 ]]; then
    RED=$'\033[0;31m'
    GREEN=$'\033[0;32m'
    YELLOW=$'\033[0;33m'
    CYAN=$'\033[0;36m'
    BOLD=$'\033[1m'
    RESET=$'\033[0m'
else
    RED="" GREEN="" YELLOW="" CYAN="" BOLD="" RESET=""
fi

# ── Counters ────────────────────────────────────────────────────────────────

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0
TESTS_SKIPPED=0
FAIL_MESSAGES=()

# ── Binary discovery ────────────────────────────────────────────────────────

# Allow overriding via $ATOMIC_BIN; otherwise discover the best available binary.
# Prefer release (60% faster on git import and large operations).
if [[ -z "${ATOMIC_BIN:-}" ]]; then
    REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

    # Prefer release binary — significantly faster for large test suites
    if [[ -x "$REPO_ROOT/target/release/atomic" ]]; then
        ATOMIC_BIN="$REPO_ROOT/target/release/atomic"
    elif [[ -x "$REPO_ROOT/target/debug/atomic" ]]; then
        ATOMIC_BIN="$REPO_ROOT/target/debug/atomic"
    else
        echo "${YELLOW}Building atomic binary …${RESET}" >&2
        (cd "$REPO_ROOT" && cargo build -p atomic-cli --quiet 2>/dev/null) || true
        if [[ -x "$REPO_ROOT/target/debug/atomic" ]]; then
            ATOMIC_BIN="$REPO_ROOT/target/debug/atomic"
        else
            echo "${RED}FATAL: could not find or build the atomic binary${RESET}" >&2
            exit 1
        fi
    fi
fi

export ATOMIC_BIN

# Show which binary is being used (helps catch stale-binary issues)
echo "${CYAN}Using: $ATOMIC_BIN${RESET}" >&2

# Convenience wrapper so tests can just write `atomic …`
atomic() {
    "$ATOMIC_BIN" "$@"
}

# ── Temp-dir management ────────────────────────────────────────────────────

# Global list of temp dirs to clean up.
_HARNESS_TMPDIRS=()

cleanup_tempdirs() {
    # ${arr[@]+...} guards against "unbound variable" on bash <4.4 when the
    # array is empty and `set -u` (nounset) is active.
    for d in ${_HARNESS_TMPDIRS[@]+"${_HARNESS_TMPDIRS[@]}"}; do
        rm -rf "$d" 2>/dev/null || true
    done
}
trap cleanup_tempdirs EXIT

# Create a fresh temp directory and cd into it.
# Usage:  make_temp_repo [name]
#   name — optional human-readable label (used in the dir name)
#
# Sets REPO_DIR to the absolute path.
make_temp_repo() {
    local label="${1:-test}"
    REPO_DIR="$(mktemp -d "${TMPDIR:-/tmp}/atomic-test-${label}-XXXXXX")"
    _HARNESS_TMPDIRS+=("$REPO_DIR")
    cd "$REPO_DIR"
}

# ── Repo helpers ────────────────────────────────────────────────────────────

# Initialise an atomic repository in the current directory.
# Accepts optional arguments forwarded to `atomic init`.
init_repo() {
    atomic init "$@" >/dev/null 2>&1
}

# Create a file with given content.  Parent dirs are created automatically.
# Usage:  create_file <path> [content]
create_file() {
    local path="$1"
    local content="${2:-hello from $path}"
    mkdir -p "$(dirname "$path")"
    printf '%s' "$content" > "$path"
}

# Append content to an existing file.
append_file() {
    local path="$1"
    local content="${2:-\nappended content}"
    printf '%s' "$content" >> "$path"
}

# Overwrite a file with new content.
overwrite_file() {
    local path="$1"
    local content="${2:-overwritten content}"
    printf '%s' "$content" > "$path"
}

# Create a directory (without any files).
create_dir() {
    local path="$1"
    mkdir -p "$path"
}

# ── Atomic wrappers ────────────────────────────────────────────────────────

# Run `atomic status` and capture stdout.
get_status() {
    atomic status 2>/dev/null || true
}

# Run `atomic status --short` and capture stdout.
get_status_short() {
    atomic status --short 2>/dev/null || true
}

# Add one or more paths.
add_files() {
    atomic add "$@" 2>/dev/null
}

# Record with a message (and optionally extra flags).
record_change() {
    local msg="$1"; shift
    atomic record -m "$msg" "$@" 2>&1
}

# Unrecord the last change.
unrecord_last() {
    atomic unrecord 2>&1 || true
}

# Create a new view.
new_view() {
    local name="$1"; shift
    atomic view create "$name" "$@" 2>&1
}

# Switch to a view.
switch_view() {
    local name="$1"
    atomic view switch "$name" --force 2>&1
}

# Insert changes from one view to another.
insert_from_view() {
    local from="$1"
    local to="$2"
    shift 2
    atomic insert from-view "$from" --to-view "$to" "$@" 2>&1
}

# List views.
list_views() {
    atomic view list 2>/dev/null || true
}

# ── Assertions ──────────────────────────────────────────────────────────────

# Internal: record a test result.
_pass() {
    local name="$1"
    TESTS_RUN=$((TESTS_RUN + 1))
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo "  ${GREEN}✓${RESET} $name"
}

_fail() {
    local name="$1"
    local detail="${2:-}"
    TESTS_RUN=$((TESTS_RUN + 1))
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo "  ${RED}✗${RESET} $name"
    if [[ -n "$detail" ]]; then
        echo "    ${RED}→ $detail${RESET}"
    fi
    FAIL_MESSAGES+=("$name: $detail")
}

_skip() {
    local name="$1"
    local reason="${2:-}"
    TESTS_RUN=$((TESTS_RUN + 1))
    TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
    echo "  ${YELLOW}⊘${RESET} $name ${YELLOW}(skipped${reason:+: $reason})${RESET}"
}

# Assert that the last command succeeded (exit 0).
# Usage:  assert_success "description" command args...
assert_success() {
    local desc="$1"; shift
    if "$@" >/dev/null 2>&1; then
        _pass "$desc"
    else
        _fail "$desc" "command failed: $*"
    fi
}

# Assert that the last command failed (non-zero exit).
assert_failure() {
    local desc="$1"; shift
    if "$@" >/dev/null 2>&1; then
        _fail "$desc" "expected failure but command succeeded: $*"
    else
        _pass "$desc"
    fi
}

# Assert that a file exists on disk.
assert_file_exists() {
    local desc="$1"
    local path="$2"
    if [[ -f "$path" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "file does not exist: $path"
    fi
}

# Assert that a file does NOT exist on disk.
assert_file_not_exists() {
    local desc="$1"
    local path="$2"
    if [[ ! -e "$path" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "file should not exist but does: $path"
    fi
}

# Assert that a directory exists.
assert_dir_exists() {
    local desc="$1"
    local path="$2"
    if [[ -d "$path" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "directory does not exist: $path"
    fi
}

# Assert that a directory does NOT exist.
assert_dir_not_exists() {
    local desc="$1"
    local path="$2"
    if [[ ! -d "$path" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "directory should not exist but does: $path"
    fi
}

# Assert that a file's content equals the expected string.
assert_file_content() {
    local desc="$1"
    local path="$2"
    local expected="$3"
    if [[ ! -f "$path" ]]; then
        _fail "$desc" "file does not exist: $path"
        return
    fi
    local actual
    actual="$(cat "$path")"
    if [[ "$actual" == "$expected" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "expected content '$expected', got '$actual'"
    fi
}

# Assert that stdout of a command contains a substring.
# Usage:  assert_output_contains "desc" "substring" command args...
assert_output_contains() {
    local desc="$1"
    local needle="$2"
    shift 2
    local out
    out="$("$@" 2>&1)" || true
    if echo "$out" | grep -qF "$needle"; then
        _pass "$desc"
    else
        _fail "$desc" "output did not contain '$needle'. Got: $(echo "$out" | head -5)"
    fi
}

# Assert that stdout of a command does NOT contain a substring.
assert_output_not_contains() {
    local desc="$1"
    local needle="$2"
    shift 2
    local out
    out="$("$@" 2>&1)" || true
    if echo "$out" | grep -qF "$needle"; then
        _fail "$desc" "output should not contain '$needle' but does"
    else
        _pass "$desc"
    fi
}

# Assert that `atomic status` output contains a substring.
assert_status_contains() {
    local desc="$1"
    local needle="$2"
    local out
    out="$(get_status)"
    if echo "$out" | grep -qF "$needle"; then
        _pass "$desc"
    else
        _fail "$desc" "status did not contain '$needle'. Got: $(echo "$out" | head -10)"
    fi
}

# Assert that `atomic status` output does NOT contain a substring.
assert_status_not_contains() {
    local desc="$1"
    local needle="$2"
    local out
    out="$(get_status)"
    if echo "$out" | grep -qF "$needle"; then
        _fail "$desc" "status should not contain '$needle' but does"
    else
        _pass "$desc"
    fi
}

# Assert that `atomic status --short` shows a specific flag for a path.
# Usage:  assert_status_flag "desc" "M" "src/main.rs"
assert_status_flag() {
    local desc="$1"
    local flag="$2"
    local path="$3"
    local out
    out="$(get_status_short)"
    # Escape special regex characters in flag (e.g. ? → \?)
    local escaped_flag
    escaped_flag="$(printf '%s' "$flag" | sed 's/[][\\.^$*+?{}()|]/\\&/g')"
    # Escape special regex characters in path (e.g. / → \/)
    local escaped_path
    escaped_path="$(printf '%s' "$path" | sed 's/[][\\.^$*+?{}()|]/\\&/g')"
    # Short format: "<flag> <path>"  or "<flag><flag> <path>" (e.g. "??" for untracked)
    if echo "$out" | grep -qE "^${escaped_flag}+[[:space:]]+${escaped_path}"; then
        _pass "$desc"
    else
        _fail "$desc" "expected flag '$flag' for '$path'. Status: $(echo "$out" | head -10)"
    fi
}

# Assert that a path does NOT appear in `atomic status --short`.
assert_status_no_entry() {
    local desc="$1"
    local path="$2"
    local out
    out="$(get_status_short)"
    if echo "$out" | grep -qF -- "$path"; then
        _fail "$desc" "'$path' should not appear in status but does. Status: $(echo "$out" | head -10)"
    else
        _pass "$desc"
    fi
}

# Assert that working copy is clean (no modified/added/deleted/untracked).
assert_clean() {
    local desc="${1:-working copy is clean}"
    local out
    out="$(get_status)"
    # A clean status typically says "clean" or has no entries
    if echo "$out" | grep -qiE "clean|no changes|nothing to record"; then
        _pass "$desc"
    else
        # Also accept empty-ish output
        local entries
        entries="$(echo "$out" | grep -cE '^\s*[MADUC?]' || true)"
        if [[ "$entries" -eq 0 ]]; then
            _pass "$desc"
        else
            _fail "$desc" "working copy not clean. Status: $(echo "$out" | head -10)"
        fi
    fi
}

# Assert that the current view is the expected one.
assert_current_view() {
    local desc="$1"
    local expected="$2"
    local out
    out="$(list_views)"
    # The current view is usually prefixed with * or highlighted
    if echo "$out" | grep -qE "^\*?\s*${expected}(\s|$)"; then
        _pass "$desc"
    else
        # Fallback: check current_view file
        local view_file=".atomic/current_view"
        if [[ -f "$view_file" ]]; then
            local actual
            actual="$(cat "$view_file")"
            if [[ "$actual" == "$expected" ]]; then
                _pass "$desc"
                return
            fi
        fi
        _fail "$desc" "expected current view '$expected'. Views: $(echo "$out" | head -5)"
    fi
}

# Assert that a view exists.
assert_view_exists() {
    local desc="$1"
    local name="$2"
    local out
    out="$(list_views)"
    if echo "$out" | grep -qF "$name"; then
        _pass "$desc"
    else
        _fail "$desc" "view '$name' not found. Views: $(echo "$out" | head -5)"
    fi
}

# Assert the number of entries in `atomic log`.
assert_log_count() {
    local desc="$1"
    local expected="$2"
    local out
    out="$(atomic log 2>/dev/null || true)"
    local count
    count="$(echo "$out" | grep -cE '^#|^[0-9a-zA-Z]{10,}' || true)"
    if [[ "$count" -eq "$expected" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "expected $expected log entries, got $count"
    fi
}

# ── Git Helpers ─────────────────────────────────────────────────────────────

# Check if git is available
require_git() {
    if ! command -v git &>/dev/null; then
        echo "${YELLOW}SKIPPING: git not installed${RESET}"
        exit 0
    fi
}

# Check if network is available (can reach github.com)
require_network() {
    if ! curl --silent --head --max-time 5 https://github.com &>/dev/null; then
        echo "${YELLOW}SKIPPING: network unavailable${RESET}"
        exit 0
    fi
}

# Check if network is available without exiting.
# Returns 0 (true) if reachable, 1 (false) otherwise.
# Usage:  if require_network_quiet; then ... fi
require_network_quiet() {
    curl --silent --head --max-time 5 https://github.com &>/dev/null
}

# Clone a git repo to a temp directory
# Usage: clone_git_repo <url> [ref]
# Sets: GIT_REPO_DIR
clone_git_repo() {
    local url="$1"
    local ref="${2:-HEAD}"
    GIT_REPO_DIR="$(mktemp -d "${TMPDIR:-/tmp}/atomic-git-test-XXXXXX")"
    _HARNESS_TMPDIRS+=("$GIT_REPO_DIR")
    if ! git clone --quiet "$url" "$GIT_REPO_DIR"; then
        echo "${YELLOW}SKIPPING: failed to clone git repo '$url'${RESET}"
        exit 0
    fi
    if [[ "$ref" != "HEAD" ]]; then
        if ! (cd "$GIT_REPO_DIR" && git checkout --quiet "$ref"); then
            echo "${YELLOW}SKIPPING: failed to checkout ref '$ref' in git repo '$url'${RESET}"
            exit 0
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

# Get git commit count on current branch
git_commit_count() {
    git rev-list --count HEAD 2>/dev/null || echo "0"
}

# Get current git branch name
git_current_branch() {
    git branch --show-current 2>/dev/null || git rev-parse --abbrev-ref HEAD
}

# Get the default branch name (what a fresh clone checks out)
git_default_branch() {
    # After a fresh clone, the current branch is the default
    # This also works for repos with main vs master
    git symbolic-ref --short HEAD 2>/dev/null || git branch --show-current 2>/dev/null || echo "main"
}

# Get git commit SHA (short)
git_head_sha() {
    git rev-parse --short HEAD
}

# Get git commit SHA (full)
git_head_sha_full() {
    git rev-parse HEAD
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
    local log_output
    # Capture log output, count lines that look like change entries
    log_output="$(atomic log 2>/dev/null || true)"
    if [[ -z "$log_output" ]]; then
        actual=0
    else
        # Count lines matching change entry patterns (# followed by number, or hash-like strings)
        actual="$(echo "$log_output" | grep -cE '^\s*#[0-9]+|^[0-9a-f]{8,}' || true)"
        # Ensure we have a valid number
        actual="${actual:-0}"
        # Remove any whitespace/newlines
        actual="$(echo "$actual" | tr -d '[:space:]')"
    fi
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

# ── Section headers ─────────────────────────────────────────────────────────

begin_section() {
    local title="$1"
    echo ""
    echo "${BOLD}${CYAN}── $title ──${RESET}"
}

# ── Summary ─────────────────────────────────────────────────────────────────

print_summary() {
    echo ""
    echo "${BOLD}═══════════════════════════════════════════════${RESET}"
    echo "${BOLD}  Results: ${GREEN}$TESTS_PASSED passed${RESET}, " \
         "${RED}$TESTS_FAILED failed${RESET}, " \
         "${YELLOW}$TESTS_SKIPPED skipped${RESET}" \
         "/ $TESTS_RUN total"

    if [[ ${#FAIL_MESSAGES[@]} -gt 0 ]]; then
        echo ""
        echo "${RED}  Failures:${RESET}"
        for msg in "${FAIL_MESSAGES[@]}"; do
            echo "    ${RED}• $msg${RESET}"
        done
    fi

    echo "${BOLD}═══════════════════════════════════════════════${RESET}"

    if [[ $TESTS_FAILED -gt 0 ]]; then
        return 1
    fi
    return 0
}
