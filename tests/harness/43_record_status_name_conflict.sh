#!/usr/bin/env bash
# 43_record_status_name_conflict.sh
# Regression: resolving a same-path, independent-inode name conflict must not
# leave status saying modified while record silently says the tree is clean.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"
source "$HARNESS_DIR/merge_helpers.sh"

for resolution in retained other edited; do
    begin_section "Independent file identities produce a materialized name conflict"
    make_temp_repo "record-status-name-conflict-$resolution"
    init_repo
    create_file "seed.txt" $'seed\n'
    atomic add seed.txt
    record_change "seed base"
    new_view "feature" --from dev
    new_view "target" --from dev
    switch_view "feature"
    create_file "f.txt" $'from-feature first line\nbody line\n'
    atomic add f.txt
    record_change "feature creates f.txt"
    switch_view "target"
    create_file "f.txt" $'from-target first line\nbody line\n'
    atomic add f.txt
    record_change "target creates f.txt"
    insert_from_view "feature" "target"
    assert_output_contains "fixture has a name conflict" "(name conflict)" cat f.txt

    begin_section "Record the clean resolution ($resolution)"
    # Keeping TREE's selected side exactly must still produce a patch that
    # resolves the competing identity, even though its content diff is empty.
    case "$resolution" in
        retained) clean=$'from-target first line\nbody line\n' ;;
        other) clean=$'from-feature first line\nbody line\n' ;;
        edited) clean=$'new resolved first line\nmerged body\n' ;;
    esac
    create_file "f.txt" "$clean"
    assert_status_flag "status detects the unrecorded resolution" "M" "f.txt"
    if output=$(record_change "resolve name conflict" f.txt); then
        _pass "record accepts the resolution"
    else
        _fail "record accepts the resolution" "$output"
    fi
    assert_output_contains "resolution is recorded, not a clean no-op" "resolve name conflict" atomic change
    assert_output_contains "patch depends on the competing identity" "feature creates f.txt" atomic change --show-deps
    assert_output_contains "patch depends on the retained identity" "target creates f.txt" atomic change --show-deps
    assert_status_no_entry "status is clean after recording the resolution" "f.txt"
    resolution_hash="$(tip_hash target)"

    begin_section "Resolution survives content retrieval"
    create_file "f.txt" $'unrecorded replacement to force retrieval\n'
    assert_success "restore the recorded resolution" atomic restore --force
    assert_file_content "resolved file survives without conflict markers" "f.txt" "${clean%$'\n'}"
    assert_file_content "unrelated seed survives" "seed.txt" "seed"
    assert_status_no_entry "restored resolution is clean" "f.txt"
    assert_output_not_contains "resolution clears conflict reporting" "f.txt" atomic conflicts

    begin_section "Materialize the resolution on an inheriting view"
    new_view replay --from target
    switch_view replay
    assert_file_content "replayed graph has only the resolved content" "f.txt" "${clean%$'\n'}"
    assert_status_no_entry "replayed resolution is clean" "f.txt"
    assert_output_not_contains "replayed resolution has no conflict" "f.txt" atomic conflicts

    begin_section "Resolution respects the source view's change filter"
    switch_view feature
    assert_file_content "competing content survives on its source view" "f.txt" $'from-feature first line\nbody line'
    switch_view replay
    assert_file_content "resolved content survives the round trip" "f.txt" "${clean%$'\n'}"
    assert_status_no_entry "round-trip resolution is clean" "f.txt"

    begin_section "Insert the resolution patch with its dependency closure"
    new_view receiver --from dev
    switch_view receiver
    assert_success "insert just the resolution and its dependencies" atomic insert "$resolution_hash" --deps
    assert_file_content "inserted patch materializes the resolved content" "f.txt" "${clean%$'\n'}"
    assert_status_no_entry "inserted resolution is clean" "f.txt"
    assert_output_not_contains "inserted resolution has no conflict" "f.txt" atomic conflicts
done

begin_section "Descendant drafts edit the inherited identity"
make_temp_repo "inherited-file-identity"
init_repo
create_file seed.txt $'seed\n'
atomic add seed.txt
record_change "seed"
new_view parent --from dev
switch_view parent
create_file inherited.txt $'ancestor content\n'
atomic add inherited.txt
record_change "ancestor creates inherited.txt"
new_view child --draft --parent parent
switch_view child
assert_file_content "descendant sees inherited file" inherited.txt "ancestor content"
create_file inherited.txt $'descendant edit\n'
atomic add inherited.txt
assert_status_flag "inherited file is modified, not a new file" M inherited.txt
assert_success "record inherited edit" record_change "edit inherited identity" inherited.txt
assert_output_not_contains "inherited edit does not create another identity" '"hunk_type": "FileAdd"' atomic change -f json
assert_output_contains "inherited edit depends on original creation" "ancestor creates inherited.txt" atomic change --show-deps

begin_section "Namespace resolution must preserve a concurrently renamed identity"
make_temp_repo "resolve-name-versus-rename"
init_repo
create_file seed.txt $'seed\n'
atomic add seed.txt
record_change "seed"
new_view left --from dev
new_view right --from dev
switch_view left
create_file f.txt $'left identity content\n'
atomic add f.txt
record_change "left creates f.txt"
switch_view right
create_file f.txt $'right identity content\n'
atomic add f.txt
record_change "right creates f.txt"
insert_from_view left right
assert_output_contains "two visible identities really conflict" "(name conflict)" cat f.txt
create_file f.txt $'right identity content\n'
assert_success "record namespace selection" record_change "select right identity" f.txt
switch_view left
assert_file_content "left has its original content before rename" f.txt "left identity content"
atomic move f.txt saved.txt
record_change "rename left identity"
rename_hash="$(tip_hash left)"
switch_view right
assert_success "insert concurrent rename" atomic insert "$rename_hash" --deps
assert_file_content "selected identity still owns f.txt" f.txt "right identity content"
assert_file_content "renamed identity retains its content" saved.txt "left identity content"
new_view combined --from right
switch_view combined
assert_file_content "selected identity survives replay" f.txt "right identity content"
assert_file_content "renamed identity survives replay" saved.txt "left identity content"

print_summary
