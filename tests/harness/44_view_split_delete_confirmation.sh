#!/usr/bin/env bash
# CLI regression: split requires risk acknowledgment, and deletion requires it
# when a draft holds the last references to changes. Auto-discovered by run_all.
# Run: ATOMIC_BIN=/path/to/atomic bash tests/harness/run_all.sh 44

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# Make the no-terminal path deterministic even when run from an interactive
# shell. Assertions/captured output redirect stdout and stderr away from a TTY.
noninteractive() {
    atomic "$@" </dev/null 2>&1
}

# Match actual JSON fields, not message text or truncated display hashes.
latest_hash() {
    local out pattern='"hash"[[:space:]]*:[[:space:]]*"([^"]+)"'
    out="$(atomic log --view "$1" -n 1 -f json)" || return
    [[ "$out" =~ $pattern ]] || return 1
    printf '%s\n' "${BASH_REMATCH[1]}"
}

view_has_change() {
    local out pattern="\"hash\"[[:space:]]*:[[:space:]]*\"$2\""
    out="$(atomic log --view "$1" -f json)" || return
    [[ "$out" =~ $pattern ]]
}

view_exists() {
    local out pattern="\"name\"[[:space:]]*:[[:space:]]*\"$1\""
    out="$(atomic view list --all --json)" || return
    [[ "$out" =~ $pattern ]]
}

begin_section "Split preview and unavailable confirmation preserve source and files"

make_temp_repo "split-confirmation"
init_repo --view dev
create_file base.txt $'base\n'
add_files base.txt >/dev/null
record_change "record base" >/dev/null
BASE_HASH="$(latest_hash dev)"
create_file work.txt $'important work\n'
add_files work.txt >/dev/null
record_change "record important work" >/dev/null
WORK_HASH="$(latest_hash dev)"

assert_success "dry-run needs no confirmation" \
    noninteractive view split wip --last 1 --dry-run
assert_failure "dry-run creates no draft" view_exists wip
assert_success "dry-run retains the source reference" view_has_change dev "$WORK_HASH"
assert_file_content "dry-run preserves working content" work.txt "important work"

assert_failure "split refuses without a terminal or --confirm" \
    noninteractive view split wip --last 1
assert_output_contains "refusal explains how to acknowledge the risks" \
    "Re-run with --confirm" noninteractive view split wip --last 1
assert_output_contains "split warns about later orphaning" \
    "Deleting that draft" noninteractive view split wip --last 1
assert_failure "refused split creates no draft" view_exists wip
assert_success "refused split retains the source reference" view_has_change dev "$WORK_HASH"
assert_file_content "refused split preserves working content" work.txt "important work"

assert_success "explicit confirmation allows split and switch" \
    noninteractive view split wip --last 1 --confirm --switch
assert_current_view "--switch enters the new draft" wip
assert_failure "source no longer owns extracted work" view_has_change dev "$WORK_HASH"
assert_success "draft owns extracted work" view_has_change wip "$WORK_HASH"
assert_success "source retains unrelated base" view_has_change dev "$BASE_HASH"
assert_file_content "draft still materializes extracted work" work.txt "important work"

begin_section "Orphaning deletion requires confirmation and lists the last references"

assert_success "return to source" switch_view dev
assert_file_not_exists "extracted file is absent from source" work.txt
assert_failure "orphaning deletion refuses without a terminal or --force" \
    noninteractive view delete wip
assert_output_contains "deletion reports orphan count" \
    "would orphan 1 change(s)" noninteractive view delete wip
assert_output_contains "deletion lists the full orphan hash" \
    "$WORK_HASH" noninteractive view delete wip
assert_output_contains "deletion gives the explicit acknowledgment flag" \
    "Re-run with --force" noninteractive view delete wip
assert_output_not_contains "inherited base is not an orphan candidate" \
    "$BASE_HASH" noninteractive view delete wip
assert_success "refused deletion preserves draft" view_exists wip
assert_success "refused deletion preserves its last reference" view_has_change wip "$WORK_HASH"

assert_success "--force acknowledges orphaning and deletes the draft" \
    noninteractive view delete wip --force
assert_failure "forced deletion removes the draft" view_exists wip
assert_failure "orphaned work does not return to source" view_has_change dev "$WORK_HASH"
assert_success "view deletion retains the change object" atomic change "$WORK_HASH"
assert_success "base reference survives deletion" view_has_change dev "$BASE_HASH"
assert_file_content "base working content survives deletion" base.txt "base"

begin_section "Retaining extracted work elsewhere makes draft deletion safe"

make_temp_repo "split-retained"
init_repo --view dev
create_file work.txt $'retained work\n'
add_files work.txt >/dev/null
record_change "record retained work" >/dev/null
WORK_HASH="$(latest_hash dev)"
assert_success "split work into a draft" \
    noninteractive view split retained --last 1 --confirm
assert_success "insert extracted change back into source" atomic insert "$WORK_HASH" --deps
assert_success "source now references the extracted change" view_has_change dev "$WORK_HASH"
assert_success "retained draft deletes without --force or a terminal" \
    noninteractive view delete retained
assert_failure "retained draft is deleted" view_exists retained
assert_success "source keeps the change after draft deletion" view_has_change dev "$WORK_HASH"
assert_file_content "retained work remains materialized" work.txt "retained work"

assert_success "create an inherited-only draft" atomic view create inherited --parent dev
assert_success "inherited-only draft deletes without confirmation" noninteractive view delete inherited
assert_success "deleting inheriting draft keeps source references" view_has_change dev "$WORK_HASH"

begin_section "Dependency checks and cascaded split counts survive confirmation gating"

make_temp_repo "split-cascade-confirmation"
init_repo --view dev
create_file f.txt $'first version\n'
add_files f.txt >/dev/null
record_change "create file" >/dev/null
CREATE_HASH="$(latest_hash dev)"
overwrite_file f.txt $'second version\n'
record_change "edit file" >/dev/null
EDIT_HASH="$(latest_hash dev)"

assert_failure "dependent changes still block splitting without --cascade" \
    noninteractive view split cascade "$CREATE_HASH"
assert_failure "blocked split creates no draft" view_exists cascade
assert_success "cascade dry-run does not prompt" \
    noninteractive view split cascade "$CREATE_HASH" --cascade --dry-run
assert_failure "cascade still requires explicit confirmation" \
    noninteractive view split cascade "$CREATE_HASH" --cascade
assert_output_contains "warning includes the full cascaded move count" \
    "removes 2 change(s)" noninteractive view split cascade "$CREATE_HASH" --cascade
assert_output_contains "warning distinguishes the additional dependent" \
    "1 dependent change(s) included" noninteractive view split cascade "$CREATE_HASH" --cascade
assert_success "refused cascade retains creation" view_has_change dev "$CREATE_HASH"
assert_success "refused cascade retains dependent edit" view_has_change dev "$EDIT_HASH"
assert_file_content "refused cascade preserves edited working content" f.txt "second version"

assert_success "acknowledged cascade moves both changes" \
    noninteractive view split cascade "$CREATE_HASH" --cascade --confirm --switch
assert_success "draft contains original creation" view_has_change cascade "$CREATE_HASH"
assert_success "draft contains dependent edit" view_has_change cascade "$EDIT_HASH"
assert_failure "source no longer references creation" view_has_change dev "$CREATE_HASH"
assert_failure "source no longer references dependent edit" view_has_change dev "$EDIT_HASH"
assert_file_content "cascade draft materializes the edited state" f.txt "second version"

print_summary
