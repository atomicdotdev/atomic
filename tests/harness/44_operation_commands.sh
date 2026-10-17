#!/usr/bin/env bash
# CB-1C operation history, inverse transition, and content-preservation contract.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

operation_head() {
    atomic op log -n 1 --json 2>/dev/null |
        sed -n 's/.*"head": "\([A-Z2-7]*\)".*/\1/p' |
        head -1
}

begin_section "Operation log and show expose canonical journal data"
make_temp_repo "operation-commands"
init_repo
BASE_VIEW="$(atomic view list 2>/dev/null | awk '/^\*/ { print $2 }')"

create_file "state.txt" "base content"
assert_success "track operation fixture" atomic add state.txt
assert_success "record operation fixture" atomic record -m "record base operation fixture"
RECORD_HEAD="$(operation_head)"
if [[ ${#RECORD_HEAD} -eq 52 ]]; then
    _pass "op log returns a full operation head"
else
    _fail "op log returns a full operation head" "got '$RECORD_HEAD'"
fi
assert_output_contains "op show exposes record kind" '"kind": "record"' \
    atomic op show "$RECORD_HEAD" --json
assert_output_contains "op show exposes metadata leases" '"metadata"' \
    atomic op show "$RECORD_HEAD" --json
assert_output_contains "human op show exposes receipts" "Receipts:" \
    atomic op show "${RECORD_HEAD:0:8}"

begin_section "Switch undo and restore preserve selected view content"
assert_success "create operation feature view" \
    atomic view create operation-feature --draft --parent "$BASE_VIEW"
assert_success "switch to operation feature" atomic view switch operation-feature
overwrite_file "state.txt" "feature content"
assert_success "record feature content" atomic record -m "record feature operation fixture"
assert_success "switch back to base" atomic view switch "$BASE_VIEW"
assert_file_content "base content selected before switch undo" "state.txt" "base content"
SWITCH_HEAD="$(operation_head)"
assert_output_contains "switch head is journaled" '"kind": "switch_view"' \
    atomic op show "$SWITCH_HEAD" --json
assert_success "undo current switch operation" atomic op undo --json
assert_file_content "switch undo restores feature content" "state.txt" "feature content"
assert_output_contains "undo records its target relation" "${SWITCH_HEAD}" \
    atomic op show "$(operation_head)" --json
assert_success "restore original switch after-state" atomic op restore "$SWITCH_HEAD" --json
assert_file_content "switch restore selects base content" "state.txt" "base content"
assert_output_contains "restore operation is journaled" '"kind": "restore"' \
    atomic op show "$(operation_head)" --json

begin_section "Record undo preserves bytes and change objects"
overwrite_file "state.txt" "base content after record"
assert_success "record content for record undo" atomic record -m "record undo fixture"
RECORD_UNDO_TARGET="$(operation_head)"
CHANGE_FILES_BEFORE="$(find .atomic/changes -type f | wc -l | tr -d '[:space:]')"
assert_output_contains "record undo target is a record operation" '"kind": "record"' \
    atomic op show "$RECORD_UNDO_TARGET" --json
assert_success "undo record operation" atomic op undo "$RECORD_UNDO_TARGET" --json
assert_file_content "record undo leaves working bytes untouched" \
    "state.txt" "base content after record"
CHANGE_FILES_AFTER="$(find .atomic/changes -type f | wc -l | tr -d '[:space:]')"
if [[ "$CHANGE_FILES_AFTER" -ge "$CHANGE_FILES_BEFORE" ]]; then
    _pass "record undo preserves content-addressed change objects"
else
    _fail "record undo preserves content-addressed change objects" \
        "before=$CHANGE_FILES_BEFORE after=$CHANGE_FILES_AFTER"
fi
assert_output_contains "record undo is journaled as Undo" '"kind": "undo"' \
    atomic op show "$(operation_head)" --json
assert_success "restore recorded metadata" atomic op restore "$RECORD_UNDO_TARGET" --json
assert_file_content "record restore retains working bytes" \
    "state.txt" "base content after record"
assert_output_contains "operation log includes inverse history" '"kind": "undo"' \
    atomic op log -n 6 --json
assert_output_contains "operation log includes restore history" '"kind": "restore"' \
    atomic op log -n 6 --json

begin_section "Journaled materialization honors restrictive umask"
CHANGE_REF="$(atomic log -n 1 -f oneline 2>/dev/null | awk 'NR == 1 { print $1 }')"
assert_success "unrecord file before absent-path materialization" atomic unrecord
rm -f state.txt
if (umask 077; atomic insert "$CHANGE_REF" >/dev/null 2>&1); then
    _pass "insert rematerializes the absent file under umask 077"
else
    _fail "insert rematerializes the absent file under umask 077" \
        "atomic insert $CHANGE_REF failed"
fi
MODE="$(stat -f '%Lp' state.txt 2>/dev/null || stat -c '%a' state.txt 2>/dev/null || echo unknown)"
if [[ "$MODE" == "600" ]]; then
    _pass "newly materialized file mode is restricted to 0600"
else
    _fail "newly materialized file mode is restricted to 0600" "mode=$MODE"
fi

print_summary
