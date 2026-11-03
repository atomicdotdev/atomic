#!/usr/bin/env bash
# 08_draft_view_insert.sh — Draft view insert correctness at scale.
#
# Simulates object transfer without exercising network push or the API server:
# changes are recorded on a client draft view, copied to a server repository,
# and inserted into the same draft view on the server.
#
# The critical invariant: after inserting N sequential changes that modify
# the same file, the server's working copy must contain ONLY the final
# state — not a concatenation of all intermediate states.
#
# Each incoming native span edit must replace the intended prior content
# rather than accumulating intermediate versions.
#
# Invariants tested at every step:
#
#   1. No function/identifier duplication in the output file
#   2. File line count stays within expected bounds (not growing unbounded)
#   3. Content matches what the client had at that revision
#   4. Each change's NEW content is present
#   5. Each change's OLD content (that was replaced) is absent
#   6. After switching to dev, the draft view's files are NOT visible
#   7. After switching back to the draft view, content is still correct
#
# Scale targets:
#   - 10 sequential changes to the same file
#   - Each change modifies existing lines AND adds new lines
#   - Verifies O(1) file size growth per change (not O(N) duplication)

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"
SNAPSHOT_DIR="$(mktemp -d "${TMPDIR:-/tmp}/atomic-test-insert-snapshots-XXXXXX")"
_HARNESS_TMPDIRS+=("$SNAPSHOT_DIR")

# ═══════════════════════════════════════════════════════════════════════════
# Helper: copy a single change file from client to server
# ═══════════════════════════════════════════════════════════════════════════

copy_change_to_server() {
    local hash="$1"
    local client_changes="$2"
    local server_changes="$3"
    local prefix="${hash:0:2}"
    local src_dir="$client_changes/$prefix"
    local dst_dir="$server_changes/$prefix"

    mkdir -p "$dst_dir"

    local src_file="$src_dir/${hash}.change"
    if [[ -f "$src_file" ]]; then
        cp "$src_file" "$dst_dir/" && cmp -s "$src_file" "$dst_dir/${hash}.change"
    else
        echo "missing client change object: $src_file" >&2
        return 1
    fi
}

# ═══════════════════════════════════════════════════════════════════════════
# Helper: get the full hash of the most recently recorded change
# ═══════════════════════════════════════════════════════════════════════════

capture_change_hashes() {
    find "$CLIENT_DIR/.atomic/changes" -name '*.change' -type f \
        | xargs -I{} basename {} .change | sort
}

find_new_change_hash() {
    local changes
    changes="$(comm -13 "$1" "$2")"
    if [[ -z "$changes" || "$changes" == *$'\n'* ]]; then
        echo "expected exactly one new change object, got: $changes" >&2
        return 1
    fi
    printf '%s\n' "$changes"
}

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Setup: Client repo with agent view"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "client-scale"
CLIENT_DIR="$REPO_DIR"
init_repo

AGENT_VIEW="agent-ses_scale-test"

require_success "create client agent view" new_view "$AGENT_VIEW"
require_success "switch to client agent view" switch_view "$AGENT_VIEW"
assert_current_view "Client on agent view" "$AGENT_VIEW"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Record 10 sequential changes on agent view"
# ═══════════════════════════════════════════════════════════════════════════
#
# Each change modifies src/app.ts:
#   - Change 1: create the file with version=1
#   - Change 2-10: bump version, add a new feature function
#
# After change N, the file should have:
#   - version = "N"
#   - N-1 feature_X() functions (added by changes 2..N)
#   - Exactly ONE main() function
#   - Exactly ONE console.log call

mkdir -p src

# Arrays to store hashes and expected content per change
declare -a CHANGE_HASHES
declare -a EXPECTED_VERSIONS
declare -a EXPECTED_FEATURES
declare -a EXPECTED_LINE_COUNTS
declare -a SNAPSHOTS  # full file content at each step

NUM_CHANGES=10

# Each change exercises a MIX of operations:
#
#   Change 1:  CREATE file — initial content (add only)
#   Change 2:  MODIFY greeting string, ADD a helper function
#   Change 3:  DELETE the helper, MODIFY main to use inline logic
#   Change 4:  ADD a config block, MODIFY version
#   Change 5:  MODIFY config values, DELETE a comment line
#   Change 6:  ADD error handling, MODIFY main signature
#   Change 7:  DELETE error handling, ADD logging module, MODIFY version
#   Change 8:  MODIFY logging format, DELETE an import, ADD metrics
#   Change 9:  DELETE metrics, MODIFY main back to simple, ADD footer comment
#   Change 10: MODIFY everything — final clean version
#
# The revisions exercise successive additions, replacements, and deletions.

generate_version() {
    local v="$1"
    case "$v" in
        1)
            cat << 'V1'
// App v1 — initial
const VERSION = "1";

function greet(name: string): string {
  return `Hello, ${name}!`;
}

function main(): void {
  console.log(greet("World"));
}

main();
V1
            ;;
        2)
            # MODIFY: greet now uses color. ADD: helper function formatName
            cat << 'V2'
// App v2 — add color + helper
const VERSION = "2";

function formatName(name: string): string {
  return name.toUpperCase();
}

function greet(name: string): string {
  return `Hello, ${formatName(name)}!`;
}

function main(): void {
  console.log(greet("World"));
}

main();
V2
            ;;
        3)
            # DELETE: formatName helper. MODIFY: greet inlines the logic
            cat << 'V3'
// App v3 — inline formatting
const VERSION = "3";

function greet(name: string): string {
  return `Hello, ${name.toUpperCase()}!`;
}

function main(): void {
  const result = greet("World");
  console.log(result);
}

main();
V3
            ;;
        4)
            # ADD: config block. MODIFY: main reads config
            cat << 'V4'
// App v4 — add config
const VERSION = "4";

const config = {
  greeting: "Hello",
  loud: true,
};

function greet(name: string): string {
  const g = config.loud ? config.greeting.toUpperCase() : config.greeting;
  return `${g}, ${name}!`;
}

function main(): void {
  console.log(greet("World"));
}

main();
V4
            ;;
        5)
            # MODIFY: config values changed. DELETE: comment line
            cat << 'V5'
const VERSION = "5";

const config = {
  greeting: "Hey",
  loud: false,
  emoji: true,
};

function greet(name: string): string {
  const suffix = config.emoji ? " 👋" : "";
  return `${config.greeting}, ${name}!${suffix}`;
}

function main(): void {
  console.log(greet("World"));
}

main();
V5
            ;;
        6)
            # ADD: try/catch error handling. MODIFY: main signature
            cat << 'V6'
const VERSION = "6";

const config = {
  greeting: "Hey",
  loud: false,
  emoji: true,
};

function greet(name: string): string {
  const suffix = config.emoji ? " 👋" : "";
  return `${config.greeting}, ${name}!${suffix}`;
}

function main(args: string[]): void {
  try {
    const name = args[0] || "World";
    console.log(greet(name));
  } catch (e) {
    console.error("Failed:", e);
  }
}

main(process.argv.slice(2));
V6
            ;;
        7)
            # DELETE: try/catch. ADD: logger module. MODIFY: version
            cat << 'V7'
const VERSION = "7";

const logger = {
  info: (msg: string) => console.log(`[INFO] ${msg}`),
  error: (msg: string) => console.error(`[ERROR] ${msg}`),
};

const config = {
  greeting: "Hey",
  emoji: true,
};

function greet(name: string): string {
  const suffix = config.emoji ? " 👋" : "";
  return `${config.greeting}, ${name}!${suffix}`;
}

function main(): void {
  logger.info(greet("World"));
}

main();
V7
            ;;
        8)
            # MODIFY: logger format. DELETE: emoji config. ADD: metrics counter
            cat << 'V8'
const VERSION = "8";

let callCount = 0;

const logger = {
  info: (msg: string) => console.log(`[${new Date().toISOString()}] ${msg}`),
};

const config = {
  greeting: "Hey",
};

function greet(name: string): string {
  callCount++;
  return `${config.greeting}, ${name}!`;
}

function main(): void {
  logger.info(greet("World"));
  logger.info(`Calls: ${callCount}`);
}

main();
V8
            ;;
        9)
            # DELETE: callCount metrics. MODIFY: back to simple main. ADD: footer
            cat << 'V9'
const VERSION = "9";

const config = {
  greeting: "Hello",
};

function greet(name: string): string {
  return `${config.greeting}, ${name}!`;
}

function main(): void {
  console.log(greet("World"));
}

main();
// End of app
V9
            ;;
        10)
            # MODIFY: final clean version — changes greeting, version, adds type
            cat << 'V10'
const VERSION = "10";

type Config = { greeting: string; formal: boolean };

const config: Config = {
  greeting: "Greetings",
  formal: true,
};

function greet(name: string): string {
  const title = config.formal ? "esteemed " : "";
  return `${config.greeting}, ${title}${name}!`;
}

function main(): void {
  console.log(greet("World"));
}

main();
V10
            ;;
    esac
}

for i in $(seq 1 $NUM_CHANGES); do
    generate_version "$i" > src/app.ts

    # Save snapshot
    SNAPSHOTS[$i]="$(cat src/app.ts)"
    cp src/app.ts "$SNAPSHOT_DIR/v$i"
    EXPECTED_LINE_COUNTS[$i]="$(wc -l < src/app.ts | tr -d ' ')"

    if [[ "$i" -eq 1 ]]; then
        require_success "add client app.ts" atomic add src/app.ts
    fi

    capture_change_hashes > "$SNAPSHOT_DIR/before"
    require_capture "record version $i" record_change "Version ${i}"
    REC_OUT="$COMMAND_OUTPUT"
    capture_change_hashes > "$SNAPSHOT_DIR/after"
    require_capture "identify version $i object" find_new_change_hash "$SNAPSHOT_DIR/before" "$SNAPSHOT_DIR/after"
    HASH="$COMMAND_OUTPUT"

    if [[ -n "$HASH" ]]; then
        CHANGE_HASHES[$i]="$HASH"
        _pass "Change $i: ${HASH:0:12} — ${EXPECTED_LINE_COUNTS[$i]} lines"
    else
        _fail "Record change $i" "Could not find change hash. Output: $REC_OUT"
        print_summary
        exit 1
    fi
done

# Final client state
CLIENT_FINAL="$(cat src/app.ts)"
CLIENT_FINAL_LINES="$(wc -l < src/app.ts | tr -d ' ')"
assert_success "client final snapshot matches bytes" cmp -s "$SNAPSHOT_DIR/v$NUM_CHANGES" src/app.ts

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Setup: Server repo"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "server-scale"
SERVER_DIR="$REPO_DIR"
init_repo

CLIENT_CHANGES="$CLIENT_DIR/.atomic/changes"
SERVER_CHANGES="$SERVER_DIR/.atomic/changes"

# Copy ALL change files to server
for i in $(seq 1 $NUM_CHANGES); do
    HASH="${CHANGE_HASHES[$i]}"
    require_success "copy change $i to server byte-for-byte" copy_change_to_server "$HASH" "$CLIENT_CHANGES" "$SERVER_CHANGES"
done

# Create the same agent view on the server
require_success "create server agent view" new_view "$AGENT_VIEW"
assert_view_exists "Server has agent view" "$AGENT_VIEW"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Insert changes 1-${NUM_CHANGES} into draft view, verify after each"
# ═══════════════════════════════════════════════════════════════════════════
#
# This is the critical test loop. After each insert:
#   - Switch to the draft view
#   - Read the file
#   - Verify no duplication
#   - Verify content matches client snapshot

cd "$SERVER_DIR"

for i in $(seq 1 $NUM_CHANGES); do
    HASH="${CHANGE_HASHES[$i]}"
    EXPECTED_LINES="${EXPECTED_LINE_COUNTS[$i]}"

    # Insert
    require_success "insert version $i into agent view" atomic insert "$HASH" --view "$AGENT_VIEW"

    # Switch to draft view to trigger output_working_copy
    require_success "switch to agent view for version $i" switch_view "$AGENT_VIEW"

    # Read the file
    CONTENT="$(cat src/app.ts 2>/dev/null || echo "")"
    ACTUAL_LINES="$(echo "$CONTENT" | wc -l | tr -d ' ')"

    # ── Invariant 1: No function duplication ─────────────────────────
    # main() must appear exactly once regardless of which version
    MAIN_COUNT="$(echo "$CONTENT" | grep -c "function main" || true)"
    if [[ "$MAIN_COUNT" -eq 1 ]]; then
        _pass "v${i}: 'function main' appears exactly once"
    else
        _fail "v${i}: main() duplication" \
            "'function main' appears $MAIN_COUNT times (expected 1). Lines: $ACTUAL_LINES"
        echo "    File content:"
        echo "$CONTENT" | head -40 | sed 's/^/      /'
    fi

    # ── Invariant 2: Correct version constant ────────────────────────
    if echo "$CONTENT" | grep -qF "VERSION = \"${i}\""; then
        _pass "v${i}: version constant is correct"
    else
        FOUND_VER="$(echo "$CONTENT" | grep 'VERSION = ' | head -1)"
        _fail "v${i}: version constant" \
            "Expected VERSION = \"${i}\", found: $FOUND_VER"
    fi

    # ── Invariant 3: No OLD version constant ─────────────────────────
    # The previous version's string must have been REPLACED, not appended
    if [[ "$i" -gt 1 ]]; then
        OLD_VER="$((i - 1))"
        OLD_VER_COUNT="$(echo "$CONTENT" | grep -c "VERSION = \"${OLD_VER}\"" || true)"
        if [[ "$OLD_VER_COUNT" -eq 0 ]]; then
            _pass "v${i}: old version ${OLD_VER} is gone (line was replaced)"
        else
            _fail "v${i}: old version still present" \
                "VERSION = \"${OLD_VER}\" still in file — delete+insert not working"
        fi
    fi

    # ── Invariant 4: Content that was DELETED is actually gone ────────
    # Spot-check removals specific to each version transition
    case "$i" in
        3)
            # v3 deleted formatName helper (was in v2)
            if echo "$CONTENT" | grep -qF "formatName"; then
                _fail "v${i}: deleted content still present" \
                    "formatName should have been deleted in v3"
            else
                _pass "v${i}: formatName correctly deleted"
            fi
            ;;
        5)
            # v5 deleted the "// App v4" comment (was in v4)
            if echo "$CONTENT" | grep -qF "// App v"; then
                _fail "v${i}: deleted comment still present" \
                    "'// App v' comment should have been deleted"
            else
                _pass "v${i}: old comment correctly deleted"
            fi
            ;;
        7)
            # v7 deleted try/catch (was in v6)
            if echo "$CONTENT" | grep -qF "try {"; then
                _fail "v${i}: deleted try/catch still present"
            else
                _pass "v${i}: try/catch correctly deleted"
            fi
            ;;
        9)
            # v9 deleted callCount metrics (was in v8)
            if echo "$CONTENT" | grep -qF "callCount"; then
                _fail "v${i}: deleted metrics still present"
            else
                _pass "v${i}: callCount metrics correctly deleted"
            fi
            ;;
    esac

    # ── Invariant 5: Content that was ADDED is present ───────────────
    case "$i" in
        4)
            if echo "$CONTENT" | grep -qF "config = {"; then
                _pass "v${i}: config block correctly added"
            else
                _fail "v${i}: added content missing" "config block not found"
            fi
            ;;
        6)
            if echo "$CONTENT" | grep -qF "catch (e)"; then
                _pass "v${i}: error handling correctly added"
            else
                _fail "v${i}: added content missing" "try/catch not found"
            fi
            ;;
        8)
            if echo "$CONTENT" | grep -qF "callCount"; then
                _pass "v${i}: metrics counter correctly added"
            else
                _fail "v${i}: added content missing" "callCount not found"
            fi
            ;;
        10)
            if echo "$CONTENT" | grep -qF "type Config"; then
                _pass "v${i}: Config type correctly added"
            else
                _fail "v${i}: added content missing" "type Config not found"
            fi
            ;;
    esac

    # ── Invariant 6: Reasonable file size (no unbounded growth) ──────
    # Allow 30% tolerance over expected
    MAX_LINES="$(( EXPECTED_LINES + EXPECTED_LINES / 3 + 3 ))"
    if [[ "$ACTUAL_LINES" -le "$MAX_LINES" ]]; then
        _pass "v${i}: $ACTUAL_LINES lines (expected ~${EXPECTED_LINES})"
    else
        _fail "v${i}: file size" \
            "$ACTUAL_LINES lines (expected ~${EXPECTED_LINES}, max $MAX_LINES) — likely duplicated"
    fi

    # ── Invariant 7: Content matches client snapshot exactly ─────────
    CLIENT_SNAP="${SNAPSHOTS[$i]}"
    if [[ "$CONTENT" == "$CLIENT_SNAP" ]]; then
        _pass "v${i}: content matches client snapshot"
    else
        _fail "v${i}: content mismatch" \
            "Server content differs from client at version $i"
    fi
    assert_success "v${i}: content matches client byte-for-byte" cmp -s "$SNAPSHOT_DIR/v$i" src/app.ts

    # Switch back to dev between inserts (simulates server staying on dev)
    require_success "switch to dev after version $i" switch_view "dev"
done

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Final verification: dev isolation"
# ═══════════════════════════════════════════════════════════════════════════
#
# After all 10 changes (with adds, modifies, AND deletes) on the draft
# view, dev should still be empty — the app.ts file should NOT exist on dev.

require_success "select dev for isolation check" switch_view "dev"
assert_current_view "On dev for isolation check" "dev"

if [[ ! -f "src/app.ts" ]]; then
    _pass "Dev: src/app.ts does not exist (all changes on draft view)"
else
    DEV_CONTENT="$(cat src/app.ts)"
    DEV_LINES="$(echo "$DEV_CONTENT" | wc -l | tr -d ' ')"
    _fail "Dev: file should not exist" \
        "src/app.ts has $DEV_LINES lines on dev (should not exist)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Final verification: draft view round-trip"
# ═══════════════════════════════════════════════════════════════════════════

require_success "switch to agent for final verification" switch_view "$AGENT_VIEW"
assert_current_view "on agent for final verification" "$AGENT_VIEW"

FINAL_CONTENT="$(cat src/app.ts 2>/dev/null || echo "")"
FINAL_LINES="$(echo "$FINAL_CONTENT" | wc -l | tr -d ' ')"

FINAL_MAIN_COUNT="$(echo "$FINAL_CONTENT" | grep -c "function main" || true)"

if [[ "$FINAL_MAIN_COUNT" -eq 1 ]]; then
    _pass "Final: 'function main' appears exactly once after round-trip"
else
    _fail "Final: main() duplication after round-trip" \
        "'function main' appears $FINAL_MAIN_COUNT times"
fi

if echo "$FINAL_CONTENT" | grep -qF "VERSION = \"${NUM_CHANGES}\""; then
    _pass "Final: version is ${NUM_CHANGES}"
else
    _fail "Final: version" \
        "Expected VERSION = \"${NUM_CHANGES}\""
fi

# v10 should have the Config type (added), NOT callCount (deleted in v9),
# NOT try/catch (deleted in v7), NOT formatName (deleted in v3)
if echo "$FINAL_CONTENT" | grep -qF "type Config"; then
    _pass "Final: has Config type from v10"
else
    _fail "Final: missing Config type"
fi

for gone_term in "formatName" "try {" "callCount" "emoji"; do
    if echo "$FINAL_CONTENT" | grep -qF "$gone_term"; then
        _fail "Final: stale content '$gone_term'" \
            "Term '$gone_term' was deleted in an earlier version but still present"
    else
        _pass "Final: no stale content '$gone_term'"
    fi
done

if [[ "$FINAL_CONTENT" == "$CLIENT_FINAL" ]]; then
    _pass "Final: server content matches client exactly"
else
    _fail "Final: content mismatch" \
        "Server content differs from client's final state"
fi
assert_success "final content matches client byte-for-byte" cmp -s "$SNAPSHOT_DIR/v$NUM_CHANGES" src/app.ts

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Scale check: file size growth is linear, not quadratic"
# ═══════════════════════════════════════════════════════════════════════════
#
# With N changes, each adding ~4 lines (one feature function), the file
# should be roughly: base(~6 lines) + (N-1) * 4 lines = ~42 lines at N=10.
#
# If the duplication bug exists, the file would be:
#   sum(expected_lines[1..N]) = roughly N * average_size ≈ 200+ lines
#
# We check that the final file is under 2x the expected size.

EXPECTED_FINAL_LINES="${EXPECTED_LINE_COUNTS[$NUM_CHANGES]}"
DOUBLE_EXPECTED=$(( EXPECTED_FINAL_LINES * 2 ))

if [[ "$FINAL_LINES" -le "$DOUBLE_EXPECTED" ]]; then
    _pass "Scale: final file $FINAL_LINES lines ≤ 2x expected ($EXPECTED_FINAL_LINES)"
else
    _fail "Scale: quadratic growth detected" \
        "Final file has $FINAL_LINES lines but expected ~${EXPECTED_FINAL_LINES}. \
This indicates O(N) duplication — each insert is concatenating instead of merging."
fi

# For extra confidence: the sum of ALL intermediate file sizes should be
# much larger than the final file if there's no duplication.
TOTAL_INTERMEDIATE=0
for i in $(seq 1 $NUM_CHANGES); do
    TOTAL_INTERMEDIATE=$(( TOTAL_INTERMEDIATE + EXPECTED_LINE_COUNTS[$i] ))
done

if [[ "$FINAL_LINES" -lt "$TOTAL_INTERMEDIATE" ]]; then
    _pass "Scale: final ($FINAL_LINES) < sum of intermediates ($TOTAL_INTERMEDIATE) — no accumulation"
else
    _fail "Scale: accumulation detected" \
        "Final $FINAL_LINES ≥ sum of intermediates $TOTAL_INTERMEDIATE"
fi

# ═══════════════════════════════════════════════════════════════════════════

print_summary
