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
# This simulates transfer by copying change objects; it does not exercise the
# API server, network transport, authentication, or remote push protocol.
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
SNAPSHOT_DIR="$(mktemp -d "${TMPDIR:-/tmp}/atomic-test-push-snapshots-XXXXXX")"
_HARNESS_TMPDIRS+=("$SNAPSHOT_DIR")

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

require_success "add client source" atomic add src/index.ts
CLIENT_V1="$SNAPSHOT_DIR/client-v1"
cp src/index.ts "$CLIENT_V1"

# Extract the exact hash of the change file created by the most recent
# record operation. Sorting by filename is unsafe because repo init writes
# bootstrap changes whose hashes may sort after later user changes.
capture_change_hashes() {
    find "$REPO_DIR/.atomic/changes" -name "*.change" -type f \
        | xargs -I{} basename {} .change | sort
}

find_new_change_hash() {
    local before_file="$1"
    local after_file="$2"

    local changes
    changes="$(comm -13 "$before_file" "$after_file")"
    if [[ -z "$changes" || "$changes" == *$'\n'* ]]; then
        echo "expected exactly one new change object, got: $changes" >&2
        return 1
    fi
    printf '%s\n' "$changes"
}

BEFORE_1="$SNAPSHOT_DIR/before-1"
AFTER_1="$SNAPSHOT_DIR/after-1"
capture_change_hashes > "$BEFORE_1"

require_capture "record initial TypeScript file" record_change "Initial TypeScript file"
REC1_OUT="$COMMAND_OUTPUT"

capture_change_hashes > "$AFTER_1"
require_capture "identify first change object" find_new_change_hash "$BEFORE_1" "$AFTER_1"
HASH1="$COMMAND_OUTPUT"

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
BEFORE_2="$SNAPSHOT_DIR/before-2"
AFTER_2="$SNAPSHOT_DIR/after-2"
capture_change_hashes > "$BEFORE_2"

require_capture "record ANSI color codes" record_change "Add ANSI color codes"
REC2_OUT="$COMMAND_OUTPUT"

capture_change_hashes > "$AFTER_2"
require_capture "identify second change object" find_new_change_hash "$BEFORE_2" "$AFTER_2"
HASH2="$COMMAND_OUTPUT"

if [[ -n "$HASH2" && "$HASH2" != "$HASH1" ]]; then
    _pass "Recorded change 2 on client: ${HASH2:0:12}"
else
    _fail "Record change 2" "Could not find second change file. Record output: $REC2_OUT"
    print_summary
    exit 1
fi

# Save the client's final file content for comparison
CLIENT_CONTENT="$(cat src/index.ts)"
CLIENT_LINES="$(wc -l < src/index.ts | tr -d ' ')"
CLIENT_V2="$SNAPSHOT_DIR/client-v2"
cp src/index.ts "$CLIENT_V2"

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
        cp "$src_file" "$dst_dir/" && cmp -s "$src_file" "$dst_dir/${hash}.change"
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

require_success "insert change 1 into dev" atomic insert "$HASH1" --view dev

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

assert_success "first server content matches client bytes" cmp -s "$CLIENT_V1" src/index.ts

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Server Push: Create draft view and insert change 2"
# ═══════════════════════════════════════════════════════════════════════════
#
# This simulates the API creating an agent session view on first push
# from that agent, then inserting the agent's change into it.

LOCAL_VIEW="agent-ses_test-harness"

# Create the draft view (same as what the API push handler does)
require_success "create draft view '$LOCAL_VIEW'" new_view "$LOCAL_VIEW"

assert_view_exists "Draft view exists" "$LOCAL_VIEW"

# Insert change 2 into the draft view.
# Change 2 depends on change 1, which is on dev (shared).
# The draft inherits change 1 and selects change 2 through its view filter.
require_success "insert change 2 into draft view" atomic insert "$HASH2" --view "$LOCAL_VIEW"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Server Push: Switch to draft view and verify output"
# ═══════════════════════════════════════════════════════════════════════════
#
# The critical test: switch to the draft view and verify the working copy.
# This is where the bug manifested — the file was duplicated or showed
# the wrong content.

require_success "switch to draft view" switch_view "$LOCAL_VIEW"
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
assert_success "draft content matches client byte-for-byte" cmp -s "$CLIENT_V2" src/index.ts

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Server Push: Dev perspective unaffected by draft view"
# ═══════════════════════════════════════════════════════════════════════════
#
# After switching back to dev, the file should show change 1's content
# (the original without color codes). Change 2 is only on the draft view.

require_success "switch to dev" switch_view "dev"
assert_current_view "Back on dev" "dev"
assert_success "dev content matches original client bytes" cmp -s "$CLIENT_V1" src/index.ts

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

require_success "switch back to draft" switch_view "$LOCAL_VIEW"
assert_current_view "Back on draft view" "$LOCAL_VIEW"

CONTENT_ROUNDTRIP="$(cat src/index.ts 2>/dev/null || echo "")"
GREET_COUNT_RT="$(echo "$CONTENT_ROUNDTRIP" | grep -c "function greet" || true)"

if [[ "$GREET_COUNT_RT" -eq 1 ]]; then
    _pass "Round-trip: 'function greet' appears exactly once"
else
    _fail "Round-trip: no duplication" \
        "'function greet' appears $GREET_COUNT_RT times after round-trip"
fi
assert_success "round-trip content matches client byte-for-byte" cmp -s "$CLIENT_V2" src/index.ts

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

BEFORE_3="$SNAPSHOT_DIR/before-3"
AFTER_3="$SNAPSHOT_DIR/after-3"
capture_change_hashes > "$BEFORE_3"

require_capture "record farewell function" record_change "Add farewell function"
REC3_OUT="$COMMAND_OUTPUT"

capture_change_hashes > "$AFTER_3"
require_capture "identify third change object" find_new_change_hash "$BEFORE_3" "$AFTER_3"
HASH3="$COMMAND_OUTPUT"

CLIENT_CONTENT_V3="$(cat src/index.ts)"
CLIENT_V3="$SNAPSHOT_DIR/client-v3"
cp src/index.ts "$CLIENT_V3"

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
require_success "select draft view" switch_view "$LOCAL_VIEW"

require_success "insert change 3 into draft view" atomic insert "$HASH3" --view "$LOCAL_VIEW"

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
assert_success "third change matches client byte-for-byte" cmp -s "$CLIENT_V3" src/index.ts

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

require_success "switch to dev after third change" switch_view "dev"
assert_current_view "on dev after third change" "dev"
assert_success "dev still matches original client bytes" cmp -s "$CLIENT_V1" src/index.ts

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
