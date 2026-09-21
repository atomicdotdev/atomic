#!/usr/bin/env bash
# 39_ambient_inode_views.sh — Ambient inode identity across view closures.
#
# Covers the causal distinctions that TREE projection must not blur:
#   1. A child draft inherits a file introduced by its draft parent.
#   2. Selecting an already-ambient change is idempotent and does not append
#      lifecycle operations derived from the active TREE projection.
#   3. Sibling drafts may independently create the same path; combining them
#      surfaces both identities honestly instead of silently choosing an owner.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"
source "$HARNESS_DIR/merge_helpers.sh"

journal_fingerprint() {
    if [[ -f .atomic/deferred-tree-ops.json ]]; then
        cksum .atomic/deferred-tree-ops.json
    else
        printf 'missing\n'
    fi
}

assert_equal_value() {
    local desc="$1"
    local expected="$2"
    local actual="$3"
    if [[ "$actual" == "$expected" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "expected '$expected', got '$actual'"
    fi
}



# ── Case 1: draft parent closure is inherited by its child ──────────────────
begin_section "Descendant draft inherits parent file identity"
make_temp_repo ambient-inode-descendant
init_repo

new_view ab12 --draft --parent dev --switch >/dev/null
printf 'from-ab12\n' > f.txt
add_files f.txt >/dev/null
record_change "ab12 adds f.txt" >/dev/null

new_view cd23 --draft --parent ab12 --switch >/dev/null
assert_file_content "cd23 materializes ab12's inherited file" f.txt "from-ab12"
assert_clean "cd23 starts clean with inherited file"

ADD_OUTPUT="$(atomic add f.txt 2>&1)"
assert_clean "adding inherited file is idempotent"
assert_file_content "idempotent add preserves inherited content" f.txt "from-ab12"

printf 'from-cd23\n' > f.txt
record_change "cd23 edits inherited f.txt" >/dev/null
assert_file_content "cd23 records against inherited file" f.txt "from-cd23"

switch_view ab12 >/dev/null
assert_file_content "parent remains at its own visible generation" f.txt "from-ab12"
switch_view cd23 >/dev/null
assert_file_content "child restores its edit on the same inherited file" f.txt "from-cd23"

# ── Case 2: selecting ambient graph data does not rewrite TREE history ──────
begin_section "Cross-view insertion is closure-only and idempotent"
make_temp_repo ambient-inode-insert
init_repo

new_view feature --draft --parent dev --switch >/dev/null
printf 'ambient content\n' > ambient.txt
add_files ambient.txt >/dev/null
record_change "feature adds ambient.txt" >/dev/null

JOURNAL_BEFORE="$(journal_fingerprint)"
switch_view dev >/dev/null
insert_from_view feature dev >/dev/null
JOURNAL_AFTER_FIRST="$(journal_fingerprint)"
assert_equal_value "first insert does not append snapshot-derived TREE ops" \
    "$JOURNAL_BEFORE" "$JOURNAL_AFTER_FIRST"
assert_file_content "dev materializes selected ambient file" ambient.txt "ambient content"

insert_from_view feature dev >/dev/null
JOURNAL_AFTER_SECOND="$(journal_fingerprint)"
assert_equal_value "repeated insert leaves TREE lifecycle metadata unchanged" \
    "$JOURNAL_AFTER_FIRST" "$JOURNAL_AFTER_SECOND"
assert_file_content "repeated insert preserves content" ambient.txt "ambient content"
assert_clean "repeated ambient insert leaves dev clean"

# ── Case 3: siblings create distinct identities and conflict honestly ───────
begin_section "Sibling same-path creates surface an identity conflict"
make_temp_repo ambient-inode-siblings
init_repo

new_view bill --draft --parent dev --switch >/dev/null
printf 'from-bill\n' > f.txt
add_files f.txt >/dev/null
record_change "bill creates f.txt" >/dev/null

switch_view dev >/dev/null
new_view sally --draft --parent dev --switch >/dev/null
printf 'from-sally\n' > f.txt
add_files f.txt >/dev/null
record_change "sally creates f.txt" >/dev/null

JOURNAL_BEFORE_CONFLICT="$(journal_fingerprint)"
insert_from_view bill sally >/dev/null 2>&1 || true
JOURNAL_AFTER_CONFLICT="$(journal_fingerprint)"
assert_equal_value "combining siblings does not invent TREE lifecycle ops" \
    "$JOURNAL_BEFORE_CONFLICT" "$JOURNAL_AFTER_CONFLICT"
assert_markers "same-path sibling identities surface conflict markers" f.txt
assert_present "Bill's identity content is preserved" f.txt "from-bill"
assert_present "Sally's identity content is preserved" f.txt "from-sally"
assert_occurrences "Bill's content appears once" f.txt "from-bill" 1
assert_occurrences "Sally's content appears once" f.txt "from-sally" 1
assert_honest "same-path identity conflict has an honest exit state" f.txt

CONFLICT_SNAPSHOT="$(snapshot_file f.txt)"
switch_view bill >/dev/null
assert_file_content "Bill's sibling view retains its own file" f.txt "from-bill"
switch_view sally >/dev/null
assert_file_stable "combined sibling conflict survives view switching" \
    f.txt "$CONFLICT_SNAPSHOT"

# ── Case 4: renaming one identity must preserve the other path owner ─────────
begin_section "Rename one same-path identity without moving its sibling"
make_temp_repo ambient-inode-rename-side
init_repo

new_view bill --draft --parent dev --switch >/dev/null
printf 'bill body\n' > f.txt
add_files f.txt >/dev/null
record_change "bill creates f.txt" >/dev/null

switch_view dev >/dev/null
new_view sally --draft --parent dev --switch >/dev/null
printf 'sally body\n' > f.txt
add_files f.txt >/dev/null
record_change "sally creates f.txt" >/dev/null
insert_from_view bill sally >/dev/null 2>&1 || true
assert_markers "rename setup has a same-path conflict" f.txt

switch_view bill >/dev/null
mv f.txt bill.txt
record_change "bill renames f.txt to bill.txt" >/dev/null

switch_view sally >/dev/null
insert_from_view bill sally >/dev/null 2>&1 || true
assert_file_content "renamed Bill inode materializes at bill.txt" bill.txt "bill body"
assert_file_content "Sally inode remains at f.txt" f.txt "sally body"
assert_no_markers "separate paths resolve the prior name conflict" f.txt

switch_view bill >/dev/null
assert_file_content "Bill rename survives a switch" bill.txt "bill body"
assert_file_not_exists "Bill no longer has f.txt" f.txt
switch_view sally >/dev/null
assert_file_content "Sally path survives a switch" f.txt "sally body"
assert_file_content "combined view retains Bill's renamed inode" bill.txt "bill body"

# ── Case 5: deleting one identity must preserve the other path owner ─────────
begin_section "Delete one same-path identity without deleting its sibling"
make_temp_repo ambient-inode-delete-side
init_repo

new_view bill --draft --parent dev --switch >/dev/null
printf 'bill body\n' > f.txt
add_files f.txt >/dev/null
record_change "bill creates f.txt" >/dev/null

switch_view dev >/dev/null
new_view sally --draft --parent dev --switch >/dev/null
printf 'sally body\n' > f.txt
add_files f.txt >/dev/null
record_change "sally creates f.txt" >/dev/null
insert_from_view bill sally >/dev/null 2>&1 || true
assert_markers "delete setup has a same-path conflict" f.txt

switch_view bill >/dev/null
rm f.txt
record_change "bill deletes f.txt" >/dev/null

switch_view sally >/dev/null
insert_from_view bill sally >/dev/null 2>&1 || true
assert_file_content "deleting Bill inode preserves Sally inode" f.txt "sally body"
assert_no_markers "single surviving identity has no conflict markers" f.txt
assert_clean "resolved delete-vs-create view is clean"

switch_view bill >/dev/null
assert_file_not_exists "Bill's deleted inode remains absent" f.txt
switch_view sally >/dev/null
assert_file_content "Sally inode survives delete switch round-trip" f.txt "sally body"

# ── Case 6: insertion order must not change the combined identity conflict ───
begin_section "Opposite insertion orders converge to the same conflict"
make_temp_repo ambient-inode-order
init_repo

new_view bill --draft --parent dev --switch >/dev/null
printf 'bill body\n' > f.txt
add_files f.txt >/dev/null
record_change "bill creates f.txt" >/dev/null

switch_view dev >/dev/null
new_view sally --draft --parent dev --switch >/dev/null
printf 'sally body\n' > f.txt
add_files f.txt >/dev/null
record_change "sally creates f.txt" >/dev/null

switch_view dev >/dev/null
new_view merge-a --draft --parent dev >/dev/null
new_view merge-b --draft --parent dev >/dev/null

switch_view merge-a >/dev/null
insert_from_view bill merge-a >/dev/null 2>&1 || true
insert_from_view sally merge-a >/dev/null 2>&1 || true
assert_markers "merge-a surfaces the same-path conflict" f.txt
MERGE_A_SNAPSHOT="$(snapshot_file f.txt)"

switch_view merge-b >/dev/null
insert_from_view sally merge-b >/dev/null 2>&1 || true
insert_from_view bill merge-b >/dev/null 2>&1 || true
assert_markers "merge-b surfaces the same-path conflict" f.txt
assert_file_stable "opposite insert order yields identical materialization" \
    f.txt "$MERGE_A_SNAPSHOT"
assert_present "opposite-order result retains Bill" f.txt "bill body"
assert_present "opposite-order result retains Sally" f.txt "sally body"
assert_honest "opposite-order conflict has an honest exit state" f.txt

# ── Case 7: concurrent renames retain both incomparable destinations ─────────
begin_section "Concurrent renames preserve both names for one inode"
make_temp_repo ambient-inode-concurrent-rename
init_repo
printf 'shared body\n' > original.txt
add_files original.txt >/dev/null
record_change "add original" >/dev/null

new_view left --draft --parent dev --switch >/dev/null
mv original.txt left.txt
record_change "rename original to left" >/dev/null

switch_view dev >/dev/null
new_view right --draft --parent dev --switch >/dev/null
mv original.txt right.txt
record_change "rename original to right" >/dev/null
insert_from_view left right >/dev/null 2>&1 || true

assert_file_content "left concurrent destination is preserved" left.txt "shared body"
assert_file_content "right concurrent destination is preserved" right.txt "shared body"
assert_file_not_exists "concurrently superseded original path is absent" original.txt

switch_view left >/dev/null
assert_file_content "left source view retains only left destination" left.txt "shared body"
assert_file_not_exists "left source does not inherit right destination" right.txt
switch_view right >/dev/null
assert_file_content "combined view restores left destination" left.txt "shared body"
assert_file_content "combined view restores right destination" right.txt "shared body"

# ── Case 8: sequential renames causally supersede earlier destinations ───────
begin_section "Sequential renames retain only the causal successor"
make_temp_repo ambient-inode-sequential-rename
init_repo
printf 'chain body\n' > a.txt
add_files a.txt >/dev/null
record_change "add a" >/dev/null
mv a.txt b.txt
record_change "rename a to b" >/dev/null
mv b.txt c.txt
record_change "rename b to c" >/dev/null

assert_file_content "sequential rename ends at c.txt" c.txt "chain body"
assert_file_not_exists "sequential rename removes a.txt" a.txt
assert_file_not_exists "later rename supersedes b.txt" b.txt

new_view descendant --draft --parent dev --switch >/dev/null
assert_file_content "descendant inherits final sequential destination" c.txt "chain body"
assert_file_not_exists "descendant does not resurrect intermediate name" b.txt

print_summary
