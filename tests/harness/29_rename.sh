#!/usr/bin/env bash
# chmod +x tests/harness/29_rename.sh
#
# 29_rename.sh — Rename / move end-to-end (rubric A10 groundwork).
#
# Drives the REAL `atomic mv` binary to prove that a rename records as a genuine
# move (inode/history preserved), not a delete+add, and that the working copy
# stays consistent. This is the CLI-level guard for the staged rename effort
# (docs/MERGE-CONFLICT-RUBRIC.md §6.7):
#   Raw filesystem moves — similarity is advisory `ProbableMove` evidence.
#   `atomic mv` — stages the original inode at the destination, so rename plus
#                 arbitrary edits records authoritatively as one FileMove.
#
# The op-level FileMove / inode-preservation proof lives in the Rust suite
# (atomic-repository rename_tests.rs); here we assert the user-facing outcome:
# correct round-trip + honest status + `doctor` consistency, which catches the
# pre-fix breakage where `atomic mv` left an untracked file and a stale copy.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"
source "$HARNESS_DIR/merge_helpers.sh"

echo "${BOLD}Rename / move (atomic mv → record)${RESET}"

# Assert `atomic status --short` reports nothing (clean tree).
assert_status_clean() {
    local desc="$1"
    local out
    if ! out="$(atomic status --short 2>&1)"; then
        _fail "$desc" "status command failed:\n$(echo "$out" | sed 's/^/      /')"
        return
    fi
    if [[ -z "$out" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "expected clean status, got:
$(echo "$out" | sed 's/^/      /')"
    fi
}

# Assert `atomic doctor check` reports the working copy consistent (exit 0).
assert_doctor_consistent() {
    local desc="$1"
    local out
    if out="$(atomic doctor check 2>&1)"; then
        _pass "$desc"
    else
        _fail "$desc" "atomic doctor check failed: $out"
    fi
}

# Assert a `status --short` line is present (e.g. 'D  old.txt', '?? new.txt').
assert_status_has() {
    local desc="$1" pattern="$2"
    local out
    if ! out="$(atomic status --short 2>&1)"; then
        _fail "$desc" "status command failed:\n$(echo "$out" | sed 's/^/      /')"
        return
    fi
    if echo "$out" | grep -qE "$pattern"; then
        _pass "$desc"
    else
        _fail "$desc" "status --short missing /$pattern/. got:
$(echo "$out" | sed 's/^/      /')"
    fi
}

# ── Stable-inode staging ─────────────────────────────────────────────────────
begin_section "atomic mv stages the original inode at the destination"
make_temp_repo rename-mv-shape
init_repo
printf 'line1\nline2\nline3\n' > old.txt
require_success "add original file" atomic add old.txt
require_success "record base file" record_change "base"
require_success "stage pure rename" atomic mv old.txt new.txt
# Before record the destination is staged as Added with the original inode;
# the graph-backed source claim remains available to the record planner.
assert_status_has "mv: destination is staged" '^A[[:space:]]+new\.txt$'
require_success "record pure rename" record_change "pure rename"
assert_file_not_exists "pure rename removes old path" old.txt
assert_file_equals "pure rename preserves exact bytes" new.txt $'line1\nline2\nline3\n'
assert_status_clean "pure rename is clean after recording"
assert_output_contains "pure rename shows move evidence" "authoritative: old.txt" atomic change

# ── Destination and unsupported-kind safety ──────────────────────────────────
begin_section "atomic mv refuses unsafe destinations and directory moves"
make_temp_repo rename-mv-safety
init_repo
printf 'source bytes\n' > source.txt
printf 'tracked destination bytes\n' > tracked.txt
require_success "add source and tracked destination" atomic add source.txt tracked.txt
require_success "record source and tracked destination" record_change "base"
printf 'untracked destination bytes\n' > untracked.txt
assert_failure "mv: refuses existing untracked destination without force" \
    atomic mv source.txt untracked.txt
assert_file_equals "mv: source survives untracked-destination refusal" \
    source.txt $'source bytes\n'
assert_file_equals "mv: untracked destination survives refusal" \
    untracked.txt $'untracked destination bytes\n'
assert_failure "mv: refuses tracked destination even with force" \
    atomic mv --force source.txt tracked.txt
assert_file_equals "mv: source survives tracked-destination refusal" \
    source.txt $'source bytes\n'
assert_file_equals "mv: tracked destination survives refusal" \
    tracked.txt $'tracked destination bytes\n'
require_success "mv: force safely replaces an untracked destination" \
    atomic mv --force source.txt untracked.txt
assert_file_not_exists "mv: forced source moved" source.txt
assert_file_contains "mv: forced destination has source bytes" \
    untracked.txt "source bytes"
mkdir empty-dir
require_success "add empty directory" atomic add empty-dir
assert_failure "mv: directory source is explicitly refused" \
    atomic mv empty-dir moved-dir

# ── Rename round-trips and stays consistent ──────────────────────────────────
begin_section "atomic mv + record round-trips (content preserved, doctor clean)"
make_temp_repo rename-mv-roundtrip
init_repo
printf 'line1\nline2\nline3\n' > old.txt
require_success "add original file" atomic add old.txt
require_success "record base file" record_change "base"
require_success "stage rename" atomic mv old.txt new.txt
printf 'line1\nline2 edited after move\nline3\n' > new.txt
snapshot_dir="$(mktemp -d "${TMPDIR:-/tmp}/atomic-rename-snapshot-XXXXXX")"
_HARNESS_TMPDIRS+=("$snapshot_dir")
cp new.txt "$snapshot_dir/new.txt"
require_capture "read history before rename+edit" atomic log --format json
history_before="$COMMAND_OUTPUT"
if record_out="$(record_change "rename+edit old->new" 2>&1)"; then
    _pass "record rename with edit"
else
    _fail "record rename with edit" "$record_out"
    assert_success "refused record preserves destination bytes" cmp -s "$snapshot_dir/new.txt" new.txt
    assert_file_not_exists "refused record preserves staged source removal" old.txt
    require_capture "read history after refused rename+edit" atomic log --format json
    if [[ "$COMMAND_OUTPUT" == "$history_before" ]]; then
        _pass "refused rename+edit preserves history"
    else
        _fail "refused rename+edit preserves history" "history changed"
    fi
    _skip "rename+edit postconditions and later cases" "record refused"
    print_summary || true
    exit 1
fi
assert_file_exists     "rename: new path exists"            new.txt
assert_file_not_exists "rename: old path gone"              old.txt
assert_file_contains   "rename: edit preserved"             new.txt "line2 edited after move"
assert_no_markers      "rename: no conflict markers"        new.txt
assert_status_clean    "rename: status clean after record"
assert_doctor_consistent "rename: doctor consistent"
assert_output_contains "rename: change exposes authoritative move evidence" \
    "authoritative: old.txt" atomic change

# Content is byte-exact (no trailing-newline drift, no duplication).
assert_occurrences     "rename: line1 once"                 new.txt "line1" 1
assert_occurrences     "rename: edited line2 once"          new.txt "line2 edited after move" 1
assert_occurrences     "rename: line3 once"                 new.txt "line3" 1

# ── Rename back restores the original ────────────────────────────────────────
begin_section "atomic mv back restores the original path"
require_success "stage rename back" atomic mv new.txt old.txt
require_success "record rename back" record_change "rename back new->old"
assert_file_exists     "rename-back: old path restored"     old.txt
assert_file_not_exists "rename-back: new path gone"         new.txt
assert_file_contains   "rename-back: content intact"        old.txt "line2"
assert_status_clean    "rename-back: status clean"
assert_doctor_consistent "rename-back: doctor consistent"

# ── Rename into a subdirectory ───────────────────────────────────────────────
begin_section "atomic mv into a subdirectory"
make_temp_repo rename-mv-subdir
init_repo
printf 'hello\nworld\n' > f.txt
require_success "add subdirectory fixture" atomic add f.txt
require_success "record subdirectory fixture" record_change "base"
mkdir -p sub
require_success "stage subdirectory move" atomic mv f.txt sub/f.txt
require_success "record subdirectory move" record_change "move into sub"
assert_file_exists     "subdir move: sub/f.txt exists"      sub/f.txt
assert_file_not_exists "subdir move: f.txt gone"            f.txt
assert_file_contains   "subdir move: content preserved"     sub/f.txt "world"
assert_status_clean    "subdir move: status clean"
assert_doctor_consistent "subdir move: doctor consistent"

print_summary
