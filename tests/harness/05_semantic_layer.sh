#!/usr/bin/env bash
# 05_semantic_layer.sh — Semantic layer cross-view isolation tests.
#
# The semantic layer (CRDT: Trunk → Branch → Leaf) translates raw graph
# operations into human-readable line/token operations for diff, blame,
# and change inspection.  These tests verify that semantic metadata
# respects the same view isolation guarantees as the graph layer:
#
#   - Record stats (lines, tokens) are correct per-view
#   - `atomic change` shows only the current view.s changes
#   - `atomic log` lists only changes on the current view
#   - `atomic diff` shows only modifications visible on the current view
#   - After `insert`, semantic metadata appears on the target view
#   - Divergent edits on sibling views produce independent semantic views
#
# These tests exercise the CRDT tables (crdt_trunks, crdt_branches,
# crdt_leaves) and their interaction with STACK_GRAPH edge isolation.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Semantic: Record stats reflect actual content"
# ═══════════════════════════════════════════════════════════════════════════
#
# When recording a file, the output should report accurate line and token
# counts from the semantic layer.

make_temp_repo "sem-record-stats"
init_repo

create_file "hello.rs" "fn main() {
    println!(\"hello\");
}"

require_success "add hello.rs" atomic add hello.rs

require_capture "record hello.rs" record_change "Add hello.rs"
rec_out="$COMMAND_OUTPUT"

# Record output should mention lines and tokens
if echo "$rec_out" | grep -qiE "line"; then
    _pass "record output includes line stats"
else
    _fail "record output includes line stats" "got: $(echo "$rec_out" | head -5)"
fi

if echo "$rec_out" | grep -qiE "token"; then
    _pass "record output includes token stats"
else
    _fail "record output includes token stats" "got: $(echo "$rec_out" | head -5)"
fi

# The file has exactly three lines.
if echo "$rec_out" | grep -qE '3 lines? \(\+3 -0 ~0\)'; then
    _pass "record reports exactly three added lines"
else
    _fail "record reports exactly three added lines" "got: $rec_out"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Semantic: Log only shows current view.s changes"
# ═══════════════════════════════════════════════════════════════════════════
#
# Changes recorded on feature should not appear in dev's log.

make_temp_repo "sem-log-isolation"
init_repo

create_file "base.txt" "base content"
require_success "add base.txt" atomic add base.txt
require_success "record base on dev" record_change "Add base on dev"

require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"
require_success "switch to feature" switch_view "feature"

create_file "feature.txt" "feature content"
require_success "add feature.txt" atomic add feature.txt
require_success "record feature.txt" record_change "Add feature.txt on feature"

# Log on feature should have 2 entries (base + feature file). Draft views
# hide ancestor-inherited entries by default (git-branch-log semantics);
# --all shows the full history including what was explicitly pulled in via
# insert_from_view.
require_capture "read feature log" atomic log --all
feature_log="$COMMAND_OUTPUT"
feature_count="$(echo "$feature_log" | grep -cE '^[[:space:]]*#[0-9]+|^[0-9a-f]{8,}' || true)"

if [[ $feature_count -ge 2 ]]; then
    _pass "feature log has 2+ changes ($feature_count)"
else
    _fail "feature log has 2+ changes" "got $feature_count"
fi

# Feature log should mention both messages
if echo "$feature_log" | grep -qF "Add base on dev"; then
    _pass "feature log contains base change"
else
    _fail "feature log contains base change" "not found"
fi

if echo "$feature_log" | grep -qF "Add feature.txt on feature"; then
    _pass "feature log contains feature change"
else
    _fail "feature log contains feature change" "not found"
fi

# Switch to dev — log should NOT contain the feature change
require_success "switch to dev" switch_view "dev"

require_capture "read dev log" atomic log
dev_log="$COMMAND_OUTPUT"

if echo "$dev_log" | grep -qF "Add base on dev"; then
    _pass "dev log contains base change"
else
    _fail "dev log contains base change" "not found"
fi

if echo "$dev_log" | grep -qF "Add feature.txt on feature"; then
    _fail "dev log should NOT contain feature change" "found in dev log"
else
    _pass "dev log does NOT contain feature change"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Semantic: Change detail shows correct change per view"
# ═══════════════════════════════════════════════════════════════════════════
#
# `atomic change` (latest change detail) should show the view.s own
# latest change, not another view.s.

# Continuing from previous repo
require_success "switch to feature" switch_view "feature"

require_capture "read feature change" atomic change
feature_change="$COMMAND_OUTPUT"
if echo "$feature_change" | grep -qF "Add feature.txt on feature"; then
    _pass "feature's latest change is the feature change"
else
    _fail "feature's latest change is the feature change" "got: $(echo "$feature_change" | head -5)"
fi

require_success "switch to dev" switch_view "dev"

require_capture "read dev change" atomic change
dev_change="$COMMAND_OUTPUT"
if echo "$dev_change" | grep -qF "Add base on dev"; then
    _pass "dev's latest change is the base change"
else
    _fail "dev's latest change is the base change" "got: $(echo "$dev_change" | head -5)"
fi

# Dev's change detail should NOT mention feature.txt
if echo "$dev_change" | grep -qF "feature.txt"; then
    _fail "dev's change should not mention feature.txt" "found"
else
    _pass "dev's change does not mention feature.txt"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Semantic: Insert brings change metadata to target view"
# ═══════════════════════════════════════════════════════════════════════════
#
# After inserting feature.s changes to dev, dev's log should include them.

# Continuing from previous repo
require_success "insert feature into dev" insert_from_view "feature" "dev"

require_success "select dev" switch_view "dev"

require_capture "read dev log after insert" atomic log
dev_log_after="$COMMAND_OUTPUT"
dev_count_after="$(echo "$dev_log_after" | grep -cE '^[[:space:]]*#[0-9]+|^[0-9a-f]{8,}' || true)"

if [[ $dev_count_after -ge 2 ]]; then
    _pass "dev log has 2+ changes after insert ($dev_count_after)"
else
    _fail "dev log has 2+ changes after insert" "got $dev_count_after"
fi

if echo "$dev_log_after" | grep -qF "Add feature.txt on feature"; then
    _pass "dev log contains feature change after insert"
else
    _fail "dev log contains feature change after insert" "not found"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Semantic: Diff only shows current view's modifications"
# ═══════════════════════════════════════════════════════════════════════════
#
# Modify a file on feature, then verify that `diff` on dev does NOT
# show that modification (until it's applied).

make_temp_repo "sem-diff-isolation"
init_repo

create_file "shared.rs" "fn greet() {
    println!(\"hello\");
}"
require_success "add shared.rs" atomic add shared.rs
require_success "record shared.rs" record_change "Add shared.rs"

require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"
require_success "switch to feature" switch_view "feature"

# Modify on feature (working copy change, not yet recorded)
overwrite_file "shared.rs" "fn greet() {
    println!(\"world\");
}"

# Diff on feature should show the modification
require_capture "read feature diff" atomic diff
feature_diff="$COMMAND_OUTPUT"
if echo "$feature_diff" | grep -qF "hello" && echo "$feature_diff" | grep -qF "world"; then
    _pass "diff on feature shows modification"
else
    _fail "diff on feature shows modification" "got: $feature_diff"
fi

# Record on feature
require_success "record feature modification" record_change "Modify shared.rs on feature"

# Switch to dev — working copy should have ORIGINAL content
require_success "switch to dev" switch_view "dev"

# Diff on dev should show NO changes (dev has the original, not feature's mod)
require_capture "read dev diff" atomic diff
dev_diff="$COMMAND_OUTPUT"
if echo "$dev_diff" | grep -qiE "no changes|no diff"; then
    _pass "diff on dev shows no changes"
elif [[ -z "$(echo "$dev_diff" | xargs)" ]]; then
    _pass "diff on dev is empty (no changes)"
else
    _fail "diff on dev shows no changes" "got: $dev_diff"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Semantic: Record stats differ per view for same file"
# ═══════════════════════════════════════════════════════════════════════════
#
# When the same file is modified differently on two views, the record
# output should reflect each view's independent changes.

make_temp_repo "sem-stats-diverge"
init_repo

create_file "code.py" "def hello():
    print('hello')
    return True"
require_success "add code.py" atomic add code.py
require_success "record code.py" record_change "Add code.py"

require_success "create feature-a" new_view "feature-a"
require_success "insert dev into feature-a" insert_from_view "dev" "feature-a"
require_success "switch to feature-a" switch_view "feature-a"

# Modify one line on feature-a
overwrite_file "code.py" "def hello():
    print('goodbye')
    return True"
require_capture "record feature-a edit" record_change "Change hello to goodbye"
rec_a="$COMMAND_OUTPUT"
if echo "$rec_a" | grep -qiE "line|token|change"; then
    _pass "record on feature-a reports stats"
else
    _fail "record on feature-a reports stats" "got: $rec_a"
fi

# Create feature-b from dev (independent sibling)
require_success "switch to dev" switch_view "dev"
require_success "create feature-b" new_view "feature-b"
require_success "insert dev into feature-b" insert_from_view "dev" "feature-b"
require_success "switch to feature-b" switch_view "feature-b"

# Add two new lines on feature-b
overwrite_file "code.py" "def hello():
    print('hello')
    return True

def world():
    return False"
require_capture "record feature-b edit" record_change "Add world function"
rec_b="$COMMAND_OUTPUT"
if echo "$rec_b" | grep -qiE "line|token|change"; then
    _pass "record on feature-b reports stats"
else
    _fail "record on feature-b reports stats" "got: $rec_b"
fi

# Log on feature-a should have feature-a's change, not feature-b's
require_success "switch to feature-a" switch_view "feature-a"
require_capture "read feature-a log" atomic log
fa_log="$COMMAND_OUTPUT"
if echo "$fa_log" | grep -qF "Change hello to goodbye"; then
    _pass "feature-a log has its own change"
else
    _fail "feature-a log has its own change" "not found"
fi
if echo "$fa_log" | grep -qF "Add world function"; then
    _fail "feature-a log should NOT have feature-b's change" "found"
else
    _pass "feature-a log does NOT have feature-b's change"
fi

# Log on feature-b should have feature-b's change, not feature-a's
require_success "switch to feature-b" switch_view "feature-b"
require_capture "read feature-b log" atomic log
fb_log="$COMMAND_OUTPUT"
if echo "$fb_log" | grep -qF "Add world function"; then
    _pass "feature-b log has its own change"
else
    _fail "feature-b log has its own change" "not found"
fi
if echo "$fb_log" | grep -qF "Change hello to goodbye"; then
    _fail "feature-b log should NOT have feature-a's change" "found"
else
    _pass "feature-b log does NOT have feature-a's change"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Semantic: Change file list is view-scoped"
# ═══════════════════════════════════════════════════════════════════════════
#
# `atomic change` should list only files affected by changes on the
# current view.

make_temp_repo "sem-change-files"
init_repo

create_file "alpha.txt" "alpha"
require_success "add alpha.txt" atomic add alpha.txt
require_success "record alpha on dev" record_change "Add alpha on dev"

require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"
require_success "switch to feature" switch_view "feature"

create_file "beta.txt" "beta"
require_success "add beta.txt" atomic add beta.txt
require_success "record beta on feature" record_change "Add beta on feature"

# Latest change on feature should mention beta.txt
require_capture "read feature change" atomic change
feature_change="$COMMAND_OUTPUT"
if echo "$feature_change" | grep -qF "beta.txt"; then
    _pass "feature change mentions beta.txt"
else
    _fail "feature change mentions beta.txt" "not found"
fi

# Latest change on feature should NOT mention alpha.txt
# (alpha was a different change)
if echo "$feature_change" | grep -qF "alpha.txt"; then
    _fail "feature latest change should NOT mention alpha.txt" "found (may be showing wrong change)"
else
    _pass "feature latest change does not mention alpha.txt"
fi

# Dev's latest change should mention alpha.txt
require_success "switch to dev" switch_view "dev"
require_capture "read dev change" atomic change
dev_change="$COMMAND_OUTPUT"
if echo "$dev_change" | grep -qF "alpha.txt"; then
    _pass "dev change mentions alpha.txt"
else
    _fail "dev change mentions alpha.txt" "not found"
fi

# Dev's latest change should NOT mention beta.txt
if echo "$dev_change" | grep -qF "beta.txt"; then
    _fail "dev change should NOT mention beta.txt" "found"
else
    _pass "dev change does not mention beta.txt"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Semantic: Unrecord removes change from view's log"
# ═══════════════════════════════════════════════════════════════════════════
#
# Unrecording a change should remove it from the current view's log,
# but it should remain on any view that received it via insert.

make_temp_repo "sem-unrecord-log"
init_repo

create_file "removable.txt" "removable content"
require_success "add removable.txt" atomic add removable.txt
require_success "record removable on dev" record_change "Add removable on dev"

# Insert to feature so feature has it too
require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"

# Verify feature has the change. --all needed: the change is still
# present in dev's own log at this point, so the draft-view ancestor
# filter would otherwise hide it (it was pulled in via insert_from_view).
require_success "switch to feature" switch_view "feature"
require_capture "read feature log" atomic log --all
feature_log_before="$COMMAND_OUTPUT"
if echo "$feature_log_before" | grep -qF "Add removable on dev"; then
    _pass "feature log has removable change before unrecord"
else
    _fail "feature log has removable change" "not found"
fi

# Unrecord on dev
require_success "switch to dev" switch_view "dev"
require_success "unrecord on dev" unrecord_last

# Dev log should no longer have the change
require_capture "read dev log after unrecord" atomic log
dev_log_after="$COMMAND_OUTPUT"
if echo "$dev_log_after" | grep -qF "Add removable on dev"; then
    _fail "dev log should NOT have removable change after unrecord" "still present"
else
    _pass "dev log no longer has removable change"
fi

# Feature should STILL have it (unrecord was only on dev)
require_success "switch to feature after unrecord" switch_view "feature"
require_capture "read feature log after unrecord" atomic log
feature_log_after="$COMMAND_OUTPUT"
if echo "$feature_log_after" | grep -qF "Add removable on dev"; then
    _pass "feature log still has removable change after dev unrecord"
else
    _fail "feature log still has removable change" "not found"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Semantic: Multiple files in single change"
# ═══════════════════════════════════════════════════════════════════════════
#
# A change that adds multiple files should list all of them in the
# change detail and track them in the semantic layer.

make_temp_repo "sem-multi-file-change"
init_repo

create_file "src/main.rs" "fn main() {}"
create_file "src/lib.rs" "pub fn lib() {}"
create_file "README.md" "# Project"

require_success "add main.rs" atomic add src/main.rs
require_success "add lib.rs" atomic add src/lib.rs
require_success "add README.md" atomic add README.md

require_capture "record project files" record_change "Add project files"
rec_out="$COMMAND_OUTPUT"

# Record should mention multiple files
if echo "$rec_out" | grep -qE "3 files? changed"; then
    _pass "record reports three files changed"
else
    _fail "record reports three files changed" "got: $rec_out"
fi

# Change detail should list all three files
require_capture "read project change" atomic change
change_out="$COMMAND_OUTPUT"
for f in main.rs lib.rs README.md; do
    if echo "$change_out" | grep -qF "$f"; then
        _pass "change detail lists $f"
    else
        _fail "change detail lists $f" "not found in: $change_out"
    fi
done

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Semantic: Line-level edit tracking"
# ═══════════════════════════════════════════════════════════════════════════
#
# When modifying specific lines, the record stats and change detail should
# reflect the line-level granularity.

make_temp_repo "sem-line-edit"
init_repo

create_file "lines.txt" "line one
line two
line three
line four
line five"
require_success "add lines.txt" atomic add lines.txt
require_success "record five lines" record_change "Add five lines"

# Modify lines 2 and 4
overwrite_file "lines.txt" "line one
LINE TWO MODIFIED
line three
LINE FOUR MODIFIED
line five"
require_capture "record line edits" record_change "Modify lines 2 and 4"
rec_out="$COMMAND_OUTPUT"

# Record should report line changes
if echo "$rec_out" | grep -qiE "line"; then
    _pass "modification record reports line stats"
else
    _fail "modification record reports line stats" "got: $(echo "$rec_out" | head -5)"
fi

# Change detail should reference lines.txt
require_capture "read line edit change" atomic change
change_out="$COMMAND_OUTPUT"
if echo "$change_out" | grep -qF "lines.txt"; then
    _pass "change detail mentions lines.txt"
else
    _fail "change detail mentions lines.txt" "not found"
fi

if echo "$change_out" | grep -qF "Modify lines 2 and 4"; then
    _pass "change detail has correct message"
else
    _fail "change detail has correct message" "not found"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Semantic: Token-level stats in record output"
# ═══════════════════════════════════════════════════════════════════════════
#
# The record output should include token-level statistics from the CRDT
# semantic layer, not just line-level.

make_temp_repo "sem-token-stats"
init_repo

create_file "tokens.rs" "fn compute(x: i32) -> i32 {
    x + 1
}"
require_success "add tokens.rs" atomic add tokens.rs
require_capture "record tokens.rs" record_change "Add tokens.rs"
rec_out="$COMMAND_OUTPUT"

# Should report token stats
if echo "$rec_out" | grep -qiE "token"; then
    _pass "record includes token-level stats"
else
    _fail "record includes token-level stats" "got: $(echo "$rec_out" | head -5)"
fi

# Modify a single token (change + to *)
overwrite_file "tokens.rs" "fn compute(x: i32) -> i32 {
    x * 1
}"
require_capture "record token edit" record_change "Change + to *"
rec_mod="$COMMAND_OUTPUT"
if echo "$rec_mod" | grep -qiE "token"; then
    _pass "modification record includes token stats"
else
    _fail "modification record includes token stats" "got: $(echo "$rec_mod" | head -5)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Semantic: Full cross-view lifecycle with semantic verification"
# ═══════════════════════════════════════════════════════════════════════════
#
# End-to-end test combining graph isolation with semantic layer checks:
#   1. Record a file with known content on dev
#   2. Create feature, insert dev's changes
#   3. Modify the file on feature, verify record stats
#   4. Switch to dev — verify log/change don't show feature's modification
#   5. Insert feature→dev — verify log/change now include it
#   6. Verify file content is correct on both views

make_temp_repo "sem-full-lifecycle"
init_repo

# Step 1: record on dev
create_file "app.py" "def main():
    print('version 1')
    return 0"
require_success "add app.py" atomic add app.py
require_success "record app.py v1" record_change "Add app.py v1"

require_capture "read initial dev log" atomic log
dev_log_v1="$COMMAND_OUTPUT"
dev_count_v1="$(echo "$dev_log_v1" | grep -cE '^[[:space:]]*#[0-9]+|^[0-9a-f]{8,}' || true)"

# Step 2: create feature, insert dev
require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"
require_success "switch to feature" switch_view "feature"

# Step 3: modify on feature
overwrite_file "app.py" "def main():
    print('version 2 from feature')
    return 0"
require_capture "record feature update" record_change "Update to v2 on feature"
rec_feature="$COMMAND_OUTPUT"

if echo "$rec_feature" | grep -qiE "line|token"; then
    _pass "feature record shows semantic stats"
else
    _fail "feature record shows semantic stats" "got: $rec_feature"
fi

# --all: feature inherited "Add app.py v1" via insert_from_view, which the
# draft-view ancestor filter would otherwise hide since it's also on dev.
require_capture "read feature log" atomic log --all
feature_log="$COMMAND_OUTPUT"
feature_count="$(echo "$feature_log" | grep -cE '^[[:space:]]*#[0-9]+|^[0-9a-f]{8,}' || true)"
if [[ $feature_count -ge 2 ]]; then
    _pass "feature log has 2+ entries ($feature_count)"
else
    _fail "feature log has 2+ entries" "got $feature_count"
fi

# Step 4: switch to dev, verify isolation
require_success "switch to dev" switch_view "dev"

require_capture "read isolated dev log" atomic log
dev_log_isolated="$COMMAND_OUTPUT"
dev_count_isolated="$(echo "$dev_log_isolated" | grep -cE '^[[:space:]]*#[0-9]+|^[0-9a-f]{8,}' || true)"

if [[ $dev_count_isolated -eq $dev_count_v1 ]]; then
    _pass "dev log unchanged after feature record ($dev_count_isolated changes)"
else
    _fail "dev log unchanged" "was $dev_count_v1, now $dev_count_isolated"
fi

if echo "$dev_log_isolated" | grep -qF "Update to v2 on feature"; then
    _fail "dev log should NOT have feature's change" "found"
else
    _pass "dev log does NOT have feature's change"
fi

# Content should be v1
assert_file_content "app.py has v1 on dev" "app.py" "def main():
    print('version 1')
    return 0"

# Step 5: insert feature→dev
require_success "insert feature into dev" insert_from_view "feature" "dev"

require_success "select dev" switch_view "dev"

require_capture "read dev log after insert" atomic log
dev_log_after_apply="$COMMAND_OUTPUT"
dev_count_after="$(echo "$dev_log_after_apply" | grep -cE '^[[:space:]]*#[0-9]+|^[0-9a-f]{8,}' || true)"

if [[ $dev_count_after -gt $dev_count_v1 ]]; then
    _pass "dev log grew after insert ($dev_count_after changes)"
else
    _fail "dev log grew after insert" "still $dev_count_after"
fi

if echo "$dev_log_after_apply" | grep -qF "Update to v2 on feature"; then
    _pass "dev log now contains feature's change"
else
    _fail "dev log now contains feature's change" "not found"
fi

# Step 6: content correct on both
assert_file_content "app.py has v2 on dev after insert" "app.py" "def main():
    print('version 2 from feature')
    return 0"

require_success "switch to feature" switch_view "feature"
assert_file_content "app.py has v2 on feature" "app.py" "def main():
    print('version 2 from feature')
    return 0"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Semantic: Three views, selective insert, log isolation"
# ═══════════════════════════════════════════════════════════════════════════
#
# Three sibling views each with unique changes.  Insert selectively
# and verify that log/change on each view reflects ONLY what was
# applied to it.

make_temp_repo "sem-three-view"
init_repo

create_file "base.txt" "base"
require_success "add base.txt" atomic add base.txt
require_success "record base" record_change "Base commit"

# Create three siblings from dev
for stack in alpha beta gamma; do
    require_success "create $stack view" new_view "$stack"
    require_success "insert dev into $stack" insert_from_view "dev" "$stack"
    require_success "switch to $stack" switch_view "$stack"

    create_file "${stack}.txt" "${stack} content"
    require_success "add ${stack}.txt" atomic add "${stack}.txt"
    require_success "record ${stack}.txt" record_change "Add ${stack}.txt"
done

# Alpha log should have base + alpha, NOT beta or gamma
require_success "switch to alpha" switch_view "alpha"
require_capture "read alpha log" atomic log
alpha_log="$COMMAND_OUTPUT"
if echo "$alpha_log" | grep -qF "Add alpha.txt"; then
    _pass "alpha log has alpha change"
else
    _fail "alpha log has alpha change" "not found"
fi
if echo "$alpha_log" | grep -qF "Add beta.txt"; then
    _fail "alpha log should NOT have beta change" "found"
else
    _pass "alpha log does NOT have beta change"
fi
if echo "$alpha_log" | grep -qF "Add gamma.txt"; then
    _fail "alpha log should NOT have gamma change" "found"
else
    _pass "alpha log does NOT have gamma change"
fi

# Insert alpha→dev only.  Beta and gamma should NOT appear on dev.
require_success "insert alpha into dev" insert_from_view "alpha" "dev"
require_success "switch to dev" switch_view "dev"

require_capture "read dev log" atomic log
dev_log="$COMMAND_OUTPUT"
if echo "$dev_log" | grep -qF "Add alpha.txt"; then
    _pass "dev log has alpha change after insert"
else
    _fail "dev log has alpha change after insert" "not found"
fi
if echo "$dev_log" | grep -qF "Add beta.txt"; then
    _fail "dev log should NOT have beta change" "found"
else
    _pass "dev log does NOT have beta change"
fi
if echo "$dev_log" | grep -qF "Add gamma.txt"; then
    _fail "dev log should NOT have gamma change" "found"
else
    _pass "dev log does NOT have gamma change"
fi

# Insert beta→dev.  Now dev has base + alpha + beta, but NOT gamma.
require_success "insert beta into dev" insert_from_view "beta" "dev"
require_success "select dev" switch_view "dev"

require_capture "read dev log after beta" atomic log
dev_log2="$COMMAND_OUTPUT"
if echo "$dev_log2" | grep -qF "Add beta.txt"; then
    _pass "dev log has beta change after second insert"
else
    _fail "dev log has beta change after second insert" "not found"
fi
if echo "$dev_log2" | grep -qF "Add gamma.txt"; then
    _fail "dev log STILL should NOT have gamma change" "found"
else
    _pass "dev log STILL does NOT have gamma change"
fi

# Gamma is untouched — should only have base + gamma
require_success "switch to gamma" switch_view "gamma"
require_capture "read gamma log" atomic log
gamma_log="$COMMAND_OUTPUT"
if echo "$gamma_log" | grep -qF "Add gamma.txt"; then
    _pass "gamma log has gamma change"
else
    _fail "gamma log has gamma change" "not found"
fi
if echo "$gamma_log" | grep -qF "Add alpha.txt"; then
    _fail "gamma log should NOT have alpha change" "found"
else
    _pass "gamma log does NOT have alpha change"
fi
if echo "$gamma_log" | grep -qF "Add beta.txt"; then
    _fail "gamma log should NOT have beta change" "found"
else
    _pass "gamma log does NOT have beta change"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Semantic: Diff shows nothing after recording (clean state)"
# ═══════════════════════════════════════════════════════════════════════════
#
# After recording all changes, `diff` should show nothing (working copy
# matches the graph state).

make_temp_repo "sem-diff-clean"
init_repo

create_file "clean.txt" "clean content"
require_success "add clean.txt" atomic add clean.txt
require_success "record clean.txt" record_change "Add clean.txt"

require_capture "read clean diff" atomic diff
diff_out="$COMMAND_OUTPUT"
if echo "$diff_out" | grep -qiE "no changes|no diff"; then
    _pass "diff shows no changes after record"
elif [[ -z "$(echo "$diff_out" | xargs)" ]]; then
    _pass "diff is empty after record (clean)"
else
    _fail "diff shows no changes after record" "got: $diff_out"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Semantic: Change count matches across operations"
# ═══════════════════════════════════════════════════════════════════════════
#
# Track the change count through a series of operations and verify
# log entries match expectations at each step.

make_temp_repo "sem-change-count"
init_repo

# Capture the baseline count from init (init creates 2 changes:
# "Initialize repository" and "Initialize vault").
require_capture "read baseline log" atomic log
baseline_log="$COMMAND_OUTPUT"
BASE="$(echo "$baseline_log" | grep -cE '^[[:space:]]*#[0-9]+|^[0-9a-f]{8,}' || true)"

# Record 3 changes on dev
for i in 1 2 3; do
    create_file "file${i}.txt" "content ${i}"
    require_success "add file${i}.txt" atomic add "file${i}.txt"
    require_success "record file${i}.txt" record_change "Add file${i}"
done

require_capture "read dev log" atomic log
dev_log="$COMMAND_OUTPUT"
dev_count="$(echo "$dev_log" | grep -cE '^[[:space:]]*#[0-9]+|^[0-9a-f]{8,}' || true)"
expected_dev=$((BASE + 3))
if [[ $dev_count -eq $expected_dev ]]; then
    _pass "dev has exactly $expected_dev changes (base $BASE + 3)"
else
    _fail "dev has exactly $expected_dev changes" "got $dev_count"
fi

# Create feature from dev, add 2 more changes
require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"
require_success "switch to feature" switch_view "feature"

for i in 4 5; do
    create_file "file${i}.txt" "content ${i}"
    require_success "add file${i}.txt" atomic add "file${i}.txt"
    require_success "record file${i}.txt" record_change "Add file${i}"
done

# --all: expected_feature counts the 3 inherited dev changes too, which
# the draft-view ancestor filter would otherwise hide.
require_capture "read feature log" atomic log --all
feature_log="$COMMAND_OUTPUT"
feature_count="$(echo "$feature_log" | grep -cE '^[[:space:]]*#[0-9]+|^[0-9a-f]{8,}' || true)"
expected_feature=$((BASE + 5))
if [[ $feature_count -eq $expected_feature ]]; then
    _pass "feature has exactly $expected_feature changes (base $BASE + 3 inherited + 2 own)"
else
    _fail "feature has exactly $expected_feature changes" "got $feature_count"
fi

# Dev should still have exactly BASE+3
require_success "switch to dev" switch_view "dev"
require_capture "read dev log" atomic log
dev_log2="$COMMAND_OUTPUT"
dev_count2="$(echo "$dev_log2" | grep -cE '^[[:space:]]*#[0-9]+|^[0-9a-f]{8,}' || true)"
if [[ $dev_count2 -eq $expected_dev ]]; then
    _pass "dev still has exactly $expected_dev changes"
else
    _fail "dev still has exactly $expected_dev changes" "got $dev_count2"
fi

# Insert feature→dev.  Dev should now have BASE+5.
require_success "insert feature into dev" insert_from_view "feature" "dev"
require_success "select dev" switch_view "dev"

require_capture "read dev log after insert" atomic log
dev_log3="$COMMAND_OUTPUT"
dev_count3="$(echo "$dev_log3" | grep -cE '^[[:space:]]*#[0-9]+|^[0-9a-f]{8,}' || true)"
expected_after_insert=$((BASE + 5))
if [[ $dev_count3 -eq $expected_after_insert ]]; then
    _pass "dev has $expected_after_insert changes after insert"
else
    _fail "dev has $expected_after_insert changes after insert" "got $dev_count3"
fi

# Unrecord last on dev.  Should go back to BASE+4.
require_success "unrecord dev change" unrecord_last
require_capture "read dev log after unrecord" atomic log
dev_log4="$COMMAND_OUTPUT"
dev_count4="$(echo "$dev_log4" | grep -cE '^[[:space:]]*#[0-9]+|^[0-9a-f]{8,}' || true)"
expected_after_unrecord=$((BASE + 4))
if [[ $dev_count4 -eq $expected_after_unrecord ]]; then
    _pass "dev has $expected_after_unrecord changes after unrecord"
else
    _fail "dev has $expected_after_unrecord changes after unrecord" "got $dev_count4"
fi

# Feature should still have BASE+5 (unrecord was on dev). --all: same
# ancestor-filter reasoning as above.
require_success "switch to feature" switch_view "feature"
require_capture "read feature log" atomic log --all
feature_log2="$COMMAND_OUTPUT"
feature_count2="$(echo "$feature_log2" | grep -cE '^[[:space:]]*#[0-9]+|^[0-9a-f]{8,}' || true)"
if [[ $feature_count2 -eq $expected_feature ]]; then
    _pass "feature still has $expected_feature changes after dev unrecord"
else
    _fail "feature still has $expected_feature changes" "got $feature_count2"
fi

# ═══════════════════════════════════════════════════════════════════════════

print_summary
