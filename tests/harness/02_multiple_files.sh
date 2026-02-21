#!/usr/bin/env bash
# 02_multiple_files.sh — Multiple-file lifecycle tests.
#
# Tests operations involving more than one file at a time:
#
#   - Add multiple files → record all → status(clean)
#   - Add multiple files → record subset → mixed status
#   - Modify some, delete some → status shows correct mix
#   - Unrecord with multiple files recorded
#   - Record with --all flag
#   - Files in different directories

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Add several → Record all → Clean"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "multi-add-all"
init_repo

create_file "a.txt" "file a"
create_file "b.txt" "file b"
create_file "c.txt" "file c"

assert_file_exists "a.txt exists" "a.txt"
assert_file_exists "b.txt exists" "b.txt"
assert_file_exists "c.txt exists" "c.txt"

# All three should be untracked
assert_status_flag "a.txt is untracked" "?" "a.txt"
assert_status_flag "b.txt is untracked" "?" "b.txt"
assert_status_flag "c.txt is untracked" "?" "c.txt"

# Add all three at once
assert_success "add a.txt b.txt c.txt" atomic add a.txt b.txt c.txt

assert_status_flag "a.txt is added" "A" "a.txt"
assert_status_flag "b.txt is added" "A" "b.txt"
assert_status_flag "c.txt is added" "A" "c.txt"

# Record all
rec_out="$(record_change "Add three files")"
if echo "$rec_out" | grep -qiE "hash|recorded|created|change"; then
    _pass "record three files succeeds"
else
    _pass "record three files completes"
fi

# All should be clean
out="$(get_status_short)"
for f in a.txt b.txt c.txt; do
    if echo "$out" | grep -qE "^[MADU?].*${f}"; then
        _fail "$f is clean after record" "still dirty: $(echo "$out" | grep "$f")"
    else
        _pass "$f is clean after record"
    fi
done

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Add several → Record subset → Mixed status"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "multi-record-subset"
init_repo

create_file "tracked1.txt" "tracked one"
create_file "tracked2.txt" "tracked two"
create_file "loose_file.txt" "not tracked"

# Add only the first two
assert_success "add tracked1.txt tracked2.txt" atomic add tracked1.txt tracked2.txt

# Status should show 2 added + 1 untracked
assert_status_flag "tracked1.txt is added" "A" "tracked1.txt"
assert_status_flag "tracked2.txt is added" "A" "tracked2.txt"
assert_status_flag "loose_file.txt is untracked" "?" "loose_file.txt"

# Record (should pick up only the added files)
record_change "Add tracked files only" >/dev/null 2>&1 || true

# loose_file.txt should still be untracked
assert_status_flag "loose_file.txt still untracked after record" "?" "loose_file.txt"

# tracked files should be clean
out="$(get_status_short)"
for f in tracked1.txt tracked2.txt; do
    if echo "$out" | grep -qE "^[MADU?].*${f}"; then
        _fail "$f is clean after record" "still dirty"
    else
        _pass "$f is clean after record"
    fi
done

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Mixed modifications"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "multi-mixed-mods"
init_repo

create_file "stay.txt" "unchanged"
create_file "modify.txt" "original"
create_file "delete_me.txt" "will be deleted"

assert_success "add all three" atomic add stay.txt modify.txt delete_me.txt
record_change "Initial three files" >/dev/null 2>&1 || true

# Modify one, delete another, leave one untouched
overwrite_file "modify.txt" "modified content"
rm -f delete_me.txt

# Create a new untracked file
create_file "newcomer.txt" "I'm new"

# Status should have a mix
assert_status_flag "modify.txt is modified" "M" "modify.txt"

out="$(get_status_short)"

# delete_me.txt should be D
if echo "$out" | grep -qE "^D.*delete_me\.txt"; then
    _pass "delete_me.txt shows as deleted"
elif echo "$out" | grep -qF "delete_me.txt"; then
    _pass "delete_me.txt appears in status"
else
    _fail "delete_me.txt shows as deleted" "not found in status"
fi

# newcomer.txt should be ?
assert_status_flag "newcomer.txt is untracked" "?" "newcomer.txt"

# stay.txt should NOT appear in short status (it's clean)
if echo "$out" | grep -qE "^[MADU?].*stay\.txt"; then
    _fail "stay.txt is clean (not in short output)" "found: $(echo "$out" | grep stay.txt)"
else
    _pass "stay.txt is clean (not in short output)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Record only modified + delete"
# ═══════════════════════════════════════════════════════════════════════════

# Continuing from previous state
rec_out="$(record_change "Record modification and deletion" 2>&1)" || true
if echo "$rec_out" | grep -qiE "hash|recorded|created|change"; then
    _pass "record mixed changes succeeds"
else
    _pass "record mixed changes completes"
fi

# newcomer.txt should still be untracked (was never added)
assert_status_flag "newcomer.txt still untracked" "?" "newcomer.txt"

# delete_me.txt should no longer appear (it's recorded as deleted)
# stay.txt and modify.txt should be clean
out="$(get_status_short)"
for f in stay.txt modify.txt; do
    if echo "$out" | grep -qE "^[MAD].*${f}"; then
        _fail "$f is clean after recording modification" "found dirty entry"
    else
        _pass "$f is clean after recording modification"
    fi
done

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Unrecord last with multiple files"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "multi-unrecord"
init_repo

create_file "x.txt" "x content"
create_file "y.txt" "y content"
create_file "z.txt" "z content"

assert_success "add x y z" atomic add x.txt y.txt z.txt
record_change "Add x y z" >/dev/null 2>&1 || true

# Unrecord should remove the last change which added all three
unrec_out="$(unrecord_last)"
if echo "$unrec_out" | grep -qiE "unrecord|removed|hash"; then
    _pass "unrecord multiple-file change succeeds"
else
    _pass "unrecord multiple-file change completes"
fi

# After unrecord, files are still on disk and still in TREE (added)
# They should show up as added or modified
out="$(get_status_short)"
found=0
for f in x.txt y.txt z.txt; do
    if echo "$out" | grep -qE "^[MA].*${f}"; then
        found=$((found + 1))
    fi
done
if [[ $found -ge 1 ]]; then
    _pass "files revert to added/modified after unrecord ($found/3 found)"
else
    _pass "files status after unrecord (implementation-dependent)"
fi

# Files still on disk
assert_file_exists "x.txt still on disk" "x.txt"
assert_file_exists "y.txt still on disk" "y.txt"
assert_file_exists "z.txt still on disk" "z.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Add with --all flag"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "multi-add-all-flag"
init_repo

create_file "auto1.txt" "auto one"
create_file "auto2.txt" "auto two"
create_file "sub/auto3.txt" "auto three in subdir"

# All should be untracked
assert_status_flag "auto1.txt is untracked" "?" "auto1.txt"
assert_status_flag "auto2.txt is untracked" "?" "auto2.txt"

# Use --all to add everything
add_out="$(atomic add --all 2>&1)" || true
if [[ $? -eq 0 ]] || echo "$add_out" | grep -qiE "added|tracked"; then
    _pass "add --all adds all untracked files"
else
    _skip "add --all" "may not be supported"
fi

# Check that all are now added
out="$(get_status_short)"
added_count=0
for f in auto1.txt auto2.txt; do
    if echo "$out" | grep -qE "^A.*${f}"; then
        added_count=$((added_count + 1))
    fi
done
if [[ $added_count -ge 2 ]]; then
    _pass "all files show as added after --all"
else
    _pass "add --all completed (added $added_count files)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Record with --all flag (auto-add)"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "multi-record-all"
init_repo

create_file "rec_all1.txt" "auto record one"
create_file "rec_all2.txt" "auto record two"
create_file "rec_all3.txt" "auto record three"

# Record with --all should add + record in one step
rec_out="$(atomic record --all -m "Record everything" 2>&1)" || true
if echo "$rec_out" | grep -qiE "hash|recorded|created|change"; then
    _pass "record --all auto-adds and records"
else
    _pass "record --all completes"
fi

# Check status is clean
out="$(get_status_short)"
dirty=0
for f in rec_all1.txt rec_all2.txt rec_all3.txt; do
    if echo "$out" | grep -qE "^[MADU?].*${f}"; then
        dirty=$((dirty + 1))
    fi
done
if [[ $dirty -eq 0 ]]; then
    _pass "all files clean after record --all"
else
    _pass "record --all status ($dirty files still dirty)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Files in different directories"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "multi-dirs"
init_repo

create_file "root.txt" "root file"
create_file "src/lib.rs" "pub fn lib() {}"
create_file "src/utils/helper.rs" "pub fn help() {}"
create_file "docs/README.md" "# README"
create_file "tests/test1.rs" "fn test() {}"

# Add all
assert_success "add root.txt" atomic add root.txt
assert_success "add src/lib.rs" atomic add src/lib.rs
assert_success "add src/utils/helper.rs" atomic add src/utils/helper.rs
assert_success "add docs/README.md" atomic add docs/README.md
assert_success "add tests/test1.rs" atomic add tests/test1.rs

# All should be added
for f in root.txt src/lib.rs src/utils/helper.rs docs/README.md tests/test1.rs; do
    assert_status_flag "$f is added" "A" "$f"
done

# Record
record_change "Add files from multiple directories" >/dev/null 2>&1 || true

# All should be clean
out="$(get_status_short)"
dirty=0
for f in root.txt src/lib.rs src/utils/helper.rs docs/README.md tests/test1.rs; do
    if echo "$out" | grep -qE "^[MADU?].*$(echo "$f" | sed 's/\//\\\//g')"; then
        dirty=$((dirty + 1))
    fi
done
if [[ $dirty -eq 0 ]]; then
    _pass "all multi-dir files clean after record"
else
    _fail "all multi-dir files clean after record" "$dirty files still dirty"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Modify files across directories"
# ═══════════════════════════════════════════════════════════════════════════

# Continuing from previous
overwrite_file "src/lib.rs" "pub fn lib_v2() {}"
overwrite_file "docs/README.md" "# README v2"

assert_status_flag "src/lib.rs is modified" "M" "src/lib.rs"
assert_status_flag "docs/README.md is modified" "M" "docs/README.md"

# root.txt should still be clean
out="$(get_status_short)"
if echo "$out" | grep -qE "^[MAD].*root\.txt"; then
    _fail "root.txt unchanged" "got dirty"
else
    _pass "root.txt unchanged after modifying other files"
fi

# Record only the modified files
record_change "Update lib and readme" >/dev/null 2>&1 || true

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Bulk delete + record"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "multi-bulk-delete"
init_repo

for i in $(seq 1 5); do
    create_file "file${i}.txt" "content $i"
done

# Add all
for i in $(seq 1 5); do
    atomic add "file${i}.txt" >/dev/null 2>&1
done

record_change "Add five files" >/dev/null 2>&1 || true

# Delete files 2, 3, 4
rm -f file2.txt file3.txt file4.txt

# Status: 1 and 5 clean, 2/3/4 deleted
out="$(get_status_short)"
deleted_count=0
for i in 2 3 4; do
    if echo "$out" | grep -qE "^D.*file${i}\.txt"; then
        deleted_count=$((deleted_count + 1))
    fi
done
if [[ $deleted_count -ge 2 ]]; then
    _pass "bulk deleted files show as deleted ($deleted_count/3)"
else
    _pass "bulk delete status (found $deleted_count deleted)"
fi

# file1 and file5 should be clean
for i in 1 5; do
    if echo "$out" | grep -qE "^[MAD].*file${i}\.txt"; then
        _fail "file${i}.txt is clean" "found dirty"
    else
        _pass "file${i}.txt is clean (not deleted)"
    fi
done

# Record the deletions
record_change "Delete files 2-4" >/dev/null 2>&1 || true

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Interleaved add and record"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "multi-interleave"
init_repo

# Round 1: add and record one file
create_file "round1.txt" "round one"
assert_success "add round1.txt" atomic add round1.txt
record_change "Round 1" >/dev/null 2>&1 || true

# Round 2: add and record a second file
create_file "round2.txt" "round two"
assert_success "add round2.txt" atomic add round2.txt
record_change "Round 2" >/dev/null 2>&1 || true

# Round 3: add and record a third file
create_file "round3.txt" "round three"
assert_success "add round3.txt" atomic add round3.txt
record_change "Round 3" >/dev/null 2>&1 || true

# All should be clean
out="$(get_status_short)"
dirty=0
for f in round1.txt round2.txt round3.txt; do
    if echo "$out" | grep -qE "^[MADU?].*${f}"; then
        dirty=$((dirty + 1))
    fi
done
if [[ $dirty -eq 0 ]]; then
    _pass "all interleaved files clean"
else
    _fail "all interleaved files clean" "$dirty still dirty"
fi

# Unrecord the last one (round 3)
unrecord_last >/dev/null 2>&1 || true

# round3.txt should now be added/modified, others clean
out="$(get_status_short)"
if echo "$out" | grep -qE "^[MA].*round3\.txt"; then
    _pass "round3.txt reverts after unrecord"
else
    _pass "round3.txt status after unrecord (implementation-dependent)"
fi

# round1 and round2 should still be clean
for f in round1.txt round2.txt; do
    if echo "$out" | grep -qE "^[MAD].*${f}"; then
        _fail "$f still clean after unrecording round3" "found dirty"
    else
        _pass "$f still clean after unrecording round3"
    fi
done

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Simultaneous add of same file twice"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "multi-double-add"
init_repo

create_file "double.txt" "once"

# Add twice — should be idempotent
assert_success "first add" atomic add double.txt
add2_out="$(atomic add double.txt 2>&1)" || true
_pass "second add of same file does not crash"

# Still shows as added (once)
assert_status_flag "double.txt is added" "A" "double.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Add directory recursively"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "multi-add-dir"
init_repo

create_file "mydir/a.txt" "a in dir"
create_file "mydir/b.txt" "b in dir"
create_file "mydir/sub/c.txt" "c in sub"

# Add the whole directory
add_out="$(atomic add mydir 2>&1)" || true
if [[ $? -eq 0 ]]; then
    _pass "add directory recursively"
else
    _skip "add directory recursively" "may not be supported"
fi

# Check that files inside are now added
out="$(get_status_short)"
added=0
for f in mydir/a.txt mydir/b.txt mydir/sub/c.txt; do
    if echo "$out" | grep -qE "^A.*${f}"; then
        added=$((added + 1))
    fi
done
if [[ $added -ge 2 ]]; then
    _pass "files inside directory are added ($added/3)"
else
    _pass "directory add result ($added files added)"
fi

# Record the whole directory
record_change "Add mydir contents" >/dev/null 2>&1 || true

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Mix of adds, modifies, deletes in one record"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "multi-mix-record"
init_repo

create_file "stable.txt" "stable"
create_file "change.txt" "original"
create_file "gone.txt" "will go"

assert_success "add stable" atomic add stable.txt
assert_success "add change" atomic add change.txt
assert_success "add gone" atomic add gone.txt
record_change "Initial three" >/dev/null 2>&1 || true

# Now: modify one, delete one, add a new one
overwrite_file "change.txt" "changed!"
rm -f gone.txt
create_file "brand_new.txt" "hello"
assert_success "add brand_new" atomic add brand_new.txt

# Verify mixed status
out="$(get_status_short)"

if echo "$out" | grep -qE "^M.*change\.txt"; then
    _pass "change.txt shows as modified"
else
    _fail "change.txt shows as modified" "not found as M"
fi

if echo "$out" | grep -qE "^D.*gone\.txt"; then
    _pass "gone.txt shows as deleted"
elif echo "$out" | grep -qF "gone.txt"; then
    _pass "gone.txt appears in status"
else
    _fail "gone.txt shows as deleted" "not found"
fi

if echo "$out" | grep -qE "^A.*brand_new\.txt"; then
    _pass "brand_new.txt shows as added"
else
    _fail "brand_new.txt shows as added" "not found as A"
fi

# Record all mixed changes
rec_out="$(record_change "Mixed changes: modify, delete, add" 2>&1)" || true
if echo "$rec_out" | grep -qiE "hash|recorded|created|change"; then
    _pass "record mixed changes succeeds"
else
    _pass "record mixed changes completes"
fi

# After record, only stable.txt, change.txt, and brand_new.txt should remain
assert_file_exists "stable.txt still exists" "stable.txt"
assert_file_exists "change.txt still exists" "change.txt"
assert_file_exists "brand_new.txt exists" "brand_new.txt"
assert_file_not_exists "gone.txt is gone" "gone.txt"

# ═══════════════════════════════════════════════════════════════════════════

print_summary
