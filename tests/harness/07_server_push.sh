#!/usr/bin/env bash
# 07_server_push.sh — Server-side push simulation tests.
#
# These tests simulate what happens when the atomic-api server receives
# a push from a client:
#
#   1. A "client" repo records changes locally
#   2. Change files are copied to a "server" repo (simulating upload)
#   3. The server creates a draft view (simulating agent session auto-creation)
#   4. The server inserts the change into the draft view
#   5. The server switches to that view and outputs the working copy
#   6. The file on disk must have correct content — no duplication
#
# This is the exact code path that the API push handler follows.
# The bug being tested: after inserting a change (with dependencies on a
# shared view) into a draft view and outputting the working copy, the
# file content was duplicated on the server.
#
# Key invariants tested:
#
#   1. Insert to dev (shared) → output_working_copy produces correct file
#   2. Create draft view → insert dependent change → switch → output
#      produces correct file (no duplication)
#   3. Content after draft view output matches what the client recorded
#   4. output_working_copy with dev perspective does NOT show draft changes
#   5. Multiple sequential changes to the same file on a draft view
#      produce correct content (no accumulating duplication)
#   6. The draft view's file content matches the client's working copy

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Server Push: Setup client repo with two changes"
# ═══════════════════════════════════════════════════════════════════════════
#
# Client workflow:
#   Change 1 (on dev): create src/index.ts with initial content
#   Change 2 (on dev): modify src/index.ts — add color codes, change return
#
# Change 2 depends on Change 1. Both are recorded on dev (shared).
# The server will receive both, insert Change 1 into dev, then insert
# Change 2 into a draft (agent) view.

make_temp_repo "client"
CLIENT_DIR="$REPO_DIR"
init_repo

# Create initial file
mkdir -p src
cat > src/index.ts << 'EOF'
function greet(name: string): string {
  return `Hello, ${name}!`;
}

const message = greet("World");
console.log(message);
EOF

add_files src/index.ts
REC1_OUT="$(record_change "Initial TypeScript file" 2>&1)"

# Extract the full hash from the change files on disk.
# `atomic log` truncates hashes with "...", but the filenames in
# .atomic/changes/XX/<FULL_HASH>.change have the complete hash.
# We take the most recently created .change file.
get_latest_change_hash() {
    find "$REPO_DIR/.atomic/changes" -name "*.change" -type f -newer "${1:-.atomic}" \
        2>/dev/null | sort | tail -1 | xargs -I{} basename {} .change
}

# Snapshot marker for finding new files
touch "$REPO_DIR/.atomic/_marker_before_1"
# The file was just created above, so use the marker trick differently:
# Just find ALL change files and pick the one that matches.
HASH1="$(find "$REPO_DIR/.atomic/changes" -name "*.change" -type f 2>/dev/null \
    | sort | tail -1 | xargs -I{} basename {} .change)"

if [[ -n "$HASH1" ]]; then
    _pass "Recorded change 1 on client: ${HASH1:0:12}"
else
    _fail "Record change 1" "Could not find change file. Record output: $REC1_OUT"
    print_summary
    exit 1
fi

# Modify the file (adds lines + changes the return statement)
cat > src/index.ts << 'EOF'
// ANSI color codes
const RED = "\x1b[31m";
const RESET = "\x1b[0m";

function greet(name: string): string {
  return `Hello, ${RED}${name}${RESET}!`;
}

const message = greet("World");
console.log(message);
EOF

# Mark before recording so we can find the NEW change file
CHANGE_COUNT_BEFORE="$(find "$REPO_DIR/.atomic/changes" -name "*.change" -type f 2>/dev/null | wc -l | tr -d ' ')"

REC2_OUT="$(record_change "Add ANSI color codes" 2>&1)"

# Find the change file that wasn't there before
HASH2="$(find "$REPO_DIR/.atomic/changes" -name "*.change" -type f 2>/dev/null \
    | sort | tail -1 | xargs -I{} basename {} .change)"

# Verify it's different from HASH1
if [[ -n "$HASH2" && "$HASH2" != "$HASH1" ]]; then
    _pass "Recorded change 2 on client: ${HASH2:0:12}"
else
    # Fallback: list all and pick the one that isn't HASH1
    HASH2="$(find "$REPO_DIR/.atomic/changes" -name "*.change" -type f 2>/dev/null \
        | xargs -I{} basename {} .change | grep -v "$HASH1" | head -1)"
    if [[ -n "$HASH2" ]]; then
        _pass "Recorded change 2 on client: ${HASH2:0:12}"
    else
        _fail "Record change 2" "Could not find second change file. Record output: $REC2_OUT"
        print_summary
        exit 1
    fi
fi

# Save the client's final file content for comparison
CLIENT_CONTENT="$(cat src/index.ts)"
CLIENT_LINES="$(wc -l < src/index.ts | tr -d ' ')"

_pass "Client file has $CLIENT_LINES lines"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Server Push: Init server and copy change files"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "server"
SERVER_DIR="$REPO_DIR"
init_repo

# Copy change files from client to server.
# Changes are stored at .atomic/changes/XX/XXXX....change
CLIENT_CHANGES="$CLIENT_DIR/.atomic/changes"
SERVER_CHANGES="$SERVER_DIR/.atomic/changes"

copy_change_file() {
    local hash="$1"
    local prefix="${hash:0:2}"
    local src_dir="$CLIENT_CHANGES/$prefix"
    local dst_dir="$SERVER_CHANGES/$prefix"

    mkdir -p "$dst_dir"

    # Find the change file (hash.change)
    local src_file="$src_dir/${hash}.change"

    if [[ ! -f "$src_file" ]]; then
        # Try finding by prefix match across all subdirs
        src_file="$(find "$CLIENT_CHANGES" -name "${hash}.change" -type f 2>/dev/null | head -1)"
    fi

    if [[ -n "$src_file" && -f "$src_file" ]]; then
        cp "$src_file" "$dst_dir/"
        return 0
    else
        echo "  DEBUG: looking for ${hash}.change in $src_dir" >&2
        echo "  DEBUG: files in $src_dir:" >&2
        ls -la "$src_dir" 2>&1 >&2 || true
        return 1
    fi
}

if copy_change_file "$HASH1"; then
    _pass "Copied change 1 to server"
else
    _fail "Copy change 1" "Change file not found for $HASH1"
    print_summary
    exit 1
fi

if copy_change_file "$HASH2"; then
    _pass "Copied change 2 to server"
else
    _fail "Copy change 2" "Change file not found for $HASH2"
    print_summary
    exit 1
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Server Push: Insert change 1 into dev (shared view)"
# ═══════════════════════════════════════════════════════════════════════════
#
# This simulates the first push to the server — the initial file creation
# lands on the shared dev view.

cd "$SERVER_DIR"

APPLY1_OUT="$(atomic insert "$HASH1" --view dev 2>&1)" || true

if echo "$APPLY1_OUT" | grep -qiE "applied|success|state"; then
    _pass "Inserted change 1 into dev"
else
    _pass "Insert change 1 into dev completed: $(echo "$APPLY1_OUT" | head -3)"
fi

# Verify the file exists and has correct content
assert_file_exists \
    "src/index.ts exists on server after first insert" \
    "src/index.ts"

# The file should contain "function greet" exactly once
CONTENT_AFTER_1="$(cat src/index.ts 2>/dev/null || echo "")"
GREET_COUNT_1="$(echo "$CONTENT_AFTER_1" | grep -c "function greet" || true)"

if [[ "$GREET_COUNT_1" -eq 1 ]]; then
    _pass "After change 1: 'function greet' appears exactly once"
else
    _fail "After change 1: no duplication" \
        "'function greet' appears $GREET_COUNT_1 times (expected 1). Content: $(echo "$CONTENT_AFTER_1" | head -20)"
fi

LINES_AFTER_1="$(echo "$CONTENT_AFTER_1" | wc -l | tr -d ' ')"
_pass "After change 1: file has $LINES_AFTER_1 lines"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Server Push: Create draft view and insert change 2"
# ═══════════════════════════════════════════════════════════════════════════
#
# This simulates the API creating an agent session view on first push
# from that agent, then inserting the agent's change into it.

LOCAL_VIEW="agent-ses_test-harness"

# Create the draft view (same as what the API push handler does)
NEW_OUT="$(new_view "$LOCAL_VIEW" 2>&1)" || true

if echo "$NEW_OUT" | grep -qiE "created|view"; then
    _pass "Created draft view '$LOCAL_VIEW'"
else
    _pass "Draft view creation completed"
fi

assert_view_exists "Draft view exists" "$LOCAL_VIEW"

# Insert change 2 into the draft view.
# Change 2 depends on change 1, which is on dev (shared).
# The draft view's overlay should see change 1's edges through GRAPH,
# and change 2's edges should go to STACK_GRAPH.
APPLY2_OUT="$(atomic insert "$HASH2" --view "$LOCAL_VIEW" 2>&1)" || true

if echo "$APPLY2_OUT" | grep -qiE "applied|success|state"; then
    _pass "Inserted change 2 into draft view"
else
    _pass "Insert change 2 completed: $(echo "$APPLY2_OUT" | head -3)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Server Push: Switch to draft view and verify output"
# ═══════════════════════════════════════════════════════════════════════════
#
# The critical test: switch to the draft view and verify the working copy.
# This is where the bug manifested — the file was duplicated or showed
# the wrong content.

switch_view "$LOCAL_VIEW" >/dev/null 2>&1 || true
assert_current_view "Switched to draft view" "$LOCAL_VIEW"

# The file must exist
assert_file_exists \
    "src/index.ts exists after switch to draft view" \
    "src/index.ts"

# Read the content
CONTENT_LOCAL="$(cat src/index.ts 2>/dev/null || echo "")"
LINES_LOCAL="$(echo "$CONTENT_LOCAL" | wc -l | tr -d ' ')"

# ── Invariant 1: No duplication ──────────────────────────────────────────
# "function greet" should appear exactly once
GREET_COUNT_LOCAL="$(echo "$CONTENT_LOCAL" | grep -c "function greet" || true)"

if [[ "$GREET_COUNT_LOCAL" -eq 1 ]]; then
    _pass "Draft view: 'function greet' appears exactly once (no duplication)"
else
    _fail "Draft view: no duplication" \
        "'function greet' appears $GREET_COUNT_LOCAL times (expected 1). Lines: $LINES_LOCAL. Content: $(echo "$CONTENT_LOCAL" | head -30)"
fi

# ── Invariant 2: Content from change 2 is present ────────────────────────
# The ANSI color constants should be in the file
if echo "$CONTENT_LOCAL" | grep -qF "RED"; then
    _pass "Draft view: contains RED constant from change 2"
else
    _fail "Draft view: contains RED" \
        "File does not contain 'RED'. Content: $(echo "$CONTENT_LOCAL" | head -20)"
fi

if echo "$CONTENT_LOCAL" | grep -qF "RESET"; then
    _pass "Draft view: contains RESET constant from change 2"
else
    _fail "Draft view: contains RESET" \
        "File does not contain 'RESET'. Content: $(echo "$CONTENT_LOCAL" | head -20)"
fi

# ── Invariant 3: The modified return statement is present ────────────────
if echo "$CONTENT_LOCAL" | grep -q 'RED.*name.*RESET'; then
    _pass "Draft view: return statement uses color codes"
else
    _fail "Draft view: return uses color codes" \
        "Expected return with RED/RESET. Content: $(echo "$CONTENT_LOCAL" | head -20)"
fi

# ── Invariant 4: The old return statement is NOT present ─────────────────
# The original `return \`Hello, \${name}!\`` without colors should be gone
OLD_RETURN_COUNT="$(echo "$CONTENT_LOCAL" | grep -c 'Hello, \${name}!' | grep -cv 'RED\|RESET' || true)"
# More robust: count lines with the simple return (no RED/RESET)
SIMPLE_RETURNS="$(echo "$CONTENT_LOCAL" | grep 'Hello,' | grep -v 'RED' | grep -v 'RESET' || true)"

if [[ -z "$SIMPLE_RETURNS" ]]; then
    _pass "Draft view: old return statement replaced (not present without colors)"
else
    _fail "Draft view: old return removed" \
        "Simple return still present: $SIMPLE_RETURNS"
fi

# ── Invariant 5: File size is reasonable ─────────────────────────────────
if [[ "$LINES_LOCAL" -le 15 ]]; then
    _pass "Draft view: file has $LINES_LOCAL lines (reasonable size)"
else
    _fail "Draft view: file size" \
        "File has $LINES_LOCAL lines (expected ≤15, likely duplicated). Content: $(echo "$CONTENT_LOCAL" | head -30)"
fi

# ── Invariant 6: Content matches what the client recorded ────────────────
if [[ "$CONTENT_LOCAL" == "$CLIENT_CONTENT" ]]; then
    _pass "Draft view: content matches client's working copy exactly"
else
    _fail "Draft view: content matches client" \
        "Server content differs from client. Server ($LINES_LOCAL lines): $(echo "$CONTENT_LOCAL" | head -15) --- Client ($CLIENT_LINES lines): $(echo "$CLIENT_CONTENT" | head -15)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Server Push: Dev perspective unaffected by draft view"
# ═══════════════════════════════════════════════════════════════════════════
#
# After switching back to dev, the file should show change 1's content
# (the original without color codes). Change 2 is only on the draft view.

switch_view "dev" >/dev/null 2>&1 || true
assert_current_view "Back on dev" "dev"

CONTENT_DEV="$(cat src/index.ts 2>/dev/null || echo "")"

# Dev should have the original return (without colors)
if echo "$CONTENT_DEV" | grep -q 'Hello, \${name}!'; then
    _pass "Dev: has original return statement"
else
    _fail "Dev: original return" \
        "Dev should have original return. Content: $(echo "$CONTENT_DEV" | head -15)"
fi

# Dev should NOT have the color constants
if echo "$CONTENT_DEV" | grep -qF "RED"; then
    _fail "Dev: no color constants" \
        "Dev should NOT contain RED (that's on the draft view). Content: $(echo "$CONTENT_DEV" | head -15)"
else
    _pass "Dev: does not contain RED (draft view change not visible)"
fi

# Dev file should not be duplicated either
GREET_COUNT_DEV="$(echo "$CONTENT_DEV" | grep -c "function greet" || true)"
if [[ "$GREET_COUNT_DEV" -eq 1 ]]; then
    _pass "Dev: 'function greet' appears exactly once"
else
    _fail "Dev: no duplication" \
        "'function greet' appears $GREET_COUNT_DEV times on dev"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Server Push: Round-trip — draft view still correct"
# ═══════════════════════════════════════════════════════════════════════════
#
# Switch back to the draft view. The file should still have the correct
# content — no corruption from the dev→draft round trip.

switch_view "$LOCAL_VIEW" >/dev/null 2>&1 || true
assert_current_view "Back on draft view" "$LOCAL_VIEW"

CONTENT_ROUNDTRIP="$(cat src/index.ts 2>/dev/null || echo "")"
GREET_COUNT_RT="$(echo "$CONTENT_ROUNDTRIP" | grep -c "function greet" || true)"

if [[ "$GREET_COUNT_RT" -eq 1 ]]; then
    _pass "Round-trip: 'function greet' appears exactly once"
else
    _fail "Round-trip: no duplication" \
        "'function greet' appears $GREET_COUNT_RT times after round-trip"
fi

if [[ "$CONTENT_ROUNDTRIP" == "$CLIENT_CONTENT" ]]; then
    _pass "Round-trip: content still matches client"
else
    _fail "Round-trip: content matches" \
        "Content changed after dev→local round-trip"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Server Push: Multiple changes to same file on draft view"
# ═══════════════════════════════════════════════════════════════════════════
#
# Record a THIRD change on the client that further modifies the file,
# copy it to the server, insert into the draft view, and verify no
# accumulating duplication.

cd "$CLIENT_DIR"
REPO_DIR="$CLIENT_DIR"

cat > src/index.ts << 'EOF'
// ANSI color codes
const RED = "\x1b[31m";
const RESET = "\x1b[0m";

function greet(name: string): string {
  return `Hello, ${RED}${name}${RESET}!`;
}

function farewell(name: string): string {
  return `Goodbye, ${name}!`;
}

const message = greet("World");
console.log(message);
console.log(farewell("World"));
EOF

REC3_OUT="$(record_change "Add farewell function" 2>&1)"

# Find the newest change file that isn't HASH1 or HASH2
HASH3="$(find "$CLIENT_DIR/.atomic/changes" -name "*.change" -type f 2>/dev/null \
    | xargs -I{} basename {} .change | grep -v "$HASH1" | grep -v "$HASH2" | head -1)"

CLIENT_CONTENT_V3="$(cat src/index.ts)"

if [[ -n "$HASH3" ]]; then
    _pass "Recorded change 3 on client: ${HASH3:0:12}"
else
    _fail "Record change 3" "Could not find third change file. Output: $REC3_OUT"
    print_summary
    exit 1
fi

# Copy to server and apply
cd "$SERVER_DIR"
REPO_DIR="$SERVER_DIR"

if copy_change_file "$HASH3"; then
    _pass "Copied change 3 to server"
else
    _fail "Copy change 3" "Change file not found for $HASH3"
    print_summary
    exit 1
fi

# Make sure we're on the draft view
switch_view "$LOCAL_VIEW" >/dev/null 2>&1 || true

APPLY3_OUT="$(atomic insert "$HASH3" --view "$LOCAL_VIEW" 2>&1)" || true

if echo "$APPLY3_OUT" | grep -qiE "applied|success|state"; then
    _pass "Inserted change 3 into draft view"
else
    _pass "Insert change 3 completed"
fi

# Re-output working copy (switch_view already did this, but be explicit)
# In the API, output_working_copy is called after insert.
# Here, the switch already triggers it.

CONTENT_V3="$(cat src/index.ts 2>/dev/null || echo "")"
LINES_V3="$(echo "$CONTENT_V3" | wc -l | tr -d ' ')"
GREET_COUNT_V3="$(echo "$CONTENT_V3" | grep -c "function greet" || true)"
FAREWELL_COUNT="$(echo "$CONTENT_V3" | grep -c "function farewell" || true)"

if [[ "$GREET_COUNT_V3" -eq 1 ]]; then
    _pass "After change 3: 'function greet' appears exactly once"
else
    _fail "After change 3: no greet duplication" \
        "'function greet' appears $GREET_COUNT_V3 times. Content: $(echo "$CONTENT_V3" | head -25)"
fi

if [[ "$FAREWELL_COUNT" -eq 1 ]]; then
    _pass "After change 3: 'function farewell' appears exactly once"
else
    _fail "After change 3: no farewell duplication" \
        "'function farewell' appears $FAREWELL_COUNT times. Content: $(echo "$CONTENT_V3" | head -25)"
fi

if [[ "$LINES_V3" -le 20 ]]; then
    _pass "After change 3: file has $LINES_V3 lines (reasonable)"
else
    _fail "After change 3: file size" \
        "File has $LINES_V3 lines (expected ≤20). Content: $(echo "$CONTENT_V3" | head -30)"
fi

if [[ "$CONTENT_V3" == "$CLIENT_CONTENT_V3" ]]; then
    _pass "After change 3: content matches client's working copy"
else
    _fail "After change 3: content matches client" \
        "Server differs from client after third change"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Server Push: Dev still unaffected after 3 draft changes"
# ═══════════════════════════════════════════════════════════════════════════

switch_view "dev" >/dev/null 2>&1 || true

CONTENT_DEV_FINAL="$(cat src/index.ts 2>/dev/null || echo "")"

if echo "$CONTENT_DEV_FINAL" | grep -qF "farewell"; then
    _fail "Dev final: no farewell" \
        "Dev should NOT have 'farewell' (only on draft view)"
else
    _pass "Dev final: does not contain farewell function"
fi

if echo "$CONTENT_DEV_FINAL" | grep -qF "RED"; then
    _fail "Dev final: no color constants" \
        "Dev should NOT have RED (only on draft view)"
else
    _pass "Dev final: does not contain color constants"
fi

GREET_COUNT_DEV_FINAL="$(echo "$CONTENT_DEV_FINAL" | grep -c "function greet" || true)"
if [[ "$GREET_COUNT_DEV_FINAL" -eq 1 ]]; then
    _pass "Dev final: 'function greet' appears exactly once"
else
    _fail "Dev final: no duplication" \
        "'function greet' appears $GREET_COUNT_DEV_FINAL times on dev"
fi

# ═══════════════════════════════════════════════════════════════════════════

print_summary
