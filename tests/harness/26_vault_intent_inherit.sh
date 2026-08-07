#!/usr/bin/env bash
# 26_vault_intent_inherit.sh — Inherited intents/memories become queryable.
#
# Spec for the clone → `atomic vault sync` → queryable contract (the flow
# Lee described for option 1): when User B inherits User A's vault content
# through changes, B's local index must make it addressable — not just
# present as bytes.
#
# Regression guard for the manifest-index gap: intent entries used to reach
# redb through the ingestion path (`update_manifest_for_store` leaves intent
# summaries to "higher-level methods"), so `vault list` showed the entry but
# `intent list` read an empty manifest and bare-number references resolved
# through a freshly derived — wrong — prefix.
#
# Invariants tested on the receiving side, after `atomic vault sync`:
#
#   1. `intent list` shows the inherited intent under its ORIGINAL human key
#   2. `intent show <human-key>` and `intent show <bare-seq>` both resolve
#   3. `memory list` shows the inherited memory
#   4. A locally created intent allocates the NEXT seq — it never re-issues
#      an inherited human key (allocator advanced past inherited entries)
#   5. A second `vault sync` is idempotent — nothing duplicated or lost

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 26_vault_intent_inherit${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

# ── Helper: copy all change files from one repo to another ──────────────

copy_all_changes() {
    local src_changes="$1/.atomic/changes"
    local dst_changes="$2/.atomic/changes"

    find "$src_changes" -name "*.change" -type f 2>/dev/null | while read -r f; do
        local rel="${f#$src_changes/}"
        local dst_dir="$dst_changes/$(dirname "$rel")"
        mkdir -p "$dst_dir"
        cp "$f" "$dst_dir/"
    done
}

get_change_hashes() {
    find "$1/.atomic/changes" -name "*.change" -type f 2>/dev/null \
        | sort | xargs -I{} basename {} .change
}

# Extract the first PROJECT::author::seq human key from `intent list` output.
first_human_key() {
    atomic intent list 2>/dev/null | grep -oE '[^ ]+::[^ ]+::[0-9]+' | head -1
}

# ════════════════════════════════════════════════════════════════════════════
begin_section "User A: create intent + memory, record"
# ════════════════════════════════════════════════════════════════════════════

make_temp_repo "vault-inherit-A"
REPO_A="$REPO_DIR"
init_repo --vault

atomic intent new "Inherited from A" >/dev/null 2>&1
A_KEY="$(first_human_key)"
if [[ -n "$A_KEY" ]]; then
    _pass "User A: intent created ($A_KEY)"
else
    _fail "User A: intent created" "no human key in intent list"
fi
A_SEQ="${A_KEY##*::}"

atomic memory new --kind lesson --id inherited-note \
    --text "Vault entries must survive inheritance." >/dev/null 2>&1
assert_output_contains "User A: memory listed" "inherited-note" \
    atomic memory list

record_change "vault: intent + memory" -a >/dev/null 2>&1 || true

A_HASHES="$(get_change_hashes "$REPO_A")"
if [[ -n "$A_HASHES" ]]; then
    _pass "User A: recorded vault content"
else
    _fail "User A: recorded vault content" "no change files produced"
fi

# ════════════════════════════════════════════════════════════════════════════
begin_section "User B: inherit changes, bootstrap, vault sync"
# ════════════════════════════════════════════════════════════════════════════

make_temp_repo "vault-inherit-B"
REPO_B="$REPO_DIR"
init_repo --no-vault

copy_all_changes "$REPO_A" "$REPO_B"
cd "$REPO_B"
for hash in $A_HASHES; do
    atomic insert "$hash" --view dev >/dev/null 2>&1 || true
done
atomic view switch dev >/dev/null 2>&1 || true
assert_file_exists "User B: inherited memory materialized" \
    ".vault/memory/inherited-note.md"

# Bootstrap the vault tables from the materialized files (clone does this
# automatically; we simulated the transfer with insert), then sync.
atomic vault init >/dev/null 2>&1 || true
atomic vault sync >/dev/null 2>&1 || true

# ════════════════════════════════════════════════════════════════════════════
begin_section "User B: inherited intent + memory are queryable"
# ════════════════════════════════════════════════════════════════════════════

assert_output_contains "User B: intent list shows inherited key" "$A_KEY" \
    atomic intent list

assert_output_contains "User B: intent show <human-key> resolves" \
    "Inherited from A" atomic intent show "$A_KEY"

assert_output_contains "User B: intent show <bare-seq> resolves" \
    "Inherited from A" atomic intent show "$A_SEQ"

assert_output_contains "User B: memory list shows inherited memory" \
    "inherited-note" atomic memory list

# ════════════════════════════════════════════════════════════════════════════
begin_section "User B: local create allocates next seq; sync is idempotent"
# ════════════════════════════════════════════════════════════════════════════

atomic intent new "Created on B" >/dev/null 2>&1

B_LIST="$(atomic intent list 2>/dev/null)"
B_COUNT="$(echo "$B_LIST" | grep -cE '[^ ]+::[^ ]+::[0-9]+' || true)"
if [[ "$B_COUNT" == "2" ]]; then
    _pass "User B: both intents listed after local create"
else
    _fail "User B: both intents listed" "expected 2 keys, got $B_COUNT: $B_LIST"
fi

if [[ "$(echo "$B_LIST" | grep -oE '[^ ]+::[^ ]+::[0-9]+' | sort -u | wc -l | tr -d ' ')" == "2" ]]; then
    _pass "User B: local intent got a fresh key (no collision with $A_KEY)"
else
    _fail "User B: allocator collision" "duplicate human keys: $B_LIST"
fi

atomic vault sync >/dev/null 2>&1 || true
B_COUNT_AFTER="$(atomic intent list 2>/dev/null | grep -cE '[^ ]+::[^ ]+::[0-9]+' || true)"
if [[ "$B_COUNT_AFTER" == "2" ]]; then
    _pass "User B: second vault sync is idempotent"
else
    _fail "User B: second vault sync" "expected 2 keys, got $B_COUNT_AFTER"
fi

# ════════════════════════════════════════════════════════════════════════════

print_summary
