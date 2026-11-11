#!/usr/bin/env bash
# 19_unrecord.sh — Unrecord command tests.
#
# Tests the complete unrecord workflow including:
#
#   - Basic unrecord + re-record cycle
#   - Unrecord of a file-add change
#   - Full create → record → modify → record → unrecord → re-record cycle
#   - Unrecord affecting multiple files
#   - Change file preservation after unrecord
#   - Status/add consistency after unrecord

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Unrecord: Basic unrecord + re-record cycle"
# ═══════════════════════════════════════════════════════════════════════════
#
# Scenario: create file, add, record, modify, record modification,
# unrecord last → status should show modified (not untracked),
# then re-record should succeed.

make_temp_repo "unrecord-basic"
init_repo

create_file "greeting.txt" "Hello, World!"
require_success "add greeting.txt" atomic add greeting.txt
require_success "record greeting.txt" record_change "Add greeting.txt"

# Verify clean after first record
assert_status_no_entry "greeting.txt is clean after initial record" "greeting.txt"

# Modify and record the modification
overwrite_file "greeting.txt" "Hello, Modified World!"
assert_status_flag "greeting.txt shows M after modify" "M" "greeting.txt"

require_success "record greeting modification" record_change "Modify greeting.txt"

assert_status_no_entry "greeting.txt is clean after modification record" "greeting.txt"

# Unrecord the modification
require_success "unrecord last succeeds" unrecord_last

# After unrecord, file should show as modified (disk has new content, graph
# has the old content from the first record)
assert_status_flag "greeting.txt shows modified after unrecord" "M" "greeting.txt"

# File should NOT show as untracked — it was recorded in the first change
assert_file_content "greeting bytes preserved after unrecord" "greeting.txt" "Hello, Modified World!"

# Re-record should succeed
require_success "re-record after unrecord succeeds" record_change "Re-record greeting.txt"

# Should be clean again
assert_status_no_entry "greeting.txt is clean after re-record" "greeting.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Unrecord: File-add change"
# ═══════════════════════════════════════════════════════════════════════════
#
# Scenario: create file, add, record (introduces the file into the graph),
# then unrecord → file should appear as added or untracked in status,
# and should not be stuck in limbo where add says "already tracked"
# but status says "untracked".

make_temp_repo "unrecord-file-add"
init_repo

create_file "newfile.txt" "I am brand new"
require_success "add newfile.txt" atomic add newfile.txt
assert_status_flag "newfile.txt shows A after add" "A" "newfile.txt"

require_success "record newfile.txt succeeds" record_change "Add newfile.txt"

# Unrecord the file-add change
require_success "unrecord file-add change succeeds" unrecord_last

# File should appear as added (A) or untracked (?) — either is acceptable
# as long as the system is internally consistent
assert_status_flag "newfile.txt is untracked after unrecord" "?" "newfile.txt"

# The file should still exist on disk
assert_file_exists "newfile.txt still exists on disk after unrecord" "newfile.txt"
assert_file_content "newfile.txt content preserved after unrecord" \
    "newfile.txt" "I am brand new"

# If status says untracked (?), add should accept it
# If status says added (A), add should either succeed or say already tracked
require_success "add after unrecord succeeds" atomic add newfile.txt
assert_status_flag "newfile.txt is staged after re-add" "A" "newfile.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Unrecord: Full modify cycle"
# ═══════════════════════════════════════════════════════════════════════════
#
# Full cycle: create → add → record → modify → record modification →
# unrecord modification → verify status shows modified → re-record works.

make_temp_repo "unrecord-full-cycle"
init_repo

create_file "cycle.txt" "version one"
require_success "add cycle.txt" atomic add cycle.txt
require_success "record cycle.txt v1" record_change "Add cycle.txt v1"

# Modify file
overwrite_file "cycle.txt" "version two"
assert_status_flag "cycle.txt is modified after overwrite" "M" "cycle.txt"

# Record the modification
require_success "record cycle.txt v2" record_change "Modify cycle.txt to v2"

assert_status_no_entry "cycle.txt is clean after recording v2" "cycle.txt"

# Unrecord the modification
require_success "unrecord v2 modification succeeds" unrecord_last

# Status should show modified — disk has "version two", graph has "version one"
assert_status_flag "cycle.txt shows modified after unrecording v2" "M" "cycle.txt"

# Disk content should still have the new version
assert_file_content "disk has version two after unrecord" "cycle.txt" "version two"

# Re-record should work
require_success "re-record cycle.txt succeeds" record_change "Re-record cycle.txt v2"

# Clean after re-record
assert_status_no_entry "cycle.txt is clean after re-record" "cycle.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Unrecord: Multiple files"
# ═══════════════════════════════════════════════════════════════════════════
#
# Record a change touching 3 files, then unrecord →
# all 3 should show correct status.

make_temp_repo "unrecord-multi"
init_repo

create_file "alpha.txt" "alpha content"
create_file "beta.txt" "beta content"
create_file "gamma.txt" "gamma content"

require_success "add all three files" atomic add alpha.txt beta.txt gamma.txt

# Record all three in a single change
require_success "record three files" record_change "Add alpha, beta, gamma"

# Verify all clean
for f in alpha.txt beta.txt gamma.txt; do
    assert_status_no_entry "$f is clean after record" "$f"
done

# Modify all three and record
overwrite_file "alpha.txt" "alpha modified"
overwrite_file "beta.txt" "beta modified"
overwrite_file "gamma.txt" "gamma modified"

require_success "record all three modifications" record_change "Modify all three files"

# Verify clean again
for f in alpha.txt beta.txt gamma.txt; do
    assert_status_no_entry "$f is clean after modification record" "$f"
done

# Unrecord the modification change
require_success "unrecord multi-file change succeeds" unrecord_last

# All three should show as modified (or added) after unrecord
for f in alpha.txt beta.txt gamma.txt; do
    assert_status_flag "$f shows modified after unrecord" "M" "$f"
done

# All files should still exist on disk with modified content
assert_file_content "alpha.txt has modified content on disk" "alpha.txt" "alpha modified"
assert_file_content "beta.txt has modified content on disk" "beta.txt" "beta modified"
assert_file_content "gamma.txt has modified content on disk" "gamma.txt" "gamma modified"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Unrecord: Change file preservation"
# ═══════════════════════════════════════════════════════════════════════════
#
# After unrecord, the change hash file should still exist in .atomic/changes/.
# Unrecord removes the change from the view's history but doesn't delete
# the change data itself.

make_temp_repo "unrecord-preserve-change"
init_repo

create_file "preserve.txt" "will be preserved"
require_success "add preserve.txt" atomic add preserve.txt

require_success "record preserve.txt" record_change "Add preserve.txt"
require_capture "read recorded change hash" atomic log --format json --count 1
if ! recorded_hash="$(printf '%s\n' "$COMMAND_OUTPUT" | python3 -c 'import json,sys; entries=json.load(sys.stdin); assert len(entries)==1; print(entries[0]["hash"])')"; then
    _fail "extract recorded change hash" "log: $COMMAND_OUTPUT"
    print_summary || true
    exit 1
fi
change_file=".atomic/changes/${recorded_hash:0:2}/${recorded_hash}.change"
assert_file_exists "recorded change object exists" "$change_file"
require_capture "hash recorded change object before unrecord" shasum -a 256 "$change_file"
object_digest_before="$COMMAND_OUTPUT"

# Count change files before unrecord
change_count_before="$(find .atomic/changes/ -type f 2>/dev/null | wc -l | tr -d '[:space:]')"
if [[ "$change_count_before" -gt 0 ]]; then
    _pass "change files exist before unrecord (count: $change_count_before)"
else
    _fail "change files exist before unrecord" \
        "no files found in .atomic/changes/"
fi

# Capture the change file list before unrecord
change_files_before="$(find .atomic/changes/ -type f 2>/dev/null | sort)"

# Unrecord
require_success "unrecord preserve.txt" unrecord_last

# Count change files after unrecord — should still be the same
change_count_after="$(find .atomic/changes/ -type f 2>/dev/null | wc -l | tr -d '[:space:]')"
if [[ "$change_count_after" -eq "$change_count_before" ]]; then
    _pass "change files preserved after unrecord (count: $change_count_after)"
else
    _fail "change files preserved after unrecord" \
        "before: $change_count_before, after: $change_count_after"
fi

# Verify the exact same files still exist
change_files_after="$(find .atomic/changes/ -type f 2>/dev/null | sort)"
if [[ "$change_files_before" == "$change_files_after" ]]; then
    _pass "same change files present before and after unrecord"
else
    _fail "same change files present before and after unrecord" "before: $change_files_before; after: $change_files_after"
fi
require_capture "hash recorded change object after unrecord" shasum -a 256 "$change_file"
if [[ "$COMMAND_OUTPUT" == "$object_digest_before" ]]; then
    _pass "unrecord preserves recorded object bytes"
else
    _fail "unrecord preserves recorded object bytes" "before: $object_digest_before; after: $COMMAND_OUTPUT"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Unrecord: Status and add consistency"
# ═══════════════════════════════════════════════════════════════════════════
#
# After unrecord, `atomic status` and `atomic add` should agree:
#   - If status says "untracked" (?), add should accept it
#   - If status says "modified" (M), add should say "already tracked"
#   - If status says "added" (A), add should say "already tracked"
# The system should never be in a state where status says one thing but
# add contradicts it.

make_temp_repo "unrecord-consistency"
init_repo

# -- Test 1: Unrecord a file-add, then check status/add consistency --
create_file "cons_new.txt" "new file for consistency"
require_success "add cons_new.txt" atomic add cons_new.txt
require_success "record cons_new.txt" record_change "Add cons_new.txt"

# Unrecord
require_success "unrecord cons_new.txt" unrecord_last

assert_status_flag "unrecorded addition is untracked" "?" "cons_new.txt"
require_success "re-add untracked file" atomic add cons_new.txt
assert_status_flag "re-added file is staged" "A" "cons_new.txt"
assert_file_content "re-add preserves file content" "cons_new.txt" "new file for consistency"

# -- Test 2: Unrecord a modification, then check status/add consistency --
make_temp_repo "unrecord-consistency-modify"
init_repo

create_file "cons_mod.txt" "original content"
require_success "add cons_mod.txt" atomic add cons_mod.txt
require_success "record cons_mod.txt" record_change "Add cons_mod.txt"

overwrite_file "cons_mod.txt" "modified content"
require_success "record cons_mod modification" record_change "Modify cons_mod.txt"

# Unrecord the modification
require_success "unrecord cons_mod modification" unrecord_last

assert_status_flag "unrecorded modification remains tracked and modified" "M" "cons_mod.txt"
require_success "add handles tracked modified file" atomic add cons_mod.txt
assert_status_flag "modified status remains after add" "M" "cons_mod.txt"
assert_file_content "tracked add preserves modified bytes" "cons_mod.txt" "modified content"

# -- Test 3: Verify that status is stable (calling it twice yields same result) --
require_capture "read first status" get_status_short
out1="$COMMAND_OUTPUT"
require_capture "read second status" get_status_short
out2="$COMMAND_OUTPUT"
if [[ "$out1" == "$out2" ]]; then
    _pass "status is idempotent after unrecord"
else
    _fail "status is idempotent after unrecord" \
        "first call and second call differ"
fi

# ═══════════════════════════════════════════════════════════════════════════

print_summary
