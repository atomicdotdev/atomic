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
require_success "add a.txt b.txt c.txt" atomic add a.txt b.txt c.txt

assert_status_flag "a.txt is added" "A" "a.txt"
assert_status_flag "b.txt is added" "A" "b.txt"
assert_status_flag "c.txt is added" "A" "c.txt"

# Record all
require_success "record three files succeeds" record_change "Add three files"

# All should be clean
for f in a.txt b.txt c.txt; do
    assert_status_no_entry "$f is clean after record" "$f"
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
require_success "add tracked1.txt tracked2.txt" atomic add tracked1.txt tracked2.txt

# Status should show 2 added + 1 untracked
assert_status_flag "tracked1.txt is added" "A" "tracked1.txt"
assert_status_flag "tracked2.txt is added" "A" "tracked2.txt"
assert_status_flag "loose_file.txt is untracked" "?" "loose_file.txt"

# Record (should pick up only the added files)
require_success "record tracked files only" record_change "Add tracked files only"

# loose_file.txt should still be untracked
assert_status_flag "loose_file.txt still untracked after record" "?" "loose_file.txt"

# tracked files should be clean
for f in tracked1.txt tracked2.txt; do
    assert_status_no_entry "$f is clean after record" "$f"
done

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Mixed modifications"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "multi-mixed-mods"
init_repo

create_file "stay.txt" "unchanged"
create_file "modify.txt" "original"
create_file "delete_me.txt" "will be deleted"

require_success "add all three" atomic add stay.txt modify.txt delete_me.txt
require_success "record initial three files" record_change "Initial three files"

# Modify one, delete another, leave one untouched
overwrite_file "modify.txt" "modified content"
rm -f delete_me.txt

# Create a new untracked file
create_file "newcomer.txt" "I'm new"

# Status should have a mix
assert_status_flag "modify.txt is modified" "M" "modify.txt"

# delete_me.txt should be D
assert_status_flag "delete_me.txt shows as deleted" "D" "delete_me.txt"

# newcomer.txt should be ?
assert_status_flag "newcomer.txt is untracked" "?" "newcomer.txt"

# stay.txt should NOT appear in short status (it's clean)
assert_status_no_entry "stay.txt is clean (not in short output)" "stay.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Record only modified + delete"
# ═══════════════════════════════════════════════════════════════════════════

# Continuing from previous state
require_success "record mixed changes succeeds" record_change "Record modification and deletion"

# newcomer.txt should still be untracked (was never added)
assert_status_flag "newcomer.txt still untracked" "?" "newcomer.txt"

# delete_me.txt should no longer appear (it's recorded as deleted)
# stay.txt and modify.txt should be clean
for f in stay.txt modify.txt; do
    assert_status_no_entry "$f is clean after recording modification" "$f"
done
assert_status_no_entry "deleted file is absent from status after record" "delete_me.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Unrecord last with multiple files"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "multi-unrecord"
init_repo

create_file "x.txt" "x content"
create_file "y.txt" "y content"
create_file "z.txt" "z content"

require_success "add x y z" atomic add x.txt y.txt z.txt
require_success "record x y z" record_change "Add x y z"

# Unrecord should remove the last change which added all three
require_success "unrecord multiple-file change succeeds" unrecord_last

# Unrecording their original addition leaves the files untracked on disk.
for f in x.txt y.txt z.txt; do
    assert_status_flag "$f is untracked after unrecord" "?" "$f"
done

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
require_success "add --all adds all untracked files" atomic add --all

# Check that all are now added
for f in auto1.txt auto2.txt sub/auto3.txt; do
    assert_status_flag "$f is added after --all" "A" "$f"
done

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Record with --all flag (auto-add)"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "multi-record-all"
init_repo

create_file "rec_all1.txt" "auto record one"
create_file "rec_all2.txt" "auto record two"
create_file "rec_all3.txt" "auto record three"

# Record with --all should add + record in one step
require_success "record --all auto-adds and records" atomic record --all -m "Record everything"

# Check status is clean
for f in rec_all1.txt rec_all2.txt rec_all3.txt; do
    assert_status_no_entry "$f is clean after record --all" "$f"
done

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
require_success "record files from multiple directories" record_change "Add files from multiple directories"

# All should be clean
for f in root.txt src/lib.rs src/utils/helper.rs docs/README.md tests/test1.rs; do
    assert_status_no_entry "$f is clean after record" "$f"
done

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Modify files across directories"
# ═══════════════════════════════════════════════════════════════════════════

# Continuing from previous
overwrite_file "src/lib.rs" "pub fn lib_v2() {}"
overwrite_file "docs/README.md" "# README v2"

assert_status_flag "src/lib.rs is modified" "M" "src/lib.rs"
assert_status_flag "docs/README.md is modified" "M" "docs/README.md"

# root.txt should still be clean
assert_status_no_entry "root.txt unchanged after modifying other files" "root.txt"

# Record only the modified files
require_success "record updated lib and readme" record_change "Update lib and readme"

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

require_success "record five files" record_change "Add five files"

# Delete files 2, 3, 4
rm -f file2.txt file3.txt file4.txt

# Status: 1 and 5 clean, 2/3/4 deleted
for i in 2 3 4; do
    assert_status_flag "file${i}.txt is deleted" "D" "file${i}.txt"
done

# file1 and file5 should be clean
for i in 1 5; do
    assert_status_no_entry "file${i}.txt is clean (not deleted)" "file${i}.txt"
done

# Record the deletions
require_success "record deleted files 2-4" record_change "Delete files 2-4"
assert_clean "clean after recording bulk deletions"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Interleaved add and record"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "multi-interleave"
init_repo

# Round 1: add and record one file
create_file "round1.txt" "round one"
assert_success "add round1.txt" atomic add round1.txt
require_success "record round 1" record_change "Round 1"

# Round 2: add and record a second file
create_file "round2.txt" "round two"
assert_success "add round2.txt" atomic add round2.txt
require_success "record round 2" record_change "Round 2"

# Round 3: add and record a third file
create_file "round3.txt" "round three"
assert_success "add round3.txt" atomic add round3.txt
require_success "record round 3" record_change "Round 3"

# All should be clean
for f in round1.txt round2.txt round3.txt; do
    assert_status_no_entry "$f is clean after interleaved records" "$f"
done

# Unrecord the last one (round 3)
require_success "unrecord round 3" unrecord_last

# The unrecorded addition leaves round3.txt untracked, while others stay clean.
assert_status_flag "round3.txt is untracked after unrecord" "?" "round3.txt"

# round1 and round2 should still be clean
for f in round1.txt round2.txt; do
    assert_status_no_entry "$f still clean after unrecording round3" "$f"
done

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multiple Files: Simultaneous add of same file twice"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "multi-double-add"
init_repo

create_file "double.txt" "once"

# Add twice — should be idempotent
assert_success "first add" atomic add double.txt
assert_success "second add of same file succeeds" atomic add double.txt

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
require_success "add directory recursively" atomic add mydir

# Check that files inside are now added
for f in mydir/a.txt mydir/b.txt mydir/sub/c.txt; do
    assert_status_flag "$f is added recursively" "A" "$f"
done

# Record the whole directory
require_success "record mydir contents" record_change "Add mydir contents"

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
require_success "record initial mixed files" record_change "Initial three"

# Now: modify one, delete one, add a new one
overwrite_file "change.txt" "changed!"
rm -f gone.txt
create_file "brand_new.txt" "hello"
assert_success "add brand_new" atomic add brand_new.txt

# Verify mixed status
assert_status_flag "change.txt shows as modified" "M" "change.txt"
assert_status_flag "gone.txt shows as deleted" "D" "gone.txt"
assert_status_flag "brand_new.txt shows as added" "A" "brand_new.txt"

# Record all mixed changes
require_success "record mixed changes succeeds" record_change "Mixed changes: modify, delete, add"

# After record, only stable.txt, change.txt, and brand_new.txt should remain
assert_file_exists "stable.txt still exists" "stable.txt"
assert_file_exists "change.txt still exists" "change.txt"
assert_file_exists "brand_new.txt exists" "brand_new.txt"
assert_file_not_exists "gone.txt is gone" "gone.txt"
assert_clean "working copy is clean after mixed record"

# ═══════════════════════════════════════════════════════════════════════════

print_summary
