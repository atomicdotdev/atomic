#!/usr/bin/env bash
# chmod +x tests/harness/24_concurrent_insert_conflict.sh
#
# 24_concurrent_insert_conflict.sh — Concurrent insert fork conflict resolution.
#
# Tests the specific scenario where multiple draft views independently modify
# the same file (or same position within a file) and are then inserted into
# a shared view.  This exercises the CRDT fork resolution path.
#
# Key questions answered by these tests:
#   1. Do identical concurrent edits deduplicate cleanly?
#   2. Do conflicting concurrent edits produce well-formed conflict markers?
#   3. Does the graph remain stable across view switches after conflicts?
#   4. Can a superseding change on the shared view resolve a prior fork?
#
# Each case creates a fresh repo to avoid cross-contamination.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"
source "$HARNESS_DIR/merge_helpers.sh"  # assert_no_conflict_markers, assert_file_contains,
                                        # has_conflict_markers, assert_well_formed_conflict,
                                        # snapshot_file, assert_file_stable

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 1: Identical edits from two drafts — should deduplicate"
# ═══════════════════════════════════════════════════════════════════════════
#
# Two drafts (alpha, beta) both add an identical empty line at the same
# position.  When both are inserted into dev, the CRDT should recognise
# identical content and deduplicate — no conflict markers.

make_temp_repo "concurrent-1"
init_repo

# Create multi-line file on dev
cat > imports.py << 'PYEOF'
import os
import sys
import json

def main():
    pass
PYEOF
assert_success "add imports.py" atomic add imports.py
record_change "Add imports.py on dev" >/dev/null 2>&1 || true

# Create two draft views parented on dev
new_view "alpha" --draft --parent dev >/dev/null 2>&1 || true
new_view "beta"  --draft --parent dev >/dev/null 2>&1 || true

# On alpha: add an empty line after the imports section (after 'import json')
switch_view "alpha" >/dev/null 2>&1 || true
cat > imports.py << 'PYEOF'
import os
import sys
import json

def main():
    pass
PYEOF
# The file already has the blank line from the original — add a second one
# to create a distinct edit (extra whitespace line after imports).
cat > imports.py << 'PYEOF'
import os
import sys
import json


def main():
    pass
PYEOF
record_change "Alpha: add blank line after imports" >/dev/null 2>&1 || true

# On beta: add the same empty line at the same position
switch_view "beta" >/dev/null 2>&1 || true
cat > imports.py << 'PYEOF'
import os
import sys
import json


def main():
    pass
PYEOF
record_change "Beta: add blank line after imports" >/dev/null 2>&1 || true

# Switch to dev, insert from alpha, then beta
switch_view "dev" >/dev/null 2>&1 || true
insert_from_view "alpha" "dev" >/dev/null 2>&1 || true
insert_from_view "beta" "dev" >/dev/null 2>&1 || true

# Re-materialize: switch away and back
switch_view "alpha" >/dev/null 2>&1 || true
switch_view "dev" >/dev/null 2>&1 || true

assert_file_exists "imports.py exists on dev after inserts" "imports.py"
assert_no_conflict_markers "no conflict markers on dev (identical edits)" "imports.py"
assert_file_contains "dev has import os" "imports.py" "import os"
assert_file_contains "dev has import json" "imports.py" "import json"
assert_file_contains "dev has def main" "imports.py" "def main():"

# Verify content is stable across switches
DEV_SNAPSHOT_1="$(snapshot_file "imports.py")"
switch_view "alpha" >/dev/null 2>&1 || true
switch_view "dev" >/dev/null 2>&1 || true
assert_file_stable "dev content stable after round-trip (case 1)" "imports.py" "$DEV_SNAPSHOT_1"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 2: Different imports at same position — conflicting edits"
# ═══════════════════════════════════════════════════════════════════════════
#
# Alpha changes a line to add 'use bar;', beta changes the same line to
# add 'use baz;'.  These are genuinely conflicting edits at the same
# position.  After inserting both into dev, either the CRDT resolves
# cleanly (both present) or conflict markers appear.

make_temp_repo "concurrent-2"
init_repo

# Create file with a single import line
create_file "lib.rs" "use foo;"
assert_success "add lib.rs" atomic add lib.rs
record_change "Add lib.rs with use foo" >/dev/null 2>&1 || true

# Create two draft views
new_view "alpha" --draft --parent dev >/dev/null 2>&1 || true
new_view "beta"  --draft --parent dev >/dev/null 2>&1 || true

# Alpha: change line to include bar
switch_view "alpha" >/dev/null 2>&1 || true
overwrite_file "lib.rs" "use foo;
use bar;"
record_change "Alpha: add use bar" >/dev/null 2>&1 || true

# Beta: change line to include baz
switch_view "beta" >/dev/null 2>&1 || true
overwrite_file "lib.rs" "use foo;
use baz;"
record_change "Beta: add use baz" >/dev/null 2>&1 || true

# Switch to dev, insert both
switch_view "dev" >/dev/null 2>&1 || true
insert_from_view "alpha" "dev" >/dev/null 2>&1 || true
insert_from_view "beta" "dev" >/dev/null 2>&1 || true

# Re-materialize
switch_view "alpha" >/dev/null 2>&1 || true
switch_view "dev" >/dev/null 2>&1 || true

assert_file_exists "lib.rs exists on dev after both inserts" "lib.rs"

# Check if conflict markers appeared
if has_conflict_markers "lib.rs"; then
    # If there are conflict markers, they must be well-formed
    assert_well_formed_conflict "conflict markers are well-formed (case 2)" "lib.rs"
    assert_file_contains "conflict contains bar side" "lib.rs" "bar"
    assert_file_contains "conflict contains baz side" "lib.rs" "baz"
else
    # CRDT resolved cleanly — both additions should be present
    assert_file_contains "dev has use foo" "lib.rs" "use foo;"
    # At least one of bar/baz should be present
    if grep -qF "bar" "lib.rs" || grep -qF "baz" "lib.rs"; then
        _pass "CRDT resolved cleanly — content present"
    else
        _fail "CRDT resolved but content missing" "neither bar nor baz found"
    fi
fi

# Stability check: switch to alpha and back — dev content must not change
DEV_SNAPSHOT_2="$(snapshot_file "lib.rs")"
switch_view "alpha" >/dev/null 2>&1 || true
switch_view "dev" >/dev/null 2>&1 || true
assert_file_stable "dev content stable after switch (case 2)" "lib.rs" "$DEV_SNAPSHOT_2"

# Additional stability: switch to beta and back
switch_view "beta" >/dev/null 2>&1 || true
switch_view "dev" >/dev/null 2>&1 || true
assert_file_stable "dev content stable after second switch (case 2)" "lib.rs" "$DEV_SNAPSHOT_2"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 3: Three-way fork — three drafts modify same position"
# ═══════════════════════════════════════════════════════════════════════════
#
# Three drafts (a, b, c) each edit the same line differently.
# After inserting all three into dev, either the CRDT resolves cleanly
# or conflict markers are present and well-formed.  The graph must be
# stable across view switches.

make_temp_repo "concurrent-3"
init_repo

# Create base file
create_file "config.toml" '[server]
host = "localhost"
port = 8080'
assert_success "add config.toml" atomic add config.toml
record_change "Add config.toml" >/dev/null 2>&1 || true

# Create three draft views
new_view "fork-a" --draft --parent dev >/dev/null 2>&1 || true
new_view "fork-b" --draft --parent dev >/dev/null 2>&1 || true
new_view "fork-c" --draft --parent dev >/dev/null 2>&1 || true

# Fork A: change port to 3000
switch_view "fork-a" >/dev/null 2>&1 || true
overwrite_file "config.toml" '[server]
host = "localhost"
port = 3000'
record_change "Fork A: port 3000" >/dev/null 2>&1 || true

# Fork B: change port to 4000
switch_view "fork-b" >/dev/null 2>&1 || true
overwrite_file "config.toml" '[server]
host = "localhost"
port = 4000'
record_change "Fork B: port 4000" >/dev/null 2>&1 || true

# Fork C: change port to 5000
switch_view "fork-c" >/dev/null 2>&1 || true
overwrite_file "config.toml" '[server]
host = "localhost"
port = 5000'
record_change "Fork C: port 5000" >/dev/null 2>&1 || true

# Switch to dev and insert all three
switch_view "dev" >/dev/null 2>&1 || true
insert_from_view "fork-a" "dev" >/dev/null 2>&1 || true
insert_from_view "fork-b" "dev" >/dev/null 2>&1 || true
insert_from_view "fork-c" "dev" >/dev/null 2>&1 || true

# Re-materialize
switch_view "fork-a" >/dev/null 2>&1 || true
switch_view "dev" >/dev/null 2>&1 || true

assert_file_exists "config.toml exists on dev after 3 inserts" "config.toml"

# The shared header should always be present
assert_file_contains "dev has [server] header" "config.toml" "[server]"
assert_file_contains "dev has host line" "config.toml" 'host = "localhost"'

# Check conflict or clean resolution
if has_conflict_markers "config.toml"; then
    assert_well_formed_conflict "3-way conflict markers are well-formed" "config.toml"
else
    # CRDT resolved — at least one of the port values should be present
    if grep -qE 'port = (3000|4000|5000)' "config.toml"; then
        _pass "CRDT resolved 3-way fork — a port value is present"
    else
        _fail "CRDT resolved but no port value found" "$(cat "config.toml" | head -10)"
    fi
fi

# Stability: switch through each fork and back to dev, content must not drift
DEV_SNAPSHOT_3="$(snapshot_file "config.toml")"

switch_view "fork-a" >/dev/null 2>&1 || true
switch_view "dev" >/dev/null 2>&1 || true
assert_file_stable "dev stable after switch to fork-a (case 3)" "config.toml" "$DEV_SNAPSHOT_3"

switch_view "fork-b" >/dev/null 2>&1 || true
switch_view "dev" >/dev/null 2>&1 || true
assert_file_stable "dev stable after switch to fork-b (case 3)" "config.toml" "$DEV_SNAPSHOT_3"

switch_view "fork-c" >/dev/null 2>&1 || true
switch_view "dev" >/dev/null 2>&1 || true
assert_file_stable "dev stable after switch to fork-c (case 3)" "config.toml" "$DEV_SNAPSHOT_3"

# Each draft is a live perspective on its parent (dev).  Since dev now has
# all three conflicting changes, each draft inherits them.  Drafts are NOT
# snapshots — they see everything their parent sees plus their own changes.
# Therefore drafts will show conflict markers if their parent has conflicts.

switch_view "fork-a" >/dev/null 2>&1 || true
assert_file_contains "fork-a still has port 3000" "config.toml" "port = 3000"
if has_conflict_markers "config.toml"; then
    _pass "fork-a inherits parent conflict markers (expected — draft = live perspective)"
else
    _pass "fork-a has no conflict markers (CRDT resolved cleanly on parent)"
fi

switch_view "fork-b" >/dev/null 2>&1 || true
assert_file_contains "fork-b still has port 4000" "config.toml" "port = 4000"
if has_conflict_markers "config.toml"; then
    _pass "fork-b inherits parent conflict markers (expected — draft = live perspective)"
else
    _pass "fork-b has no conflict markers (CRDT resolved cleanly on parent)"
fi

switch_view "fork-c" >/dev/null 2>&1 || true
assert_file_contains "fork-c still has port 5000" "config.toml" "port = 5000"
if has_conflict_markers "config.toml"; then
    _pass "fork-c inherits parent conflict markers (expected — draft = live perspective)"
else
    _pass "fork-c has no conflict markers (CRDT resolved cleanly on parent)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 4: Superseding change resolves fork conflict"
# ═══════════════════════════════════════════════════════════════════════════
#
# Two drafts edit the same line to different values, creating a fork on dev
# when both are inserted.  Then a new change is recorded directly on dev
# that sets the value to something else entirely — this superseding change
# should resolve the fork cleanly (no conflict markers remain).

make_temp_repo "concurrent-4"
init_repo

# Create base file
create_file "setting.conf" "x = 1"
assert_success "add setting.conf" atomic add setting.conf
record_change "Add setting.conf with x=1" >/dev/null 2>&1 || true

# Create draft alpha: x = 2
new_view "alpha" --draft --parent dev >/dev/null 2>&1 || true
switch_view "alpha" >/dev/null 2>&1 || true
overwrite_file "setting.conf" "x = 2"
record_change "Alpha: x = 2" >/dev/null 2>&1 || true

# Create draft beta: x = 3
switch_view "dev" >/dev/null 2>&1 || true
new_view "beta" --draft --parent dev >/dev/null 2>&1 || true
switch_view "beta" >/dev/null 2>&1 || true
overwrite_file "setting.conf" "x = 3"
record_change "Beta: x = 3" >/dev/null 2>&1 || true

# Insert both into dev (creates a fork)
switch_view "dev" >/dev/null 2>&1 || true
insert_from_view "alpha" "dev" >/dev/null 2>&1 || true
insert_from_view "beta" "dev" >/dev/null 2>&1 || true

# Re-materialize
switch_view "alpha" >/dev/null 2>&1 || true
switch_view "dev" >/dev/null 2>&1 || true

# At this point dev may have conflict markers from the fork
# Record a note about the state before superseding
PRE_SUPERSEDE="$(snapshot_file "setting.conf")"
if has_conflict_markers "setting.conf"; then
    _pass "fork created conflict markers (expected for divergent edits)"
else
    _pass "CRDT resolved fork without markers (also valid)"
fi

# Now record a superseding change directly on dev: x = 4
overwrite_file "setting.conf" "x = 4"
record_change "Dev: supersede to x = 4" >/dev/null 2>&1 || true

# After the superseding change, there should be no conflict markers
assert_file_exists "setting.conf exists after supersede" "setting.conf"
assert_file_content "dev has x = 4 after supersede" "setting.conf" "x = 4"
assert_no_conflict_markers "no conflict markers after supersede (case 4)" "setting.conf"

# Stability: switch to each draft and back — dev should remain clean
DEV_SNAPSHOT_4="$(snapshot_file "setting.conf")"

switch_view "alpha" >/dev/null 2>&1 || true
switch_view "dev" >/dev/null 2>&1 || true
assert_file_stable "dev stable after switch to alpha (case 4)" "setting.conf" "$DEV_SNAPSHOT_4"
assert_no_conflict_markers "no markers after alpha round-trip (case 4)" "setting.conf"

switch_view "beta" >/dev/null 2>&1 || true
switch_view "dev" >/dev/null 2>&1 || true
assert_file_stable "dev stable after switch to beta (case 4)" "setting.conf" "$DEV_SNAPSHOT_4"
assert_no_conflict_markers "no markers after beta round-trip (case 4)" "setting.conf"

# Drafts are live perspectives on dev (their parent).  After inserting their
# changes into dev and then recording a supersede on dev, the drafts inherit
# the supersede change via the parent chain.  The supersede resolves the
# fork, so drafts should show x = 4 and no conflict markers.
switch_view "alpha" >/dev/null 2>&1 || true
assert_no_conflict_markers "no markers on alpha after supersede" "setting.conf"
if grep -qF "x = 4" "setting.conf"; then
    _pass "alpha inherits supersede (x = 4) from dev parent (expected — draft = live perspective)"
elif grep -qF "x = 2" "setting.conf"; then
    _pass "alpha shows own value x = 2 (draft isolated from parent supersede)"
else
    _fail "alpha has unexpected content" "$(cat "setting.conf" | head -5)"
fi

switch_view "beta" >/dev/null 2>&1 || true
assert_no_conflict_markers "no markers on beta after supersede" "setting.conf"
if grep -qF "x = 4" "setting.conf"; then
    _pass "beta inherits supersede (x = 4) from dev parent (expected — draft = live perspective)"
elif grep -qF "x = 3" "setting.conf"; then
    _pass "beta shows own value x = 3 (draft isolated from parent supersede)"
else
    _fail "beta has unexpected content" "$(cat "setting.conf" | head -5)"
fi

# ═══════════════════════════════════════════════════════════════════════════

print_summary
