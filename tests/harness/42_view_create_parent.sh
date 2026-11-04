#!/usr/bin/env bash
# 42_view_create_parent.sh
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
# Run against the harness binary: ATOMIC_BIN=path/to/atomic ./42_view_create_parent.sh
# Pre-fix builds fail at "seed.txt survives the switch into --parent child".

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ───────────────────────────────────────────────────────────────────────────
begin_section "Setup: base repo with a tracked file on dev"
# ───────────────────────────────────────────────────────────────────────────
make_temp_repo "view-create-parent"
init_repo

create_file "seed.txt" "seed content"
require_success "add seed.txt" atomic add seed.txt
require_success "record seed base" record_change "seed base"

# ───────────────────────────────────────────────────────────────────────────
begin_section "Bug: --parent child is empty, switch wipes the tree"
# ───────────────────────────────────────────────────────────────────────────
require_success "create --parent child view" atomic view create child --parent dev
assert_view_exists "--parent child view is created" child
require_capture "read child scope and parent" atomic view list --all --json
assert_success "child is a draft parented on dev" python3 -c \
    'import json,sys; rows=json.loads(sys.argv[1])["views"]; assert any(r["name"]=="child" and r["scope"]=="draft" and r["parent"]=="dev" for r in rows)' "$COMMAND_OUTPUT"

require_success "switch to child" switch_view "child"
assert_current_view "now on child" "child"

# THE REGRESSION: the child inherited dev's state, so switching must keep
# the tracked files on disk.
assert_file_content "seed.txt survives the switch into --parent child" "seed.txt" "seed content"

# ───────────────────────────────────────────────────────────────────────────
begin_section "Round-trip: dev still holds the file after switching back"
# ───────────────────────────────────────────────────────────────────────────
require_success "switch to dev" switch_view "dev"
assert_current_view "back on dev" "dev"
assert_file_content "seed.txt is intact back on dev" "seed.txt" "seed content"

# ───────────────────────────────────────────────────────────────────────────
print_summary
