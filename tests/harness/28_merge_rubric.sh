#!/usr/bin/env bash
# chmod +x tests/harness/28_merge_rubric.sh
#
# 28_merge_rubric.sh — Merge-rubric matrix (Axis A × Axis B).
#
# ★ CANONICAL front door for merge/materialization coverage. ★
#
# docs/MERGE-CONFLICT-RUBRIC.md defines the scenario space as a grid of
# edit-relationships (Axis A) × convergence-pathways (Axis B) with four uniform
# invariants. This suite is the systematic matrix; start here to see what merge
# behavior is guaranteed. The other merge suites are REGRESSION RECORDS for
# specific historical bugs:
#   17_cross_view_merge          — cross-view insert content duplication
#   22_switch_conflict_markers   — view-switch never invents markers
#   24_concurrent_insert_conflict— concurrent-draft fork resolution
#   27_merge_ordering_duplication— merge ordering + tail duplication
# Shared helpers for all of them live in merge_helpers.sh.
#
# The four invariants asserted in every applicable cell:
#   1. No silent duplication — logical lines appear the expected number of
#      times (probe lines exactly once outside conflict markers).
#   2. No false conflict     — commuting/independent edits produce no markers.
#   3. No lost edit          — both sides' content survives.
#   4. Honest exit state     — on-disk markers ⇔ `atomic status` Conflicted ⇔
#      `atomic conflicts` lists the file.
#
# Each cell builds a FRESH repo so graphs cannot contaminate one another.
#
# Not yet covered (rubric §2 remaining gaps): token-level auto-merge A5/A6,
# rename tracking A10/A11 (needs a rename command), remote-pull pathway B5,
# unrecord-by-hash B10. (A12/A15 fixed and asserted; B7 N-way asserted below.)

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"
# Canonical merge helpers: marker checks, assert_present/occurrences,
# assert_honest, xfail_correct + preds, current_view/tip_hash, write_or_delete,
# build_case, BASE_5.
source "$HARNESS_DIR/merge_helpers.sh"

echo "${BOLD}Merge-rubric matrix (Axis A × Axis B)${RESET}"

# ── A1 × B1: disjoint edits, single-change insert (clean) ────────────────────
begin_section "A1×B1 disjoint edits, single insert"
make_temp_repo rubric-a1b1
build_case "$BASE_5" \
    $'alpha\nbeta-A\ngamma\ndelta\nepsilon\n' \
    $'alpha\nbeta\ngamma\ndelta-B\nepsilon\n'
atomic insert "$FEATURE_HASH" >/dev/null 2>&1
assert_no_markers  "disjoint single-insert: no markers"        f.txt
assert_present     "disjoint single-insert: side A present"    f.txt "beta-A"
assert_present     "disjoint single-insert: side B present"    f.txt "delta-B"
assert_occurrences "disjoint single-insert: no tail dup"       f.txt "epsilon" 1
assert_honest      "disjoint single-insert: honest exit state" f.txt

# ── A2 × B2: adjacent edits, bulk insert-from-view (clean) ───────────────────
begin_section "A2×B2 adjacent edits, bulk insert"
make_temp_repo rubric-a2b2
build_case "$BASE_5" \
    $'alpha\nbeta-A\ngamma\ndelta\nepsilon\n' \
    $'alpha\nbeta\ngamma-B\ndelta\nepsilon\n'
insert_from_view feature "$BASE_VIEW" >/dev/null 2>&1
assert_no_markers  "adjacent bulk-insert: no markers"        f.txt
assert_present     "adjacent bulk-insert: side A present"    f.txt "beta-A"
assert_present     "adjacent bulk-insert: side B present"    f.txt "gamma-B"
assert_occurrences "adjacent bulk-insert: no tail dup"       f.txt "epsilon" 1
assert_honest      "adjacent bulk-insert: honest exit state" f.txt

# ── A3 × B2: identical concurrent edits, dedup (clean) ───────────────────────
begin_section "A3×B2 identical concurrent edits (dedup)"
make_temp_repo rubric-a3b2
build_case "$BASE_5" \
    $'alpha\nSAME-EDIT\ngamma\ndelta\nepsilon\n' \
    $'alpha\nSAME-EDIT\ngamma\ndelta\nepsilon\n'
insert_from_view feature "$BASE_VIEW" >/dev/null 2>&1
assert_no_markers  "identical edits: no markers"                 f.txt
assert_occurrences "identical edits: deduped to a single copy"   f.txt "SAME-EDIT" 1
assert_occurrences "identical edits: no tail dup"                f.txt "epsilon" 1
assert_honest      "identical edits: honest exit state"          f.txt

# ── A4 × B1: same-position edits, single insert (genuine conflict) ───────────
begin_section "A4×B1 same-position conflict, single insert"
make_temp_repo rubric-a4b1
build_case "$BASE_5" \
    $'alpha\nAAA\nbeta\ngamma\ndelta\nepsilon\n' \
    $'alpha\nBBB\nbeta\ngamma\ndelta\nepsilon\n'
atomic insert "$FEATURE_HASH" >/dev/null 2>&1
assert_markers     "same-position: conflict markers present"   f.txt
assert_present     "same-position: side A present"             f.txt "AAA"
assert_present     "same-position: side B present"             f.txt "BBB"
assert_occurrences "same-position: tail not duplicated"        f.txt "epsilon" 1
assert_occurrences "same-position: shared line not duplicated" f.txt "gamma" 1
assert_honest      "same-position: honest exit state"          f.txt

# ── B6: repeated insert is idempotent (no duplication) ───────────────────────
begin_section "B6 repeated insert is idempotent"
make_temp_repo rubric-b6
build_case "$BASE_5" \
    $'alpha\nbeta-A\ngamma\ndelta\nepsilon\n' \
    $'alpha\nbeta\ngamma\ndelta-B\nepsilon\n'
insert_from_view feature "$BASE_VIEW" >/dev/null 2>&1
first_sum="$(cksum f.txt | awk '{print $1"-"$2}')"
# Insert the same change set again — must be a no-op.
insert_from_view feature "$BASE_VIEW" >/dev/null 2>&1
atomic status >/dev/null 2>&1 || true
second_sum="$(cksum f.txt | awk '{print $1"-"$2}')"
if [[ "$first_sum" == "$second_sum" ]]; then
    _pass "repeated insert leaves content byte-identical"
else
    _fail "repeated insert leaves content byte-identical" \
        "content changed on re-insert ($first_sum -> $second_sum)"
fi
assert_occurrences "repeated insert: side A not duplicated" f.txt "beta-A"  1
assert_occurrences "repeated insert: side B not duplicated" f.txt "delta-B" 1
assert_no_markers  "repeated insert: still no markers"      f.txt
assert_honest      "repeated insert: honest exit state"     f.txt

# ── B4: view switch round-trip after a clean merge (no invented markers) ─────
begin_section "B4 view-switch round-trip stays clean"
make_temp_repo rubric-b4
build_case "$BASE_5" \
    $'alpha\nbeta-A\ngamma\ndelta\nepsilon\n' \
    $'alpha\nbeta\ngamma\ndelta-B\nepsilon\n'
insert_from_view feature "$BASE_VIEW" >/dev/null 2>&1
before_sum="$(cksum f.txt | awk '{print $1"-"$2}')"
switch_view feature >/dev/null 2>&1 || true
switch_view "$BASE_VIEW" >/dev/null 2>&1 || true
after_sum="$(cksum f.txt | awk '{print $1"-"$2}')"
assert_no_markers "switch round-trip: no markers on shared view" f.txt
if [[ "$before_sum" == "$after_sum" ]]; then
    _pass "switch round-trip: content stable"
else
    _fail "switch round-trip: content stable" "content changed across switch"
fi
assert_honest "switch round-trip: honest exit state" f.txt

# ── Whole-file delete via insert, other side unchanged ──────────────────────
#
# Fixed (ATOM::25): a whole-file delete is now recorded as GraphOp::FileDel
# carrying deletion edges for EVERY content vertex, and insert removes the
# stale working-copy file once the content is dead on the view. Regression
# guard: previously only the first line dropped and the tail survived with a
# "clean" status (docs/MERGE-CONFLICT-RUBRIC.md §6.5).
begin_section "delete×B1 whole-file delete via insert, other side unchanged"
make_temp_repo rubric-del-none
build_case "$BASE_5" "__DELETE__" "__UNCHANGED__"
atomic insert "$FEATURE_HASH" >/dev/null 2>&1
assert_file_not_exists "whole-file delete removes the file"      f.txt
assert_honest          "whole-file delete: honest exit state"    f.txt

# ── A7 × B2: delete vs modify ────────────────────────────────────────────────
#
# Patch-theory semantics: each line's fate is independent. Feature deleted
# every line; dev's modification of one line created a NEW vertex the delete
# never touched. Correct merge = exactly the surviving modified line. The
# unmodified lines are legitimately deleted — that is not "silent loss".
begin_section "A7×B2 delete vs modify"
make_temp_repo rubric-a7b2
build_case "$BASE_5" \
    "__DELETE__" \
    $'alpha\nbeta-MOD\ngamma\ndelta\nepsilon\n'
insert_from_view feature "$BASE_VIEW" >/dev/null 2>&1
assert_present     "delete-vs-modify: modified line survives"      f.txt "beta-MOD"
assert_occurrences "delete-vs-modify: deleted lines are gone"      f.txt "alpha" 0
assert_no_markers  "delete-vs-modify: no conflict markers"         f.txt
assert_honest      "delete-vs-modify: honest exit state"           f.txt

# ── A8 × B2: delete vs delete (both sides delete → file gone) ────────────────
begin_section "A8×B2 delete vs delete"
make_temp_repo rubric-a8b2
build_case "$BASE_5" "__DELETE__" "__DELETE__"
insert_from_view feature "$BASE_VIEW" >/dev/null 2>&1
assert_file_not_exists "delete-vs-delete: file stays removed"      f.txt
assert_honest          "delete-vs-delete: honest exit state"       f.txt

# ── A9 × B2: delete region vs insert INSIDE the region ───────────────────────
#
# Verified by audit (ATOM::29): feature deletes lines 2-4; base inserts INSIDE
# between the deleted lines. Atomic reattaches the orphaned insertion to the
# surviving neighbours (line1/line5) rather than raising a zombie conflict.
# That is a deliberate CRDT reattachment policy, not silent loss: INSIDE is
# preserved, the deleted lines are legitimately gone, and the three honesty
# signals agree (clean). The rubric's ⚠ "zombie conflict" label is an
# editorial preference, not an invariant — all four invariants hold here.
begin_section "A9×B2 delete region vs insert inside (reattach, lossless)"
make_temp_repo rubric-a9b2
build_case $'line1\nline2\nline3\nline4\nline5\n' \
    $'line1\nline5\n' \
    $'line1\nline2\nINSIDE\nline3\nline4\nline5\n'
insert_from_view feature "$BASE_VIEW" >/dev/null 2>&1
assert_present     "delete-vs-insert-inside: insertion survives"     f.txt "INSIDE"
assert_occurrences "delete-vs-insert-inside: deleted line gone"      f.txt "line3" 0
assert_occurrences "delete-vs-insert-inside: insertion not duped"    f.txt "INSIDE" 1
assert_no_markers  "delete-vs-insert-inside: no markers"             f.txt
assert_honest      "delete-vs-insert-inside: honest exit state"      f.txt

# ── A14 × B1: no-trailing-newline boundary edits, byte-exact ─────────────────
#
# Verified by audit (ATOM::29): base has NO trailing newline; the two sides
# edit the first and last lines disjointly. The merge must keep both edits and
# must NOT invent a trailing newline the base never had.
begin_section "A14×B1 no-trailing-newline, byte-exact"
make_temp_repo rubric-a14b1
build_case $'alpha\nbeta\ngamma' \
    $'alpha-A\nbeta\ngamma' \
    $'alpha\nbeta\ngamma-B'
atomic insert "$FEATURE_HASH" >/dev/null 2>&1
assert_no_markers  "no-newline: no markers"               f.txt
assert_present     "no-newline: side A present"           f.txt "alpha-A"
assert_present     "no-newline: side B present"           f.txt "gamma-B"
if [[ -f f.txt && "$(tail -c1 f.txt | od -An -c | tr -d ' ')" != '\n' ]]; then
    _pass "no-newline: no trailing newline invented"
else
    _fail "no-newline: no trailing newline invented" \
        "base had no trailing newline but the merge produced one"
fi
assert_honest      "no-newline: honest exit state"        f.txt

# ── B9: resolve over markers, then switch away & back (resolution sticks) ────
#
# Verified by audit (ATOM::29): a genuine conflict is resolved by recording
# clean content over the markers. The resolution must survive a view-switch
# round-trip — markers must NOT resurrect and status must stay clean.
begin_section "B9 resolution sticks across switch round-trip"
make_temp_repo rubric-b9
build_case $'alpha\nbeta\ngamma\n' \
    $'alpha\nAAA\ngamma\n' \
    $'alpha\nBBB\ngamma\n'
atomic insert "$FEATURE_HASH" >/dev/null 2>&1
assert_markers "B9 precondition: conflict surfaced" f.txt
printf 'alpha\nRESOLVED\ngamma\n' > f.txt
record_change "resolve conflict" >/dev/null 2>&1
assert_no_markers "B9: markers gone after resolve"        f.txt
assert_honest     "B9: honest (clean) after resolve"      f.txt
switch_view feature >/dev/null 2>&1 || true
switch_view "$BASE_VIEW" >/dev/null 2>&1 || true
assert_no_markers  "B9: markers do not resurrect after switch round-trip" f.txt
assert_present     "B9: resolved content persists"        f.txt "RESOLVED"
assert_occurrences "B9: resolved line not duplicated"     f.txt "RESOLVED" 1
assert_honest      "B9: honest exit state after round-trip" f.txt

# ── B12: diamond — same change reaches a view via two paths (dedup by hash) ──
#
# Verified by audit (ATOM::29): a draft change is inserted into `release` and
# directly into `stage`; then `release`→`stage` is inserted. The second path to
# the same change hash must be a no-op, not a duplicate of the edit. (The base
# session view is named `dev`, so this cell uses `stage` to avoid a collision.)
begin_section "B12 diamond dedup by change hash"
make_temp_repo rubric-b12
init_repo
printf 'l1\nl2\nl3\n' > f.txt
add_files f.txt >/dev/null
record_change "base" >/dev/null
new_view release >/dev/null
new_view stage >/dev/null
new_view draft >/dev/null
switch_view draft >/dev/null
printf 'l1\nl2-EDIT\nl3\n' > f.txt
record_change "draft edit" >/dev/null
insert_from_view draft   release >/dev/null 2>&1
insert_from_view draft   stage   >/dev/null 2>&1
insert_from_view release stage   >/dev/null 2>&1
switch_view stage >/dev/null 2>&1 || true
assert_occurrences "diamond: edit appears exactly once"   f.txt "l2-EDIT" 1
assert_occurrences "diamond: tail not duplicated"         f.txt "l3" 1
assert_no_markers  "diamond: no markers"                  f.txt
assert_honest      "diamond: honest exit state"           f.txt

# ── A15: binary / non-UTF-8 same-position edits ──────────────────────────────
#
# A same-position edit to binary content is surfaced as a whole-file conflict
# (markers + status C + listed). FIXED (ATOM::31): the binary edit now records
# as a whole-file replace that DELETES the base (it used to route through
# globalize_replace's pure-insertion branch with empty deleted_lines, never
# deleting the base — so the original bytes leaked OUTSIDE the markers). The
# conflict body now contains only the two edited versions, no base residue.
begin_section "A15 binary same-position conflict (no base residue)"
make_temp_repo rubric-a15
init_repo
printf '\x00\x01\x02BASE\x03\x04\n' > b.bin
add_files b.bin >/dev/null
record_change "base" >/dev/null
BASE_VIEW="$(current_view)"
new_view feature >/dev/null
switch_view feature >/dev/null
printf '\x00\x01\x02AAAA\x03\x04\n' > b.bin
record_change "edit A" >/dev/null
A15_HASH="$(tip_hash feature)"
switch_view "$BASE_VIEW" >/dev/null
printf '\x00\x01\x02BBBB\x03\x04\n' > b.bin
record_change "edit B" >/dev/null
atomic insert "$A15_HASH" >/dev/null 2>&1
assert_markers        "A15: binary conflict is surfaced (markers)" b.bin
assert_honest         "A15: binary conflict honest exit state"     b.bin
assert_file_contains  "A15: side A (AAAA) present"                 b.bin "AAAA"
assert_file_contains  "A15: side B (BBBB) present"                 b.bin "BBBB"
# The base bytes must NOT survive a replace-vs-replace conflict body.
assert_file_not_contains "A15: no base residue outside markers"   b.bin "BASE"

# ── B7: N-way fork — three concurrent sides at the same position ─────────────
#
# Verified by audit (ATOM::29) and re-confirmed against the current binary
# (ATOM::32): three drafts each insert a DISTINCT line at the same position,
# then all three are inserted into one view. N-way is harder than 2-way — the
# conflict is an SCC with >2 vertices and the markers must NEST correctly:
# exactly one START, N-1 separators, one END for N sides. Each side and each
# shared line must appear exactly once, and the exit state must be honest.
# (The base session view is named `dev`, so this cell uses s1/s2/s3 forks.)
begin_section "B7 N-way fork (three concurrent sides nest correctly)"
make_temp_repo rubric-b7
init_repo
printf 'alpha\nbeta\ngamma\n' > f.txt
add_files f.txt >/dev/null
record_change "base" >/dev/null
BASE_VIEW="$(current_view)"
new_view s1 >/dev/null
new_view s2 >/dev/null
new_view s3 >/dev/null
switch_view s1 >/dev/null
printf 'alpha\nONE\nbeta\ngamma\n' > f.txt
record_change "edit s1" >/dev/null
B7_H1="$(tip_hash s1)"
switch_view "$BASE_VIEW" >/dev/null
switch_view s2 >/dev/null
printf 'alpha\nTWO\nbeta\ngamma\n' > f.txt
record_change "edit s2" >/dev/null
B7_H2="$(tip_hash s2)"
switch_view "$BASE_VIEW" >/dev/null
switch_view s3 >/dev/null
printf 'alpha\nTHREE\nbeta\ngamma\n' > f.txt
record_change "edit s3" >/dev/null
B7_H3="$(tip_hash s3)"
switch_view "$BASE_VIEW" >/dev/null
atomic insert "$B7_H1" >/dev/null 2>&1
atomic insert "$B7_H2" >/dev/null 2>&1
atomic insert "$B7_H3" >/dev/null 2>&1
assert_markers        "B7: conflict surfaced (markers)"          f.txt
assert_present        "B7: side ONE present"                     f.txt "ONE"
assert_present        "B7: side TWO present"                     f.txt "TWO"
assert_present        "B7: side THREE present"                   f.txt "THREE"
assert_occurrences    "B7: side ONE not duplicated"              f.txt "ONE" 1
assert_occurrences    "B7: side TWO not duplicated"              f.txt "TWO" 1
assert_occurrences    "B7: side THREE not duplicated"            f.txt "THREE" 1
assert_occurrences    "B7: shared alpha once"                    f.txt "alpha" 1
assert_occurrences    "B7: shared beta once"                     f.txt "beta" 1
assert_occurrences    "B7: shared gamma once"                    f.txt "gamma" 1
# Nested N-way markers: exactly one START, N-1 (=2) separators, one END.
assert_occurrence_count "B7: exactly one START marker"           f.txt '^>>>>>>>' 1
assert_occurrence_count "B7: two separators nest three sides"    f.txt '^=======' 2
assert_occurrence_count "B7: exactly one END marker"             f.txt '^<<<<<<<' 1
assert_honest         "B7: honest exit state"                    f.txt

# ── A12: independent same-path create on both sides (name conflict) ──────────
#
# FIXED (ATOM::30): both views independently CREATE new.txt as separate inodes.
# TREE is single-valued so the later recorder shadowed the first; inserting one
# side's create into the other used to report success while silently
# materializing only one inode's content (rubric A12, the sole SILENT
# corruption the ATOM::29 audit found). Now materialize walks REV_TREE, detects
# that ≥ 2 inodes are visible+alive at the path, and renders a name conflict
# with markers wrapping BOTH bodies — surfaced honestly via status/conflicts.
begin_section "A12 same-path independent create (name conflict surfaced)"
make_temp_repo rubric-a12
init_repo
printf 'seed\n' > seed.txt
add_files seed.txt >/dev/null
record_change "base" >/dev/null
BASE_VIEW="$(current_view)"
new_view feature >/dev/null
switch_view feature >/dev/null
printf 'from-feature\n' > new.txt
add_files new.txt >/dev/null
record_change "feature creates new.txt" >/dev/null
A12_HASH="$(tip_hash feature)"
switch_view "$BASE_VIEW" >/dev/null
printf 'from-base\n' > new.txt
add_files new.txt >/dev/null
record_change "base creates new.txt" >/dev/null
atomic insert "$A12_HASH" >/dev/null 2>&1
assert_markers     "A12: name conflict surfaced with markers"   new.txt
assert_present     "A12: feature's create preserved"           new.txt "from-feature"
assert_present     "A12: base's create preserved"              new.txt "from-base"
assert_occurrences "A12: feature side not duplicated"          new.txt "from-feature" 1
assert_occurrences "A12: base side not duplicated"             new.txt "from-base" 1
assert_honest      "A12: honest exit state"                    new.txt

# ── A10: rename vs edit (inode survives the rename) ───────────────────────
#
# FIXED (ATOM::34–36): a rename records as a GraphOp::FileMove reusing the
# original inode, and inserting it into another view applies the move. Here a
# draft renames f→g while the base view edits f's content; because the move
# reuses the inode, the concurrent edit rides along to the new path. Correct
# merge = the file at g.txt with the edited line, no conflict, honest/clean.
begin_section "A10 rename vs edit (inode survives, edit preserved)"
make_temp_repo rubric-a10
init_repo
printf 'line1\nline2\nline3\n' > f.txt
add_files f.txt >/dev/null
record_change "base" >/dev/null
BASE_VIEW="$(current_view)"
new_view feature >/dev/null
switch_view feature >/dev/null
mv f.txt g.txt                      # raw rename → FileMove on record
record_change "rename f->g" >/dev/null
A10_HASH="$(tip_hash feature)"
switch_view "$BASE_VIEW" >/dev/null
printf 'line1\nEDITED\nline3\n' > f.txt   # base edits the same file
record_change "edit line2 on base" >/dev/null
atomic insert "$A10_HASH" >/dev/null 2>&1
assert_file_exists     "rename-vs-edit: new path g.txt present"   g.txt
assert_file_not_exists "rename-vs-edit: old path f.txt gone"     f.txt
assert_present         "rename-vs-edit: concurrent edit survived" g.txt "EDITED"
assert_occurrences     "rename-vs-edit: edit not duplicated"      g.txt "EDITED" 1
assert_no_markers      "rename-vs-edit: no conflict markers"      g.txt
assert_honest          "rename-vs-edit: honest exit state"       g.txt

# ── A11: rename vs rename (same file, different targets) ───────────────────
#
# BUG (tracked, ATOM::37): two views rename the SAME inode to DIFFERENT names.
# Inserting one view's rename into the other silently resolves last-writer-wins
# — one name is kept, the other is dropped, and `status` is clean with no
# conflict surfaced. The rubric's correct outcome (A11) is a name conflict
# (one inode cannot live at two paths). Fixing it needs graph-level detection
# of an inode with ≥2 alive name-edges plus a name-conflict honesty signal that
# the current marker-in-file model does not provide (docs §6.7). Correct =
# a surfaced conflict OR both names preserved; today it is neither.
begin_section "A11 rename vs rename (name conflict; tracked)"
make_temp_repo rubric-a11
init_repo
printf 'shared content\n' > orig.txt
add_files orig.txt >/dev/null
record_change "base" >/dev/null
BASE_VIEW="$(current_view)"
new_view feature >/dev/null
switch_view feature >/dev/null
mv orig.txt feat-name.txt
record_change "rename orig->feat-name" >/dev/null
A11_HASH="$(tip_hash feature)"
switch_view "$BASE_VIEW" >/dev/null
mv orig.txt base-name.txt
record_change "rename orig->base-name" >/dev/null
atomic insert "$A11_HASH" >/dev/null 2>&1
# Correct = a surfaced name conflict, OR both destination names preserved.
pred_a11_name_conflict_surfaced() {
    if atomic conflicts --short 2>/dev/null | grep -qE ':'; then
        return 0   # some conflict surfaced
    fi
    [[ -f feat-name.txt && -f base-name.txt ]]   # both names preserved
}
xfail_correct "A11: rename-vs-rename surfaces a name conflict" pred_a11_name_conflict_surfaced

if [[ "${KNOWN_BUGS:-0}" -gt 0 ]]; then
    echo ""
    echo "${YELLOW}  ${KNOWN_BUGS} known bug(s) tracked as expected-failures (see docs/MERGE-CONFLICT-RUBRIC.md).${RESET}"
fi

print_summary
