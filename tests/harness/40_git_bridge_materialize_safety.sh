#!/usr/bin/env bash
# 40_git_bridge_materialize_safety.sh — Offline bridge-switch collision safety.
#
# Exercises pre-materialization refusal for untracked path-shape collisions and
# the explicit before-materialize failpoint. Mid-write rollback remains outside
# this test until bridge switching has an operation journal.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 40_git_bridge_materialize_safety${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

begin_section "Prerequisites"
require_git

atomic_current_view() {
    atomic view list 2>/dev/null | awk '/^\*/ { print $2 }'
}

checkpoint_hash() {
    shasum -a 256 .atomic/bridge/workspace.json | awk '{ print $1 }'
}

snapshot_complete_files() {
    local destination="$1"
    : > "$destination"
    find . \
        \( -path './.git' -o -path './.atomic' \) -prune -o \
        -print | LC_ALL=C sort | while IFS= read -r path; do
        if [[ -f "$path" ]]; then
            printf 'file %s ' "${path#./}" >> "$destination"
            shasum -a 256 "$path" | awk '{ print $1 }' >> "$destination"
        elif [[ -d "$path" ]]; then
            printf 'dir %s\n' "${path#./}" >> "$destination"
        elif [[ -L "$path" ]]; then
            printf 'link %s %s\n' "${path#./}" "$(readlink "$path")" >> "$destination"
        else
            printf 'other %s\n' "${path#./}" >> "$destination"
        fi
    done
}

assert_clean_statuses() {
    local label="$1"
    local git_status atomic_status
    git_status="$(git status --short)"
    atomic_status="$(atomic status --short 2>/dev/null || true)"
    if [[ -z "$git_status" ]]; then
        _pass "$label: Git status is clean"
    else
        _fail "$label: Git status is clean" "$git_status"
    fi
    if [[ -z "$atomic_status" ]]; then
        _pass "$label: Atomic status is clean"
    else
        _fail "$label: Atomic status is clean" "$atomic_status"
    fi
}

assert_aligned() {
    local label="$1"
    local expected="$2"
    if [[ "$(git_current_branch)" == "$expected" && "$(atomic_current_view)" == "$expected" ]]; then
        _pass "$label"
    else
        _fail "$label" "expected $expected/$expected, got $(git_current_branch)/$(atomic_current_view)"
    fi
}

assert_failed_switch_is_unchanged() {
    local label="$1"
    local expected_diagnostic="$2"
    shift 2
    local before_manifest before_head before_branch before_view before_checkpoint
    local after_manifest output rc

    before_manifest="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-safety-before-XXXXXX")"
    after_manifest="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-safety-after-XXXXXX")"
    _HARNESS_TMPDIRS+=("$before_manifest" "$after_manifest")
    snapshot_complete_files "$before_manifest"
    before_head="$(git_head_sha_full)"
    before_branch="$(git_current_branch)"
    before_view="$(atomic_current_view)"
    before_checkpoint="$(checkpoint_hash)"

    set +e
    output="$("$@" 2>&1)"
    rc=$?
    set -e

    if [[ "$rc" -ne 0 ]]; then
        _pass "$label: command fails"
    else
        _fail "$label: command fails" "command unexpectedly succeeded"
    fi
    if echo "$output" | grep -qiE "$expected_diagnostic"; then
        _pass "$label: diagnostic names the refusal"
    else
        _fail "$label: diagnostic names the refusal" "$output"
    fi
    if [[ "$(git_head_sha_full)" == "$before_head" && "$(git_current_branch)" == "$before_branch" ]]; then
        _pass "$label: Git HEAD and branch are unchanged"
    else
        _fail "$label: Git HEAD and branch are unchanged" \
            "expected $before_branch at $before_head, got $(git_current_branch) at $(git_head_sha_full)"
    fi
    if [[ "$(atomic_current_view)" == "$before_view" ]]; then
        _pass "$label: Atomic view is unchanged"
    else
        _fail "$label: Atomic view is unchanged" \
            "expected $before_view, got $(atomic_current_view)"
    fi
    if [[ "$(checkpoint_hash)" == "$before_checkpoint" ]]; then
        _pass "$label: bridge checkpoint hash is unchanged"
    else
        _fail "$label: bridge checkpoint hash is unchanged" \
            "expected $before_checkpoint, got $(checkpoint_hash)"
    fi
    snapshot_complete_files "$after_manifest"
    if cmp -s "$before_manifest" "$after_manifest"; then
        _pass "$label: complete files and path types are unchanged"
    else
        _fail "$label: complete files and path types are unchanged" \
            "$(diff -u "$before_manifest" "$after_manifest" || true)"
    fi
}

retry_switch_round_trip() {
    local label="$1"
    assert_success "$label: retry switch succeeds" atomic git bridge switch feature
    assert_aligned "$label: retry aligns Git and Atomic on feature" feature
    assert_clean_statuses "$label: feature after retry"
    assert_success "$label: bridge verify succeeds" atomic git bridge verify
    assert_success "$label: return to main succeeds" atomic git bridge switch "$MAIN"
    assert_aligned "$label: return aligns Git and Atomic on main" "$MAIN"
    assert_clean_statuses "$label: main after return"
    assert_success "$label: bridge verify succeeds after return" atomic git bridge verify
}

begin_section "Create two already projected views"
make_temp_repo "git-bridge-materialize-safety"
init_git_repo
create_file "README.md" "bridge materialize safety\n"
git add README.md
git commit --quiet -m "Initial main projection"
MAIN="$(git_current_branch)"

assert_success "import main into Atomic" atomic git import --no-vault
assert_success "create feature view" atomic view create feature --draft --parent "$MAIN"
assert_success "align feature view and Git branch" atomic view switch feature --force
create_file "feature-marker.txt" "feature bridge checkpoint\n"
git add feature-marker.txt
git commit --quiet -m "Establish feature bridge checkpoint"
assert_success "establish projected feature checkpoint" atomic git bridge reconcile
create_file "private-collision.txt" "tracked feature private path\n"
create_file "nested-collision/child.txt" "tracked nested feature path\n"
create_file "directory-collision" "tracked feature regular file\n"
assert_success "add feature collision targets to Atomic" \
    atomic add private-collision.txt nested-collision/child.txt directory-collision
assert_success "record feature collision targets in Atomic" \
    atomic record -m "Feature collision targets"
assert_success "project feature into Git" atomic git bridge reconcile
assert_clean_statuses "projected feature"
assert_success "verify projected feature" atomic git bridge verify
assert_success "switch projected feature to main" atomic git bridge switch "$MAIN"
assert_aligned "main is the starting bridge projection" "$MAIN"
printf '%s\n' \
    'private-collision.txt' \
    'nested-collision' \
    'directory-collision' > .atomicignore
printf '%s\n' \
    'private-collision.txt' \
    'nested-collision' \
    'directory-collision/' >> .git/info/exclude
assert_success "record private collision ignores on main" \
    atomic record -m "Configure private bridge paths"
assert_success "checkpoint private collision ignores on main" atomic git bridge reconcile
assert_clean_statuses "projected main"
assert_success "verify projected main" atomic git bridge verify

begin_section "Ignored private regular file collides with target tracked file"
create_file "private-collision.txt" "private untracked content must survive refusal\n"
assert_failed_switch_is_unchanged \
    "private regular-file collision" 'collision|untracked|private-collision' \
    atomic git bridge switch feature
rm private-collision.txt
retry_switch_round_trip "private regular-file collision"

begin_section "Untracked parent file blocks target nested file"
create_file "nested-collision" "untracked parent file must survive refusal\n"
assert_failed_switch_is_unchanged \
    "nested parent-file collision" 'collision|untracked|nested-collision|parent' \
    atomic git bridge switch feature
rm nested-collision
retry_switch_round_trip "nested parent-file collision"

begin_section "Untracked directory collides with target tracked regular file"
create_file "directory-collision/private.txt" "untracked directory content must survive refusal\n"
assert_failed_switch_is_unchanged \
    "directory-versus-file collision" 'collision|untracked|directory-collision|directory' \
    atomic git bridge switch feature
rm -rf directory-collision
retry_switch_round_trip "directory-versus-file collision"

begin_section "Explicit failure before materialization"
assert_failed_switch_is_unchanged \
    "before-materialize failpoint" 'failpoint|before.materializ|ATOMIC_TEST_BRIDGE_FAIL_BEFORE_MATERIALIZE' \
    env ATOMIC_TEST_BRIDGE_FAIL_BEFORE_MATERIALIZE=1 "$ATOMIC_BIN" git bridge switch feature
retry_switch_round_trip "before-materialize failpoint"

begin_section "Mid-write rollback coverage"
_pass "numbered operation-recovery suite owns the effect-to-receipt crash window"
echo "  ${YELLOW}NOTE:${RESET} 43_git_bridge_operation_recovery.sh exercises partial tracked removal, rollback, and ordinary retry."

print_summary
