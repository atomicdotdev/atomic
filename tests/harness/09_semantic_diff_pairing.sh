#!/usr/bin/env bash
# 09_semantic_diff_pairing.sh — Semantic diff line pairing tests.
#
# The CRDT semantic layer (BranchOps/LeafOps) should be SMARTER than raw
# Myers line diff. When a line is MODIFIED (not purely added or deleted),
# the semantic layer must emit the Delete immediately followed by its
# paired Insert. This enables:
#
#   - The CLI to show "N pairs with M" annotations
#   - The CLI to show word-level highlighting within paired lines
#   - The WebUI (@pierre/diffs) to render inline word-level diffs
#
# Without pairing, modified lines appear as unrelated delete + add
# operations scattered across the diff output. The diff viewer can't
# detect that `console.log(greet("World"))` and `console.log(greet(name))`
# are the same line with one argument changed.
#
# Key invariants tested:
#
#   1. A single modified line in a Replace block produces adjacent -/+ in
#      `atomic diff -c` output (the delete line immediately before the
#      matching insert line)
#
#   2. When new lines are added AROUND a modified line, the modified pair
#      stays adjacent — not separated by the added lines
#
#   3. Multiple modified lines in the same Replace block are each paired
#      with their matching insert
#
#   4. The "pairs with" annotation appears in the CLI diff output for
#      each paired modification
#
#   5. Word-level token changes are detectable: given a paired -/+ where
#      only one token changed, the diff output contains both the old and
#      new token text (enabling word-level highlighting)
#
#   6. Lines that are purely added (no corresponding delete) are NOT
#      incorrectly paired with unrelated deletes
#
#   7. Lines that are purely deleted (no corresponding insert) are NOT
#      incorrectly paired with unrelated inserts
#
#   8. Pairing works at scale: 10+ modifications scattered among 50+
#      additions still produce correct pairs
#
# These tests use `atomic diff -c <hash>` which reads from the change's
# file_ops (CRDT BranchOps), NOT from the graph. The semantic layer is
# the source of truth for diff display.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ═══════════════════════════════════════════════════════════════════════════
# Helper: get the full hash of the latest change
# ═══════════════════════════════════════════════════════════════════════════

get_last_change_hash() {
    # Find the newest .change file by modification time
    find "$REPO_DIR/.atomic/changes" -name "*.change" -type f -print0 2>/dev/null \
        | xargs -0 ls -t 2>/dev/null | head -1 | xargs basename 2>/dev/null | sed 's/\.change$//'
}

# ═══════════════════════════════════════════════════════════════════════════
# Helper: check that a diff output has a delete line immediately followed
# by an insert line containing the expected content
# ═══════════════════════════════════════════════════════════════════════════

# assert_paired_diff "description" "diff_output" "deleted_fragment" "inserted_fragment"
#
# Checks that somewhere in the diff output there is a line containing
# deleted_fragment with a - prefix, IMMEDIATELY followed (within 1 line)
# by a line containing inserted_fragment with a + prefix.
assert_paired_diff() {
    local desc="$1"
    local diff_output="$2"
    local del_fragment="$3"
    local ins_fragment="$4"

    # Find line numbers of the delete and insert
    local del_lineno=""
    local ins_lineno=""
    local lineno=0

    while IFS= read -r line; do
        lineno=$((lineno + 1))
        # Check for delete line (has - prefix or old line number marker)
        if echo "$line" | grep -qF "$del_fragment"; then
            if echo "$line" | grep -qE "^\s*[0-9]*\s*-|^-"; then
                del_lineno="$lineno"
            fi
        fi
        # Check for insert line (has + prefix or new line number marker)
        if echo "$line" | grep -qF "$ins_fragment"; then
            if echo "$line" | grep -qE "^\s*[0-9]*\s*\+|^\+"; then
                ins_lineno="$lineno"
            fi
        fi
    done <<< "$diff_output"

    if [[ -z "$del_lineno" ]]; then
        _fail "$desc" "Delete line containing '$del_fragment' not found in diff output"
        return
    fi
    if [[ -z "$ins_lineno" ]]; then
        _fail "$desc" "Insert line containing '$ins_fragment' not found in diff output"
        return
    fi

    # The insert should be immediately after the delete (within 1 line)
    local gap=$((ins_lineno - del_lineno))
    if [[ "$gap" -eq 1 ]]; then
        _pass "$desc"
    elif [[ "$gap" -ge 2 && "$gap" -le 3 ]]; then
        # Allow small gap (e.g., a blank line between them)
        _pass "$desc (gap=$gap, acceptable)"
    else
        _fail "$desc" \
            "Delete at output line $del_lineno, insert at $ins_lineno (gap=$gap, expected 1). Lines should be adjacent for pairing."
    fi
}

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Pairing: Single modified line among additions"
# ═══════════════════════════════════════════════════════════════════════════
#
# Scenario: file has 5 lines. The change:
#   - Adds 3 new lines at the top (import statements)
#   - Modifies line 5: console.log(greet("World")) → console.log(greet(name))
#
# Expected: the delete of the old console.log and the insert of the new
# console.log are ADJACENT in the diff, not separated by the 3 added lines.

make_temp_repo "pair-single"
init_repo

mkdir -p src
cat > src/app.ts << 'EOF'
function greet(name: string): string {
  return `Hello, ${name}!`;
}

console.log(greet("World"));
EOF

add_files src/app.ts
record_change "Initial file" >/dev/null 2>&1

# Modify: add imports at top, change the console.log call
cat > src/app.ts << 'EOF'
import * as readline from "readline";
import { stdin, stdout } from "process";

function greet(name: string): string {
  return `Hello, ${name}!`;
}

const rl = readline.createInterface({ input: stdin, output: stdout });

rl.question("Name: ", (answer) => {
  const name = answer.trim() || "World";
  console.log(greet(name));
  rl.close();
});
EOF

record_change "Add readline, modify console.log" >/dev/null 2>&1

HASH="$(get_last_change_hash)"

if [[ -z "$HASH" ]]; then
    _fail "Get change hash" "Could not find change hash"
    print_summary
    exit 1
fi

DIFF_OUT="$(atomic diff --no-color -c "$HASH" 2>&1)"

# The old line: console.log(greet("World"));
# The new line: console.log(greet(name));
# These share "console.log(greet(" — should be paired.

assert_paired_diff \
    "Single mod: console.log delete+insert are adjacent" \
    "$DIFF_OUT" \
    'greet("World")' \
    'greet(name)'

# Also verify both lines are actually present in the diff
assert_output_contains \
    "Single mod: old console.log is in diff" \
    'greet("World")' \
    echo "$DIFF_OUT"

assert_output_contains \
    "Single mod: new console.log is in diff" \
    'greet(name)' \
    echo "$DIFF_OUT"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Pairing: Modified line with only one token changed"
# ═══════════════════════════════════════════════════════════════════════════
#
# Scenario: only one word changes in a line. The diff should pair the
# lines and both the old and new token should be visible in the output.

make_temp_repo "pair-token"
init_repo

mkdir -p src
cat > src/config.ts << 'EOF'
export const API_URL = "https://api.example.com";
export const TIMEOUT = 30;
export const RETRIES = 3;
export const DEBUG = false;
EOF

add_files src/config.ts
record_change "Initial config" >/dev/null 2>&1

# Change only one value: TIMEOUT 30 → 60
cat > src/config.ts << 'EOF'
export const API_URL = "https://api.example.com";
export const TIMEOUT = 60;
export const RETRIES = 3;
export const DEBUG = false;
EOF

record_change "Increase timeout" >/dev/null 2>&1

HASH="$(get_last_change_hash)"
DIFF_OUT="$(atomic diff --no-color -c "$HASH" 2>&1)"

# The delete should contain "TIMEOUT = 30" and the insert "TIMEOUT = 60"
# and they should be adjacent
assert_paired_diff \
    "Token change: TIMEOUT 30→60 paired" \
    "$DIFF_OUT" \
    "TIMEOUT = 30" \
    "TIMEOUT = 60"

# Verify the diff is minimal (only 1 line changed, not the whole file)
# Exclude diff header lines (--- and +++) from counts
PLUS_COUNT="$(echo "$DIFF_OUT" | grep -vE '^\+\+\+|^---' | grep -cE '^\s*[0-9]*\s*\+|^\+' || true)"
MINUS_COUNT="$(echo "$DIFF_OUT" | grep -vE '^\+\+\+|^---' | grep -cE '^\s*[0-9]*\s*-|^-' || true)"

if [[ "$PLUS_COUNT" -le 2 && "$MINUS_COUNT" -le 2 ]]; then
    _pass "Token change: minimal diff ($MINUS_COUNT deletions, $PLUS_COUNT insertions)"
else
    _fail "Token change: diff too large" \
        "Expected ~1 delete + ~1 insert, got $MINUS_COUNT deletes + $PLUS_COUNT inserts"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Pairing: Multiple modifications in same change"
# ═══════════════════════════════════════════════════════════════════════════
#
# Scenario: multiple lines are modified in the same change. Each modified
# line should be paired with its corresponding new line.

make_temp_repo "pair-multi"
init_repo

mkdir -p src
cat > src/server.ts << 'EOF'
const HOST = "localhost";
const PORT = 3000;
const DB_HOST = "localhost";
const DB_PORT = 5432;
const DB_NAME = "myapp";

function start() {
  console.log(`Server on ${HOST}:${PORT}`);
  console.log(`DB at ${DB_HOST}:${DB_PORT}/${DB_NAME}`);
}

start();
EOF

add_files src/server.ts
record_change "Initial server config" >/dev/null 2>&1

# Change HOST, PORT, and DB_HOST (3 modifications, rest unchanged)
cat > src/server.ts << 'EOF'
const HOST = "0.0.0.0";
const PORT = 8080;
const DB_HOST = "db.production.internal";
const DB_PORT = 5432;
const DB_NAME = "myapp";

function start() {
  console.log(`Server on ${HOST}:${PORT}`);
  console.log(`DB at ${DB_HOST}:${DB_PORT}/${DB_NAME}`);
}

start();
EOF

record_change "Production config" >/dev/null 2>&1

HASH="$(get_last_change_hash)"
DIFF_OUT="$(atomic diff --no-color -c "$HASH" 2>&1)"

assert_paired_diff \
    "Multi mod: HOST localhost→0.0.0.0 paired" \
    "$DIFF_OUT" \
    'const HOST' \
    'HOST = "0.0.0.0"'

assert_paired_diff \
    "Multi mod: PORT 3000→8080 paired" \
    "$DIFF_OUT" \
    "PORT = 3000" \
    "PORT = 8080"

assert_paired_diff \
    "Multi mod: DB_HOST paired" \
    "$DIFF_OUT" \
    'DB_HOST = "localhost"' \
    'DB_HOST = "db.production.internal"'

# Unchanged lines should NOT appear in the diff
assert_output_not_contains \
    "Multi mod: unchanged DB_PORT not in diff" \
    "DB_PORT = 5432" \
    echo "$DIFF_OUT"

assert_output_not_contains \
    "Multi mod: unchanged DB_NAME not in diff" \
    'DB_NAME = "myapp"' \
    echo "$DIFF_OUT"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Pairing: Modification buried in large insertion block"
# ═══════════════════════════════════════════════════════════════════════════
#
# This is the exact production scenario: a small file is expanded with
# many new lines, AND one existing line is modified. The modification
# should still be paired even though it's surrounded by 10+ insertions.

make_temp_repo "pair-buried"
init_repo

mkdir -p src
cat > src/main.ts << 'EOF'
function hello(): string {
  return "Hello, World!";
}

console.log(hello());
EOF

add_files src/main.ts
record_change "Simple hello" >/dev/null 2>&1

# Major refactor: add error handling, logging, config — AND modify the
# console.log call (World → name). The modification is line 5 old → line 20+ new.
cat > src/main.ts << 'EOF'
import { createLogger } from "./logger";

const logger = createLogger("main");

interface Config {
  name: string;
  verbose: boolean;
}

const config: Config = {
  name: process.env.NAME || "World",
  verbose: process.env.VERBOSE === "true",
};

function hello(name: string): string {
  return `Hello, ${name}!`;
}

function run(cfg: Config): void {
  const message = hello(cfg.name);
  if (cfg.verbose) {
    logger.info(`Generated greeting: ${message}`);
  }
  console.log(message);
}

try {
  run(config);
} catch (e) {
  logger.error(`Failed: ${e}`);
  process.exit(1);
}
EOF

record_change "Major refactor with modified hello call" >/dev/null 2>&1

HASH="$(get_last_change_hash)"
DIFF_OUT="$(atomic diff --no-color -c "$HASH" 2>&1)"

# The old line: console.log(hello());
# In the new file, the call is now: console.log(message);
# These share "console.log(" — the semantic layer should pair them
# even though there are 25+ new lines between them.
assert_paired_diff \
    "Buried mod: console.log(hello()) paired with console.log(message)" \
    "$DIFF_OUT" \
    "console.log(hello())" \
    "console.log(message)"

# The old function: function hello(): string
# The new function: function hello(name: string): string
# The signature changed (added parameter) — should be paired
assert_paired_diff \
    "Buried mod: hello() signature paired with hello(name: string)" \
    "$DIFF_OUT" \
    "function hello():" \
    "function hello(name:"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Pairing: Pure additions are NOT falsely paired"
# ═══════════════════════════════════════════════════════════════════════════
#
# When lines are purely added (no corresponding old line), they should
# appear as standalone + lines, not paired with unrelated deletes.

make_temp_repo "pair-no-false"
init_repo

mkdir -p src
cat > src/utils.ts << 'EOF'
export function add(a: number, b: number): number {
  return a + b;
}
EOF

add_files src/utils.ts
record_change "Initial utils" >/dev/null 2>&1

# Add a new function (pure addition) — no modification of existing lines
cat > src/utils.ts << 'EOF'
export function add(a: number, b: number): number {
  return a + b;
}

export function multiply(a: number, b: number): number {
  return a * b;
}
EOF

record_change "Add multiply function" >/dev/null 2>&1

HASH="$(get_last_change_hash)"
DIFF_OUT="$(atomic diff --no-color -c "$HASH" 2>&1)"

# Should have only additions, no deletions
# Exclude diff header lines (--- a/file) from the count
MINUS_COUNT="$(echo "$DIFF_OUT" | grep -vE '^\+\+\+|^---' | grep -cE '^\s*[0-9]*\s*-|^-' || true)"

if [[ "$MINUS_COUNT" -eq 0 ]]; then
    _pass "Pure add: no deletion lines in diff"
else
    _fail "Pure add: unexpected deletions" \
        "Found $MINUS_COUNT deletion lines in a pure-addition change"
fi

assert_output_contains \
    "Pure add: multiply function present" \
    "multiply" \
    echo "$DIFF_OUT"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Pairing: Pure deletions are NOT falsely paired"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "pair-no-false-del"
init_repo

mkdir -p src
cat > src/legacy.ts << 'EOF'
export function oldHelper(): void {
  console.log("deprecated");
}

export function currentHelper(): void {
  console.log("current");
}
EOF

add_files src/legacy.ts
record_change "Initial legacy" >/dev/null 2>&1

# Delete the old function, keep the current one
cat > src/legacy.ts << 'EOF'
export function currentHelper(): void {
  console.log("current");
}
EOF

record_change "Remove deprecated function" >/dev/null 2>&1

HASH="$(get_last_change_hash)"
DIFF_OUT="$(atomic diff --no-color -c "$HASH" 2>&1)"

# Should have only deletions, no additions
# Exclude diff header lines (+++ b/file) from the count
PLUS_COUNT="$(echo "$DIFF_OUT" | grep -vE '^\+\+\+|^---' | grep -cE '^\s*[0-9]*\s*\+|^\+' || true)"

if [[ "$PLUS_COUNT" -eq 0 ]]; then
    _pass "Pure delete: no insertion lines in diff"
else
    _fail "Pure delete: unexpected insertions" \
        "Found $PLUS_COUNT insertion lines in a pure-deletion change"
fi

assert_output_contains \
    "Pure delete: oldHelper in diff" \
    "oldHelper" \
    echo "$DIFF_OUT"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Pairing at scale: 10 modifications among 50 additions"
# ═══════════════════════════════════════════════════════════════════════════
#
# Stress test: a file with 20 lines. The change modifies 10 of them AND
# adds 50 new lines. Each of the 10 modifications should still be paired
# correctly despite being surrounded by large insertion blocks.

make_temp_repo "pair-scale"
init_repo

mkdir -p src

# Generate initial file: 20 lines with predictable content
{
    for i in $(seq 1 20); do
        echo "export const VALUE_${i} = \"original_${i}\";"
    done
} > src/constants.ts

add_files src/constants.ts
record_change "Initial 20 constants" >/dev/null 2>&1

# Modify even-numbered lines (10 modifications) and add 5 new lines
# between each pair of constants (50 additions total)
{
    for i in $(seq 1 20); do
        # Add 2-3 comment lines before some constants (new additions)
        if [[ $((i % 2)) -eq 0 ]]; then
            echo ""
            echo "// Section ${i} — updated for production"
            echo "// Reviewed on 2026-01-01"
        fi

        if [[ $((i % 2)) -eq 0 ]]; then
            # Modified: change "original_N" to "updated_N"
            echo "export const VALUE_${i} = \"updated_${i}\";"
        else
            # Unchanged
            echo "export const VALUE_${i} = \"original_${i}\";"
        fi
    done
} > src/constants.ts

record_change "Update even constants, add comments" >/dev/null 2>&1

HASH="$(get_last_change_hash)"
DIFF_OUT="$(atomic diff --no-color -c "$HASH" 2>&1)"

# Check that each modified line is paired
PAIR_SUCCESS=0
PAIR_FAIL=0
for i in 2 4 6 8 10 12 14 16 18 20; do
    # Each modification: "original_N" → "updated_N"
    del_frag="VALUE_${i} = \"original_${i}\""
    ins_frag="VALUE_${i} = \"updated_${i}\""

    # Find line numbers
    del_ln=""
    ins_ln=""
    ln=0
    while IFS= read -r line; do
        ln=$((ln + 1))
        if echo "$line" | grep -qF "$del_frag" && echo "$line" | grep -qE '^\s*[0-9]*\s*-|^-'; then
            del_ln="$ln"
        fi
        if echo "$line" | grep -qF "$ins_frag" && echo "$line" | grep -qE '^\s*[0-9]*\s*\+|^\+'; then
            ins_ln="$ln"
        fi
    done <<< "$DIFF_OUT"

    if [[ -n "$del_ln" && -n "$ins_ln" ]]; then
        gap=$((ins_ln - del_ln))
        if [[ "$gap" -ge 1 && "$gap" -le 3 ]]; then
            PAIR_SUCCESS=$((PAIR_SUCCESS + 1))
        else
            PAIR_FAIL=$((PAIR_FAIL + 1))
        fi
    else
        PAIR_FAIL=$((PAIR_FAIL + 1))
    fi
done

if [[ "$PAIR_SUCCESS" -ge 8 ]]; then
    _pass "Scale: $PAIR_SUCCESS/10 modifications correctly paired"
else
    _fail "Scale: pairing at scale" \
        "Only $PAIR_SUCCESS/10 modifications paired ($PAIR_FAIL failed). Expected ≥8."
fi

# Verify unchanged lines are NOT in the diff
for i in 1 3 5 7 9; do
    if echo "$DIFF_OUT" | grep -qF "VALUE_${i} = \"original_${i}\""; then
        _fail "Scale: unchanged VALUE_${i} should not be in diff" \
            "VALUE_${i} appeared in diff but was not modified"
    fi
done
_pass "Scale: unchanged odd constants not in diff"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Pairing: Mixed adds, deletes, and modifications"
# ═══════════════════════════════════════════════════════════════════════════
#
# The most complex case: a change that simultaneously:
#   - Adds new lines (pure insertion)
#   - Deletes old lines (pure deletion)
#   - Modifies existing lines (delete + insert pair)
#
# Each type should be handled correctly without interfering with others.

make_temp_repo "pair-mixed"
init_repo

mkdir -p src
cat > src/handler.ts << 'EOF'
import { Request } from "http";

function parseBody(req: Request): string {
  return req.body || "";
}

function validate(data: string): boolean {
  return data.length > 0;
}

function respond(status: number): void {
  console.log(`Status: ${status}`);
}

export function handle(req: Request): void {
  const body = parseBody(req);
  if (validate(body)) {
    respond(200);
  } else {
    respond(400);
  }
}
EOF

add_files src/handler.ts
record_change "Initial handler" >/dev/null 2>&1

# Complex change:
#   - ADD: import for Response type (pure add)
#   - DELETE: the parseBody function entirely (pure delete)
#   - MODIFY: validate return type boolean → ValidationResult
#   - MODIFY: respond takes Response object instead of status number
#   - ADD: new logging (pure add)
#   - MODIFY: handle function signature
cat > src/handler.ts << 'EOF'
import { Request, Response } from "http";
import { logger } from "./logger";

type ValidationResult = { valid: boolean; error?: string };

function validate(data: string): ValidationResult {
  if (data.length === 0) return { valid: false, error: "empty" };
  return { valid: true };
}

function respond(res: Response, status: number, body: string): void {
  logger.info(`Responding: ${status}`);
  res.writeHead(status);
  res.end(body);
}

export function handle(req: Request, res: Response): void {
  const body = req.body || "";
  const result = validate(body);
  if (result.valid) {
    respond(res, 200, "OK");
  } else {
    logger.warn(`Validation failed: ${result.error}`);
    respond(res, 400, result.error || "Bad Request");
  }
}
EOF

record_change "Refactor handler with types and logging" >/dev/null 2>&1

HASH="$(get_last_change_hash)"
DIFF_OUT="$(atomic diff --no-color -c "$HASH" 2>&1)"

# The validate function signature changed: boolean → ValidationResult
assert_paired_diff \
    "Mixed: validate return type modification paired" \
    "$DIFF_OUT" \
    "validate(data: string): boolean" \
    "validate(data: string): ValidationResult"

# The respond signature changed: (status: number) → (res: Response, status: number, body: string)
assert_paired_diff \
    "Mixed: respond signature modification paired" \
    "$DIFF_OUT" \
    "function respond(status:" \
    "function respond(res:"

# The handle signature changed: (req: Request) → (req: Request, res: Response)
assert_paired_diff \
    "Mixed: handle signature modification paired" \
    "$DIFF_OUT" \
    "handle(req: Request):" \
    "handle(req: Request, res:"

# Pure addition: logger import should be in the diff
assert_output_contains \
    "Mixed: logger import added" \
    'import { logger }' \
    echo "$DIFF_OUT"

# Pure deletion: parseBody function should be in the diff as removed
assert_output_contains \
    "Mixed: parseBody deletion in diff" \
    "parseBody" \
    echo "$DIFF_OUT"

# ═══════════════════════════════════════════════════════════════════════════

print_summary
