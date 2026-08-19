#!/usr/bin/env bash
# 32_review_gate_insert.sh — Executable spec for squash-import → insert originals
#
# Walks the workflow from SPEC-review-gate-insert-originals.md:
#
#   1. git repo + `atomic git import`  → shared view (named after git default)
#   2. Agent work in an Atomic draft    → per-turn CHANGE RECORDS (the originals)
#   3. Simulated GitHub squash-merge    → a single git commit on the shared
#      branch whose body carries the originals' `Atomic-Changes` trailers
#      (multiple blocks, mimicking GitHub concatenating each squashed body)
#   4. `atomic git import --incremental --branch <shared>`
#
# TARGET (SPEC §2, §4): the squash must NOT become a new change record. Instead
# the importer inserts the named originals into the shared view (they already
# live in the graph) and represents the squash as an aggregate ReviewGate TAG
# (from…to, count, inserted:true). Deleting the draft must then be safe.
#
# STATUS: the insert path is not implemented yet. Target-behavior checks are
# gated on $EXPECT_INSERT:
#   - unset/0 → reported as pending (skipped), suite stays green
#   - 1       → hard assertions, so this file drives implementation red→green
#
# What already holds today (hard-asserted here, no gate):
#   - the import succeeds and the shared view materializes the squash content
#   - a ReviewGate tag is created for the squash
#   - its metadata lists EVERY original hash across ALL trailer blocks
#     (the BUG-review-gate-original-hashes.md fix, validated end-to-end)

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ── Target-behavior gate ──────────────────────────────────────────────────
#
# assert_target <desc> <cmd...>
#   pass    if the command succeeds (target behavior is present)
#   fail    if it does not AND EXPECT_INSERT=1 (drives implementation)
#   pending otherwise (feature not built yet — keeps the suite green)
assert_target() {
    local desc="$1"; shift
    if "$@" >/dev/null 2>&1; then
        _pass "$desc"
    elif [[ "${EXPECT_INSERT:-0}" == "1" ]]; then
        _fail "$desc" "target behavior (SPEC) not met"
    else
        _skip "$desc" "pending: insert-originals (SPEC §4) not implemented"
    fi
}

# Predicate helpers (used as commands for assert_* so they run under the gate).
#
# main_has_record matches the change's `hash` FIELD exactly — not a raw substring
# of the JSON. A squash change record embeds the originals' `Atomic-Changes`
# trailers in its commit message, so a substring grep would spuriously match
# even when the record was never inserted. Exact hash-field membership is the
# real "is this original a change in the shared view?" test.
# These predicates capture command output into a variable and then match with a
# here-string, deliberately avoiding `cmd | grep -q`. Under `set -o pipefail`
# (active via helpers.sh), `grep -q` exits on first match and closes the pipe;
# the still-writing `atomic` process then dies with SIGPIPE, and pipefail
# reports the pipeline as failed even though the text matched. Two more notes:
#   * `grep -a` forces text mode — `tag show` / the router emit UTF-8 glyphs
#     (…, ✓, —) that make macOS/BSD grep treat the stream as binary.
#   * `main_has_record` matches the `hash` FIELD exactly (whole line), so a
#     squash record's embedded trailer text can't produce a false positive.
main_view_hashes() {
    atomic log --view "$MAIN" -f json 2>/dev/null \
        | grep -ao '"hash"[[:space:]]*:[[:space:]]*"[^"]*"' \
        | sed -E 's/.*"([^"]+)"$/\1/'
}
main_has_record() {
    local out; out="$(main_view_hashes)"
    grep -qxaF "$1" <<<"$out"
}
tag_show_contains() {
    local out; out="$(atomic tag show "$TAG_NAME" 2>/dev/null)"
    grep -qaF "$1" <<<"$out"
}
# Dry-run router recommendation for the current target (SPEC §4.4).
dry_run_recommends_insert() {
    local out; out="$(atomic git import --incremental --branch "$MAIN" --dry-run 2>&1)"
    grep -qaiE "squash|atomic header|insert" <<<"$out"
}
# The shared view materializes `src/two.rs` after switching to it.
shared_view_materializes_two() {
    atomic view switch "$MAIN" --force >/dev/null 2>&1 && test -f src/two.rs
}

# ── Suite banner ──────────────────────────────────────────────────────────

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 32_review_gate_insert${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

# ════════════════════════════════════════════════════════════════════════
# Section 1: Prerequisites
# ════════════════════════════════════════════════════════════════════════

begin_section "Prerequisites"
require_git

# ════════════════════════════════════════════════════════════════════════
# Section 2: Seed the shared view from git
# ════════════════════════════════════════════════════════════════════════

begin_section "Seed shared view from git import"

make_temp_repo "review-gate-insert"
init_git_repo
git_commit "Initial" "README.md" "# Project"

assert_success "git import seeds the shared view" atomic git import --no-vault

MAIN="$(git_default_branch)"
_pass "shared view is '$MAIN'"

# ════════════════════════════════════════════════════════════════════════
# Section 3: Agent work in a draft view — the original change records
# ════════════════════════════════════════════════════════════════════════

begin_section "Record originals in a draft view"

assert_success "create + switch to draft" \
    atomic view create agent --draft --parent "$MAIN" --switch

# Two per-turn changes. Their on-disk content must match what the squash tree
# will contain, so inserting them reproduces the squash exactly (SPEC §5/D1).
create_file "src/one.rs" "fn one() {}"
add_files "src/one.rs"
record_change "feat: one" >/dev/null

create_file "src/two.rs" "fn two() {}"
add_files "src/two.rs"
record_change "feat: two" >/dev/null

# Capture the two originals' hashes (newest-first). The draft's own changes are
# the two most recent, regardless of whether inherited history is shown.
ORIG=()
while IFS= read -r h; do
    [[ -n "$h" ]] && ORIG+=("$h")
done < <(atomic log --view agent -f json 2>/dev/null \
            | grep -o '"hash"[[:space:]]*:[[:space:]]*"[^"]*"' \
            | sed -E 's/.*"([^"]+)"$/\1/')

HASH_TWO="${ORIG[0]:-}"   # newest  (feat: two)
HASH_ONE="${ORIG[1]:-}"   # older   (feat: one)

if [[ -n "$HASH_ONE" && -n "$HASH_TWO" && "$HASH_ONE" != "$HASH_TWO" ]]; then
    _pass "captured two original change hashes"
else
    _fail "captured two original change hashes" \
        "one='$HASH_ONE' two='$HASH_TWO' (log parse failed?)"
    print_summary
    exit 1
fi

# ════════════════════════════════════════════════════════════════════════
# Section 4: Simulate the GitHub squash merge onto the shared branch
# ════════════════════════════════════════════════════════════════════════

begin_section "Simulate squash-merge commit with multi-block trailers"

# Return to the shared view so the working copy is the shared baseline; the
# draft-only files disappear, then we recreate them as the squash tree.
switch_view "$MAIN" >/dev/null

# The squash tree == shared baseline + both originals' content.
create_file "src/one.rs" "fn one() {}"
create_file "src/two.rs" "fn two() {}"
git add src/one.rs src/two.rs

# GitHub concatenates each squashed commit's body, so the merge commit carries
# ONE Atomic-Changes block per original. A bullet line heads each block so the
# final paragraph is never mistaken for an `atomic git push` self-push trailer.
git commit -q -m "Add feature one and two (#1)

* feat: one
Atomic-View: agent
Atomic-Changes: $HASH_ONE

* feat: two
Atomic-View: agent
Atomic-Changes: $HASH_TWO"

assert_output_contains "squash commit carries both trailers (block 1)" \
    "$HASH_ONE" git log -1 --format=%B
assert_output_contains "squash commit carries both trailers (block 2)" \
    "$HASH_TWO" git log -1 --format=%B

# ════════════════════════════════════════════════════════════════════════
# Section 5: Dry-run router (SPEC §4.4) — advisory classification
# ════════════════════════════════════════════════════════════════════════

begin_section "Dry-run classifies the incoming squash"

# Always true today: the forecast reports the one new commit.
assert_output_contains "dry-run forecasts one new commit" \
    "1 commit" atomic git import --incremental --branch "$MAIN" --dry-run

# TARGET (SPEC §4.4): the forecast should recognise atomic headers and route
# the user to the insert path rather than a plain count.
assert_target "dry-run flags an insertable atomic squash" dry_run_recommends_insert

# ════════════════════════════════════════════════════════════════════════
# Section 6: Incremental import of the squash
# ════════════════════════════════════════════════════════════════════════

begin_section "Incremental import: squash → shared view"

MAIN_BEFORE="$(atomic log --view "$MAIN" 2>/dev/null | grep -c '^#' || true)"

assert_success "incremental import of squash" \
    atomic git import --incremental --branch "$MAIN" --no-vault

switch_view "$MAIN" >/dev/null

# Always-true guarantees (both current and target behavior satisfy these):
assert_file_content "shared view materializes one.rs" "src/one.rs" "fn one() {}"
assert_file_content "shared view materializes two.rs" "src/two.rs" "fn two() {}"

TAG_NAME="pr-1"
assert_output_contains "ReviewGate tag created for the squash" \
    "$TAG_NAME" atomic tag list

# Prerequisite fix (BUG-review-gate-original-hashes.md), validated end-to-end:
# the ReviewGate metadata must list EVERY original hash, not just the first.
assert_success "ReviewGate records first original hash" tag_show_contains "$HASH_ONE"
assert_success "ReviewGate records second original hash" tag_show_contains "$HASH_TWO"

# ════════════════════════════════════════════════════════════════════════
# Section 7: TARGET behavior — originals inserted, squash is a tag
# ════════════════════════════════════════════════════════════════════════

begin_section "Target: originals live in the shared view (SPEC §2, §4)"

# The shared view must reference the ORIGINAL records, not a new squash record.
assert_target "shared view contains original one.rs record" main_has_record "$HASH_ONE"
assert_target "shared view contains original two.rs record" main_has_record "$HASH_TWO"

# The squash must NOT add a brand-new change record: with insert, the shared
# view grows by exactly the two originals (not by a single squash record).
MAIN_AFTER="$(atomic log --view "$MAIN" 2>/dev/null | grep -c '^#' || true)"
assert_target "shared view grew by exactly the two originals" \
    test "$MAIN_AFTER" -eq "$((MAIN_BEFORE + 2))"

# The ReviewGate is an aggregate over the inserted records. `tag show` renders
# the metadata (not raw JSON), so assert against the rendered provenance block.
assert_target "ReviewGate marks the aggregate as inserted" tag_show_contains "inserted"
assert_target "ReviewGate shows the aggregate range" tag_show_contains "Aggregate"
assert_target "ReviewGate records the git squash provenance" tag_show_contains "squash"

# ════════════════════════════════════════════════════════════════════════
# Section 8: TARGET behavior — deleting the draft is safe
# ════════════════════════════════════════════════════════════════════════

begin_section "Target: draft is disposable, provenance survives (SPEC §1)"

# Drafts must be deletable without losing the granular history: the shared view
# now references the originals, so they stay alive and materializable.
atomic view delete agent --force >/dev/null 2>&1 || \
    atomic view delete agent >/dev/null 2>&1 || true

assert_target "originals still in shared view after draft deletion" \
    main_has_record "$HASH_ONE"
assert_target "shared view still materializes after draft deletion" \
    shared_view_materializes_two

# ── Summary ────────────────────────────────────────────────────────────────

print_summary
