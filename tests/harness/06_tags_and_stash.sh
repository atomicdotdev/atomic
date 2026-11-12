#!/usr/bin/env bash
# 06_tags_and_stash.sh — Tag and stash lifecycle tests.
#
# Tags are named state snapshots — lightweight references to a view's
# Merkle state at a point in time.  Stash temporarily saves uncommitted
# working-copy changes to an orphan view, restores the working copy to
# a clean state, and can replay those changes later.
#
# These tests verify:
#
#   - Tag create / list / show / delete
#   - Tags are scoped to the view they were created on
#   - Tags survive view switches
#   - Stash push saves dirty state and restores clean working copy
#   - Stash pop restores the dirty state and removes the stash
#   - Stash apply restores without removing
#   - Stash list / drop / clear
#   - Stash across view switches
#   - Multiple stashes (LIFO ordering)
#   - Stash with untracked files
#   - Stash + tag interaction (tag before stash, pop after)

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Tags: Create and list"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "tag-basic"
init_repo

create_file "v1.txt" "version 1"
require_success "add v1.txt" atomic add v1.txt
require_success "record v1" record_change "Release v1"

# Create a tag
require_capture "create tag v1.0" atomic tag create v1.0
tag_out="$COMMAND_OUTPUT"
if echo "$tag_out" | grep -qiE "created|tag|v1.0"; then
    _pass "tag create v1.0 succeeds"
else
    _fail "tag create v1.0 succeeds" "got: $(echo "$tag_out" | head -3)"
fi

# List tags — should contain v1.0
require_capture "list tags" atomic tag list
list_out="$COMMAND_OUTPUT"
if echo "$list_out" | grep -qF "v1.0"; then
    _pass "tag list shows v1.0"
else
    _fail "tag list shows v1.0" "got: $(echo "$list_out" | head -5)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Tags: Show tag detail"
# ═══════════════════════════════════════════════════════════════════════════

require_capture "show tag v1.0" atomic tag show v1.0
show_out="$COMMAND_OUTPUT"
if echo "$show_out" | grep -qF "v1.0"; then
    _pass "tag show displays tag name"
else
    _fail "tag show displays tag name" "got: $(echo "$show_out" | head -5)"
fi

if echo "$show_out" | grep -qiE "view.*dev|dev"; then
    _pass "tag show mentions the view"
else
    _fail "tag show mentions the view" "got: $show_out"
fi

if echo "$show_out" | grep -qiE "state|merkle|sequence"; then
    _pass "tag show includes state info"
else
    _fail "tag show includes state info" "got: $show_out"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Tags: Multiple tags"
# ═══════════════════════════════════════════════════════════════════════════

create_file "v2.txt" "version 2"
require_success "add v2.txt" atomic add v2.txt
require_success "record v2" record_change "Release v2"

require_capture "create tag v2.0" atomic tag create v2.0
tag_out2="$COMMAND_OUTPUT"
if echo "$tag_out2" | grep -qiE "created|tag|v2.0"; then
    _pass "tag create v2.0 succeeds"
else
    _fail "tag create v2.0 succeeds" "got: $(echo "$tag_out2" | head -3)"
fi

require_capture "list both tags" atomic tag list
list_out2="$COMMAND_OUTPUT"
found_tags=0
for t in v1.0 v2.0; do
    if echo "$list_out2" | grep -qF "$t"; then
        found_tags=$((found_tags + 1))
    fi
done
if [[ $found_tags -eq 2 ]]; then
    _pass "tag list shows both v1.0 and v2.0"
else
    _fail "tag list shows both tags" "found $found_tags/2"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Tags: Duplicate tag name fails"
# ═══════════════════════════════════════════════════════════════════════════

if dup_out="$(atomic tag create v1.0 2>&1)"; then
    _fail "duplicate tag name rejected" "command succeeded: $dup_out"
elif echo "$dup_out" | grep -qiE "already|exists|error|duplicate"; then
    _pass "duplicate tag name rejected"
else
    _fail "duplicate tag name rejected" "got: $(echo "$dup_out" | head -3)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Tags: Delete a tag"
# ═══════════════════════════════════════════════════════════════════════════

require_capture "delete tag v1.0" atomic tag delete v1.0
del_out="$COMMAND_OUTPUT"
if echo "$del_out" | grep -qiE "deleted|removed|v1.0"; then
    _pass "tag delete v1.0 succeeds"
else
    _fail "tag delete v1.0 succeeds" "got: $(echo "$del_out" | head -3)"
fi

# v1.0 should be gone, v2.0 should remain
require_capture "list tags after delete" atomic tag list
list_after="$COMMAND_OUTPUT"
if echo "$list_after" | grep -qF "v1.0"; then
    _fail "v1.0 gone after delete" "still present"
else
    _pass "v1.0 gone after delete"
fi
if echo "$list_after" | grep -qF "v2.0"; then
    _pass "v2.0 still present after deleting v1.0"
else
    _fail "v2.0 still present" "not found"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Tags: Delete nonexistent tag is idempotent"
# ═══════════════════════════════════════════════════════════════════════════

require_capture "delete nonexistent tag" atomic tag delete no-such-tag
del_missing="$COMMAND_OUTPUT"
if echo "$del_missing" | grep -qiE "not found|does not exist"; then
    _pass "delete nonexistent tag reports absence"
else
    _fail "delete nonexistent tag reports absence" "got: $del_missing"
fi
assert_output_contains "existing v2 tag remains" "v2.0" atomic tag list

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Tags: Tags survive view switches"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "tag-switch"
init_repo

create_file "base.txt" "base"
require_success "add base.txt" atomic add base.txt
require_success "record base" record_change "base commit"
require_success "create release-1 tag" atomic tag create release-1

require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"
require_success "switch to feature" switch_view "feature"

# Tag should still be visible from feature
require_capture "list tags on feature" atomic tag list
list_from_feature="$COMMAND_OUTPUT"
if echo "$list_from_feature" | grep -qF "release-1"; then
    _pass "tag visible from feature view"
else
    _fail "tag visible from feature view" "not found"
fi

# Switch back to dev — tag still there
require_success "switch to dev" switch_view "dev"
require_capture "list tags on dev" atomic tag list
list_back="$COMMAND_OUTPUT"
if echo "$list_back" | grep -qF "release-1"; then
    _pass "tag persists after view round-trip"
else
    _fail "tag persists after view round-trip" "not found"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Tags: Tag on feature view"
# ═══════════════════════════════════════════════════════════════════════════

# Continuing from previous
require_success "switch to feature" switch_view "feature"
create_file "feat.txt" "feature"
require_success "add feat.txt" atomic add feat.txt
require_success "record feature work" record_change "feature work"
require_success "create feature tag" atomic tag create feature-done

require_capture "list feature tags" atomic tag list
list_feat="$COMMAND_OUTPUT"
if echo "$list_feat" | grep -qF "feature-done"; then
    _pass "tag created on feature view"
else
    _fail "tag created on feature view" "not found"
fi

# Both tags should be visible
found_both=0
for t in release-1 feature-done; do
    if echo "$list_feat" | grep -qF "$t"; then
        found_both=$((found_both + 1))
    fi
done
if [[ $found_both -eq 2 ]]; then
    _pass "both tags visible ($found_both/2)"
else
    _fail "both tags visible" "found $found_both/2"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Stash: Basic push and pop"
# ═══════════════════════════════════════════════════════════════════════════
#
# Core stash workflow:
#   1. Make dirty changes
#   2. `stash` saves them and restores clean state
#   3. `stash pop` restores the dirty state

make_temp_repo "stash-basic"
init_repo

create_file "main.rs" "fn main() { v1 }"
require_success "add main.rs" atomic add main.rs
require_success "record initial main.rs" record_change "Initial main.rs"

# Dirty the file
overwrite_file "main.rs" "fn main() { dirty }"
assert_status_flag "main.rs is modified" "M" "main.rs"

# Stash
require_capture "stash modified main.rs" atomic stash
stash_out="$COMMAND_OUTPUT"
if echo "$stash_out" | grep -qiE "saved|stash"; then
    _pass "stash push succeeds"
else
    _fail "stash push succeeds" "got: $(echo "$stash_out" | head -3)"
fi

# Working copy should be clean (restored to v1)
assert_file_content "main.rs restored to v1 after stash" "main.rs" "fn main() { v1 }"

assert_status_no_entry "main.rs clean after stash" "main.rs"

# Pop
require_capture "pop main.rs stash" atomic stash pop
pop_out="$COMMAND_OUTPUT"
if echo "$pop_out" | grep -qiE "applied|pop|stash"; then
    _pass "stash pop succeeds"
else
    _fail "stash pop succeeds" "got: $(echo "$pop_out" | head -3)"
fi

# Dirty content should be back
assert_file_content "main.rs has dirty content after pop" "main.rs" "fn main() { dirty }"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Stash: List shows stashes"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "stash-list"
init_repo

create_file "f.txt" "clean"
require_success "add f.txt" atomic add f.txt
require_success "record base" record_change "base"

overwrite_file "f.txt" "dirty"
require_success "stash modification" atomic stash

# List should show the stash
require_capture "list stashes" atomic stash list
list_out="$COMMAND_OUTPUT"
if echo "$list_out" | grep -qE "stash@\{0\}|On dev"; then
    _pass "stash list shows stash@{0}"
else
    _fail "stash list shows stash" "got: $(echo "$list_out" | head -5)"
fi

# Pop it to clean up
require_success "pop stash" atomic stash pop

# List should be empty now
require_capture "list stashes after pop" atomic stash list
list_after="$COMMAND_OUTPUT"
if echo "$list_after" | grep -qiE "no stash|empty" || ! echo "$list_after" | grep -qE "stash@"; then
    _pass "stash list empty after pop"
else
    _fail "stash list empty after pop" "still has entries"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Stash: Apply without removing"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "stash-apply"
init_repo

create_file "f.txt" "original"
require_success "add f.txt" atomic add f.txt
require_success "record base" record_change "base"

overwrite_file "f.txt" "modified"
require_success "stash modification" atomic stash

# Apply (not pop) — restores content but keeps the stash
require_capture "apply stash" atomic stash apply
apply_out="$COMMAND_OUTPUT"
if echo "$apply_out" | grep -qiE "applied|stash"; then
    _pass "stash apply succeeds"
else
    _fail "stash apply succeeds" "got: $(echo "$apply_out" | head -3)"
fi

# Content should be restored
assert_file_content "f.txt restored by apply" "f.txt" "modified"

# Stash should still exist
require_capture "list stashes after apply" atomic stash list
list_still="$COMMAND_OUTPUT"
if echo "$list_still" | grep -qE "stash@"; then
    _pass "stash still exists after apply (not pop)"
else
    _fail "stash still exists after apply" "not found"
fi

# Clean up with drop
require_success "drop applied stash" atomic stash drop

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Stash: Drop a specific stash"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "stash-drop"
init_repo

create_file "f.txt" "base"
require_success "add f.txt" atomic add f.txt
require_success "record base" record_change "base"

overwrite_file "f.txt" "stashed"
require_success "stash modification" atomic stash

# Drop the stash
require_capture "drop stash" atomic stash drop
drop_out="$COMMAND_OUTPUT"
if echo "$drop_out" | grep -qiE "dropped|removed|stash"; then
    _pass "stash drop succeeds"
else
    _fail "stash drop succeeds" "got: $(echo "$drop_out" | head -3)"
fi

# List should be empty
require_capture "list stashes after drop" atomic stash list
list_after_drop="$COMMAND_OUTPUT"
if echo "$list_after_drop" | grep -qE "stash@"; then
    _fail "stash list empty after drop" "still has entries"
else
    _pass "stash list empty after drop"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Stash: No changes to stash"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "stash-noop"
init_repo

create_file "f.txt" "clean"
require_success "add f.txt" atomic add f.txt
require_success "record clean state" record_change "clean state"

# Stash with no dirty changes
require_capture "stash clean working copy" atomic stash
noop_out="$COMMAND_OUTPUT"
if echo "$noop_out" | grep -qiE "no.*change|nothing|clean|no local"; then
    _pass "stash with clean working copy reports nothing to save"
else
    _fail "stash with clean working copy reports nothing to save" "got: $noop_out"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Stash: Pop with no stashes fails gracefully"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "stash-no-pop"
init_repo

if pop_none="$(atomic stash pop 2>&1)"; then
    _fail "pop with no stashes fails" "command succeeded: $pop_none"
elif echo "$pop_none" | grep -qiE "no stash|not found|empty|error|invalid"; then
    _pass "pop with no stashes fails gracefully"
else
    _fail "pop with no stashes fails" "got: $(echo "$pop_none" | head -3)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Stash: Stash preserves file content exactly"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "stash-content"
init_repo

create_file "code.py" "def hello():
    print('hello')
    return True"
require_success "add code.py" atomic add code.py
require_success "record initial code.py" record_change "initial code.py"

# Make a specific modification
overwrite_file "code.py" "def hello():
    print('goodbye')
    return False"

# Stash
require_success "stash code.py" atomic stash

# Verify clean state
assert_file_content "code.py clean after stash" "code.py" "def hello():
    print('hello')
    return True"

# Pop and verify exact content
require_success "pop code.py stash" atomic stash pop

assert_file_content "code.py has exact modified content after pop" "code.py" "def hello():
    print('goodbye')
    return False"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Stash: Stash across view switch"
# ═══════════════════════════════════════════════════════════════════════════
#
# Stash on dev, switch to feature, do work, switch back, pop stash.

make_temp_repo "stash-cross-view"
init_repo

create_file "shared.txt" "shared"
require_success "add shared.txt" atomic add shared.txt
require_success "record base" record_change "base"

# Dirty the file and stash
overwrite_file "shared.txt" "dirty on dev"
require_success "stash dev modification" atomic stash
assert_file_content "shared.txt clean after stash" "shared.txt" "shared"

# Switch to feature, do work
require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"
require_success "switch to feature" switch_view "feature"

create_file "feature.txt" "feature work"
require_success "add feature.txt" atomic add feature.txt
require_success "record feature work" record_change "feature work"

# Switch back to dev
require_success "switch to dev" switch_view "dev"
assert_file_content "shared.txt still clean on dev" "shared.txt" "shared"

# Stash list should still have the stash
require_capture "list cross-view stash" atomic stash list
list_cross="$COMMAND_OUTPUT"
if echo "$list_cross" | grep -qE "stash@"; then
    _pass "stash persists across view switches"
else
    _fail "stash persists across view switches" "not found"
fi

# Pop the stash
require_success "pop dev stash" atomic stash pop
assert_file_content "shared.txt dirty after pop" "shared.txt" "dirty on dev"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Stash: Multiple stashes"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "stash-multi"
init_repo

create_file "f.txt" "v0"
require_success "add f.txt" atomic add f.txt
require_success "record v0" record_change "v0"

# First stash
overwrite_file "f.txt" "stash-1"
require_success "create first stash" atomic stash -m "first stash"

# Second stash
overwrite_file "f.txt" "stash-2"
require_success "create second stash" atomic stash -m "second stash"

# Should have 2 stashes
require_capture "list both stashes" atomic stash list
list_multi="$COMMAND_OUTPUT"
stash_count="$(echo "$list_multi" | grep -c "stash@" || true)"
if [[ $stash_count -ge 2 ]]; then
    _pass "two stashes in list ($stash_count)"
else
    _fail "two stashes in list" "got $stash_count"
fi

# Pop most recent first (LIFO)
require_success "pop second stash" atomic stash pop

# After first pop, should have 1 stash left
require_capture "list stashes after first pop" atomic stash list
list_after_pop1="$COMMAND_OUTPUT"
stash_count2="$(echo "$list_after_pop1" | grep -c "stash@" || true)"
if [[ $stash_count2 -eq 1 ]]; then
    _pass "one stash left after first pop"
else
    _fail "one stash left after first pop" "found $stash_count2"
fi

# Pop second
require_success "pop first stash" atomic stash pop

require_capture "list stashes after second pop" atomic stash list
list_after_pop2="$COMMAND_OUTPUT"
if echo "$list_after_pop2" | grep -qE "stash@"; then
    _fail "all stashes popped" "still has entries"
else
    _pass "all stashes popped"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Stash: Clear all stashes"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "stash-clear"
init_repo

create_file "f.txt" "base"
require_success "add f.txt" atomic add f.txt
require_success "record base" record_change "base"

# Create a couple of stashes
overwrite_file "f.txt" "s1"
require_success "create first stash" atomic stash -m "s1"
overwrite_file "f.txt" "s2"
require_success "create second stash" atomic stash -m "s2"

# Clear all
require_capture "clear stashes" atomic stash clear --force
clear_out="$COMMAND_OUTPUT"
if echo "$clear_out" | grep -qiE "cleared|removed|dropped|stash"; then
    _pass "stash clear succeeds"
else
    _fail "stash clear succeeds" "got: $clear_out"
fi

# List should be empty
require_capture "list stashes after clear" atomic stash list
list_cleared="$COMMAND_OUTPUT"
if echo "$list_cleared" | grep -qE "stash@"; then
    _fail "stash list empty after clear" "still has entries"
else
    _pass "stash list empty after clear"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Stash: Stash with new untracked files (--include-untracked)"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "stash-untracked"
init_repo

create_file "tracked.txt" "tracked"
require_success "add tracked.txt" atomic add tracked.txt
require_success "record base" record_change "base"

# Create an untracked file + modify a tracked one
create_file "newfile.txt" "I am new"
overwrite_file "tracked.txt" "modified tracked"

# Stash with --include-untracked
require_capture "stash untracked and modified files" atomic stash push --include-untracked
stash_iu="$COMMAND_OUTPUT"
if echo "$stash_iu" | grep -qiE "saved|stash"; then
    _pass "stash with --include-untracked succeeds"
else
    _fail "stash with --include-untracked succeeds" "got: $stash_iu"
fi

# tracked.txt should be restored
assert_file_content "tracked.txt restored" "tracked.txt" "tracked"
assert_file_not_exists "newfile.txt is removed while stashed" "newfile.txt"

# Pop and verify
require_success "pop stash with untracked file" atomic stash pop
assert_file_content "tracked.txt modified after pop" "tracked.txt" "modified tracked"
assert_file_content "newfile.txt restored after pop" "newfile.txt" "I am new"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Stash: Stash with custom message"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "stash-message"
init_repo

create_file "f.txt" "base"
require_success "add f.txt" atomic add f.txt
require_success "record base" record_change "base"

overwrite_file "f.txt" "wip"
require_success "stash custom message" atomic stash -m "work in progress"

require_capture "list custom-message stash" atomic stash list
list_msg="$COMMAND_OUTPUT"
if echo "$list_msg" | grep -qiE "work.in.progress|work-in-progress|wip"; then
    _pass "stash list shows custom message"
else
    _fail "stash list shows custom message" "got: $list_msg"
fi

require_success "pop custom-message stash" atomic stash pop

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Tags + Stash: Tag before stash, pop, verify state"
# ═══════════════════════════════════════════════════════════════════════════
#
# Tag the clean state, stash dirty changes, verify tag still works,
# pop stash, verify content.

make_temp_repo "tag-stash-combo"
init_repo

create_file "app.txt" "version 1"
require_success "add app.txt" atomic add app.txt
require_success "record v1" record_change "v1"

# Tag the v1 state
require_success "create v1 tag" atomic tag create v1-release

# Dirty and stash
overwrite_file "app.txt" "work in progress"
require_success "stash WIP" atomic stash

# Tag should still be there
require_capture "list tags after stash" atomic tag list
list_combo="$COMMAND_OUTPUT"
if echo "$list_combo" | grep -qF "v1-release"; then
    _pass "tag survives stash operation"
else
    _fail "tag survives stash operation" "not found"
fi

# Content should be clean (v1)
assert_file_content "app.txt is v1 after stash" "app.txt" "version 1"

# Record v2 and tag it
overwrite_file "app.txt" "version 2"
require_success "record v2" record_change "v2"
require_success "create v2 tag" atomic tag create v2-release

# Both tags should exist
require_capture "list both tags" atomic tag list
list_both="$COMMAND_OUTPUT"
found=0
for t in v1-release v2-release; do
    if echo "$list_both" | grep -qF "$t"; then
        found=$((found + 1))
    fi
done
if [[ $found -eq 2 ]]; then
    _pass "both tags present ($found/2)"
else
    _fail "both tags present" "found $found/2"
fi

# Pop the stash — should restore dirty WIP state
require_success "pop WIP stash" atomic stash pop

assert_file_content "app.txt has WIP after pop" "app.txt" "work in progress"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Tags + Stash: Full cross-view workflow"
# ═══════════════════════════════════════════════════════════════════════════
#
# 1. Record and tag v1 on dev
# 2. Create feature, insert dev, record feature work
# 3. Switch to dev, start work, stash it
# 4. Record v2 on dev, tag v2
# 5. Pop stash, continue working
# 6. Insert feature to dev
# 7. Verify all tags, content, and history

make_temp_repo "full-tag-stash"
init_repo

# Step 1: v1 on dev + tag
create_file "core.txt" "core v1"
require_success "add core.txt" atomic add core.txt
require_success "record core v1" record_change "Core v1"
require_success "create v1 tag" atomic tag create v1.0

# Step 2: feature work
require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"
require_success "switch to feature" switch_view "feature"

create_file "feature.txt" "feature content"
require_success "add feature.txt" atomic add feature.txt
require_success "record feature work" record_change "Feature work"

# Step 3: switch to dev, start work, stash
require_success "switch to dev" switch_view "dev"
overwrite_file "core.txt" "core v1 + WIP changes"
assert_status_flag "core.txt modified on dev" "M" "core.txt"

require_success "stash dev WIP" atomic stash -m "dev WIP"
assert_file_content "core.txt clean after stash" "core.txt" "core v1"

# Step 4: record v2, tag
overwrite_file "core.txt" "core v2"
require_success "record core v2" record_change "Core v2"
require_success "create v2 tag" atomic tag create v2.0

# Verify tags
require_capture "list tags after v2" atomic tag list
tags_now="$COMMAND_OUTPUT"
for t in v1.0 v2.0; do
    if echo "$tags_now" | grep -qF "$t"; then
        _pass "tag $t exists after v2"
    else
        _fail "tag $t exists after v2" "not found"
    fi
done

# Step 5: pop stash
require_success "pop dev WIP" atomic stash pop
assert_file_content "core.txt has WIP after pop" "core.txt" "core v1 + WIP changes"

# Insertion must refuse to refresh a dirty working copy without discarding it.
if insert_out="$(insert_from_view "feature" "dev")"; then
    _fail "insert refuses unrecorded WIP" "insert unexpectedly succeeded: $insert_out"
    print_summary || true
    exit 1
elif echo "$insert_out" | grep -qi 'unrecorded changes'; then
    _pass "insert refuses unrecorded WIP"
else
    _fail "insert refuses unrecorded WIP" "unexpected refusal: $insert_out"
    print_summary || true
    exit 1
fi
assert_file_content "refused insert preserves WIP" "core.txt" "core v1 + WIP changes"
require_success "record restored WIP" record_change "Record restored WIP"

# Step 6: insert feature to dev after preserving the WIP in history.
require_success "insert feature into dev" insert_from_view "feature" "dev"
require_success "select dev" switch_view "dev"
assert_file_exists "feature.txt on dev after insert" "feature.txt"
assert_file_content "recorded WIP survives insert" "core.txt" "core v1 + WIP changes"

# Step 7: verify tags still intact
require_capture "list tags after insert" atomic tag list
tags_final="$COMMAND_OUTPUT"
for t in v1.0 v2.0; do
    if echo "$tags_final" | grep -qF "$t"; then
        _pass "tag $t survives full workflow"
    else
        _fail "tag $t survives full workflow" "not found"
    fi
done

# Verify feature is intact
require_success "switch to feature" switch_view "feature"
assert_file_exists "feature.txt still on feature" "feature.txt"
assert_file_exists "core.txt on feature" "core.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Stash: Stash + switch + stash (nested scenario)"
# ═══════════════════════════════════════════════════════════════════════════
#
# Stash on dev, switch to feature, stash there, switch back, pop dev stash

make_temp_repo "stash-nested"
init_repo

create_file "f.txt" "base"
require_success "add f.txt" atomic add f.txt
require_success "record base" record_change "base"

require_success "create feature view" new_view "feature"
require_success "insert dev into feature" insert_from_view "dev" "feature"

# Stash on dev
overwrite_file "f.txt" "dev dirty"
require_success "stash dev changes" atomic stash -m "dev stash"

# Switch to feature, make dirty changes, stash
require_success "switch to feature" switch_view "feature"
overwrite_file "f.txt" "feature dirty"
require_success "stash feature changes" atomic stash -m "feature stash"

# Should have 2 stashes total
require_capture "list nested stashes" atomic stash list
list_nested="$COMMAND_OUTPUT"
nested_count="$(echo "$list_nested" | grep -c "stash@" || true)"
if [[ $nested_count -ge 2 ]]; then
    _pass "two stashes from different views ($nested_count)"
else
    _fail "two stashes from different views" "got $nested_count"
fi

# Switch back to dev, pop most recent stash (feature's)
require_success "switch to dev" switch_view "dev"
require_success "pop most recent stash" atomic stash pop

# Pop the dev stash
require_success "pop remaining stash" atomic stash pop

# Should have no stashes left
require_capture "list stashes after both pops" atomic stash list
list_final="$COMMAND_OUTPUT"
if echo "$list_final" | grep -qE "stash@"; then
    _fail "all stashes popped" "still has entries"
else
    _pass "all stashes popped after nested scenario"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Stash: Keep flag (stash without restoring clean state)"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "stash-keep"
init_repo

create_file "f.txt" "original"
require_success "add f.txt" atomic add f.txt
require_success "record base" record_change "base"

overwrite_file "f.txt" "dirty"
require_success "stash with keep" atomic stash push --keep

# With --keep, the working copy should NOT be restored to clean state
# (the dirty content remains on disk)
assert_file_content "f.txt still dirty with --keep" "f.txt" "dirty"

# But a stash should still exist
require_capture "list kept stash" atomic stash list
list_keep="$COMMAND_OUTPUT"
if echo "$list_keep" | grep -qE "stash@"; then
    _pass "stash created with --keep"
else
    _fail "stash created with --keep" "not found"
fi

# Clean up
require_success "drop kept stash" atomic stash drop

# ═══════════════════════════════════════════════════════════════════════════

print_summary
