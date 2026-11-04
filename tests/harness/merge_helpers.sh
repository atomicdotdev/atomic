#!/usr/bin/env bash
# merge_helpers.sh — Shared helpers for the merge / materialization suites.
#
# Canonical home for the assertions, conflict-marker checks, repo builders,
# and xfail/honesty machinery that the merge suites used to each re-implement.
# Source it AFTER helpers.sh:
#
#   HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
#   source "$HARNESS_DIR/helpers.sh"
#   source "$HARNESS_DIR/merge_helpers.sh"
#
# Front door for "what merge behavior do we guarantee": suite 28_merge_rubric
# (the canonical A×B coverage matrix). The other merge suites are regression
# records for specific historical bugs:
#   17_cross_view_merge          — cross-view insert content duplication
#   22_switch_conflict_markers   — view-switch never invents markers
#   24_concurrent_insert_conflict— concurrent-draft fork resolution
#   27_merge_ordering_duplication— merge ordering + tail duplication
#
# NOTE: bodies here are the de-duplicated union of what those suites defined;
# where a name was shared its bodies were already behavior-identical.

# Belt-and-suspenders: work even if sourced standalone.
if ! declare -F _pass >/dev/null 2>&1; then
    _MERGE_HELPERS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    # shellcheck source=helpers.sh
    source "$_MERGE_HELPERS_DIR/helpers.sh"
fi

# ── Conflict-marker checks ───────────────────────────────────────────────────
#
# Two spellings are preserved intentionally:
#   * the anchored 7-char form (`^>>>>>>>` …) — precise, used by 27/28
#   * the unanchored 6-char form (`>>>>>>` …) — lenient, used by 22/24
# Atomic's markers are inverted relative to git: START '>>>>>>>',
# SEPARATOR '=======', END '<<<<<<<' (atomic-core/src/output/traits.rs).

# Returns 0 if the file has (lenient) conflict markers, 1 if clean.
has_conflict_markers() {
    local path="$1"
    grep -qE '>>>>>>|<<<<<<|=======' "$path" 2>/dev/null
}

# Assert NO markers (lenient form).
assert_no_conflict_markers() {
    local desc="$1" path="$2"
    if [[ ! -f "$path" ]]; then _fail "$desc" "file does not exist: $path"; return; fi
    if has_conflict_markers "$path"; then
        _fail "$desc" "conflict markers found in $path. Content: $(cat "$path" | head -20)"
    else
        _pass "$desc"
    fi
}

# Assert NO markers (anchored form).
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

# Assert markers ARE present (anchored START).
assert_markers() {
    local desc="$1" path="$2"
    if [[ -f "$path" ]] && grep -qE '^>>>>>>>' "$path" 2>/dev/null; then
        _pass "$desc"
    else
        _fail "$desc" "expected conflict markers in $path but found none. Content:
$( [[ -f "$path" ]] && nl -ba "$path" | sed 's/^/      /' || echo '      (missing)')"
    fi
}

# Assert markers are well-formed (all three sides present).
assert_well_formed_conflict() {
    local desc="$1" path="$2"
    if [[ ! -f "$path" ]]; then _fail "$desc" "file does not exist: $path"; return; fi
    local has_open has_mid has_close
    has_open=$(grep -cE '<<<<<<' "$path" 2>/dev/null || true)
    has_mid=$(grep -cE '=======' "$path" 2>/dev/null || true)
    has_close=$(grep -cE '>>>>>>' "$path" 2>/dev/null || true)
    if [[ "$has_open" -gt 0 && "$has_mid" -gt 0 && "$has_close" -gt 0 ]]; then
        _pass "$desc"
    else
        _fail "$desc" "malformed conflict: <<<<<<=$has_open, =======$has_mid, >>>>>>=$has_close in $path"
    fi
}

# ── Content assertions ───────────────────────────────────────────────────────

# Assert a file contains a substring.
assert_file_contains() {
    local desc="$1" path="$2" needle="$3"
    if [[ ! -f "$path" ]]; then _fail "$desc" "file does not exist: $path"; return; fi
    if grep -qF "$needle" "$path"; then
        _pass "$desc"
    else
        _fail "$desc" "'$needle' not found in $path"
    fi
}

# Assert an existing file does NOT contain a substring.
assert_file_not_contains() {
    local desc="$1" path="$2" needle="$3"
    if [[ ! -f "$path" ]]; then _fail "$desc" "file does not exist: $path"; return; fi
    if grep -qF "$needle" "$path"; then
        _fail "$desc" "'$needle' should not be in $path but was found"
    else
        _pass "$desc"
    fi
}

# Assert a substring alias used by 28 (present-in-file).
assert_present() {
    local desc="$1" path="$2" needle="$3"
    if [[ -f "$path" ]] && grep -qF "$needle" "$path"; then
        _pass "$desc"
    else
        _fail "$desc" "'$needle' not found in $path"
    fi
}

# Count substring (regex) occurrences; prints the count.
count_occurrences() {
    local file="$1" pattern="$2"
    local count status
    if count="$(grep -c -- "$pattern" "$file" 2>&1)"; then
        printf '%s\n' "$count"
    else
        status=$?
        if [[ $status -eq 1 ]]; then
            printf '%s\n' "$count"
        else
            printf 'count failed (exit %s): %s\n' "$status" "$count" >&2
            return "$status"
        fi
    fi
}

# Assert a regex/substring appears exactly N times (substring count).
assert_occurrence_count() {
    local desc="$1" file="$2" pattern="$3" expected="$4"
    if [[ ! -f "$file" ]]; then _fail "$desc" "file does not exist: $file"; return; fi
    local actual
    if ! actual="$(count_occurrences "$file" "$pattern" 2>&1)"; then
        _fail "$desc" "$actual"
        return
    fi
    if [[ "$actual" -eq "$expected" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "expected '$pattern' to appear $expected time(s), got $actual"
    fi
}

# Assert an EXACT LINE appears exactly N times (fixed-string, whole-line).
assert_occurrences() {
    local desc="$1" path="$2" needle="$3" expected="$4"
    local actual=0
    if [[ ! -f "$path" ]]; then _fail "$desc" "file does not exist: $path"; return; fi
    actual=$(grep -cxF "$needle" "$path" 2>/dev/null || true)
    if [[ "$actual" == "$expected" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "'$needle' appears $actual time(s), expected $expected"
    fi
}

# Assert a file has at most N lines.
assert_max_lines() {
    local desc="$1" file="$2" max="$3"
    if [[ ! -f "$file" ]]; then _fail "$desc" "file does not exist: $file"; return; fi
    local count
    count="$(wc -l < "$file" | tr -d ' ')"
    if [[ "$count" -le "$max" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "file has $count lines, expected at most $max"
    fi
}

# Assert a file has exactly N lines.
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

# Assert a file equals an expected body EXACTLY.
#
# Compares two REAL files — no process substitution. Sandboxed environments
# can block /dev/fd (making `diff <(printf …)` fail with EPERM); the old form
# treated that tool error (diff exit >= 2) as a content mismatch with an empty
# diff body. diff exit: 0 identical, 1 differ, >= 2 tool error.
assert_file_equals() {
    local desc="$1" path="$2" expected="$3"
    if [[ ! -f "$path" ]]; then _fail "$desc" "file does not exist: $path"; return; fi
    local expected_file
    expected_file="$(mktemp "${TMPDIR:-/tmp}/harness-expected-XXXXXX")"
    printf '%s' "$expected" > "$expected_file"
    local diff_out diff_exit
    if diff_out="$(diff -u "$expected_file" "$path" 2>&1)"; then
        diff_exit=0
    else
        diff_exit=$?
    fi
    rm -f "$expected_file"
    case $diff_exit in
        0) _pass "$desc" ;;
        1) _fail "$desc" "content mismatch:
$(echo "$diff_out" | sed 's/^/      /' | head -30)" ;;
        *) _fail "$desc" "diff tool error (exit $diff_exit): $diff_out" ;;
    esac
}

# Snapshot a file's content for later comparison.
snapshot_file() {
    local path="$1"
    if [[ -f "$path" ]]; then cat "$path"; else echo "__MISSING__"; fi
}

# Assert a file's content is identical to a saved snapshot.
assert_file_stable() {
    local desc="$1" path="$2" expected_snapshot="$3"
    if [[ ! -f "$path" ]]; then _fail "$desc" "file does not exist: $path"; return; fi
    local actual
    actual="$(cat "$path")"
    if [[ "$actual" == "$expected_snapshot" ]]; then
        _pass "$desc"
    else
        _fail "$desc" "content changed. Expected $(echo "$expected_snapshot" | wc -c | tr -d ' ') bytes, got $(echo "$actual" | wc -c | tr -d ' ') bytes"
    fi
}

# ── View / repo helpers ──────────────────────────────────────────────────────

# The view name a fresh repo starts on (the one marked '*').
current_view() {
    local output
    if ! output="$(atomic view list --all --json 2>&1)"; then
        printf 'view list failed: %s\n' "$output" >&2
        return 1
    fi
    printf '%s\n' "$output" | python3 -c 'import json,sys; name=json.load(sys.stdin)["current_view"]; assert isinstance(name,str) and name; print(name)'
}

# Hash of the most recent change on a view.
tip_hash() {
    local output
    if ! output="$(atomic log --format json --count 1 --view "$1" 2>&1)"; then
        printf 'log failed: %s\n' "$output" >&2
        return 1
    fi
    printf '%s\n' "$output" | python3 -c 'import json,sys; rows=json.load(sys.stdin); assert len(rows)==1 and len(rows[0]["hash"])==52; print(rows[0]["hash"])'
}
# Backwards-compatible alias (27 spelling).
tip_hash_of_view() { tip_hash "$1"; }

# "__DELETE__" removes f.txt; anything else is written verbatim.
write_or_delete() {
    if [[ "$1" == "__DELETE__" ]]; then rm -f f.txt; else printf '%s' "$1" > f.txt; fi
}

# The default 5-line base used by several cases.
BASE_5=$'alpha\nbeta\ngamma\ndelta\nepsilon\n'

# Build a repo with two views that each edit f.txt from a common base, then
# insert side-a into the base view. Leaves the working copy on the base view
# with change A inserted. Sets BASE_VIEW. (27's builder.)
#
#   $1 — content of f.txt on the draft (change A)
#   $2 — content of f.txt on the base view (change B)
#   $3 — shared starting content (optional; defaults to a 5-line file)
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
    require_success "add base file" atomic add f.txt
    require_success "record base file" record_change "base"

    require_capture "read base view" current_view
    BASE_VIEW="$COMMAND_OUTPUT"

    require_success "create side-a" new_view side-a
    require_success "switch to side-a" switch_view side-a
    printf '%s' "$a_content" > f.txt
    require_success "record edit A" record_change "edit A"
    local hash_a
    require_capture "read edit A hash" tip_hash_of_view side-a
    hash_a="$COMMAND_OUTPUT"

    require_success "switch back to base view" switch_view "$BASE_VIEW"
    printf '%s' "$b_content" > f.txt
    require_success "record edit B" record_change "edit B"

    local snapshot_dir history_before output
    snapshot_dir="$(mktemp -d "${TMPDIR:-/tmp}/atomic-merge-builder-XXXXXX")"
    _HARNESS_TMPDIRS+=("$snapshot_dir")
    cp f.txt "$snapshot_dir/f.txt"
    require_capture "read base history before insert" atomic log --format json --view "$BASE_VIEW" --all
    history_before="$COMMAND_OUTPUT"
    if output="$(atomic insert "$hash_a" 2>&1)"; then
        _pass "insert edit A"
    else
        _fail "insert edit A" "$output"
        assert_success "refused insert preserves file bytes" cmp -s "$snapshot_dir/f.txt" f.txt
        assert_current_view "refused insert preserves base view" "$BASE_VIEW"
        require_capture "read base history after refusal" atomic log --format json --view "$BASE_VIEW" --all
        if [[ "$COMMAND_OUTPUT" == "$history_before" ]]; then
            _pass "refused insert preserves base history"
        else
            _fail "refused insert preserves base history" "history changed"
        fi
        _skip "merge postconditions and later cases" "insert failed"
        print_summary || true
        exit 1
    fi
}

# Build a base repo, a `feature` draft with edit A, and edit B on the base
# view; leaves the working copy on the base view. Sets BASE_VIEW and
# FEATURE_HASH. Does NOT insert — the caller chooses the pathway. (28's
# builder.) "$b" == "__UNCHANGED__" records no edit B.
build_case() {
    local base="$1" a="$2" b="$3"
    init_repo
    printf '%s' "$base" > f.txt
    require_success "add base file" atomic add f.txt
    require_success "record base file" record_change "base"
    require_capture "read base view" current_view
    BASE_VIEW="$COMMAND_OUTPUT"

    require_success "create feature" new_view feature
    require_success "switch to feature" switch_view feature
    write_or_delete "$a"
    require_success "record edit A" record_change "edit A"
    require_capture "read edit A hash" tip_hash feature
    FEATURE_HASH="$COMMAND_OUTPUT"

    require_success "switch to base view" switch_view "$BASE_VIEW"
    if [[ "$b" != "__UNCHANGED__" ]]; then
        write_or_delete "$b"
        require_success "record edit B" record_change "edit B"
    fi
}

# ── Expected-failure (xfail) machinery ───────────────────────────────────────
#
# `xfail_correct desc predicate...` runs a predicate that returns 0 when the
# CORRECT behavior holds. While the bug is unfixed the predicate fails and is
# reported as a loud KNOWN-BUG line that does NOT fail the suite. If it ever
# succeeds (bug fixed) that is a HARD FAILURE, forcing promotion to a real
# assertion.
KNOWN_BUGS=0
xfail_correct() {
    local desc="$1"; shift
    if "$@" >/dev/null 2>&1; then
        _fail "$desc" "xpass: the correct behavior now holds — promote this xfail to a real assertion"
    else
        TESTS_RUN=$((TESTS_RUN + 1))
        TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
        KNOWN_BUGS=$((KNOWN_BUGS + 1))
        echo "  ${YELLOW}⊘ KNOWN BUG${RESET} $desc ${YELLOW}(tracked; correct behavior not yet implemented)${RESET}"
    fi
}

# Correctness predicates (return 0 when the CORRECT behavior holds).
pred_file_absent() { [[ ! -f f.txt ]]; }
pred_conflict_surfaced() {
    grep -qE '^>>>>>>>' f.txt 2>/dev/null \
        && atomic conflicts --short 2>/dev/null | grep -qE '^f\.txt:'
}
pred_delete_or_conflict() { pred_file_absent || pred_conflict_surfaced; }
pred_no_silent_line_loss() {
    pred_conflict_surfaced || grep -qxF 'alpha' f.txt 2>/dev/null
}

# The honesty invariant: the three conflict signals must agree.
#   m = file on disk carries a START marker
#   s = `atomic status --short` reports the file Conflicted (C)
#   l = `atomic conflicts --short` lists the file
assert_honest() {
    local desc="$1" path="$2"
    local m s l
    if [[ -f "$path" ]] && grep -qE '^>>>>>>>' "$path" 2>/dev/null; then m=1; else m=0; fi
    local sstatus cshort esc
    if ! sstatus="$(atomic status --short 2>&1)"; then
        _fail "$desc" "status command failed: $sstatus"
        return
    fi
    if ! cshort="$(atomic conflicts --short 2>&1)"; then
        _fail "$desc" "conflicts command failed: $cshort"
        return
    fi
    esc="$(printf '%s' "$path" | sed 's/[].[^$*+?{}()|\\/]/\\&/g')"
    if echo "$sstatus" | grep -qE "^C[[:space:]]+${esc}$"; then s=1; else s=0; fi
    if echo "$cshort" | grep -qE "^${esc}:"; then l=1; else l=0; fi

    if [[ "$m" == "$s" && "$s" == "$l" ]]; then
        _pass "$desc (markers=$m status=$s conflicts=$l)"
    else
        _fail "$desc" "conflict signals disagree: markers=$m status=$s conflicts=$l
      status --short:
$(echo "$sstatus" | sed 's/^/        /')
      conflicts --short:
$(echo "$cshort" | sed 's/^/        /')"
    fi
}
