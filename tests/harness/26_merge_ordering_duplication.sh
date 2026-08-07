#!/usr/bin/env bash
# chmod +x tests/harness/26_merge_ordering_duplication.sh
#
# 26_merge_ordering_duplication.sh — Merge ordering and content duplication.
#
# Regression coverage for two defects in the graph-to-file output path, both
# of which corrupt a file while reporting complete success.
#
#   1. Independent changes are merged as a conflict.
#      Change A prepends a line, change B appends one. They touch disjoint
#      regions of the file, so patch theory says they commute and the merge is
#      unambiguous. Instead the output invents a conflict between untouched
#      base content and B's line, and relocates B's line from the end of the
#      file to the middle.
#
#   2. Content after a conflict is emitted once per conflict side.
#      When two changes really do contend for the same position, the conflict
#      region itself renders correctly — but every line *after* it is written
#      twice. The duplication scales with the length of the tail, so a small
#      conflict near the top of a large file nearly doubles it.
#
# Both failures are silent: `atomic insert` prints success with no conflict
# warning, and `atomic status` then reports a clean working tree, so the
# repository believes the mangled content is correct. That is what makes this
# a data-integrity bug rather than a cosmetic one — a subsequent `record`
# captures the duplicate text as though the user had written it.
#
# Discovered when a pull mangled an agent-integration plugin: a 236-line file
# became 327 lines and then 454, every declaration appearing twice, with a
# single small conflict marked near the top.
#
# Each case builds a fresh repo so the graphs cannot contaminate one another.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ── Local helpers ───────────────────────────────────────────────────────────

# Assert a file has exactly the expected number of lines.
# Off-by-N here is the whole symptom, so the failure prints the file.
assert_line_count() {
    local desc="$1" path="$2" expected="$3"
    if [[ ! -f "$path" ]]; then _fail "$desc" "file does not exist: $path"; return; fi
    local actual
    actual=$(wc -l < "$path" | tr -d ' ')
    if [[ "$actual" == "$expected" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "expected $expected lines, got $actual. Content:
$(nl -ba "$path" | sed 's/^/      /')"
    fi
}

# Assert a file contains a substring. (Not in helpers.sh — each suite that
# needs it defines its own, as 24_concurrent_insert_conflict.sh does.)
assert_file_contains() {
    local desc="$1" path="$2" needle="$3"
    if [[ ! -f "$path" ]]; then _fail "$desc" "file does not exist: $path"; return; fi
    if grep -qF "$needle" "$path"; then
        _pass "$desc"
    else
        _fail "$desc" "'$needle' not found in $path"
    fi
}

# Assert a specific line appears exactly `expected` times.
assert_occurrences() {
    local desc="$1" path="$2" needle="$3" expected="$4"
    if [[ ! -f "$path" ]]; then _fail "$desc" "file does not exist: $path"; return; fi
    local actual
    actual=$(grep -cxF "$needle" "$path" 2>/dev/null || true)
    if [[ "$actual" == "$expected" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "'$needle' appears $actual time(s), expected $expected"
    fi
}

# Assert the file carries no conflict markers.
#
# Atomic's markers are inverted relative to git: START is '>>>>>>>',
# SEPARATOR '=======', END '<<<<<<<' (atomic-core/src/output/traits.rs).
assert_no_markers() {
    local desc="$1" path="$2"
    if [[ ! -f "$path" ]]; then _fail "$desc" "file does not exist: $path"; return; fi
    if grep -qE '^(>>>>>>>|=======|<<<<<<<)' "$path" 2>/dev/null; then
        _fail "$desc" "unexpected conflict markers. Content:
$(nl -ba "$path" | sed 's/^/      /')"
    else
        _pass "$desc"
    fi
}

# Assert the file matches an expected body exactly.
assert_file_equals() {
    local desc="$1" path="$2" expected="$3"
    if [[ ! -f "$path" ]]; then _fail "$desc" "file does not exist: $path"; return; fi
    if diff -u <(printf '%s' "$expected") "$path" >/dev/null 2>&1; then
        _pass "$desc"
    else
        _fail "$desc" "content mismatch:
$(diff -u <(printf '%s' "$expected") "$path" | sed 's/^/      /' | head -30)"
    fi
}

# The default view name a fresh repo starts on.
current_view() {
    atomic view list 2>/dev/null | awk '/^\*/{print $2}'
}

# Hash of the most recent change on a view.
tip_hash_of_view() {
    local view="$1"
    atomic log --view "$view" 2>/dev/null \
        | grep -oE '=== [A-Z0-9]{12}' | head -1 | cut -d' ' -f2
}

# Build a repo with two views that each edit `f.txt` from a common base.
#
#   $1 — content of f.txt on the draft view (change A)
#   $2 — content of f.txt on the base view  (change B)
#   $3 — the shared starting content (optional; defaults to a 5-line file)
#
# The base matters: each change is recorded as a delta against it, so a base
# that does not resemble the two edits makes both changes full-file rewrites.
# Two independent full rewrites really do contribute separate copies of their
# shared text, which is correct behaviour and not what these cases probe.
#
# Leaves the repo checked out on the base view with change A inserted, so the
# caller can inspect the merged result. Sets BASE_VIEW.
build_divergent_repo() {
    local a_content="$1" b_content="$2"
    local base_content="${3:-line1
line2
line3
line4
line5
}"

    init_repo
    printf '%s' "$base_content" > f.txt
    add_files f.txt >/dev/null
    record_change "base" >/dev/null

    BASE_VIEW="$(current_view)"

    new_view side-a >/dev/null
    switch_view side-a >/dev/null
    printf '%s' "$a_content" > f.txt
    record_change "edit A" >/dev/null
    local hash_a
    hash_a="$(tip_hash_of_view side-a)"

    switch_view "$BASE_VIEW" >/dev/null
    printf '%s' "$b_content" > f.txt
    record_change "edit B" >/dev/null

    atomic insert "$hash_a" >/dev/null 2>&1
}

echo "${BOLD}Merge ordering and content duplication${RESET}"

# ── Case 1: independent changes must merge without a conflict ──────────────
#
# A prepends, B appends. Disjoint regions, so the merge is unambiguous and
# the result is fully determined: A's line, the base body, then B's line.

begin_section "Independent (commuting) changes"

make_temp_repo merge-independent
build_divergent_repo \
    'AAA-top
line1
line2
line3
line4
line5
' \
    'line1
line2
line3
line4
line5
BBB-bottom
'

assert_no_markers "independent changes merge without conflict markers" f.txt
assert_line_count "independent merge has no extra lines" f.txt 7
assert_file_equals "independent merge preserves both edits in order" f.txt 'AAA-top
line1
line2
line3
line4
line5
BBB-bottom
'
assert_occurrences "base content is not duplicated" f.txt "line5" 1

# ── Case 2: a genuine conflict must not duplicate the tail ─────────────────
#
# Both changes insert at the same position, so a conflict is correct. What is
# not correct is emitting the unconflicted remainder once per side.

begin_section "Genuine conflict at the same position"

make_temp_repo merge-conflict-tail
build_divergent_repo \
    'line1
AAA-inserted
line2
line3
line4
line5
' \
    'line1
BBB-inserted
line2
line3
line4
line5
'

assert_file_contains "conflict records side A" f.txt "AAA-inserted"
assert_file_contains "conflict records side B" f.txt "BBB-inserted"
assert_occurrences "tail line is not duplicated (line2)" f.txt "line2" 1
assert_occurrences "tail line is not duplicated (line5)" f.txt "line5" 1
assert_line_count "conflicted merge has no duplicated tail" f.txt 10

# ── Case 3: duplication must not scale with the tail ───────────────────────
#
# The same conflict with a 30-line tail. If the remainder is emitted per side
# the file roughly doubles, which is how a 236-line plugin reached 454.

begin_section "Tail duplication scaling"

make_temp_repo merge-conflict-long-tail

long_tail=""
for i in $(seq 1 30); do
    long_tail+="tail$i
"
done

# The base already holds the tail, so each change is a one-line insertion
# against it — the same shape as case 2, only with more to duplicate.
build_divergent_repo \
    "header
AAA
$long_tail" \
    "header
BBB
$long_tail" \
    "header
$long_tail"

tail_lines=$(grep -cE '^tail[0-9]+$' f.txt 2>/dev/null || true)
if [[ "$tail_lines" == "30" ]]; then
    _pass "30-line tail is emitted once, not once per conflict side"
else
    _fail "30-line tail is emitted once, not once per conflict side" \
        "expected 30 tail lines, got $tail_lines (file is $(wc -l < f.txt | tr -d ' ') lines)"
fi

# ── Case 4: the repository must not consider corruption clean ──────────────
#
# Whatever the merge produces, `status` has to agree with it. Reporting a
# clean tree over duplicated content is what lets a later `record` bake the
# damage into history.

begin_section "Post-merge repository state"

assert_success "status runs after a conflicted merge" atomic status
assert_occurrences "re-materializing does not add further copies" f.txt "header" 1

switch_view side-a >/dev/null 2>&1 || true
switch_view "$BASE_VIEW" >/dev/null 2>&1 || true

tail_after=$(grep -cE '^tail[0-9]+$' f.txt 2>/dev/null || true)
if [[ "$tail_after" == "30" ]]; then
    _pass "tail is stable across a view switch round trip"
else
    _fail "tail is stable across a view switch round trip" \
        "expected 30 tail lines after re-materialization, got $tail_after"
fi

print_summary
