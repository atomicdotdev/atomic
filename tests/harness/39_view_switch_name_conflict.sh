#!/usr/bin/env bash
# 39_view_switch_name_conflict.sh
#
# Regression: a view switch must NOT delete a file that has a materialization
# name-conflict when that file IS in the target view's inherited state.
#
# User-facing flow of the bug (ANGS-A20):
#   1. dev and feature independently create the same path with different
#      first lines (two inodes claim f.txt)
#   2. insert feature → dev ⇒ a name conflict is recorded for f.txt on dev
#   3. a child view is forked from dev (it inherits the conflicted lineage)
#   4. switching dev → child DELETES f.txt from the working tree (bug) —
#      the file must be kept, with its conflict markers
#
# NOTE: the child is created via `atomic view create <name>` (fork from the
# current view). The `--parent` flag creates a parented overlay whose visible
# set is EMPTY — switching into it deletes the entire working tree, which is
# a different (broader) defect not covered here.
#
# Run against the harness binary: ATOMIC_BIN=path/to/atomic ./39_view_switch_name_conflict.sh
# Pre-fix builds fail at "f.txt survives the switch".

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ───────────────────────────────────────────────────────────────────────────
begin_section "Setup: base repo with a tracked seed file"
# ───────────────────────────────────────────────────────────────────────────
make_temp_repo "view-switch-name-conflict"
init_repo

create_file "seed.txt" "seed content"
assert_success "add seed.txt" atomic add seed.txt
record_change "seed base" >/dev/null 2>&1 || true

# ───────────────────────────────────────────────────────────────────────────
begin_section "Repro: two views claim f.txt independently (name conflict)"
# ───────────────────────────────────────────────────────────────────────────
new_view "feature" >/dev/null 2>&1 || true
switch_view "feature" >/dev/null 2>&1 || true

create_file "f.txt" "from-feature first line\nbody line"
assert_success "add f.txt on feature" atomic add f.txt
record_change "feature creates f.txt" >/dev/null 2>&1 || true

switch_view "dev" >/dev/null 2>&1 || true
create_file "f.txt" "from-dev first line\nbody line"
assert_success "add f.txt on dev" atomic add f.txt
record_change "dev creates f.txt" >/dev/null 2>&1 || true

# Insert feature → dev: two inodes claim f.txt ⇒ name conflict.
insert_from_view "feature" "dev" >/dev/null 2>&1 || true

# The conflict must actually be materialized: markers on disk.
if grep -q ">>>>>>>" f.txt 2>/dev/null; then
    _pass "name-conflict markers are on disk after insert"
else
    _fail "name-conflict markers are on disk after insert" \
        "expected '>>>>>>>' in f.txt, got: $(cat f.txt 2>/dev/null)"
fi

# ───────────────────────────────────────────────────────────────────────────
begin_section "Bug: switching to an inheriting child deletes the file"
# ───────────────────────────────────────────────────────────────────────────
# Fork from the current view (dev): the child inherits dev's lineage.
new_view "child" >/dev/null 2>&1 || true
switch_view "child" >/dev/null 2>&1 || true
assert_current_view "now on child" "child"

# THE REGRESSION: the file is in the child's inherited state — it must NOT
# be deleted by the switch.
assert_file_exists "f.txt survives the switch (inherited state)" "f.txt"

# And it must still be the conflicted content, not a silent clean version.
if grep -q ">>>>>>>" f.txt 2>/dev/null; then
    _pass "conflict markers are intact after the switch"
else
    _fail "conflict markers are intact after the switch" \
        "expected '>>>>>>>' in f.txt after switch, got: $(cat f.txt 2>/dev/null)"
fi

# The inherited seed file must also survive (sanity: no collateral deletion).
assert_file_exists "seed.txt survives the switch (inherited state)" "seed.txt"

# ───────────────────────────────────────────────────────────────────────────
print_summary