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

    find "$src_changes" -name "*.change" -type f -print0 | while IFS= read -r -d '' f; do
        local rel="${f#$src_changes/}"
        local dst_dir="$dst_changes/$(dirname "$rel")"
        mkdir -p "$dst_dir" || exit 1
        cp "$f" "$dst_dir/" || exit 1
        cmp -s "$f" "$dst_dir/$(basename "$f")" || exit 1
    done
}

get_change_hashes() {
    local log
    if ! log="$(cd "$1" && atomic log --format json --reverse 2>&1)"; then
        printf 'cannot read source history: %s\n' "$log" >&2
        return 1
    fi
    printf '%s\n' "$log" | python3 -c '
import json,sys
entries=json.load(sys.stdin)
assert entries and all(isinstance(e.get("hash"),str) and len(e["hash"]) == 52 for e in entries)
print("\n".join(e["hash"] for e in entries))'
}

# ════════════════════════════════════════════════════════════════════════════
begin_section "User A: create intent + memory, record"
# ════════════════════════════════════════════════════════════════════════════

make_temp_repo "vault-inherit-A"
REPO_A="$REPO_DIR"
init_repo --vault

require_capture "User A: create intent" atomic intent new "Inherited from A"
A_KEY="$(printf '%s\n' "$COMMAND_OUTPUT" | sed -n 's/.*Created intent: \([A-Za-z0-9:_-]*\).*/\1/p' | head -1)"
A_INTENT_FILE="$(printf '%s\n' "$COMMAND_OUTPUT" | sed -n 's|.*file: \.vault/||p' | head -1)"
if [[ -n "$A_KEY" ]]; then
    _pass "User A: intent created ($A_KEY)"
else
    _fail "User A: intent created" "no human key in intent list"
    print_summary || true
    exit 1
fi
A_SEQ="${A_KEY##*::}"
if [[ ! "$A_SEQ" =~ ^[0-9]+$ || ! "$A_INTENT_FILE" =~ ^intents/[0-9A-HJKMNP-TV-Z]{26}/intent\.md$ ]]; then
    _fail "User A: intent identity and path" "key=$A_KEY file=$A_INTENT_FILE"
    print_summary || true
    exit 1
fi
assert_file_exists "User A: intent file exists" ".vault/$A_INTENT_FILE"

require_success "User A: create memory" atomic memory new --kind lesson --id inherited-note \
    --text "Vault entries must survive inheritance."
assert_output_contains "User A: memory listed" "inherited-note" \
    atomic memory list

require_success "User A: record vault content" record_change "vault: intent + memory" -a

require_capture "User A: read ordered change hashes" get_change_hashes "$REPO_A"
A_HASHES="$COMMAND_OUTPUT"
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

require_success "copy changes byte-for-byte" copy_all_changes "$REPO_A" "$REPO_B"
cd "$REPO_B"
for hash in $A_HASHES; do
    require_success "User B: insert $hash" atomic insert "$hash" --view dev
done
require_success "User B: select dev" atomic view switch dev
assert_file_exists "User B: inherited memory materialized" \
    ".vault/memory/inherited-note.md"
assert_success "User B: memory bytes match source" cmp -s "$REPO_A/.vault/memory/inherited-note.md" "$REPO_B/.vault/memory/inherited-note.md"
assert_success "User B: intent bytes match source" cmp -s "$REPO_A/.vault/$A_INTENT_FILE" "$REPO_B/.vault/$A_INTENT_FILE"

# Bootstrap the vault tables from the materialized files (clone does this
# automatically; we simulated the transfer with insert), then sync.
require_success "User B: initialize vault" atomic vault init
require_success "User B: sync vault" atomic vault sync

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

require_capture "User B: create local intent" atomic intent new "Created on B"
B_KEY="$(printf '%s\n' "$COMMAND_OUTPUT" | sed -n 's/.*Created intent: \([A-Za-z0-9:_-]*\).*/\1/p' | head -1)"
if [[ -z "$B_KEY" || "$B_KEY" == "$A_KEY" || ! "${B_KEY##*::}" =~ ^[0-9]+$ ]]; then
    _fail "User B: local intent gets a distinct numeric key" "inherited=$A_KEY created=$B_KEY"
    print_summary || true
    exit 1
fi

require_capture "User B: list intents" atomic intent list --json
B_LIST="$COMMAND_OUTPUT"
B_COUNT="$(printf '%s\n' "$B_LIST" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
if [[ "$B_COUNT" == "2" ]]; then
    _pass "User B: both intents listed after local create"
else
    _fail "User B: both intents listed" "expected 2 keys, got $B_COUNT: $B_LIST"
fi

assert_success "User B: both unique intent keys and titles persist" python3 -c \
    'import json,sys; rows=json.loads(sys.argv[1]); assert len(rows)==2; assert {(r["id"],r["title"]) for r in rows} == {(sys.argv[2],"Inherited from A"),(sys.argv[3],"Created on B")}' "$B_LIST" "$A_KEY" "$B_KEY"
if [[ "${B_KEY##*::}" -eq $((10#$A_SEQ + 1)) ]]; then
    _pass "User B: local intent gets next sequence"
else
    _fail "User B: local intent gets next sequence" "inherited=$A_KEY created=$B_KEY"
fi

require_success "User B: sync vault again" atomic vault sync
require_capture "User B: list intents after second sync" atomic intent list --json
assert_success "User B: second vault sync is idempotent" python3 -c \
    'import json,sys; before=json.loads(sys.argv[1]); after=json.loads(sys.argv[2]); assert sorted((r["id"],r["title"]) for r in before)==sorted((r["id"],r["title"]) for r in after)' "$B_LIST" "$COMMAND_OUTPUT"

# ════════════════════════════════════════════════════════════════════════════

print_summary
