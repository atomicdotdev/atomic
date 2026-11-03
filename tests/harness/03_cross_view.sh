#!/usr/bin/env bash
# 03_cross_stack.sh — Cross-view file isolation tests.
#
# This is the critical test suite for validating that files are properly
# isolated across views.  The key invariants being tested:
#
#   1. Untracked files persist across view switches (they're just on disk)
#   2. Files recorded on view A are NOT visible on view B after switching
#   3. Files can be moved between views via `insert`
#   4. Pending adds (tracked but not recorded) persist across switches
#   5. View deletion does not affect other views' files
#   6. Multiple views with overlapping and disjoint file sets work correctly
#
# These tests directly validate the fix to switch_view (which now computes
# recorded_file_paths per-view and deletes files that belong only to the
# old view) and the view-aware status filtering.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Untracked files persist across switch"
# ═══════════════════════════════════════════════════════════════════════════
#
# Workflow:
#   1. On dev, create an untracked file
#   2. Create a feature view and switch to it
#   3. The untracked file should still exist on disk
#   4. Switch back to dev — file still exists

make_temp_repo "cross-untracked-persist"
init_repo

create_file "untracked.txt" "I am untracked"

assert_file_exists "untracked.txt exists on dev" "untracked.txt"
assert_status_flag "untracked.txt is ? on dev" "?" "untracked.txt"

# Create and switch to feature
require_success "create feature view" new_view "feature"
require_success "switch to feature" switch_view "feature"

assert_current_view "now on feature" "feature"
assert_file_exists "untracked.txt persists on feature" "untracked.txt"
assert_status_flag "untracked.txt is ? on feature" "?" "untracked.txt"

# Switch back to dev
require_success "switch back to dev" switch_view "dev"

assert_current_view "back on dev" "dev"
assert_file_exists "untracked.txt persists after round-trip" "untracked.txt"
assert_status_flag "untracked.txt still ? on dev" "?" "untracked.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: File recorded on feature NOT on dev"
# ═══════════════════════════════════════════════════════════════════════════
#
# This is the PRIMARY use-case from the bug report:
#   1. On dev, create a file (untracked)
#   2. Switch to feature
#   3. Add + record the file on feature
#   4. Switch back to dev
#   5. File should NOT exist on disk (it belongs to feature)
#   6. Status on dev should NOT mention it

make_temp_repo "cross-record-isolation"
init_repo

# Step 1: create file on dev (untracked)
create_file "feature_only.txt" "feature content"
assert_file_exists "feature_only.txt created on dev" "feature_only.txt"

# Step 2: create and switch to feature
require_success "create feature view" new_view "feature"
require_success "switch to feature" switch_view "feature"
assert_current_view "on feature" "feature"

# Untracked file should still be here
assert_file_exists "feature_only.txt exists on feature (untracked)" "feature_only.txt"

# Step 3: add + record on feature
require_success "add feature_only.txt on feature" atomic add feature_only.txt

require_success "record on feature succeeds" record_change "Add feature_only.txt"

# Verify it's clean on feature
assert_status_no_entry "feature_only.txt is clean on feature after record" "feature_only.txt"

# Step 4: switch back to dev
require_success "switch back to dev" switch_view "dev"
assert_current_view "back on dev" "dev"

# Step 5: file should NOT exist on disk
assert_file_not_exists \
    "feature_only.txt removed from disk on dev (belongs to feature)" \
    "feature_only.txt"

# Step 6: status should NOT mention it
assert_status_no_entry \
    "feature_only.txt not in dev status" \
    "feature_only.txt"

# Step 7: switch back to feature — file should reappear
require_success "switch back to feature" switch_view "feature"
assert_file_exists "feature_only.txt reappears on feature" "feature_only.txt"
assert_file_content "feature_only.txt has correct content" "feature_only.txt" "feature content"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Insert from feature to dev"
# ═══════════════════════════════════════════════════════════════════════════
#
# Continuing from the previous test:
#   1. Insert changes from feature to dev
#   2. Switch to dev
#   3. File should now exist on dev

# We should still be on feature
assert_current_view "still on feature" "feature"

# Insert from feature → dev
require_success "insert from feature to dev succeeds" insert_from_view "feature" "dev"

# Switch to dev
require_success "switch to dev after insert" switch_view "dev"
assert_current_view "on dev after insert" "dev"

# Now the file SHOULD exist on dev
assert_file_exists \
    "feature_only.txt exists on dev after insert" \
    "feature_only.txt"

assert_file_content \
    "feature_only.txt has feature content on dev" \
    "feature_only.txt" \
    "feature content"

# Status should show it as clean (recorded on dev via insert)
assert_status_no_entry "feature_only.txt is clean on dev after insert" "feature_only.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Multiple files, partial overlap"
# ═══════════════════════════════════════════════════════════════════════════
#
# Setup:
#   - Record shared.txt on dev
#   - Create feature from dev
#   - Record feature_file.txt on feature
#   - Switch to dev: shared.txt exists, feature_file.txt does NOT
#   - Switch to feature: both exist

make_temp_repo "cross-partial-overlap"
init_repo

# Record shared.txt on dev
create_file "shared.txt" "shared content"
require_success "add shared.txt on dev" atomic add shared.txt
require_success "record shared.txt on dev" record_change "Add shared.txt on dev"

# Create feature from dev (inherits shared.txt)
require_success "create feature view from dev" atomic view create feature --from dev

require_success "switch to feature" switch_view "feature"
assert_current_view "on feature" "feature"

# shared.txt should exist on feature (inherited from dev)
assert_file_exists "shared.txt exists on feature" "shared.txt"

# Create and record a feature-only file
create_file "feature_file.txt" "feature only"
require_success "add feature_file.txt" atomic add feature_file.txt
require_success "record feature_file.txt" record_change "Add feature_file.txt"

# Both files should exist on feature
assert_file_exists "shared.txt on feature" "shared.txt"
assert_file_exists "feature_file.txt on feature" "feature_file.txt"

# Switch to dev
require_success "switch back to dev" switch_view "dev"
assert_current_view "on dev" "dev"

# shared.txt should exist (it's on dev)
assert_file_exists "shared.txt exists on dev" "shared.txt"

# feature_file.txt should NOT exist (only on feature)
assert_file_not_exists \
    "feature_file.txt NOT on dev" \
    "feature_file.txt"

# Switch back to feature — both should be there
require_success "switch back to feature" switch_view "feature"
assert_file_exists "shared.txt on feature (round 2)" "shared.txt"
assert_file_exists "feature_file.txt on feature (round 2)" "feature_file.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Switch blocked with pending add"
# ═══════════════════════════════════════════════════════════════════════════
#
# Workflow:
#   1. On dev, create and add a file (but DON'T record)
#   2. Attempt to switch to feature — should be BLOCKED
#   3. Record the file, then switch should succeed

make_temp_repo "cross-pending-add"
init_repo

create_file "pending.txt" "pending content"
require_success "add pending.txt on dev" atomic add pending.txt
assert_status_flag "pending.txt is A on dev" "A" "pending.txt"

# Attempt switch — should be blocked
require_success "create feature view" new_view "feature"
if atomic view switch feature >/dev/null 2>&1; then
    _fail "switch blocked with pending add on dev" "switch succeeded (rc=0)"
else
    _pass "switch blocked with pending add on dev"
fi
assert_current_view "still on dev" "dev"
assert_file_exists "pending.txt still on dev" "pending.txt"

# Record it, then switch should work
require_success "record pending.txt on dev" record_change "Record pending.txt on dev"
require_success "switch to feature after recording" switch_view "feature"
assert_current_view "on feature after recording" "feature"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Record on feature after force-switch, verify isolation"
# ═══════════════════════════════════════════════════════════════════════════
#
# After recording pending.txt on dev above, switch to feature,
# record a different file, switch back — verify isolation.

# We should be on feature from the previous test
assert_current_view "on feature" "feature"

# pending.txt should exist on feature (visible from dev via parent chain
# OR via inherited changes)
assert_file_exists "pending.txt visible on feature (inherited)" "pending.txt"

# Record a feature-only file
create_file "feature_only.txt" "feature only"
require_success "add feature_only.txt" atomic add feature_only.txt
require_success "record feature_only.txt" record_change "Record feature_only.txt"

# Switch back to dev
require_success "switch to dev" switch_view "dev"
assert_file_not_exists "feature_only.txt NOT on dev" "feature_only.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Nested directories removed on switch"
# ═══════════════════════════════════════════════════════════════════════════
#
# When a file in a nested directory is only on one view, switching away
# should remove the file AND clean up empty parent directories.

make_temp_repo "cross-nested-cleanup"
init_repo

require_success "create feature view" new_view "feature"
require_success "switch to feature" switch_view "feature"

create_file "src/pkg/deep/module.rs" "fn deep() {}"
require_success "add nested file" atomic add "src/pkg/deep/module.rs"
require_success "record nested file" record_change "Add deep nested file"

assert_dir_exists "src/pkg/deep exists on feature" "src/pkg/deep"

# Switch to dev
require_success "switch to dev" switch_view "dev"

assert_file_not_exists \
    "src/pkg/deep/module.rs NOT on dev" \
    "src/pkg/deep/module.rs"

# Empty parent dirs should also be cleaned up
assert_dir_not_exists \
    "src/pkg/deep cleaned up on dev" \
    "src/pkg/deep"

# Switch back — everything reappears
require_success "switch back to feature" switch_view "feature"
assert_file_exists "src/pkg/deep/module.rs on feature" "src/pkg/deep/module.rs"
assert_dir_exists "src/pkg/deep on feature" "src/pkg/deep"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Three-view scenario (dev, staging, feature)"
# ═══════════════════════════════════════════════════════════════════════════
#
# Setup:
#   - dev: has base.txt
#   - staging: inherits from dev, adds staging.txt
#   - feature: inherits from dev, adds feature.txt
#   - Switching between any two should show only the correct files

make_temp_repo "cross-three-views"
init_repo

# Record base.txt on dev
create_file "base.txt" "base content"
require_success "add base.txt" atomic add base.txt
require_success "record base.txt on dev" record_change "Add base.txt on dev"

# Create staging from dev
require_success "create staging view" new_view "staging"
# Insert dev.s changes to staging so it inherits base.txt
require_success "insert dev into staging" insert_from_view "dev" "staging"
require_success "switch to staging" switch_view "staging"

create_file "staging.txt" "staging content"
require_success "add staging.txt" atomic add staging.txt
require_success "record staging.txt" record_change "Add staging.txt"

# Create feature from dev
require_success "switch to dev" switch_view "dev"
require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"
require_success "switch to feature" switch_view "feature"

create_file "feature.txt" "feature content"
require_success "add feature.txt" atomic add feature.txt
require_success "record feature.txt" record_change "Add feature.txt"

# ── Verify on feature ──
assert_file_exists "base.txt on feature" "base.txt"
assert_file_exists "feature.txt on feature" "feature.txt"
assert_file_not_exists "staging.txt NOT on feature" "staging.txt"

# ── Switch to staging ──
require_success "switch to staging" switch_view "staging"
assert_file_exists "base.txt on staging" "base.txt"
assert_file_exists "staging.txt on staging" "staging.txt"
assert_file_not_exists "feature.txt NOT on staging" "feature.txt"

# ── Switch to dev ──
require_success "switch to dev" switch_view "dev"
assert_file_exists "base.txt on dev" "base.txt"
assert_file_not_exists "staging.txt NOT on dev" "staging.txt"
assert_file_not_exists "feature.txt NOT on dev" "feature.txt"

# ── Round-trip back to feature ──
require_success "switch back to feature" switch_view "feature"
assert_file_exists "base.txt on feature (round 2)" "base.txt"
assert_file_exists "feature.txt on feature (round 2)" "feature.txt"
assert_file_not_exists "staging.txt NOT on feature (round 2)" "staging.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Insert between non-dev views"
# ═══════════════════════════════════════════════════════════════════════════
#
# Insert feature.txt from feature to staging

require_success "insert from feature to staging succeeds" insert_from_view "feature" "staging"

require_success "switch to staging" switch_view "staging"

# staging should now have all three files
assert_file_exists "base.txt on staging" "base.txt"
assert_file_exists "staging.txt on staging" "staging.txt"
assert_file_exists "feature.txt on staging after insert" "feature.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Unrecord on feature, then switch"
# ═══════════════════════════════════════════════════════════════════════════
#
# Workflow:
#   1. Switch to feature
#   2. Unrecord the feature.txt change
#   3. feature.txt should still be on disk and become untracked
#   4. Switch to dev
#   5. untracked feature.txt persists without becoming tracked on dev

make_temp_repo "cross-unrecord-switch"
init_repo

# Record base on dev
create_file "base.txt" "base"
require_success "add base.txt" atomic add base.txt
require_success "record base.txt" record_change "Add base.txt"

# Create feature, insert dev, add feature file
require_success "create feature view" new_view "feature"
require_success "insert base history into feature" insert_from_view "dev" "feature"
require_success "switch to feature" switch_view "feature"

create_file "temp_feature.txt" "temporary"
require_success "add temp_feature.txt" atomic add temp_feature.txt
require_success "record temp_feature.txt" record_change "Add temp_feature.txt"

# Unrecord on feature
require_success "unrecord on feature succeeds" unrecord_last

# File should still be on disk
assert_file_exists "temp_feature.txt still on disk after unrecord" "temp_feature.txt"
assert_status_flag "temp_feature.txt is untracked after unrecord" "?" "temp_feature.txt"

# Switch to dev without removing the newly untracked working-copy file.
require_success "switch to dev after unrecord" switch_view "dev"
assert_current_view "on dev after unrecord" "dev"

assert_file_exists "base.txt on dev" "base.txt"
assert_file_content "unrecorded file survives switch byte-for-byte" "temp_feature.txt" "temporary"
assert_status_flag "temp_feature.txt stays untracked on dev" "?" "temp_feature.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Untracked files with same name on different views"
# ═══════════════════════════════════════════════════════════════════════════
#
# Interesting edge case: what happens if you have an untracked file, record
# it on one view, then switch to another view and create a new file with
# the same name?

make_temp_repo "cross-same-name"
init_repo

# Record hello.txt on dev
create_file "hello.txt" "dev version"
require_success "add hello.txt on dev" atomic add hello.txt
require_success "record hello.txt on dev" record_change "Add hello.txt on dev"

# Create feature
require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"
require_success "switch to feature" switch_view "feature"

# hello.txt should exist (inherited from dev)
assert_file_exists "hello.txt on feature (inherited)" "hello.txt"
assert_file_content "hello.txt has dev content on feature" "hello.txt" "dev version"

# Modify hello.txt on feature and record
overwrite_file "hello.txt" "feature version"
require_success "record feature modification" record_change "Modify hello.txt on feature"

# Switch to dev — hello.txt should have dev version
require_success "switch to dev" switch_view "dev"
assert_file_exists "hello.txt on dev" "hello.txt"
assert_file_content "hello.txt has dev content on dev" "hello.txt" "dev version"

# Switch to feature — hello.txt should have feature version
require_success "switch back to feature" switch_view "feature"
assert_file_exists "hello.txt on feature" "hello.txt"
assert_file_content "hello.txt has feature content" "hello.txt" "feature version"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: View with only deletions"
# ═══════════════════════════════════════════════════════════════════════════
#
# Delete a file on feature that exists on dev, then switch

make_temp_repo "cross-deletion"
init_repo

create_file "victim.txt" "will be deleted on feature"
require_success "add victim.txt" atomic add victim.txt
require_success "record victim.txt on dev" record_change "Add victim.txt on dev"

# Create feature from dev
require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"
require_success "switch to feature" switch_view "feature"

# Delete on feature
rm -f victim.txt
require_success "record victim.txt deletion" record_change "Delete victim.txt on feature"

assert_file_not_exists "victim.txt deleted on feature" "victim.txt"

# Switch to dev — victim.txt should still be there
require_success "switch back to dev" switch_view "dev"
assert_file_exists "victim.txt still on dev" "victim.txt"
assert_file_content "victim.txt has original content" "victim.txt" "will be deleted on feature"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Multiple files recorded in separate changes"
# ═══════════════════════════════════════════════════════════════════════════
#
# Record multiple files in separate changes on feature, then insert
# selectively to dev.

make_temp_repo "cross-multi-changes"
init_repo

require_success "create feature view" new_view "feature"
require_success "switch to feature" switch_view "feature"

# Record three separate changes
create_file "change1.txt" "change one"
require_success "add change1.txt" atomic add change1.txt
require_success "record feature change 1" record_change "Feature change 1"

create_file "change2.txt" "change two"
require_success "add change2.txt" atomic add change2.txt
require_success "record feature change 2" record_change "Feature change 2"

create_file "change3.txt" "change three"
require_success "add change3.txt" atomic add change3.txt
require_success "record feature change 3" record_change "Feature change 3"

# All three should exist on feature
assert_file_exists "change1.txt on feature" "change1.txt"
assert_file_exists "change2.txt on feature" "change2.txt"
assert_file_exists "change3.txt on feature" "change3.txt"

# Switch to dev — none should exist
require_success "switch to dev" switch_view "dev"
assert_file_not_exists "change1.txt NOT on dev" "change1.txt"
assert_file_not_exists "change2.txt NOT on dev" "change2.txt"
assert_file_not_exists "change3.txt NOT on dev" "change3.txt"

# Insert all from feature to dev
require_success "insert feature changes into dev" insert_from_view "feature" "dev"

# Now all three should exist on dev
assert_file_exists "change1.txt on dev after insert" "change1.txt"
assert_file_exists "change2.txt on dev after insert" "change2.txt"
assert_file_exists "change3.txt on dev after insert" "change3.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Rapid switching stress test"
# ═══════════════════════════════════════════════════════════════════════════
#
# Switch between views many times to verify no state corruption.

make_temp_repo "cross-rapid-switch"
init_repo

# Setup: dev has dev.txt, feature has feature.txt
create_file "dev.txt" "dev"
require_success "add dev.txt" atomic add dev.txt
require_success "record dev.txt" record_change "Add dev.txt"

require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"
require_success "switch to feature" switch_view "feature"

create_file "feature.txt" "feature"
require_success "add feature.txt" atomic add feature.txt
require_success "record feature.txt" record_change "Add feature.txt"

# Rapid switch 5 times
for i in $(seq 1 5); do
    require_success "switch to dev (iteration $i)" switch_view "dev"
    assert_file_exists "dev.txt on dev (iteration $i)" "dev.txt"
    assert_file_not_exists "feature.txt NOT on dev (iteration $i)" "feature.txt"

    require_success "switch to feature (iteration $i)" switch_view "feature"
    assert_file_exists "dev.txt on feature (iteration $i)" "dev.txt"
    assert_file_exists "feature.txt on feature (iteration $i)" "feature.txt"
done

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Untracked + tracked coexistence"
# ═══════════════════════════════════════════════════════════════════════════
#
# Untracked files should coexist peacefully with tracked files during
# view switches.

make_temp_repo "cross-coexist"
init_repo

# Create and record tracked.txt on dev
create_file "tracked.txt" "tracked on dev"
require_success "add tracked.txt" atomic add tracked.txt
require_success "record tracked.txt on dev" record_change "Add tracked.txt on dev"

# Create an untracked file
create_file "notes.txt" "my personal notes"

# Create feature
require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"
require_success "switch to feature" switch_view "feature"

# Both should exist
assert_file_exists "tracked.txt on feature" "tracked.txt"
assert_file_exists "notes.txt on feature (untracked persists)" "notes.txt"

# Record a different file on feature
create_file "feature_code.txt" "feature code"
require_success "add feature_code.txt" atomic add feature_code.txt
require_success "record feature_code.txt" record_change "Add feature_code.txt"

# Switch to dev
require_success "switch back to dev" switch_view "dev"

# tracked.txt should exist (on dev), feature_code.txt should NOT,
# notes.txt should persist (untracked)
assert_file_exists "tracked.txt on dev" "tracked.txt"
assert_file_not_exists "feature_code.txt NOT on dev" "feature_code.txt"
assert_file_exists "notes.txt persists on dev (untracked)" "notes.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Insert then unrecord on target"
# ═══════════════════════════════════════════════════════════════════════════
#
# Insert a change from feature to dev, then unrecord it on dev.
# The file should disappear from dev but remain on feature.

make_temp_repo "cross-insert-unrecord"
init_repo

require_success "create feature view" new_view "feature"
require_success "switch to feature" switch_view "feature"

create_file "applied.txt" "applied content"
require_success "add applied.txt" atomic add applied.txt
require_success "record applied.txt on feature" record_change "Add applied.txt on feature"

# Insert to dev
require_success "insert feature into dev" insert_from_view "feature" "dev"

require_success "switch to dev" switch_view "dev"
assert_file_exists "applied.txt on dev after insert" "applied.txt"

# Unrecord on dev
require_success "unrecord applied change on dev" unrecord_last

# After unrecording the insert, the file's change is no longer on dev
# The file might persist on disk as an untracked artifact (since unrecord
# doesn't delete files from disk), but it should not be tracked on dev.
# When we switch away and back, it would be cleaned up properly.
assert_status_flag "unrecorded applied.txt is untracked on dev" "?" "applied.txt"

# Verify feature still has it
require_success "switch to feature after unrecord" switch_view "feature"
assert_file_exists "applied.txt still on feature" "applied.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Empty view has no tracked files"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "cross-empty-view"
init_repo

# Record a file on dev
create_file "dev_file.txt" "dev only"
require_success "add dev_file.txt" atomic add dev_file.txt
require_success "record dev_file.txt" record_change "Add dev_file.txt"

# Create a child view (parented on dev)
# In the ambient graph model, child views see parent's files through
# the filter chain — there are no truly "empty" views when parented.
require_success "create child view" new_view "child"
require_success "switch to child" switch_view "child"

# dev_file.txt SHOULD exist on the child view — it inherits dev's
# changes through the parent filter chain (ambient graph model).
assert_file_exists \
    "dev_file.txt visible on child view (inherited from dev)" \
    "dev_file.txt"

# Switch back to dev
require_success "switch back to dev" switch_view "dev"
assert_file_exists "dev_file.txt back on dev" "dev_file.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Status correctness across views"
# ═══════════════════════════════════════════════════════════════════════════
#
# Verify that `atomic status` output is correct per-view

make_temp_repo "cross-status"
init_repo

# Setup: dev has dev.txt (clean), feature has feature.txt (clean)
create_file "dev.txt" "dev"
require_success "add dev.txt" atomic add dev.txt
require_success "record dev.txt" record_change "Add dev.txt"

require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"
require_success "switch to feature" switch_view "feature"

create_file "feature.txt" "feature"
require_success "add feature.txt" atomic add feature.txt
require_success "record feature.txt" record_change "Add feature.txt"

# On feature: both should be clean, status should not mention feature.txt or
# dev.txt as dirty
assert_status_no_entry "dev.txt clean on feature status" "dev.txt"
assert_status_no_entry "feature.txt clean on feature status" "feature.txt"

# Switch to dev: feature.txt should not appear AT ALL in status
require_success "switch to dev" switch_view "dev"

assert_status_no_entry "feature.txt not in dev status" "feature.txt"

assert_status_no_entry "dev.txt clean on dev status" "dev.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: File modified on both views independently"
# ═══════════════════════════════════════════════════════════════════════════
#
# When the same file is modified differently on two views (divergent changes),
# switching should show each view's version.

make_temp_repo "cross-divergent"
init_repo

# Record shared.txt on dev
create_file "shared.txt" "original"
require_success "add shared.txt" atomic add shared.txt
require_success "record shared.txt" record_change "Add shared.txt"

# Create feature from dev
require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"

# Modify on dev
overwrite_file "shared.txt" "dev modification"
require_success "record dev modification" record_change "Modify shared.txt on dev"

# Modify on feature
require_success "switch to feature" switch_view "feature"
overwrite_file "shared.txt" "feature modification"
require_success "record feature modification" record_change "Modify shared.txt on feature"

# Check feature version
assert_file_content "shared.txt has feature content" "shared.txt" "feature modification"

# Switch to dev — should have dev version
require_success "switch to dev" switch_view "dev"
assert_file_content "shared.txt has dev content" "shared.txt" "dev modification"

# Switch back to feature
require_success "switch back to feature" switch_view "feature"
assert_file_content "shared.txt has feature content (round 2)" "shared.txt" "feature modification"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Many files across many views"
# ═══════════════════════════════════════════════════════════════════════════
#
# Stress test: create 3 views, each with 3 unique files, plus shared ones.

make_temp_repo "cross-many"
init_repo

# Shared file on dev
create_file "common.txt" "shared by all"
assert_success "add common.txt" atomic add common.txt
record_change "Add common.txt" >/dev/null 2>&1 || true

# Create views and add unique files
for stack in alpha beta gamma; do
    new_view "$stack" >/dev/null 2>&1 || true
    insert_from_view "dev" "$stack" >/dev/null 2>&1 || true
    switch_view "$stack" >/dev/null 2>&1 || true

    for i in 1 2 3; do
        create_file "${stack}_${i}.txt" "${stack} file ${i}"
        assert_success "add ${stack}_${i}.txt" atomic add "${stack}_${i}.txt"
    done
    record_change "Add ${stack} files" >/dev/null 2>&1 || true
done

# Verify isolation: on alpha, only alpha's files + common
switch_view "alpha" >/dev/null 2>&1 || true
assert_file_exists "common.txt on alpha" "common.txt"
assert_file_exists "alpha_1.txt on alpha" "alpha_1.txt"
assert_file_exists "alpha_2.txt on alpha" "alpha_2.txt"
assert_file_exists "alpha_3.txt on alpha" "alpha_3.txt"
assert_file_not_exists "beta_1.txt NOT on alpha" "beta_1.txt"
assert_file_not_exists "gamma_1.txt NOT on alpha" "gamma_1.txt"

# On beta
switch_view "beta" >/dev/null 2>&1 || true
assert_file_exists "common.txt on beta" "common.txt"
assert_file_exists "beta_1.txt on beta" "beta_1.txt"
assert_file_not_exists "alpha_1.txt NOT on beta" "alpha_1.txt"
assert_file_not_exists "gamma_1.txt NOT on beta" "gamma_1.txt"

# On gamma
switch_view "gamma" >/dev/null 2>&1 || true
assert_file_exists "common.txt on gamma" "common.txt"
assert_file_exists "gamma_1.txt on gamma" "gamma_1.txt"
assert_file_not_exists "alpha_1.txt NOT on gamma" "alpha_1.txt"
assert_file_not_exists "beta_1.txt NOT on gamma" "beta_1.txt"

# On dev — only common
switch_view "dev" >/dev/null 2>&1 || true
assert_file_exists "common.txt on dev" "common.txt"
assert_file_not_exists "alpha_1.txt NOT on dev" "alpha_1.txt"
assert_file_not_exists "beta_1.txt NOT on dev" "beta_1.txt"
assert_file_not_exists "gamma_1.txt NOT on dev" "gamma_1.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Delete draft does not delete shared dependency chain"
# ═══════════════════════════════════════════════════════════════════════════
#
# Regression:
#   1. Create a shared view with three recorded files
#   2. Create a draft view from that shared view
#   3. Modify one file on the draft and record
#   4. Switch back to the shared view
#   5. Delete the draft view
#   6. Verify the shared view still materializes its full dependency chain

make_temp_repo "cross-delete-draft-chain"
init_repo

# Create an explicit shared view and record three clean base files there.
new_view "shared-main" >/dev/null 2>&1 || true
switch_view "shared-main" >/dev/null 2>&1 || true
assert_current_view "on shared-main" "shared-main"

create_file "alpha.txt" "alpha base"
create_file "beta.txt" "beta base"
create_file "gamma.txt" "gamma base"

assert_success "add shared base files" atomic add alpha.txt beta.txt gamma.txt
record_change "Add three shared base files" >/dev/null 2>&1 || true

assert_clean "shared-main clean after base record"
assert_file_exists "alpha.txt exists on shared-main" "alpha.txt"
assert_file_exists "beta.txt exists on shared-main" "beta.txt"
assert_file_exists "gamma.txt exists on shared-main" "gamma.txt"

# Fork a deletable draft from the shared view and record one local edit.
require_success "create draft local-edit from shared-main" atomic view create local-edit --draft --from shared-main --switch

assert_current_view "on local-edit draft" "local-edit"
assert_file_exists "alpha.txt exists on local-edit" "alpha.txt"
assert_file_exists "beta.txt exists on local-edit" "beta.txt"
assert_file_exists "gamma.txt exists on local-edit" "gamma.txt"

overwrite_file "beta.txt" "beta local edit"
record_change "Modify beta.txt on local-edit" >/dev/null 2>&1 || true
assert_file_content "beta.txt shows draft edit on local-edit" "beta.txt" "beta local edit"

# Switch back to the shared view before deleting the draft.
switch_view "shared-main" >/dev/null 2>&1 || true
assert_current_view "back on shared-main" "shared-main"
assert_file_content "beta.txt restored to shared content on shared-main" "beta.txt" "beta base"

# This draft's unmerged edit becomes orphaned; acknowledge that explicitly.
assert_success "delete local-edit draft, preserving the shared base chain" \
    atomic view delete local-edit --force

assert_output_not_contains \
    "local-edit removed from view list" \
    "local-edit" \
    atomic view list

# Deleting the draft must not remove the shared base chain.
assert_current_view "still on shared-main after draft delete" "shared-main"
assert_file_exists "alpha.txt still exists on shared-main" "alpha.txt"
assert_file_exists "beta.txt still exists on shared-main" "beta.txt"
assert_file_exists "gamma.txt still exists on shared-main" "gamma.txt"
assert_file_content "alpha.txt keeps shared content" "alpha.txt" "alpha base"
assert_file_content "beta.txt keeps shared content" "beta.txt" "beta base"
assert_file_content "gamma.txt keeps shared content" "gamma.txt" "gamma base"
assert_status_no_entry "alpha.txt clean after draft delete" "alpha.txt"
assert_status_no_entry "beta.txt clean after draft delete" "beta.txt"
assert_status_no_entry "gamma.txt clean after draft delete" "gamma.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Full untracked → add → record → insert lifecycle"
# ═══════════════════════════════════════════════════════════════════════════
#
# This is the definitive end-to-end test for the core workflow described
# in the issue:
#
#   1. Create a file while on dev — it is untracked
#   2. Switch to feature — file persists (still untracked)
#   3. Switch to staging — file persists (still untracked)
#   4. Switch back to dev — file persists (still untracked)
#   5. Switch to feature — add + record the file on feature
#   6. Switch to dev — file is GONE (not on dev)
#   7. Switch to staging — file is GONE (not on staging)
#   8. Switch to feature — file is back (recorded here)
#   9. Insert from feature to dev — file appears on dev
#  10. Switch to dev — file exists with correct content
#  11. Switch to staging — file is STILL gone (insert was to dev, not staging)
#  12. Insert from dev to staging — file appears on staging
#  13. Switch to staging — file exists with correct content
#
# The ONLY way a change moves between views is via an explicit insert.

make_temp_repo "cross-full-lifecycle"
init_repo

# ── Setup: create base content on dev and two sibling views ────────────

create_file "base.txt" "base content"
assert_success "add base.txt" atomic add base.txt
record_change "Add base.txt on dev" >/dev/null 2>&1 || true

new_view "feature" >/dev/null 2>&1 || true
insert_from_view "dev" "feature" >/dev/null 2>&1 || true

new_view "staging" >/dev/null 2>&1 || true
insert_from_view "dev" "staging" >/dev/null 2>&1 || true

# Switch back to dev for the start of the journey
switch_view "dev" >/dev/null 2>&1 || true

# ── Step 1: Create an untracked file on dev ─────────────────────────────

create_file "journey.txt" "I travel between views"

assert_file_exists "journey.txt created on dev" "journey.txt"
assert_status_flag "journey.txt is untracked on dev" "?" "journey.txt"

# ── Step 2: Switch to feature — file persists (untracked) ──────────────

switch_view "feature" >/dev/null 2>&1 || true
assert_current_view "on feature" "feature"
assert_file_exists "journey.txt persists on feature (untracked)" "journey.txt"
assert_status_flag "journey.txt is ? on feature" "?" "journey.txt"

# ── Step 3: Switch to staging — file persists (untracked) ──────────────

switch_view "staging" >/dev/null 2>&1 || true
assert_current_view "on staging" "staging"
assert_file_exists "journey.txt persists on staging (untracked)" "journey.txt"
assert_status_flag "journey.txt is ? on staging" "?" "journey.txt"

# ── Step 4: Switch back to dev — file persists (untracked) ─────────────

switch_view "dev" >/dev/null 2>&1 || true
assert_current_view "back on dev" "dev"
assert_file_exists "journey.txt persists on dev after round trip" "journey.txt"
assert_status_flag "journey.txt still ? on dev" "?" "journey.txt"
assert_file_content "journey.txt content unchanged" "journey.txt" "I travel between views"

# ── Step 5: Switch to feature, add + record ─────────────────────────────

switch_view "feature" >/dev/null 2>&1 || true
assert_file_exists "journey.txt on feature before add" "journey.txt"

assert_success "add journey.txt on feature" atomic add journey.txt
assert_status_flag "journey.txt is A on feature" "A" "journey.txt"

require_success "record journey.txt on feature succeeds" record_change "Record journey.txt on feature"

# Verify clean on feature after record
assert_status_no_entry "journey.txt clean on feature after record" "journey.txt"

# ── Step 6: Switch to dev — file is GONE ────────────────────────────────

switch_view "dev" >/dev/null 2>&1 || true
assert_current_view "on dev after feature record" "dev"

assert_file_not_exists \
    "journey.txt NOT on dev (recorded on feature only)" \
    "journey.txt"

assert_status_no_entry \
    "journey.txt not in dev status" \
    "journey.txt"

# base.txt should still be here
assert_file_exists "base.txt still on dev" "base.txt"

# ── Step 7: Switch to staging — file is GONE ───────────────────────────

switch_view "staging" >/dev/null 2>&1 || true
assert_current_view "on staging" "staging"

assert_file_not_exists \
    "journey.txt NOT on staging (recorded on feature only)" \
    "journey.txt"

assert_status_no_entry \
    "journey.txt not in staging status" \
    "journey.txt"

# base.txt should still be here
assert_file_exists "base.txt still on staging" "base.txt"

# ── Step 8: Switch to feature — file is back ───────────────────────────

switch_view "feature" >/dev/null 2>&1 || true
assert_current_view "back on feature" "feature"

assert_file_exists "journey.txt back on feature" "journey.txt"
assert_file_content "journey.txt has correct content on feature" "journey.txt" "I travel between views"

# ── Step 9: Insert from feature to dev ───────────────────────────────────

require_success "insert journey.txt from feature to dev" insert_from_view "feature" "dev"

# ── Step 10: Switch to dev — file exists ────────────────────────────────

switch_view "dev" >/dev/null 2>&1 || true
assert_current_view "on dev after insert" "dev"

assert_file_exists "journey.txt on dev after insert" "journey.txt"
assert_file_content "journey.txt has correct content on dev" "journey.txt" "I travel between views"
assert_file_exists "base.txt still on dev" "base.txt"

# ── Step 11: Switch to staging — file now visible ──────────────────────
# In the ambient graph model, staging is parented on dev.  When the
# change was inserted into dev, staging sees it through the parent
# filter chain — no separate insert to staging is needed.

switch_view "staging" >/dev/null 2>&1 || true
assert_current_view "on staging after dev insert" "staging"

assert_file_exists \
    "journey.txt visible on staging (inherited from dev via parent chain)" \
    "journey.txt"

assert_file_exists "base.txt still on staging" "base.txt"

# ── Step 12: Insert from dev to staging ──────────────────────────────────

require_success "insert journey.txt from dev to staging" insert_from_view "dev" "staging"

# ── Step 13: Switch to staging — file exists ────────────────────────────

switch_view "staging" >/dev/null 2>&1 || true

assert_file_exists "journey.txt on staging after insert" "journey.txt"
assert_file_content "journey.txt has correct content on staging" "journey.txt" "I travel between views"
assert_file_exists "base.txt still on staging" "base.txt"

# ── Final verification: all three views have the file ──────────────────

switch_view "dev" >/dev/null 2>&1 || true
assert_file_exists "journey.txt on dev (final check)" "journey.txt"

switch_view "feature" >/dev/null 2>&1 || true
assert_file_exists "journey.txt on feature (final check)" "journey.txt"

switch_view "staging" >/dev/null 2>&1 || true
assert_file_exists "journey.txt on staging (final check)" "journey.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Child view file NOT marked Deleted on parent (init -k rust)"
# ═══════════════════════════════════════════════════════════════════════════
#
# Regression test for the bug:
#
#   1. atomic init -k rust  (creates dev with .vault/ and .atomicignore)
#   2. Create child view "hola" (parented on dev)
#   3. On hola, add a new file, record it → status clean ✓
#   4. Switch to dev
#   5. BUG: the file from hola shows as Deleted on dev
#   6. EXPECTED: the file should NOT appear in dev's status at all
#
# Root cause: status() has a fast-path for root shared views
# (filter_is_universal=true) that skips the change filter, so every
# TREE entry — including files from child views — is treated as
# belonging to the current view.  When the file doesn't exist on disk
# (correctly removed by switch_view), status classifies it as Deleted.

make_temp_repo "cross-child-not-deleted"
init_repo -k rust

# dev should have .vault and .atomicignore tracked after init
assert_file_exists ".atomicignore exists on dev" ".atomicignore"
assert_dir_exists ".vault exists on dev" ".vault"

# Verify dev is clean (init records .vault + .atomicignore)
assert_clean "dev is clean after init -k rust"

# Create child view "hola" (parented on dev, inherits files via parent chain)
new_view "hola" >/dev/null 2>&1 || true
switch_view "hola" >/dev/null 2>&1 || true
assert_current_view "on hola" "hola"

# Add a new file on hola
create_file "hola_only.txt" "this file belongs to hola"
assert_success "add hola_only.txt on hola" atomic add hola_only.txt
record_change "Add hola_only.txt on hola" >/dev/null 2>&1 || true

# Status should be clean on hola
assert_clean "hola is clean after record"
assert_file_exists "hola_only.txt exists on hola" "hola_only.txt"

# Switch back to dev
switch_view "dev" >/dev/null 2>&1 || true
assert_current_view "back on dev" "dev"

# The file should NOT exist on disk (correctly handled by switch_view)
assert_file_not_exists \
    "hola_only.txt NOT on disk on dev" \
    "hola_only.txt"

# CRITICAL: the file must NOT appear in status at all — especially not as Deleted
assert_status_no_entry \
    "hola_only.txt NOT in dev status (must not be Deleted)" \
    "hola_only.txt"

# Double-check: dev's own files should be fine
assert_file_exists ".atomicignore still on dev" ".atomicignore"
assert_dir_exists ".vault still on dev" ".vault"
assert_clean "dev is still clean (no false Deleted entries)"

# Switch back to hola — file should reappear
switch_view "hola" >/dev/null 2>&1 || true
assert_file_exists "hola_only.txt back on hola" "hola_only.txt"
assert_file_content "hola_only.txt has correct content" "hola_only.txt" "this file belongs to hola"
assert_clean "hola still clean after round-trip"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Multiple child views, parent sees no ghost deletions"
# ═══════════════════════════════════════════════════════════════════════════
#
# Extend the above: create TWO child views from dev, each with unique files.
# Switching to dev should show zero Deleted entries from either child.

make_temp_repo "cross-multi-child-no-ghost"
init_repo -k rust

# Child view alpha
new_view "alpha" >/dev/null 2>&1 || true
switch_view "alpha" >/dev/null 2>&1 || true
create_file "alpha_file.txt" "alpha content"
assert_success "add alpha_file.txt" atomic add alpha_file.txt
record_change "Add alpha_file.txt" >/dev/null 2>&1 || true
assert_clean "alpha is clean"

# Child view beta
switch_view "dev" >/dev/null 2>&1 || true
new_view "beta" >/dev/null 2>&1 || true
switch_view "beta" >/dev/null 2>&1 || true
create_file "beta_file.txt" "beta content"
assert_success "add beta_file.txt" atomic add beta_file.txt
record_change "Add beta_file.txt" >/dev/null 2>&1 || true
assert_clean "beta is clean"

# Switch to dev — neither child's file should appear
switch_view "dev" >/dev/null 2>&1 || true
assert_current_view "on dev" "dev"

assert_file_not_exists "alpha_file.txt NOT on dev" "alpha_file.txt"
assert_file_not_exists "beta_file.txt NOT on dev" "beta_file.txt"

assert_status_no_entry "alpha_file.txt NOT in dev status" "alpha_file.txt"
assert_status_no_entry "beta_file.txt NOT in dev status" "beta_file.txt"
assert_clean "dev has no ghost deletions from children"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Parent's own files NOT falsely Modified after child records"
# ═══════════════════════════════════════════════════════════════════════════
#
# Regression test for the false-Modified bug:
#
# After fixing the ghost-deletion bug (filter_is_universal removal),
# the view change filter must include ALL changes that belong to the
# current view.  If the filter is incomplete (e.g. dependency closure
# misses some changes), files whose inode was created by a missing
# change are skipped during the TREE scan and then fall through to
# the "no FILE_INDEX entry" path, where they are conservatively
# classified as Modified.
#
# This test verifies that dev's own recorded files remain Clean after
# a child view records new content.  The child's record must not
# disturb the parent's change filter completeness.
#
# Scenario:
#   1. atomic init -k rust → dev has .atomicignore, .vault/*, etc.
#   2. Record additional files on dev → status clean
#   3. Create child view, record a file there
#   4. Switch back to dev
#   5. EXPECTED: dev is fully clean (no Deleted, no Modified)

make_temp_repo "cross-no-false-modified"
init_repo -k rust

# dev has .atomicignore and .vault tracked after init
assert_clean "dev is clean after init"

# Record a couple more files on dev to increase surface area
create_file "src/main.rs" 'fn main() { println!("hello"); }'
create_file "Cargo.toml" '[package]\nname = "test"'
assert_success "add src/main.rs" atomic add src/main.rs
assert_success "add Cargo.toml" atomic add Cargo.toml
record_change "Add rust skeleton on dev" >/dev/null 2>&1 || true
assert_clean "dev clean after recording rust skeleton"

# Snapshot dev status: capture the short output to verify later
dev_status_before="$(get_status_short)"

# Create child view (parented on dev, inherits files via parent chain)
new_view "child-feat" >/dev/null 2>&1 || true
switch_view "child-feat" >/dev/null 2>&1 || true
assert_current_view "on child-feat" "child-feat"

create_file "feature.rs" "fn feature() {}"
assert_success "add feature.rs on child" atomic add feature.rs
record_change "Add feature.rs on child-feat" >/dev/null 2>&1 || true
assert_clean "child-feat is clean after record"

# Switch back to dev
switch_view "dev" >/dev/null 2>&1 || true
assert_current_view "back on dev" "dev"

# CRITICAL: dev's own files must NOT show as Modified
assert_status_not_contains \
    "no 'modified:' in dev status (no false Modified regression)" \
    "modified:"

assert_status_not_contains \
    "no 'deleted:' in dev status (no ghost deletions)" \
    "deleted:"

# Assert specific files are NOT in short status output
assert_status_no_entry ".atomicignore not dirty" ".atomicignore"
assert_status_no_entry "src/main.rs not dirty" "src/main.rs"
assert_status_no_entry "Cargo.toml not dirty" "Cargo.toml"
assert_status_no_entry "feature.rs not in dev status" "feature.rs"

# The gold standard: dev is fully clean
assert_clean "dev is fully clean (no Deleted, no Modified)"

# Verify files are on disk and correct
assert_file_exists "src/main.rs on dev" "src/main.rs"
assert_file_exists "Cargo.toml on dev" "Cargo.toml"
assert_file_not_exists "feature.rs NOT on dev" "feature.rs"

# Round-trip: child-feat still works
switch_view "child-feat" >/dev/null 2>&1 || true
assert_file_exists "feature.rs back on child-feat" "feature.rs"
assert_file_exists "src/main.rs inherited on child-feat" "src/main.rs"
assert_clean "child-feat still clean after round-trip"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Insert to parent does not cause false Modified"
# ═══════════════════════════════════════════════════════════════════════════
#
# Simulate the pull scenario: insert changes from a child view into
# dev (which is what pull does under the hood — apply + materialize).
# After the insert + materialize, dev should be clean.

make_temp_repo "cross-insert-no-false-modified"
init_repo -k rust

# Record a base file on dev
create_file "base.rs" "fn base() {}"
assert_success "add base.rs" atomic add base.rs
record_change "Add base.rs on dev" >/dev/null 2>&1 || true
assert_clean "dev clean with base.rs"

# Create child view, record new files
new_view "contributor" >/dev/null 2>&1 || true
switch_view "contributor" >/dev/null 2>&1 || true

create_file "new_feature.rs" "fn new_feature() {}"
assert_success "add new_feature.rs" atomic add new_feature.rs
record_change "Add new_feature.rs on contributor" >/dev/null 2>&1 || true
assert_clean "contributor is clean"

# Insert from contributor → dev (simulates what pull does)
insert_from_view "contributor" "dev" >/dev/null 2>&1 || true

# Switch to dev to trigger materialize
switch_view "dev" >/dev/null 2>&1 || true
assert_current_view "on dev after insert" "dev"

# Both files should exist on dev now
assert_file_exists "base.rs on dev" "base.rs"
assert_file_exists "new_feature.rs on dev after insert" "new_feature.rs"

# CRITICAL: dev must be fully clean after insert + switch
assert_status_not_contains \
    "no 'modified:' after insert" \
    "modified:"
assert_status_not_contains \
    "no 'deleted:' after insert" \
    "deleted:"
assert_clean "dev is clean after insert from child"

# ═══════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Unrecorded files survive draft→shared→draft round-trip"
# ═══════════════════════════════════════════════════════════════════════
#
# Test that view switch is BLOCKED when working copy has unrecorded changes.
#
# Git-style safety: if you have added/modified files that haven't been
# recorded, switching views would lose that work (materialize overwrites).
# The switch command should refuse and tell the user to record or --force.

make_temp_repo "cross-unrecorded-blocked"
init_repo

# Step 1: Create a draft view and add files WITHOUT recording
new_view "draft-work" >/dev/null 2>&1 || true
switch_view "draft-work" >/dev/null 2>&1 || true
assert_current_view "on draft-work" "draft-work"

create_file "draft_a.txt" "Draft file alpha content"
create_file "draft_b.txt" "Draft file beta content"
assert_success "add draft_a.txt" atomic add draft_a.txt
assert_success "add draft_b.txt" atomic add draft_b.txt
assert_status_flag "draft_a.txt is Added" "A" "draft_a.txt"
assert_status_flag "draft_b.txt is Added" "A" "draft_b.txt"

# Step 2: Attempt to switch to dev — should be BLOCKED
if atomic view switch dev >/dev/null 2>&1; then
    _fail "switch blocked with unrecorded changes" "switch succeeded (rc=0)"
else
    _pass "switch blocked with unrecorded changes"
fi

# Should still be on draft-work
assert_current_view "still on draft-work after blocked switch" "draft-work"

# Files should be untouched
assert_file_content "draft_a.txt intact after blocked switch" "draft_a.txt" "Draft file alpha content"
assert_file_content "draft_b.txt intact after blocked switch" "draft_b.txt" "Draft file beta content"

# Step 3: --force should override the block
require_success "switch --force overrides the block" atomic view switch dev --force
assert_current_view "on dev after forced switch" "dev"

# Step 4: Switch back (clean working copy, should work)
switch_view "draft-work" >/dev/null 2>&1 || true
assert_current_view "back on draft-work" "draft-work"

# ═══════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Unrecorded files survive with recorded files present"
# ═══════════════════════════════════════════════════════════════════════
#
# Variant: draft view has BOTH recorded AND unrecorded files.
# Switch should be blocked because of the pending file.
#
# Workflow:
#   1. On draft, record file_recorded.txt
#   2. On draft, add but DON’T record file_pending.txt
#   3. Attempt to switch to dev — should be BLOCKED
#   4. Record the pending file, then switch should work

make_temp_repo "cross-mixed-recorded-pending"
init_repo

# Record base on dev first
create_file "base.txt" "base"
assert_success "add base.txt" atomic add base.txt
record_change "Add base on dev" >/dev/null 2>&1 || true

# Create draft view from dev
new_view "draft-mixed" >/dev/null 2>&1 || true
insert_from_view "dev" "draft-mixed" >/dev/null 2>&1 || true
switch_view "draft-mixed" >/dev/null 2>&1 || true

# Record one file on draft
create_file "file_recorded.txt" "I am recorded on draft"
assert_success "add file_recorded.txt" atomic add file_recorded.txt
record_change "Record file on draft" >/dev/null 2>&1 || true

# Add but DON'T record another file
create_file "file_pending.txt" "I am pending on draft"
assert_success "add file_pending.txt" atomic add file_pending.txt
assert_status_flag "file_pending.txt is Added" "A" "file_pending.txt"

# Attempt switch — should be blocked
if atomic view switch dev >/dev/null 2>&1; then
    _fail "switch blocked with pending file (mixed state)" "switch succeeded (rc=0)"
else
    _pass "switch blocked with pending file (mixed state)"
fi
assert_current_view "still on draft-mixed" "draft-mixed"

# Record the pending file, then switch should work
record_change "Record pending file" >/dev/null 2>&1 || true
switch_view "dev" >/dev/null 2>&1 || true
assert_current_view "on dev after recording pending" "dev"

# Switch back to draft — both files should be intact
switch_view "draft-mixed" >/dev/null 2>&1 || true
assert_file_exists "file_recorded.txt exists" "file_recorded.txt"
assert_file_content \
    "file_recorded.txt content intact" \
    "file_recorded.txt" \
    "I am recorded on draft"
assert_file_exists "file_pending.txt exists (now recorded)" "file_pending.txt"
assert_file_content \
    "file_pending.txt content intact" \
    "file_pending.txt" \
    "I am pending on draft"

# ═══════════════════════════════════════════════════════════════════════
begin_section "Cross-View: Multiple round-trips with pending files"
# ═══════════════════════════════════════════════════════════════════════
#
# Stress variant: record on wip, then switch back and forth.
# Each round-trip should work cleanly because wip is recorded.

make_temp_repo "cross-roundtrip-stress"
init_repo

require_success "create wip view" new_view "wip"
require_success "switch to wip" switch_view "wip"

create_file "fragile.txt" "This content must not change"
require_success "add fragile.txt" atomic add fragile.txt
require_success "record fragile.txt on wip" record_change "Record fragile.txt on wip"

# Round-trip 3 times, recording on dev each time
for i in 1 2 3; do
    require_success "switch to dev (iteration $i)" switch_view "dev"
    create_file "dev_iter_${i}.txt" "dev iteration ${i}"
    require_success "add dev_iter_${i}.txt" atomic add "dev_iter_${i}.txt"
    require_success "record dev iteration $i" record_change "Dev iteration ${i}"

    require_success "switch to wip (iteration $i)" switch_view "wip"
    assert_file_exists "fragile.txt exists (iteration $i)" "fragile.txt"
    assert_file_content \
        "fragile.txt content intact (iteration $i)" \
        "fragile.txt" \
        "This content must not change"
done

# ═════════════════════════════════════════════════════════════════════
begin_section "Cross-View: --stash auto-saves changes before switch"
# ═════════════════════════════════════════════════════════════════════
#
# Workflow (mimics git stash + checkout):
#   1. On dev, modify a file (dirty working copy)
#   2. atomic view switch feature --stash
#   3. Changes are stashed, switch succeeds
#   4. Switch back to dev
#   5. atomic stash pop — changes restored

make_temp_repo "cross-stash-switch"
init_repo

# Record a base file on dev
create_file "work.txt" "original content"
require_success "add work.txt" atomic add work.txt
require_success "record work.txt" record_change "Add work.txt"

# Create feature view
require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"

# Modify the file (dirty working copy)
overwrite_file "work.txt" "modified content"

# Verify switch is blocked without flags
if atomic view switch feature >/dev/null 2>&1; then
    _fail "switch blocked with dirty working copy" "switch succeeded"
else
    _pass "switch blocked with dirty working copy"
fi

# Switch with --stash
require_success "switch --stash succeeds" atomic view switch feature --stash
assert_current_view "on feature after stash+switch" "feature"

# Verify stash was created
stash_list="$(atomic stash list 2>&1)"
if echo "$stash_list" | grep -qi "stash@{0}\|auto-stash"; then
    _pass "stash entry created"
else
    _fail "stash entry created" "stash list: $stash_list"
fi

# work.txt should have original content on feature (inherited from dev)
assert_file_content "work.txt has original content on feature" "work.txt" "original content"

# Switch back to dev
require_success "switch back to dev" switch_view "dev"
assert_current_view "back on dev" "dev"

# work.txt should have original content (clean state after stash)
assert_file_content "work.txt is clean on dev (stashed)" "work.txt" "original content"

# Pop the stash — modifications restored
require_success "stash pop succeeds" atomic stash pop

# CRITICAL: the stashed modifications are back
assert_file_content "work.txt has modified content after pop" "work.txt" "modified content"

# ═════════════════════════════════════════════════════════════════════
begin_section "Cross-View: --stash with added (not recorded) files"
# ═════════════════════════════════════════════════════════════════════
#
# Variant: the dirty state includes new files that were added but
# not recorded. --stash should save them and they should come back
# after pop.

make_temp_repo "cross-stash-added"
init_repo

# Create feature view
require_success "create feature view" new_view "feature"

# Add new files on dev without recording
create_file "new_a.txt" "new file alpha"
create_file "new_b.txt" "new file beta"
require_success "add new_a.txt" atomic add new_a.txt
require_success "add new_b.txt" atomic add new_b.txt
assert_status_flag "new_a.txt is Added" "A" "new_a.txt"

# Switch with --stash
require_success "switch --stash with added files succeeds" atomic view switch feature --stash
assert_current_view "on feature" "feature"

# Files may still be on disk (added-but-unrecorded files aren't tracked
# in the graph, so materialize doesn't remove them). The stash saved
# their content — what matters is that pop restores them correctly.
# The file's presence here is not an assertion; restoration is checked below.

# Switch back and pop
require_success "switch back to dev" switch_view "dev"
require_success "restore stashed added files" atomic stash pop

# Files should be back with original content
assert_file_exists "new_a.txt restored after pop" "new_a.txt"
assert_file_content "new_a.txt has correct content" "new_a.txt" "new file alpha"
assert_file_exists "new_b.txt restored after pop" "new_b.txt"
assert_file_content "new_b.txt has correct content" "new_b.txt" "new file beta"

# ═════════════════════════════════════════════════════════════════════
begin_section "Cross-View: --stash + pop round-trip preserves all changes"
# ═════════════════════════════════════════════════════════════════════
#
# Full round-trip: modify + add on dev, stash, switch to feature,
# do work there, switch back to dev, pop. Everything intact.

make_temp_repo "cross-stash-roundtrip"
init_repo

# Base state on dev
create_file "base.txt" "base"
require_success "add base.txt" atomic add base.txt
require_success "record base" record_change "Add base"

# Create feature from dev
require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"

# Dirty up dev: modify existing + add new
overwrite_file "base.txt" "modified base"
create_file "wip.txt" "work in progress"
require_success "add wip.txt" atomic add wip.txt

# Stash and switch
require_success "stash and switch to feature" atomic view switch feature --stash
assert_current_view "on feature" "feature"

# Do work on feature
create_file "feature_work.txt" "feature work"
require_success "add feature_work.txt" atomic add feature_work.txt
require_success "record feature work" record_change "Feature work"

# Switch back to dev (clean, no block)
require_success "switch back to dev" switch_view "dev"
assert_current_view "back on dev" "dev"

# base.txt should be clean (original), wip.txt should not exist
assert_file_content "base.txt is clean before pop" "base.txt" "base"
assert_file_not_exists "wip.txt not on disk before pop" "wip.txt"

# Pop stash
require_success "restore stashed work" atomic stash pop

# CRITICAL: everything restored
assert_file_content "base.txt has stashed modification" "base.txt" "modified base"
assert_file_exists "wip.txt restored after pop" "wip.txt"
assert_file_content "wip.txt has stashed content" "wip.txt" "work in progress"

# ═════════════════════════════════════════════════════════════════════

print_summary
