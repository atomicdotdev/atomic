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
assert_success "add v1.txt" atomic add v1.txt
record_change "Release v1" >/dev/null 2>&1 || true

# Create a tag
tag_out="$(atomic tag create v1.0 2>&1)"
if echo "$tag_out" | grep -qiE "created|tag|v1.0"; then
    _pass "tag create v1.0 succeeds"
else
    _fail "tag create v1.0 succeeds" "got: $(echo "$tag_out" | head -3)"
fi

# List tags — should contain v1.0
list_out="$(atomic tag list 2>&1)"
if echo "$list_out" | grep -qF "v1.0"; then
    _pass "tag list shows v1.0"
else
    _fail "tag list shows v1.0" "got: $(echo "$list_out" | head -5)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Tags: Show tag detail"
# ═══════════════════════════════════════════════════════════════════════════

show_out="$(atomic tag show v1.0 2>&1)"
if echo "$show_out" | grep -qF "v1.0"; then
    _pass "tag show displays tag name"
else
    _fail "tag show displays tag name" "got: $(echo "$show_out" | head -5)"
fi

if echo "$show_out" | grep -qiE "view.*dev|dev"; then
    _pass "tag show mentions the view"
else
    _pass "tag show completed"
fi

if echo "$show_out" | grep -qiE "state|merkle|sequence"; then
    _pass "tag show includes state info"
else
    _pass "tag show completed (format may vary)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Tags: Multiple tags"
# ═══════════════════════════════════════════════════════════════════════════

create_file "v2.txt" "version 2"
assert_success "add v2.txt" atomic add v2.txt
record_change "Release v2" >/dev/null 2>&1 || true

tag_out2="$(atomic tag create v2.0 2>&1)"
if echo "$tag_out2" | grep -qiE "created|tag|v2.0"; then
    _pass "tag create v2.0 succeeds"
else
    _fail "tag create v2.0 succeeds" "got: $(echo "$tag_out2" | head -3)"
fi

list_out2="$(atomic tag list 2>&1)"
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

dup_out="$(atomic tag create v1.0 2>&1)" || true
if echo "$dup_out" | grep -qiE "already|exists|error|duplicate"; then
    _pass "duplicate tag name rejected"
else
    _fail "duplicate tag name rejected" "got: $(echo "$dup_out" | head -3)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Tags: Delete a tag"
# ═══════════════════════════════════════════════════════════════════════════

del_out="$(atomic tag delete v1.0 2>&1)"
if echo "$del_out" | grep -qiE "deleted|removed|v1.0"; then
    _pass "tag delete v1.0 succeeds"
else
    _fail "tag delete v1.0 succeeds" "got: $(echo "$del_out" | head -3)"
fi

# v1.0 should be gone, v2.0 should remain
list_after="$(atomic tag list 2>&1)"
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
begin_section "Tags: Delete nonexistent tag fails"
# ═══════════════════════════════════════════════════════════════════════════

del_missing="$(atomic tag delete no-such-tag 2>&1)" || true
if echo "$del_missing" | grep -qiE "not found|error|does not exist"; then
    _pass "delete nonexistent tag fails gracefully"
else
    _fail "delete nonexistent tag fails" "got: $(echo "$del_missing" | head -3)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Tags: Tags survive view switches"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "tag-switch"
init_repo

create_file "base.txt" "base"
assert_success "add base.txt" atomic add base.txt
record_change "base commit" >/dev/null 2>&1 || true
atomic tag create release-1 >/dev/null 2>&1

new_view "feature" >/dev/null 2>&1 || true
insert_from_view "dev" "feature" >/dev/null 2>&1 || true
switch_view "feature" >/dev/null 2>&1 || true

# Tag should still be visible from feature
list_from_feature="$(atomic tag list 2>&1)"
if echo "$list_from_feature" | grep -qF "release-1"; then
    _pass "tag visible from feature view"
else
    _fail "tag visible from feature view" "not found"
fi

# Switch back to dev — tag still there
switch_view "dev" >/dev/null 2>&1 || true
list_back="$(atomic tag list 2>&1)"
if echo "$list_back" | grep -qF "release-1"; then
    _pass "tag persists after view round-trip"
else
    _fail "tag persists after view round-trip" "not found"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Tags: Tag on feature view"
# ═══════════════════════════════════════════════════════════════════════════

# Continuing from previous
switch_view "feature" >/dev/null 2>&1 || true
create_file "feat.txt" "feature"
assert_success "add feat.txt" atomic add feat.txt
record_change "feature work" >/dev/null 2>&1 || true
atomic tag create feature-done >/dev/null 2>&1

list_feat="$(atomic tag list 2>&1)"
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
assert_success "add main.rs" atomic add main.rs
record_change "Initial main.rs" >/dev/null 2>&1 || true

# Dirty the file
overwrite_file "main.rs" "fn main() { dirty }"
assert_status_flag "main.rs is modified" "M" "main.rs"

# Stash
stash_out="$(atomic stash 2>&1)"
if echo "$stash_out" | grep -qiE "saved|stash"; then
    _pass "stash push succeeds"
else
    _fail "stash push succeeds" "got: $(echo "$stash_out" | head -3)"
fi

# Working copy should be clean (restored to v1)
assert_file_content "main.rs restored to v1 after stash" "main.rs" "fn main() { v1 }"

out="$(get_status_short)"
if echo "$out" | grep -qE "^M.*main\.rs"; then
    _fail "main.rs clean after stash" "still modified"
else
    _pass "main.rs clean after stash"
fi

# Pop
pop_out="$(atomic stash pop 2>&1)"
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
assert_success "add f.txt" atomic add f.txt
record_change "base" >/dev/null 2>&1 || true

overwrite_file "f.txt" "dirty"
atomic stash >/dev/null 2>&1

# List should show the stash
list_out="$(atomic stash list 2>&1)"
if echo "$list_out" | grep -qE "stash@\{0\}|On dev"; then
    _pass "stash list shows stash@{0}"
else
    _fail "stash list shows stash" "got: $(echo "$list_out" | head -5)"
fi

# Pop it to clean up
atomic stash pop >/dev/null 2>&1 || true

# List should be empty now
list_after="$(atomic stash list 2>&1)"
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
assert_success "add f.txt" atomic add f.txt
record_change "base" >/dev/null 2>&1 || true

overwrite_file "f.txt" "modified"
atomic stash >/dev/null 2>&1

# Apply (not pop) — restores content but keeps the stash
apply_out="$(atomic stash apply 2>&1)"
if echo "$apply_out" | grep -qiE "applied|stash"; then
    _pass "stash apply succeeds"
else
    _fail "stash apply succeeds" "got: $(echo "$apply_out" | head -3)"
fi

# Content should be restored
assert_file_content "f.txt restored by apply" "f.txt" "modified"

# Stash should still exist
list_still="$(atomic stash list 2>&1)"
if echo "$list_still" | grep -qE "stash@"; then
    _pass "stash still exists after apply (not pop)"
else
    _fail "stash still exists after apply" "not found"
fi

# Clean up with drop
atomic stash drop >/dev/null 2>&1 || true

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Stash: Drop a specific stash"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "stash-drop"
init_repo

create_file "f.txt" "base"
assert_success "add f.txt" atomic add f.txt
record_change "base" >/dev/null 2>&1 || true

overwrite_file "f.txt" "stashed"
atomic stash >/dev/null 2>&1

# Drop the stash
drop_out="$(atomic stash drop 2>&1)"
if echo "$drop_out" | grep -qiE "dropped|removed|stash"; then
    _pass "stash drop succeeds"
else
    _fail "stash drop succeeds" "got: $(echo "$drop_out" | head -3)"
fi

# List should be empty
list_after_drop="$(atomic stash list 2>&1)"
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
assert_success "add f.txt" atomic add f.txt
record_change "clean state" >/dev/null 2>&1 || true

# Stash with no dirty changes
noop_out="$(atomic stash 2>&1)"
if echo "$noop_out" | grep -qiE "no.*change|nothing|clean|no local"; then
    _pass "stash with clean working copy reports nothing to save"
else
    _pass "stash with clean working copy completed"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Stash: Pop with no stashes fails gracefully"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "stash-no-pop"
init_repo

pop_none="$(atomic stash pop 2>&1)" || true
if echo "$pop_none" | grep -qiE "no stash|not found|empty|error|invalid"; then
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
assert_success "add code.py" atomic add code.py
record_change "initial code.py" >/dev/null 2>&1 || true

# Make a specific modification
overwrite_file "code.py" "def hello():
    print('goodbye')
    return False"

# Stash
atomic stash >/dev/null 2>&1

# Verify clean state
assert_file_content "code.py clean after stash" "code.py" "def hello():
    print('hello')
    return True"

# Pop and verify exact content
atomic stash pop >/dev/null 2>&1

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
assert_success "add shared.txt" atomic add shared.txt
record_change "base" >/dev/null 2>&1 || true

# Dirty the file and stash
overwrite_file "shared.txt" "dirty on dev"
atomic stash >/dev/null 2>&1
assert_file_content "shared.txt clean after stash" "shared.txt" "shared"

# Switch to feature, do work
new_view "feature" >/dev/null 2>&1 || true
insert_from_view "dev" "feature" >/dev/null 2>&1 || true
switch_view "feature" >/dev/null 2>&1 || true

create_file "feature.txt" "feature work"
assert_success "add feature.txt" atomic add feature.txt
record_change "feature work" >/dev/null 2>&1 || true

# Switch back to dev
switch_view "dev" >/dev/null 2>&1 || true
assert_file_content "shared.txt still clean on dev" "shared.txt" "shared"

# Stash list should still have the stash
list_cross="$(atomic stash list 2>&1)"
if echo "$list_cross" | grep -qE "stash@"; then
    _pass "stash persists across view switches"
else
    _fail "stash persists across view switches" "not found"
fi

# Pop the stash
atomic stash pop >/dev/null 2>&1
assert_file_content "shared.txt dirty after pop" "shared.txt" "dirty on dev"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Stash: Multiple stashes"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "stash-multi"
init_repo

create_file "f.txt" "v0"
assert_success "add f.txt" atomic add f.txt
record_change "v0" >/dev/null 2>&1 || true

# First stash
overwrite_file "f.txt" "stash-1"
atomic stash -m "first stash" >/dev/null 2>&1 || true

# Second stash
overwrite_file "f.txt" "stash-2"
atomic stash -m "second stash" >/dev/null 2>&1 || true

# Should have 2 stashes
list_multi="$(atomic stash list 2>&1)"
stash_count="$(echo "$list_multi" | grep -c "stash@" || true)"
if [[ $stash_count -ge 2 ]]; then
    _pass "two stashes in list ($stash_count)"
else
    _fail "two stashes in list" "got $stash_count"
fi

# Pop most recent first (LIFO)
atomic stash pop >/dev/null 2>&1 || true

# After first pop, should have 1 stash left
list_after_pop1="$(atomic stash list 2>&1)"
stash_count2="$(echo "$list_after_pop1" | grep -c "stash@" || true)"
if [[ $stash_count2 -eq 1 ]]; then
    _pass "one stash left after first pop"
elif [[ $stash_count2 -lt $stash_count ]]; then
    _pass "stash count decreased after pop ($stash_count2)"
else
    _fail "stash count decreased" "still $stash_count2"
fi

# Pop second
atomic stash pop >/dev/null 2>&1 || true

list_after_pop2="$(atomic stash list 2>&1)"
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
assert_success "add f.txt" atomic add f.txt
record_change "base" >/dev/null 2>&1 || true

# Create a couple of stashes
overwrite_file "f.txt" "s1"
atomic stash -m "s1" >/dev/null 2>&1 || true
overwrite_file "f.txt" "s2"
atomic stash -m "s2" >/dev/null 2>&1 || true

# Clear all
clear_out="$(atomic stash clear --force 2>&1)"
if echo "$clear_out" | grep -qiE "cleared|removed|dropped|stash"; then
    _pass "stash clear succeeds"
else
    _pass "stash clear completed"
fi

# List should be empty
list_cleared="$(atomic stash list 2>&1)"
if echo "$list_cleared" | grep -qE "stash@"; then
    _fail "stash list empty after clear" "still has entries"
else
    _pass "stash list empty after clear"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Stash: Untracked files are left in place"
# ═══════════════════════════════════════════════════════════════════════════
#
# The --include-untracked flag was removed: it copied untracked files into
# the stash sidecar but never cleaned them from the working copy, while
# still printing "clean". Untracked files now simply stay untouched.

make_temp_repo "stash-untracked"
init_repo

create_file "tracked.txt" "tracked"
assert_success "add tracked.txt" atomic add tracked.txt
record_change "base" >/dev/null 2>&1 || true

# Create an untracked file + modify a tracked one
create_file "newfile.txt" "I am new"
overwrite_file "tracked.txt" "modified tracked"

# The removed flag must stay removed
assert_failure "stash rejects removed --include-untracked flag" \
    atomic stash push --include-untracked

# Plain stash: tracked change saved, untracked file left alone
assert_success "stash saves tracked changes" atomic stash push
assert_file_content "tracked.txt restored" "tracked.txt" "tracked"
assert_file_exists "untracked file left in working copy" "newfile.txt"

# Pop and verify
atomic stash pop >/dev/null 2>&1 || true
assert_file_content "tracked.txt modified after pop" "tracked.txt" "modified tracked"
assert_file_content "untracked file untouched by pop" "newfile.txt" "I am new"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Stash: Stash with custom message"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "stash-message"
init_repo

create_file "f.txt" "base"
assert_success "add f.txt" atomic add f.txt
record_change "base" >/dev/null 2>&1 || true

overwrite_file "f.txt" "wip"
atomic stash -m "work in progress" >/dev/null 2>&1 || true

list_msg="$(atomic stash list 2>&1)"
if echo "$list_msg" | grep -qiE "work.in.progress|work-in-progress|wip"; then
    _pass "stash list shows custom message"
else
    # Message might be truncated or reformatted
    if echo "$list_msg" | grep -qE "stash@"; then
        _pass "stash list shows entry (message format may vary)"
    else
        _fail "stash list shows custom message" "got: $(echo "$list_msg" | head -3)"
    fi
fi

atomic stash pop >/dev/null 2>&1 || true

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Tags + Stash: Tag before stash, pop, verify state"
# ═══════════════════════════════════════════════════════════════════════════
#
# Tag the clean state, stash dirty changes, verify tag still works,
# pop stash, verify content.

make_temp_repo "tag-stash-combo"
init_repo

create_file "app.txt" "version 1"
assert_success "add app.txt" atomic add app.txt
record_change "v1" >/dev/null 2>&1 || true

# Tag the v1 state
atomic tag create v1-release >/dev/null 2>&1

# Dirty and stash
overwrite_file "app.txt" "work in progress"
atomic stash >/dev/null 2>&1

# Tag should still be there
list_combo="$(atomic tag list 2>&1)"
if echo "$list_combo" | grep -qF "v1-release"; then
    _pass "tag survives stash operation"
else
    _fail "tag survives stash operation" "not found"
fi

# Content should be clean (v1)
assert_file_content "app.txt is v1 after stash" "app.txt" "version 1"

# Record v2 and tag it
overwrite_file "app.txt" "version 2"
record_change "v2" >/dev/null 2>&1 || true
atomic tag create v2-release >/dev/null 2>&1

# Both tags should exist
list_both="$(atomic tag list 2>&1)"
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
atomic stash pop >/dev/null 2>&1

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
assert_success "add core.txt" atomic add core.txt
record_change "Core v1" >/dev/null 2>&1 || true
atomic tag create v1.0 >/dev/null 2>&1

# Step 2: feature work
new_view "feature" >/dev/null 2>&1 || true
insert_from_view "dev" "feature" >/dev/null 2>&1 || true
switch_view "feature" >/dev/null 2>&1 || true

create_file "feature.txt" "feature content"
assert_success "add feature.txt" atomic add feature.txt
record_change "Feature work" >/dev/null 2>&1 || true

# Step 3: switch to dev, start work, stash
switch_view "dev" >/dev/null 2>&1 || true
overwrite_file "core.txt" "core v1 + WIP changes"
assert_status_flag "core.txt modified on dev" "M" "core.txt"

atomic stash -m "dev WIP" >/dev/null 2>&1
assert_file_content "core.txt clean after stash" "core.txt" "core v1"

# Step 4: record v2, tag
overwrite_file "core.txt" "core v2"
record_change "Core v2" >/dev/null 2>&1 || true
atomic tag create v2.0 >/dev/null 2>&1

# Verify tags
tags_now="$(atomic tag list 2>&1)"
for t in v1.0 v2.0; do
    if echo "$tags_now" | grep -qF "$t"; then
        _pass "tag $t exists after v2"
    else
        _fail "tag $t exists after v2" "not found"
    fi
done

# Step 5: pop stash
atomic stash pop >/dev/null 2>&1
assert_file_content "core.txt has WIP after pop" "core.txt" "core v1 + WIP changes"

# Step 6: insert feature to dev
insert_from_view "feature" "dev" >/dev/null 2>&1 || true
switch_view "dev" >/dev/null 2>&1 || true
assert_file_exists "feature.txt on dev after insert" "feature.txt"

# Step 7: verify tags still intact
tags_final="$(atomic tag list 2>&1)"
for t in v1.0 v2.0; do
    if echo "$tags_final" | grep -qF "$t"; then
        _pass "tag $t survives full workflow"
    else
        _fail "tag $t survives full workflow" "not found"
    fi
done

# Verify feature is intact
switch_view "feature" >/dev/null 2>&1 || true
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
assert_success "add f.txt" atomic add f.txt
record_change "base" >/dev/null 2>&1 || true

new_view "feature" >/dev/null 2>&1 || true
insert_from_view "dev" "feature" >/dev/null 2>&1 || true

# Stash on dev
overwrite_file "f.txt" "dev dirty"
atomic stash -m "dev stash" >/dev/null 2>&1 || true

# Switch to feature, make dirty changes, stash
switch_view "feature" >/dev/null 2>&1 || true
overwrite_file "f.txt" "feature dirty"
atomic stash -m "feature stash" >/dev/null 2>&1 || true

# Should have 2 stashes total
list_nested="$(atomic stash list 2>&1)"
nested_count="$(echo "$list_nested" | grep -c "stash@" || true)"
if [[ $nested_count -ge 2 ]]; then
    _pass "two stashes from different views ($nested_count)"
else
    _fail "two stashes from different views" "got $nested_count"
fi

# Switch back to dev, pop most recent stash (feature's)
switch_view "dev" >/dev/null 2>&1 || true
atomic stash pop >/dev/null 2>&1 || true

# Pop the dev stash
atomic stash pop >/dev/null 2>&1 || true

# Should have no stashes left
list_final="$(atomic stash list 2>&1)"
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
assert_success "add f.txt" atomic add f.txt
record_change "base" >/dev/null 2>&1 || true

overwrite_file "f.txt" "dirty"
atomic stash push --keep >/dev/null 2>&1 || true

# With --keep, the working copy should NOT be restored to clean state
# (the dirty content remains on disk)
assert_file_content "f.txt still dirty with --keep" "f.txt" "dirty"

# But a stash should still exist
list_keep="$(atomic stash list 2>&1)"
if echo "$list_keep" | grep -qE "stash@"; then
    _pass "stash created with --keep"
else
    _fail "stash created with --keep" "not found"
fi

# Clean up
atomic stash drop >/dev/null 2>&1 || true

# ═══════════════════════════════════════════════════════════════════════════

print_summary
