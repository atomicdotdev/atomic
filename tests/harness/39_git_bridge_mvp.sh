#!/usr/bin/env bash
# 39_git_bridge_mvp.sh — Offline contract test for `atomic git bridge reconcile`.
#
# The MVP imports the checked-out Git branch, aligns it with an Atomic view,
# reconciles later Git commits without disturbing either working tree, is
# idempotent, and refuses to run while either side has ordinary edits.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 39_git_bridge_mvp${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

begin_section "Prerequisites"
require_git

atomic_current_view() {
    atomic view list 2>/dev/null | awk '/^\*/ { print $2 }'
}

assert_clean_statuses() {
    local label="$1"
    local git_status atomic_status
    git_status="$(git status --short)"
    atomic_status="$(atomic status --short 2>/dev/null || true)"
    if [[ -z "$git_status" ]]; then
        _pass "$label: git status is clean"
    else
        _fail "$label: git status is clean" "$git_status"
    fi
    if [[ -z "$atomic_status" ]]; then
        _pass "$label: atomic status is clean"
    else
        _fail "$label: atomic status is clean" "$atomic_status"
    fi
}

assert_complete_paths_match() {
    local label="$1"
    local git_paths disk_paths
    git_paths="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-git-paths-XXXXXX")"
    disk_paths="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-disk-paths-XXXXXX")"
    _HARNESS_TMPDIRS+=("$git_paths" "$disk_paths")
    git ls-tree -r --name-only HEAD | LC_ALL=C sort > "$git_paths"
    find . -type f \
        ! -path './.git/*' \
        ! -path './.atomic/*' \
        ! -path './.vault/*' \
        ! -path './.atomicignore' \
        -print | sed 's#^./##' | LC_ALL=C sort > "$disk_paths"
    if cmp -s "$git_paths" "$disk_paths"; then
        _pass "$label"
    else
        _fail "$label" "$(diff -u "$git_paths" "$disk_paths" || true)"
    fi
}

begin_section "Initial import and feature reconciliation"
make_temp_repo "git-bridge-mvp"
init_git_repo
create_file "README.md" "bridge mvp\n"
create_file "src/nested/base.txt" "base\n"
: > "tracked-empty.txt"
git add README.md src/nested/base.txt tracked-empty.txt
git commit --quiet -m "Initial"

MAIN="$(git_current_branch)"
assert_success "initial git import" atomic git import --no-vault
assert_view_exists "initial branch has matching Atomic view" "$MAIN"
assert_clean_statuses "after initial import"

assert_success "create matching Atomic feature view" \
    atomic view create feature --draft --parent "$MAIN"
assert_success "switch to aligned Atomic feature view" atomic view switch feature --force
if [[ "$(git_current_branch)" == "feature" ]]; then
    _pass "Atomic view switch aligns Git on feature branch"
else
    _fail "Atomic view switch aligns Git on feature branch" \
        "expected feature, got $(git_current_branch)"
fi
create_file "src/nested/feature.txt" "feature\n"
: > feature-empty.txt
overwrite_file "README.md" "bridge mvp\nfeature\n"
git add README.md src/nested/feature.txt feature-empty.txt
git commit --quiet -m "Feature commit"
FEATURE_HEAD="$(git_head_sha_full)"

set +e
RECONCILE_OUT="$(atomic git bridge reconcile 2>&1)"
RECONCILE_RC=$?
set -e
if [[ "$RECONCILE_RC" -eq 0 ]]; then
    _pass "bridge reconcile imports the feature commit"
else
    _fail "bridge reconcile imports the feature commit" "exit $RECONCILE_RC: $RECONCILE_OUT"
fi

assert_view_exists "feature branch has matching Atomic view" "feature"
CURRENT_VIEW="$(atomic_current_view)"
if [[ "$CURRENT_VIEW" == "feature" ]]; then
    _pass "checked-out Git branch aligns with current Atomic view"
else
    _fail "checked-out Git branch aligns with current Atomic view" \
        "expected feature, got '${CURRENT_VIEW:-<unknown>}'"
fi
if [[ "$(git_head_sha_full)" == "$FEATURE_HEAD" ]]; then
    _pass "reconcile preserves Git HEAD"
else
    _fail "reconcile preserves Git HEAD" "expected $FEATURE_HEAD, got $(git_head_sha_full)"
fi
assert_file_content "reconciled feature content is present" "src/nested/feature.txt" "feature\n"
if [[ -f feature-empty.txt && ! -s feature-empty.txt ]]; then
    _pass "feature-local zero-byte file is present after reconcile"
else
    _fail "feature-local zero-byte file is present after reconcile" \
        "feature-empty.txt missing or non-empty"
fi
assert_complete_paths_match "all Git HEAD paths exactly match working-tree paths"
assert_success "explicit bridge verify proves Git/Atomic equality" atomic git bridge verify
if [[ -s .atomic/bridge/workspace.json ]]; then
    _pass "reconcile writes a non-empty local workspace checkpoint"
else
    _fail "reconcile writes a non-empty local workspace checkpoint" ".atomic/bridge/workspace.json missing or empty"
fi
assert_clean_statuses "after feature reconcile"

if [[ -f tracked-empty.txt && ! -s tracked-empty.txt ]]; then
    _pass "tracked zero-byte file exists and remains empty"
else
    _fail "tracked zero-byte file exists and remains empty" \
        "known bridge/import empty-file fidelity bug: missing or non-empty tracked-empty.txt"
fi

begin_section "Idempotence"
BEFORE_SECOND_HEAD="$(git_head_sha_full)"
BEFORE_SECOND_COUNT="$(git_commit_count)"
set +e
SECOND_OUT="$(atomic git bridge reconcile 2>&1)"
SECOND_RC=$?
set -e
if [[ "$SECOND_RC" -eq 0 ]]; then
    _pass "second reconcile succeeds"
else
    _fail "second reconcile succeeds" "exit $SECOND_RC: $SECOND_OUT"
fi
if [[ "$(git_head_sha_full)" == "$BEFORE_SECOND_HEAD" && "$(git_commit_count)" == "$BEFORE_SECOND_COUNT" ]]; then
    _pass "second reconcile is idempotent"
else
    _fail "second reconcile is idempotent" "Git history changed"
fi
assert_clean_statuses "after second reconcile"

begin_section "Atomic-origin reconciliation"
overwrite_file "README.md" "bridge mvp\nfeature\natomic origin\n"
create_file "src/nested/atomic-origin.txt" "recorded by Atomic\n"
assert_success "add Atomic-origin file" atomic add src/nested/atomic-origin.txt
assert_success "record Atomic-origin changes" atomic record -m "Atomic-origin bridge change"

if [[ -z "$(atomic status --short 2>/dev/null || true)" ]]; then
    _pass "Atomic-origin record leaves Atomic clean"
else
    _fail "Atomic-origin record leaves Atomic clean" "$(atomic status --short 2>/dev/null || true)"
fi
if [[ -n "$(git status --short)" ]]; then
    _pass "Atomic-origin record is dirty to Git before reconcile"
else
    _fail "Atomic-origin record is dirty to Git before reconcile" "git status was clean"
fi

ATOMIC_ORIGIN_OLD_HEAD="$(git_head_sha_full)"
assert_success "reconcile exports Atomic-origin record to Git" atomic git bridge reconcile
ATOMIC_ORIGIN_NEW_HEAD="$(git_head_sha_full)"
if [[ "$ATOMIC_ORIGIN_NEW_HEAD" != "$ATOMIC_ORIGIN_OLD_HEAD" ]]; then
    _pass "Atomic-origin reconcile advances Git HEAD"
else
    _fail "Atomic-origin reconcile advances Git HEAD" "Git HEAD remained $ATOMIC_ORIGIN_OLD_HEAD"
fi
assert_clean_statuses "after Atomic-origin reconcile"
assert_success "bridge verify succeeds after Atomic-origin reconcile" atomic git bridge verify
ATOMIC_ORIGIN_BLOB="$(mktemp "${TMPDIR:-/tmp}/atomic-origin-blob-XXXXXX")"
_HARNESS_TMPDIRS+=("$ATOMIC_ORIGIN_BLOB")
git show HEAD:src/nested/atomic-origin.txt > "$ATOMIC_ORIGIN_BLOB"
if cmp -s "$ATOMIC_ORIGIN_BLOB" src/nested/atomic-origin.txt; then
    _pass "Git blob content equals the Atomic-origin file on disk"
else
    _fail "Git blob content equals the Atomic-origin file on disk" \
        "$(diff -u "$ATOMIC_ORIGIN_BLOB" src/nested/atomic-origin.txt || true)"
fi

ATOMIC_ORIGIN_SECOND_HEAD="$(git_head_sha_full)"
ATOMIC_ORIGIN_SECOND_COUNT="$(git_commit_count)"
assert_success "second Atomic-origin reconcile succeeds" atomic git bridge reconcile
if [[ "$(git_head_sha_full)" == "$ATOMIC_ORIGIN_SECOND_HEAD" && \
      "$(git_commit_count)" == "$ATOMIC_ORIGIN_SECOND_COUNT" ]]; then
    _pass "second Atomic-origin reconcile creates no Git commit"
else
    _fail "second Atomic-origin reconcile creates no Git commit" "Git history changed"
fi
assert_clean_statuses "after second Atomic-origin reconcile"

begin_section "Dirty working tree refusal is non-destructive"
append_file "README.md" "ordinary edit\n"
DIRTY_CONTENT="$(cat README.md)"
DIRTY_HEAD="$(git_head_sha_full)"

if [[ -n "$(git status --short)" ]]; then
    _pass "ordinary edit is visible to Git"
else
    _fail "ordinary edit is visible to Git" "git status was clean"
fi
if [[ -n "$(atomic status --short 2>/dev/null || true)" ]]; then
    _pass "ordinary edit is visible to Atomic"
else
    _fail "ordinary edit is visible to Atomic" "atomic status was clean"
fi

set +e
DIRTY_OUT="$(atomic git bridge reconcile 2>&1)"
DIRTY_RC=$?
set -e
if [[ "$DIRTY_RC" -ne 0 ]]; then
    _pass "reconcile refuses a dirty working tree"
else
    _fail "reconcile refuses a dirty working tree" "command unexpectedly succeeded"
fi
if [[ "$(git_head_sha_full)" == "$DIRTY_HEAD" ]]; then
    _pass "dirty refusal does not change Git HEAD"
else
    _fail "dirty refusal does not change Git HEAD" "expected $DIRTY_HEAD, got $(git_head_sha_full)"
fi
if [[ "$(cat README.md)" == "$DIRTY_CONTENT" ]]; then
    _pass "dirty refusal does not change file content"
else
    _fail "dirty refusal does not change file content" "README.md was modified"
fi
if echo "$DIRTY_OUT" | grep -qiE 'dirty|uncommitted|working (copy|tree)|status'; then
    _pass "dirty refusal explains why reconciliation stopped"
else
    _fail "dirty refusal explains why reconciliation stopped" "$DIRTY_OUT"
fi

git checkout -- README.md
assert_clean_statuses "after restoring dirty-refusal edit"

begin_section "Bridge switch materializes exact Atomic view state"

snapshot_materialized_state() {
    local paths_file="$1"
    local hashes_file="$2"
    find . -type f \
        ! -path './.git/*' \
        ! -path './.atomic/*' \
        ! -path './.vault/*' \
        ! -path './.atomicignore' \
        -print | sed 's#^./##' | LC_ALL=C sort > "$paths_file"
    : > "$hashes_file"
    while IFS= read -r path; do
        shasum -a 256 "$path" >> "$hashes_file"
    done < "$paths_file"
}

snapshot_git_rendered_state() {
    local paths_file="$1"
    local hashes_file="$2"
    local metadata_file="$3"
    local path metadata
    git ls-tree -r --name-only HEAD | LC_ALL=C sort > "$paths_file"
    : > "$hashes_file"
    : > "$metadata_file"
    while IFS= read -r path; do
        shasum -a 256 "$path" >> "$hashes_file"
        if metadata="$(stat -f '%m %i' "$path" 2>/dev/null)" || \
           metadata="$(stat -c '%Y %i' "$path" 2>/dev/null)"; then
            printf '%s  %s\n' "$metadata" "$path" >> "$metadata_file"
        fi
    done < "$paths_file"
}

FEATURE_PATHS="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-feature-paths-XXXXXX")"
FEATURE_HASHES="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-feature-hashes-XXXXXX")"
MAIN_PATHS="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-main-paths-XXXXXX")"
MAIN_HASHES="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-main-hashes-XXXXXX")"
CURRENT_PATHS="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-current-paths-XXXXXX")"
CURRENT_HASHES="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-current-hashes-XXXXXX")"
_HARNESS_TMPDIRS+=(
    "$FEATURE_PATHS" "$FEATURE_HASHES" "$MAIN_PATHS"
    "$MAIN_HASHES" "$CURRENT_PATHS" "$CURRENT_HASHES"
)

rm src/nested/base.txt
assert_success "record feature deletion for materialization coverage" \
    atomic record -m "Delete base file on feature"
set +e
FEATURE_DELETE_OUT="$(atomic git bridge reconcile 2>&1)"
FEATURE_DELETE_RC=$?
set -e
if [[ "$FEATURE_DELETE_RC" -eq 0 ]]; then
    _pass "project feature deletion to Git"
else
    _fail "project feature deletion to Git" "exit $FEATURE_DELETE_RC: $FEATURE_DELETE_OUT"
fi
assert_clean_statuses "before bridge switch materialization checks"

SENTINEL_CONTENT="untracked bridge switch sentinel"
printf '%s\n' "$SENTINEL_CONTENT" > .atomic/bridge/sentinel.txt
snapshot_materialized_state "$FEATURE_PATHS" "$FEATURE_HASHES"

append_file "README.md" "dirty switch refusal\n"
DIRTY_SWITCH_HEAD="$(git_head_sha_full)"
DIRTY_SWITCH_VIEW="$(atomic_current_view)"
DIRTY_SWITCH_README="$(cat README.md)"
DIRTY_SWITCH_PATHS="$(cat "$FEATURE_PATHS")"
set +e
DIRTY_SWITCH_OUT="$(atomic git bridge switch "$MAIN" 2>&1)"
DIRTY_SWITCH_RC=$?
set -e
if [[ "$DIRTY_SWITCH_RC" -ne 0 ]]; then
    _pass "bridge switch refuses a dirty tracked edit"
else
    _fail "bridge switch refuses a dirty tracked edit" "command unexpectedly succeeded"
fi
if [[ "$(git_head_sha_full)" == "$DIRTY_SWITCH_HEAD" && \
      "$(atomic_current_view)" == "$DIRTY_SWITCH_VIEW" ]]; then
    _pass "dirty switch refusal preserves Git HEAD and Atomic view"
else
    _fail "dirty switch refusal preserves Git HEAD and Atomic view" \
        "HEAD/view changed to $(git_head_sha_full)/$(atomic_current_view)"
fi
snapshot_materialized_state "$CURRENT_PATHS" "$CURRENT_HASHES"
if [[ "$(cat README.md)" == "$DIRTY_SWITCH_README" && \
      "$(cat "$CURRENT_PATHS")" == "$DIRTY_SWITCH_PATHS" && \
      "$(cat .atomic/bridge/sentinel.txt)" == "$SENTINEL_CONTENT" ]]; then
    _pass "dirty switch refusal preserves files and untracked sentinel"
else
    _fail "dirty switch refusal preserves files and untracked sentinel" "$DIRTY_SWITCH_OUT"
fi
git checkout -- README.md
assert_clean_statuses "after restoring dirty bridge-switch edit"

assert_success "bridge switch feature to main" atomic git bridge switch "$MAIN"
if [[ "$(git_current_branch)" == "$MAIN" && "$(atomic_current_view)" == "$MAIN" ]]; then
    _pass "main Git branch and Atomic view are aligned"
else
    _fail "main Git branch and Atomic view are aligned" \
        "got $(git_current_branch)/$(atomic_current_view)"
fi
assert_file_content "main README content is exact" README.md "bridge mvp\n"
assert_file_content "main nested base file is restored" src/nested/base.txt "base\n"
assert_file_exists "main zero-byte path is present" tracked-empty.txt
if [[ ! -s tracked-empty.txt ]]; then
    _pass "main zero-byte file remains empty"
else
    _fail "main zero-byte file remains empty" "tracked-empty.txt is non-empty"
fi
assert_file_not_exists "feature-added path is absent on main" src/nested/feature.txt
assert_file_not_exists "feature-local zero-byte path is absent on main" feature-empty.txt
assert_file_not_exists "Atomic-origin path is absent on main" src/nested/atomic-origin.txt
assert_complete_paths_match "main materialization has exact projected path list"
assert_clean_statuses "after bridge switch to main"
assert_success "bridge verify succeeds on main materialization" atomic git bridge verify
snapshot_materialized_state "$MAIN_PATHS" "$MAIN_HASHES"

assert_success "bridge switch main to feature" atomic git bridge switch feature
if [[ "$(git_current_branch)" == "feature" && "$(atomic_current_view)" == "feature" ]]; then
    _pass "feature Git branch and Atomic view are aligned"
else
    _fail "feature Git branch and Atomic view are aligned" \
        "got $(git_current_branch)/$(atomic_current_view)"
fi
assert_file_content "feature README content is exact" README.md \
    "bridge mvp\nfeature\natomic origin\n"
assert_file_content "feature added file content is exact" src/nested/feature.txt "feature\n"
assert_file_content "feature nested Atomic-origin content is exact" \
    src/nested/atomic-origin.txt "recorded by Atomic\n"
assert_file_not_exists "feature-deleted base path remains absent" src/nested/base.txt
if [[ -f tracked-empty.txt && ! -s tracked-empty.txt ]]; then
    _pass "feature zero-byte file is present and empty"
else
    _fail "feature zero-byte file is present and empty" "tracked-empty.txt missing or non-empty"
fi
if [[ -f feature-empty.txt && ! -s feature-empty.txt ]]; then
    _pass "selected-path switch recreates feature-local zero-byte file"
else
    _fail "selected-path switch recreates feature-local zero-byte file" \
        "feature-empty.txt missing or non-empty"
fi
snapshot_materialized_state "$CURRENT_PATHS" "$CURRENT_HASHES"
if cmp -s "$FEATURE_PATHS" "$CURRENT_PATHS" && cmp -s "$FEATURE_HASHES" "$CURRENT_HASHES"; then
    _pass "feature path list and content hashes survive first round trip"
else
    _fail "feature path list and content hashes survive first round trip" \
        "$(diff -u "$FEATURE_PATHS" "$CURRENT_PATHS" || true)"
fi
assert_complete_paths_match "feature materialization has exact projected path list"
assert_clean_statuses "after first bridge switch to feature"
assert_success "bridge verify succeeds on feature materialization" atomic git bridge verify

set +e
SECOND_MAIN_OUT="$(atomic git bridge switch "$MAIN" 2>&1)"
SECOND_MAIN_RC=$?
set -e
if [[ "$SECOND_MAIN_RC" -eq 0 ]]; then
    _pass "bridge switch feature back to main"
else
    _fail "bridge switch feature back to main" "exit $SECOND_MAIN_RC: $SECOND_MAIN_OUT"
fi
snapshot_materialized_state "$CURRENT_PATHS" "$CURRENT_HASHES"
if cmp -s "$MAIN_PATHS" "$CURRENT_PATHS" && cmp -s "$MAIN_HASHES" "$CURRENT_HASHES"; then
    _pass "main path list and content hashes survive second switch"
else
    _fail "main path list and content hashes survive second switch" \
        "$(diff -u "$MAIN_PATHS" "$CURRENT_PATHS" || true)"
fi
assert_clean_statuses "after second bridge switch to main"
assert_success "bridge verify succeeds after second main materialization" atomic git bridge verify

assert_success "bridge switch main back to feature" atomic git bridge switch feature
snapshot_materialized_state "$CURRENT_PATHS" "$CURRENT_HASHES"
if cmp -s "$FEATURE_PATHS" "$CURRENT_PATHS" && cmp -s "$FEATURE_HASHES" "$CURRENT_HASHES"; then
    _pass "feature path list and content hashes survive second round trip"
else
    _fail "feature path list and content hashes survive second round trip" \
        "$(diff -u "$FEATURE_PATHS" "$CURRENT_PATHS" || true)"
fi
if [[ "$(cat .atomic/bridge/sentinel.txt)" == "$SENTINEL_CONTENT" ]]; then
    _pass "untracked sentinel survives every bridge switch"
else
    _fail "untracked sentinel survives every bridge switch" ".atomic/bridge/sentinel.txt changed"
fi
if [[ "$(git_current_branch)" == "feature" && "$(atomic_current_view)" == "feature" ]]; then
    _pass "final Git branch and Atomic view are aligned on feature"
else
    _fail "final Git branch and Atomic view are aligned on feature" \
        "got $(git_current_branch)/$(atomic_current_view)"
fi
assert_clean_statuses "after repeated bridge switching"
assert_success "bridge verify succeeds after repeated switching" atomic git bridge verify

begin_section "Raw Git switch adoption does not rematerialize"

ADOPTION_PATHS_BEFORE="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-adoption-paths-before-XXXXXX")"
ADOPTION_HASHES_BEFORE="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-adoption-hashes-before-XXXXXX")"
ADOPTION_METADATA_BEFORE="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-adoption-metadata-before-XXXXXX")"
ADOPTION_PATHS_AFTER="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-adoption-paths-after-XXXXXX")"
ADOPTION_HASHES_AFTER="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-adoption-hashes-after-XXXXXX")"
ADOPTION_METADATA_AFTER="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-adoption-metadata-after-XXXXXX")"
_HARNESS_TMPDIRS+=(
    "$ADOPTION_PATHS_BEFORE" "$ADOPTION_HASHES_BEFORE" "$ADOPTION_METADATA_BEFORE"
    "$ADOPTION_PATHS_AFTER" "$ADOPTION_HASHES_AFTER" "$ADOPTION_METADATA_AFTER"
)

assert_success "raw Git switch feature to main" git switch "$MAIN"
if [[ -z "$(git status --short)" ]]; then
    _pass "raw Git switch to main leaves Git clean"
else
    _fail "raw Git switch to main leaves Git clean" "$(git status --short)"
fi
if [[ "$(atomic_current_view)" == "feature" ]]; then
    _pass "Atomic view remains feature before adopting raw Git main switch"
else
    _fail "Atomic view remains feature before adopting raw Git main switch" \
        "got $(atomic_current_view)"
fi
snapshot_git_rendered_state \
    "$ADOPTION_PATHS_BEFORE" "$ADOPTION_HASHES_BEFORE" "$ADOPTION_METADATA_BEFORE"
if cmp -s "$MAIN_PATHS" "$ADOPTION_PATHS_BEFORE" && \
   cmp -s "$MAIN_HASHES" "$ADOPTION_HASHES_BEFORE"; then
    _pass "raw Git switch renders expected complete main content"
else
    _fail "raw Git switch renders expected complete main content" \
        "$(diff -u "$MAIN_PATHS" "$ADOPTION_PATHS_BEFORE" || true)"
fi
sleep 1
assert_success "reconcile adopts raw Git switch to main" atomic git bridge reconcile
snapshot_git_rendered_state \
    "$ADOPTION_PATHS_AFTER" "$ADOPTION_HASHES_AFTER" "$ADOPTION_METADATA_AFTER"
if [[ "$(atomic_current_view)" == "$MAIN" ]]; then
    _pass "reconcile adopts main as the current Atomic view"
else
    _fail "reconcile adopts main as the current Atomic view" "got $(atomic_current_view)"
fi
assert_clean_statuses "after adopting raw Git switch to main"
assert_success "bridge verify succeeds after adopting main" atomic git bridge verify
assert_complete_paths_match "adopted main has the complete expected path list"
if cmp -s "$MAIN_PATHS" "$ADOPTION_PATHS_AFTER" && \
   cmp -s "$MAIN_HASHES" "$ADOPTION_HASHES_AFTER"; then
    _pass "adopted main content equals expected main"
else
    _fail "adopted main content equals expected main" \
        "$(diff -u "$MAIN_HASHES" "$ADOPTION_HASHES_AFTER" || true)"
fi
if cmp -s "$ADOPTION_PATHS_BEFORE" "$ADOPTION_PATHS_AFTER" && \
   cmp -s "$ADOPTION_HASHES_BEFORE" "$ADOPTION_HASHES_AFTER" && \
   cmp -s "$ADOPTION_METADATA_BEFORE" "$ADOPTION_METADATA_AFTER"; then
    _pass "main reconcile adopts Git-rendered files without rewriting them"
else
    _fail "main reconcile adopts Git-rendered files without rewriting them" \
        "content paths, hashes, mtimes, or inodes changed"
fi

assert_success "raw Git switch main to feature" git switch feature
if [[ -z "$(git status --short)" ]]; then
    _pass "raw Git switch to feature leaves Git clean"
else
    _fail "raw Git switch to feature leaves Git clean" "$(git status --short)"
fi
if [[ "$(atomic_current_view)" == "$MAIN" ]]; then
    _pass "Atomic view remains main before adopting raw Git feature switch"
else
    _fail "Atomic view remains main before adopting raw Git feature switch" \
        "got $(atomic_current_view)"
fi
snapshot_git_rendered_state \
    "$ADOPTION_PATHS_BEFORE" "$ADOPTION_HASHES_BEFORE" "$ADOPTION_METADATA_BEFORE"
if cmp -s "$FEATURE_PATHS" "$ADOPTION_PATHS_BEFORE" && \
   cmp -s "$FEATURE_HASHES" "$ADOPTION_HASHES_BEFORE"; then
    _pass "raw Git switch renders expected complete feature content"
else
    _fail "raw Git switch renders expected complete feature content" \
        "$(diff -u "$FEATURE_PATHS" "$ADOPTION_PATHS_BEFORE" || true)"
fi
sleep 1
assert_success "reconcile adopts raw Git switch to feature" atomic git bridge reconcile
snapshot_git_rendered_state \
    "$ADOPTION_PATHS_AFTER" "$ADOPTION_HASHES_AFTER" "$ADOPTION_METADATA_AFTER"
if [[ "$(atomic_current_view)" == "feature" ]]; then
    _pass "reconcile adopts feature as the current Atomic view"
else
    _fail "reconcile adopts feature as the current Atomic view" "got $(atomic_current_view)"
fi
assert_clean_statuses "after adopting raw Git switch to feature"
assert_success "bridge verify succeeds after adopting feature" atomic git bridge verify
assert_complete_paths_match "adopted feature has the complete expected path list"
if cmp -s "$FEATURE_PATHS" "$ADOPTION_PATHS_AFTER" && \
   cmp -s "$FEATURE_HASHES" "$ADOPTION_HASHES_AFTER"; then
    _pass "adopted feature content equals expected feature"
else
    _fail "adopted feature content equals expected feature" \
        "$(diff -u "$FEATURE_HASHES" "$ADOPTION_HASHES_AFTER" || true)"
fi
if cmp -s "$ADOPTION_PATHS_BEFORE" "$ADOPTION_PATHS_AFTER" && \
   cmp -s "$ADOPTION_HASHES_BEFORE" "$ADOPTION_HASHES_AFTER" && \
   cmp -s "$ADOPTION_METADATA_BEFORE" "$ADOPTION_METADATA_AFTER"; then
    _pass "feature reconcile adopts Git-rendered files without rewriting them"
else
    _fail "feature reconcile adopts Git-rendered files without rewriting them" \
        "content paths, hashes, mtimes, or inodes changed"
fi

begin_section "Raw Git branch creation adoption"

TOPIC_PATHS_BEFORE="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-topic-paths-before-XXXXXX")"
TOPIC_HASHES_BEFORE="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-topic-hashes-before-XXXXXX")"
TOPIC_METADATA_BEFORE="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-topic-metadata-before-XXXXXX")"
TOPIC_PATHS_AFTER="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-topic-paths-after-XXXXXX")"
TOPIC_HASHES_AFTER="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-topic-hashes-after-XXXXXX")"
TOPIC_METADATA_AFTER="$(mktemp "${TMPDIR:-/tmp}/atomic-bridge-topic-metadata-after-XXXXXX")"
_HARNESS_TMPDIRS+=(
    "$TOPIC_PATHS_BEFORE" "$TOPIC_HASHES_BEFORE" "$TOPIC_METADATA_BEFORE"
    "$TOPIC_PATHS_AFTER" "$TOPIC_HASHES_AFTER" "$TOPIC_METADATA_AFTER"
)

TOPIC_BASE_HEAD="$(git_head_sha_full)"
assert_success "raw Git creates bridge-topic from aligned feature" git checkout -b bridge-topic
if [[ "$(git_current_branch)" == "bridge-topic" && "$(git_head_sha_full)" == "$TOPIC_BASE_HEAD" ]]; then
    _pass "raw Git branch creation changes only the checked-out branch"
else
    _fail "raw Git branch creation changes only the checked-out branch" \
        "got $(git_current_branch) at $(git_head_sha_full), expected bridge-topic at $TOPIC_BASE_HEAD"
fi
if [[ "$(atomic_current_view)" == "feature" ]]; then
    _pass "Atomic view remains feature before adopting bridge-topic"
else
    _fail "Atomic view remains feature before adopting bridge-topic" \
        "got $(atomic_current_view)"
fi
snapshot_git_rendered_state \
    "$TOPIC_PATHS_BEFORE" "$TOPIC_HASHES_BEFORE" "$TOPIC_METADATA_BEFORE"
sleep 1
set +e
TOPIC_ADOPT_OUT="$(atomic git bridge reconcile 2>&1)"
TOPIC_ADOPT_RC=$?
set -e
if [[ "$TOPIC_ADOPT_RC" -eq 0 ]]; then
    _pass "reconcile adopts newly created bridge-topic branch"
else
    _fail "reconcile adopts newly created bridge-topic branch" "exit $TOPIC_ADOPT_RC: $TOPIC_ADOPT_OUT"
fi
snapshot_git_rendered_state \
    "$TOPIC_PATHS_AFTER" "$TOPIC_HASHES_AFTER" "$TOPIC_METADATA_AFTER"
assert_view_exists "new bridge-topic branch has a matching Atomic view" bridge-topic
if [[ "$(atomic_current_view)" == "bridge-topic" ]]; then
    _pass "new bridge-topic view is current after reconcile"
else
    _fail "new bridge-topic view is current after reconcile" "got $(atomic_current_view)"
fi
assert_clean_statuses "after adopting newly created bridge-topic"
assert_success "bridge verify succeeds after adopting bridge-topic" atomic git bridge verify
if cmp -s "$TOPIC_PATHS_BEFORE" "$TOPIC_PATHS_AFTER" && \
   cmp -s "$TOPIC_HASHES_BEFORE" "$TOPIC_HASHES_AFTER" && \
   cmp -s "$TOPIC_METADATA_BEFORE" "$TOPIC_METADATA_AFTER"; then
    _pass "bridge-topic reconcile adopts files without rewriting them"
else
    _fail "bridge-topic reconcile adopts files without rewriting them" \
        "content paths, hashes, mtimes, or inodes changed"
fi


assert_success "raw Git switch bridge-topic to feature" git switch feature
if [[ "$(atomic_current_view)" == "bridge-topic" ]]; then
    _pass "Atomic view remains bridge-topic before readopting feature"
else
    _fail "Atomic view remains bridge-topic before readopting feature" \
        "got $(atomic_current_view)"
fi
assert_success "reconcile readopts feature after raw Git switch" atomic git bridge reconcile
if [[ "$(git_current_branch)" == "feature" && "$(atomic_current_view)" == "feature" ]]; then
    _pass "Git branch and Atomic view are realigned on feature"
else
    _fail "Git branch and Atomic view are realigned on feature" \
        "got $(git_current_branch)/$(atomic_current_view)"
fi
assert_clean_statuses "after returning from bridge-topic to feature"
assert_success "bridge verify succeeds after returning to feature" atomic git bridge verify

begin_section "Two-sided divergence refusal is non-destructive"

overwrite_file "README.md" "bridge mvp\nfeature\natomic origin\ntwo-sided divergence\n"
create_file "src/nested/two-sided.txt" "advanced independently\n"
assert_success "add two-sided Atomic file" atomic add src/nested/two-sided.txt
assert_success "record Atomic side of divergence" atomic record -m "Atomic side of divergence"
assert_success "commit Git side of divergence independently" \
    git add README.md src/nested/two-sided.txt
assert_success "create independent Git divergence commit" \
    git commit --quiet -m "Git side of divergence"
assert_clean_statuses "before two-sided divergence reconcile"

DIVERGED_HEAD="$(git_head_sha_full)"
DIVERGED_COUNT="$(git_commit_count)"
DIVERGED_ATOMIC_LOG="$(atomic log 2>&1)"
DIVERGED_README="$(cat README.md)"
DIVERGED_FILE="$(cat src/nested/two-sided.txt)"
set +e
DIVERGED_OUT="$(atomic git bridge reconcile 2>&1)"
DIVERGED_RC=$?
set -e
if [[ "$DIVERGED_RC" -ne 0 ]]; then
    _pass "reconcile refuses two-sided divergence"
else
    _fail "reconcile refuses two-sided divergence" "command unexpectedly succeeded"
fi
if [[ "$(git_head_sha_full)" == "$DIVERGED_HEAD" && \
      "$(git_commit_count)" == "$DIVERGED_COUNT" ]]; then
    _pass "divergence refusal preserves Git history"
else
    _fail "divergence refusal preserves Git history" \
        "expected $DIVERGED_HEAD at count $DIVERGED_COUNT, got $(git_head_sha_full) at count $(git_commit_count)"
fi
if [[ "$(atomic log 2>&1)" == "$DIVERGED_ATOMIC_LOG" ]]; then
    _pass "divergence refusal preserves Atomic state"
else
    _fail "divergence refusal preserves Atomic state" "atomic log changed"
fi
if [[ "$(cat README.md)" == "$DIVERGED_README" && \
      "$(cat src/nested/two-sided.txt)" == "$DIVERGED_FILE" ]]; then
    _pass "divergence refusal preserves files"
else
    _fail "divergence refusal preserves files" "working-tree content changed"
fi
assert_clean_statuses "after two-sided divergence refusal"
if echo "$DIVERGED_OUT" | grep -qiE 'diverg|both|advanced|reconcile|refus'; then
    _pass "divergence refusal explains why reconciliation stopped"
else
    _fail "divergence refusal explains why reconciliation stopped" "$DIVERGED_OUT"
fi

print_summary
