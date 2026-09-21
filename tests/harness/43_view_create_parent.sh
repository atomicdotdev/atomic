#!/usr/bin/env bash
# 43_view_create_parent.sh
#
# Regression: `atomic view create <name> --parent <p>` must create a child
# view that INHERITS the parent's state — not an empty view. Switching into
# the child must therefore keep the parent's files on disk.
#
# Bug (pre-fix): `--parent` without `--draft` created a SHARED view whose
# visible change set is EMPTY (parent-chain visibility only exists for
# draft/overlay views). `view switch` into it then deleted EVERY tracked
# file from the working tree (`old_files - ∅`).
#
# Run against the harness binary: ATOMIC_BIN=path/to/atomic ./43_view_create_parent.sh
# Pre-fix builds fail at "seed.txt survives the switch into --parent child".

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ───────────────────────────────────────────────────────────────────────────
begin_section "Setup: base repo with a tracked file on dev"
# ───────────────────────────────────────────────────────────────────────────
make_temp_repo "view-create-parent"
init_repo

create_file "seed.txt" "seed content"
assert_success "add seed.txt" atomic add seed.txt
record_change "seed base" >/dev/null 2>&1 || true

# ───────────────────────────────────────────────────────────────────────────
begin_section "Bug: --parent child is empty, switch wipes the tree"
# ───────────────────────────────────────────────────────────────────────────
$ATOMIC_BIN view create child --parent dev >/dev/null 2>&1 || true

if $ATOMIC_BIN view list -a 2>/dev/null | grep -q "child"; then
    _pass "--parent child view is created"
else
    _fail "--parent child view is created" "view create failed"
fi

switch_view "child" >/dev/null 2>&1 || true
assert_current_view "now on child" "child"

# THE REGRESSION: the child inherited dev's state, so switching must keep
# the tracked files on disk.
assert_file_exists "seed.txt survives the switch into --parent child" "seed.txt"

# ───────────────────────────────────────────────────────────────────────────
begin_section "Round-trip: dev still holds the file after switching back"
# ───────────────────────────────────────────────────────────────────────────
switch_view "dev" >/dev/null 2>&1 || true
assert_current_view "back on dev" "dev"
assert_file_exists "seed.txt is intact back on dev" "seed.txt"

# ───────────────────────────────────────────────────────────────────────────
print_summary