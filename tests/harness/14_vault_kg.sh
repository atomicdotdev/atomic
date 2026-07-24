#!/usr/bin/env bash
# 14_vault_kg.sh — Test harness for Vault + Knowledge Graph
#
# Tests the vault lifecycle, KG enrichment, and query capabilities:
#   1. Vault initialization and default content
#   2. Goal lifecycle (start, stop, resume)
#   3. Intent lifecycle (create, update, link)
#   4. Record + vault integration
#   5. KG enrichment from VCS data
#   6. Vault show and query
#   7. Git import → KG enrichment → queries
#   8. Memory and materialize
#   9. Vault behavior in a repo initialized without --vault
#
# Every assertion checks both the exit code and the expected output shape.
# A branch that would pass regardless of the command's outcome is a bug in
# this suite, not a feature: the point of the harness is trustworthy greens.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 14_vault_kg${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

# ── Local helpers ───────────────────────────────────────────────────────────
# Like require_network but returns true/false instead of exiting the script.
_network_available() {
    curl --silent --head --max-time 5 https://github.com &>/dev/null
}

# Run a command, capturing combined output and exit code into OUT/RC.
# The `|| RC=$?` guard keeps a failing command from aborting the suite
# under `set -e` — failures are asserted on, not fatal.
# Usage: run_cmd atomic vault list --json
run_cmd() {
    RC=0
    OUT="$("$@" 2>&1)" || RC=$?
}

# Assert the last run_cmd exited 0 AND its output matches an ERE pattern.
# Usage: assert_ran "desc" "pattern"
assert_ran() {
    local desc="$1"
    local pattern="$2"
    if [[ $RC -ne 0 ]]; then
        _fail "$desc" "exit $RC: $(echo "$OUT" | head -2 | tr '\n' ' ')"
    elif ! echo "$OUT" | grep -qiE "$pattern"; then
        _fail "$desc" "output missing /$pattern/: $(echo "$OUT" | head -2 | tr '\n' ' ')"
    else
        _pass "$desc"
    fi
}

# ════════════════════════════════════════════════════════════════════════════
# Section 1: Vault Initialization
# ════════════════════════════════════════════════════════════════════════════

begin_section "Vault: Initialization"

make_temp_repo "vault-init"
init_repo --vault

assert_dir_exists ".vault directory created" ".vault"
assert_dir_exists ".vault/goals directory" ".vault/goals"
assert_dir_exists ".vault/intents directory" ".vault/intents"
assert_dir_exists ".vault/memory directory" ".vault/memory"
assert_dir_exists ".vault/skills directory" ".vault/skills"

# Default skills should be installed
assert_file_exists "vault skill installed" ".vault/skills/atomic-vault.md"
assert_file_exists "code intelligence skill installed" ".vault/skills/code-intelligence.md"

# Default memory index should exist
assert_file_exists "memory index installed" ".vault/memory/MEMORY.md"

# Vault list should show default entries
run_cmd atomic vault list --json
assert_ran "vault list shows default skill" "skills/atomic-vault\.md"

# ════════════════════════════════════════════════════════════════════════════
# Section 2: Goal Lifecycle
# ════════════════════════════════════════════════════════════════════════════

begin_section "Vault: Goal Lifecycle"

make_temp_repo "vault-goals"
init_repo --vault

# Start a goal with a custom name
run_cmd atomic vault goal start --name test-goal --developer "alice"
assert_ran "goal start succeeds with custom name" "Started goal: test-goal"

# Goal should appear in list
run_cmd atomic vault goal list --json
assert_ran "goal appears in list" "test-goal"

# Goal file should exist on disk
assert_file_exists "goal file materialized" ".vault/goals/test-goal/_goal.md"

# Stop with promote
run_cmd atomic vault goal stop --promote test-goal
assert_ran "goal stop --promote succeeds" "Completed goal: test-goal"

# Start another goal, then discard
run_cmd atomic vault goal start --name discard-me
assert_ran "second goal start succeeds" "Started goal: discard-me"

run_cmd atomic vault goal stop --discard discard-me
assert_ran "goal stop --discard succeeds" "Discarded goal: discard-me"

# ════════════════════════════════════════════════════════════════════════════
# Section 3: Intent Lifecycle
# ════════════════════════════════════════════════════════════════════════════

begin_section "Vault: Intent Lifecycle"

make_temp_repo "vault-intents"
init_repo --vault

# Create an intent — must print the assigned ID
run_cmd atomic intent new "Fix authentication"
assert_ran "intent new returns an ID" "Created intent: [A-Za-z0-9-]+-[0-9]+"

first_id="$(echo "$OUT" | sed -n 's/.*Created intent: \([A-Za-z0-9-]*\).*/\1/p' | head -1)"

# List intents — canonical list JSON carries id/status, not the title
run_cmd atomic intent list --json
assert_ran "intent appears in list by ID" "\"id\": \"$first_id\""

# Create a second intent
run_cmd atomic intent new "Add logging"
assert_ran "second intent new succeeds" "Created intent:"

run_cmd atomic intent list --json
intent_count="$(echo "$OUT" | grep -c '"id"' || true)"
if [[ $RC -eq 0 && "$intent_count" -ge 2 ]]; then
    _pass "multiple intents created (count: $intent_count)"
else
    _fail "multiple intents" "exit $RC, expected >= 2 ids, got $intent_count"
fi

# Show the first intent by ID
if [[ -n "$first_id" ]]; then
    run_cmd atomic intent show "$first_id" --json
    assert_ran "intent show returns detail for $first_id" "fix authentication"

    run_cmd atomic intent update "$first_id" --status in-progress
    assert_ran "intent update sets status" "Updated intent: $first_id"
else
    _fail "intent show" "could not extract intent ID from create output"
    _fail "intent update" "could not extract intent ID from create output"
fi

# Link intent to a goal (the goal must exist first)
run_cmd atomic vault goal start --name intent-goal
assert_ran "goal for linking created" "Started goal: intent-goal"

if [[ -n "$first_id" ]]; then
    run_cmd atomic intent link --goal intent-goal "$first_id"
    assert_ran "intent linked to goal" "link|intent-goal"
else
    _fail "intent link" "could not extract intent ID from create output"
fi

# ════════════════════════════════════════════════════════════════════════════
# Section 4: Record + Vault Integration
# ════════════════════════════════════════════════════════════════════════════

begin_section "Vault: Record Integration"

make_temp_repo "vault-record"
init_repo --vault

# Create and track a file
create_file "src/main.rs" 'fn main() { println!("hello"); }'
add_files "src/main.rs"

# Record should succeed (and auto-enrich the KG)
rc=0
record_change "Add main.rs" >/dev/null 2>&1 || rc=$?
if [[ $rc -eq 0 ]]; then
    _pass "record succeeds with vault enabled"
else
    _fail "record succeeds with vault enabled" "exit $rc"
fi

# Record auto-enriches: search must surface the recorded change or file
run_cmd atomic vault query search "main" --json
assert_ran "KG search finds recorded content" "change:|file:src/main\.rs"

# ════════════════════════════════════════════════════════════════════════════
# Section 5: KG Enrichment
# ════════════════════════════════════════════════════════════════════════════

begin_section "Vault: KG Enrichment"

make_temp_repo "vault-kg"
init_repo --vault

# Create some tracked files
create_file "src/auth.rs" "pub fn authenticate() { }\npub fn verify() { }"
create_file "src/main.rs" "fn main() { authenticate(); }"
add_files "src/auth.rs" "src/main.rs"
record_change "Add auth module" >/dev/null 2>&1 || true

# Run explicit KG enrichment — reports what was enriched
run_cmd atomic vault query enrich
assert_ran "KG enrich from VCS data" "Enriched: .*views.*files.*changes"

# Search for entities
run_cmd atomic vault query search "auth" --json
assert_ran "KG search finds 'auth' content" "auth"

# Query neighbors of a view node — returns a node/edge graph
run_cmd atomic vault query neighbors "view:dev" --json
assert_ran "KG neighbors query for view node" '"nodes"'

# Reindex
run_cmd atomic vault query reindex
assert_ran "KG reindex succeeds" "Indexed [0-9]+ nodes"

# ════════════════════════════════════════════════════════════════════════════
# Section 6: Vault Show and Query
# ════════════════════════════════════════════════════════════════════════════

begin_section "Vault: Show and Query"

# Reuse vault-kg repo from Section 5 (still in REPO_DIR)

# Show a vault entry (keys are vault-relative, without the .vault/ prefix)
run_cmd atomic vault show "skills/atomic-vault.md" --json
assert_ran "vault show returns skill content" "Atomic Vault"

# Embed — the default hash-embed provider needs no external config
run_cmd atomic vault query embed
assert_ran "vault query embed runs the embedding provider" "Embedded [0-9]+ total chunks"

# Ask requires an LLM API key: accept either a real answer (exit 0) or the
# explicit no-key error (non-zero) — anything else is a failure.
run_cmd atomic vault query ask "What files are in this repo?" --json
if [[ $RC -eq 0 ]]; then
    _pass "vault query ask answers with API key configured"
elif echo "$OUT" | grep -qi "API key"; then
    _pass "vault query ask fails cleanly without API key"
else
    _fail "vault query ask" "exit $RC: $(echo "$OUT" | head -2 | tr '\n' ' ')"
fi

# ════════════════════════════════════════════════════════════════════════════
# Section 7: Git Import → KG
# ════════════════════════════════════════════════════════════════════════════

begin_section "Vault: Git Import → KG"

# Skip only this section (not the whole suite — earlier sections already ran)
# when git or the network is unavailable.
if ! command -v git &>/dev/null; then
    _skip "git import KG tests" "git not installed"
elif ! _network_available; then
    _skip "git import KG tests" "no network"
else
    make_temp_repo "vault-git-kg"

    echo "  Cloning hashicorp/go-uuid..."
    GIT_KG_DIR="$(mktemp -d "${TMPDIR:-/tmp}/atomic-git-kg-XXXXXX")"
    _HARNESS_TMPDIRS+=("$GIT_KG_DIR")
    if ! git clone --quiet "https://github.com/hashicorp/go-uuid.git" "$GIT_KG_DIR" 2>/dev/null; then
        _skip "git import KG tests" "clone failed (transient network?)"
    else
        cd "$GIT_KG_DIR"

        run_cmd atomic init --vault
        if [[ $RC -eq 0 ]]; then
            _pass "init --vault in git checkout"
        else
            _fail "init --vault in git checkout" "exit $RC: $(echo "$OUT" | head -2 | tr '\n' ' ')"
        fi

        run_cmd atomic git import
        if [[ $RC -eq 0 ]]; then
            _pass "git import completes with vault"
        else
            _fail "git import completes with vault" "exit $RC: $(echo "$OUT" | tail -2 | tr '\n' ' ')"
        fi

        # Enrich KG from imported data
        run_cmd atomic vault query enrich
        assert_ran "KG enrichment after git import" "Enriched: .*views.*files.*changes"

        # Search should find content from the imported repo
        run_cmd atomic vault query search "uuid" --json
        assert_ran "KG search finds imported content" "uuid"

        # Neighbors query for the imported default view
        run_cmd atomic vault query neighbors "view:main" --json
        assert_ran "KG neighbors query after import" '"nodes"'

        # Reindex vault entries
        run_cmd atomic vault query reindex
        assert_ran "vault reindex after import" "Indexed [0-9]+ nodes"
    fi
fi

# ════════════════════════════════════════════════════════════════════════════
# Section 8: Memory and Materialize
# ════════════════════════════════════════════════════════════════════════════

begin_section "Vault: Memory and Materialize"

make_temp_repo "vault-memory"
init_repo --vault

# Memory list should show the default MEMORY.md
run_cmd atomic vault memory list --json
assert_ran "memory list shows default index" "memory/MEMORY\.md"

# Materialize all vault entries
run_cmd atomic vault materialize
assert_ran "vault materialize all" "Materialized [0-9]+ vault entries"

# Sync (deflate markdown → redb)
run_cmd atomic vault sync
assert_ran "vault sync reports status" "Synced [0-9]+ vault files|up to date"

# Materialize then sync round-trip — vault state should be stable
run_cmd atomic vault sync
assert_ran "vault sync idempotent after materialize" "up to date"

# ════════════════════════════════════════════════════════════════════════════
# Section 9: Vault in a Repo Initialized Without --vault
# ════════════════════════════════════════════════════════════════════════════

begin_section "Vault: Init Without --vault"

make_temp_repo "vault-negative"
init_repo  # no --vault

# `atomic init` always provisions a minimal vault (memory index), so vault
# commands are expected to WORK here — pin that behavior rather than
# accepting anything.
run_cmd atomic vault list --json
assert_ran "vault list works after plain init" "memory/MEMORY\.md"

# No goals were created, so the list is an empty JSON array
run_cmd atomic vault goal list --json
assert_ran "goal list is empty after plain init" '^\[\]$'

# ════════════════════════════════════════════════════════════════════════════
# Summary
# ════════════════════════════════════════════════════════════════════════════

print_summary
