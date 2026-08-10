#!/usr/bin/env bash
# chmod +x tests/harness/29_rename.sh
#
# 29_rename.sh — Rename / move end-to-end (rubric A10 groundwork).
#
# Drives the REAL `atomic mv` binary to prove that a rename records as a genuine
# move (inode/history preserved), not a delete+add, and that the working copy
# stays consistent. This is the CLI-level guard for the staged rename effort
# (docs/MERGE-CONFLICT-RUBRIC.md §6.7):
#   Stage 1 (ATOM::34) — record detects a git-style raw rename → FileMove.
#   Stage 2 (ATOM::35) — `atomic mv` no longer eagerly rewrites TREE, so it
#                         flows through that same detection.
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
    out="$(atomic status --short 2>/dev/null || true)"
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
    if atomic doctor check >/dev/null 2>&1; then
        _pass "$desc"
    else
        _fail "$desc" "atomic doctor check reported inconsistency"
    fi
}

# Assert a `status --short` line is present (e.g. 'D  old.txt', '?? new.txt').
assert_status_has() {
    local desc="$1" pattern="$2"
    local out
    out="$(atomic status --short 2>/dev/null || true)"
    if echo "$out" | grep -qE "$pattern"; then
        _pass "$desc"
    else
        _fail "$desc" "status --short missing /$pattern/. got:
$(echo "$out" | sed 's/^/      /')"
    fi
}

# ── Stage 2 mechanism: `atomic mv` does NOT eagerly track ────────────────────
begin_section "atomic mv leaves a raw-rename shape (no eager TREE update)"
make_temp_repo rename-mv-shape
init_repo
printf 'line1\nline2\nline3\n' > old.txt
add_files old.txt >/dev/null
record_change "base" >/dev/null
atomic mv old.txt new.txt >/dev/null 2>&1
# Before record: old is Deleted (tracked, gone from disk), new is Untracked.
assert_status_has "mv: old path shows Deleted"    '^D[[:space:]]+old\.txt$'
assert_status_has "mv: new path shows Untracked"  '^\?\?[[:space:]]+new\.txt$'

# ── Rename round-trips and stays consistent ──────────────────────────────────
begin_section "atomic mv + record round-trips (content preserved, doctor clean)"
make_temp_repo rename-mv-roundtrip
init_repo
printf 'line1\nline2\nline3\n' > old.txt
add_files old.txt >/dev/null
record_change "base" >/dev/null
atomic mv old.txt new.txt >/dev/null 2>&1
record_change "rename old->new" >/dev/null
assert_file_exists     "rename: new path exists"            new.txt
assert_file_not_exists "rename: old path gone"              old.txt
assert_file_contains   "rename: content preserved"          new.txt "line2"
assert_no_markers      "rename: no conflict markers"        new.txt
assert_status_clean    "rename: status clean after record"
assert_doctor_consistent "rename: doctor consistent"

# Content is byte-exact (no trailing-newline drift, no duplication).
assert_occurrences     "rename: line1 once"                 new.txt "line1" 1
assert_occurrences     "rename: line2 once"                 new.txt "line2" 1
assert_occurrences     "rename: line3 once"                 new.txt "line3" 1

# ── Rename back restores the original ────────────────────────────────────────
begin_section "atomic mv back restores the original path"
atomic mv new.txt old.txt >/dev/null 2>&1
record_change "rename back new->old" >/dev/null
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
add_files f.txt >/dev/null
record_change "base" >/dev/null
mkdir -p sub
atomic mv f.txt sub/f.txt >/dev/null 2>&1
record_change "move into sub" >/dev/null
assert_file_exists     "subdir move: sub/f.txt exists"      sub/f.txt
assert_file_not_exists "subdir move: f.txt gone"            f.txt
assert_file_contains   "subdir move: content preserved"     sub/f.txt "world"
assert_status_clean    "subdir move: status clean"
assert_doctor_consistent "subdir move: doctor consistent"

print_summary
