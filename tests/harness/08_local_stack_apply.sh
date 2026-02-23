#!/usr/bin/env bash
# 08_local_stack_apply.sh — Local stack apply correctness at scale.
#
# Tests the exact code path that the API server push handler follows:
# changes are recorded on a client repo's local (agent) stack, copied
# to a server repo, and applied to the same local stack on the server.
#
# The critical invariant: after applying N sequential changes that modify
# the same file, the server's working copy must contain ONLY the final
# state — not a concatenation of all intermediate states.
#
# This is the spec for the duplication bug:
#   When two or more changes are applied to the same local stack via
#   `atomic apply <hash> --stack <local>`, the second change's EdgeUpdate
#   (which replaces old content with new content) must properly delete
#   the old BLOCK edge in STACK_GRAPH before adding the new one.
#   Without this, the graph traversal sees BOTH the old and new content
#   vertices as alive, producing duplicated file output.
#
# Invariants tested at every step:
#
#   1. No function/identifier duplication in the output file
#   2. File line count stays within expected bounds (not growing unbounded)
#   3. Content matches what the client had at that revision
#   4. Each change's NEW content is present
#   5. Each change's OLD content (that was replaced) is absent
#   6. After switching to dev, the local stack's files are NOT visible
#   7. After switching back to the local stack, content is still correct
#
# Scale targets:
#   - 10 sequential changes to the same file
#   - Each change modifies existing lines AND adds new lines
#   - Verifies O(1) file size growth per change (not O(N) duplication)

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

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

    local src_file
    src_file="$(find "$src_dir" -name "${hash}*" -type f 2>/dev/null | head -1)"

    if [[ -z "$src_file" ]]; then
        src_file="$(find "$client_changes" -name "${hash}*" -type f 2>/dev/null | head -1)"
    fi

    if [[ -n "$src_file" && -f "$src_file" ]]; then
        cp "$src_file" "$dst_dir/"
        return 0
    fi
    return 1
}

# ═══════════════════════════════════════════════════════════════════════════
# Helper: get the full hash of the most recently recorded change
# ═══════════════════════════════════════════════════════════════════════════

# We track known hashes and find the newest one that isn't already known.
# CRITICAL: changes must be discovered in the order they were recorded,
# not alphabetically by hash. We use modification time (newest first)
# so the most recently written .change file is found.
KNOWN_HASHES=""

get_newest_change_hash() {
    local changes_dir="$1"
    local newest=""

    # Sort by modification time (newest last) so the last unknown file
    # is the most recently recorded change.
    while IFS= read -r f; do
        local h
        h="$(basename "$f" .change)"
        if echo "$KNOWN_HASHES" | grep -qF "$h"; then
            continue
        fi
        newest="$h"
    done < <(find "$changes_dir" -name "*.change" -type f -print0 2>/dev/null \
        | xargs -0 ls -tr 2>/dev/null)

    if [[ -n "$newest" ]]; then
        KNOWN_HASHES="${KNOWN_HASHES}${newest}
"
        echo "$newest"
    fi
}

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Setup: Client repo with agent stack"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "client-scale"
CLIENT_DIR="$REPO_DIR"
init_repo

AGENT_STACK="agent-ses_scale-test"

new_stack "$AGENT_STACK" >/dev/null 2>&1 || true
switch_stack "$AGENT_STACK" >/dev/null 2>&1 || true
assert_current_stack "Client on agent stack" "$AGENT_STACK"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Record 10 sequential changes on agent stack"
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
# At EVERY step, some lines are added, some are modified (replaced), and
# some are deleted. This ensures the EdgeUpdate (del old BLOCK + add new
# DELETED|BLOCK) path is exercised thoroughly.

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
    EXPECTED_LINE_COUNTS[$i]="$(wc -l < src/app.ts | tr -d ' ')"

    if [[ "$i" -eq 1 ]]; then
        add_files src/app.ts
    fi

    REC_OUT="$(record_change "Version ${i}" 2>&1)"

    HASH="$(get_newest_change_hash "$CLIENT_DIR/.atomic/changes")"

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
_pass "Client final: $CLIENT_FINAL_LINES lines"

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
    if copy_change_to_server "$HASH" "$CLIENT_CHANGES" "$SERVER_CHANGES"; then
        : # silent success
    else
        _fail "Copy change $i to server" "Hash: $HASH"
        print_summary
        exit 1
    fi
done
_pass "Copied all $NUM_CHANGES change files to server"

# Create the same agent stack on the server
new_stack "$AGENT_STACK" >/dev/null 2>&1 || true
assert_stack_exists "Server has agent stack" "$AGENT_STACK"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Apply changes 1-${NUM_CHANGES} to local stack, verify after each"
# ═══════════════════════════════════════════════════════════════════════════
#
# This is the critical test loop. After each apply:
#   - Switch to the local stack
#   - Read the file
#   - Verify no duplication
#   - Verify content matches client snapshot

cd "$SERVER_DIR"

for i in $(seq 1 $NUM_CHANGES); do
    HASH="${CHANGE_HASHES[$i]}"
    EXPECTED_LINES="${EXPECTED_LINE_COUNTS[$i]}"

    # Apply
    APPLY_OUT="$(atomic apply "$HASH" --stack "$AGENT_STACK" 2>&1)" || true

    # Switch to local stack to trigger output_working_copy
    switch_stack "$AGENT_STACK" >/dev/null 2>&1 || true

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

    # Switch back to dev between applies (simulates server staying on dev)
    switch_stack "dev" >/dev/null 2>&1 || true
done

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Final verification: dev isolation"
# ═══════════════════════════════════════════════════════════════════════════
#
# After all 10 changes (with adds, modifies, AND deletes) on the local
# stack, dev should still be empty — the app.ts file should NOT exist on dev.

switch_stack "dev" >/dev/null 2>&1 || true
assert_current_stack "On dev for isolation check" "dev"

if [[ ! -f "src/app.ts" ]]; then
    _pass "Dev: src/app.ts does not exist (all changes on local stack)"
else
    DEV_CONTENT="$(cat src/app.ts)"
    DEV_LINES="$(echo "$DEV_CONTENT" | wc -l | tr -d ' ')"
    _fail "Dev: file should not exist" \
        "src/app.ts has $DEV_LINES lines on dev (should not exist)"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Final verification: local stack round-trip"
# ═══════════════════════════════════════════════════════════════════════════

switch_stack "$AGENT_STACK" >/dev/null 2>&1 || true

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
    fi
done
_pass "Final: no stale content from deleted versions"

if [[ "$FINAL_CONTENT" == "$CLIENT_FINAL" ]]; then
    _pass "Final: server content matches client exactly"
else
    _fail "Final: content mismatch" \
        "Server content differs from client's final state"
fi

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
This indicates O(N) duplication — each apply is concatenating instead of merging."
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
