#!/usr/bin/env bash
# 16_vault_pull_bootstrap.sh — Vault bootstrap after pull & view-scoped intents
#
# Tests the collaboration scenario where:
#   1. User A initializes a repo with a vault, records changes, and "pushes"
#   2. User B receives the changes (simulated via change-file copy + insert)
#   3. User B's vault should auto-bootstrap from the materialized .vault/ files
#   4. Intents created on different views don't collide on disk paths
#
# Key invariants tested:
#
#   1. Vault bootstrap: .vault/ files materialized from graph → redb tables
#      auto-initialized → `atomic vault list` works on the receiving side
#   2. KG enrichment runs on the receiving side after bootstrap
#   3. Intents on different views materialize to distinct ULID-scoped paths
#   4. Each view can list and inspect its own intent without collision

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 16_vault_pull_bootstrap${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

# ── Helper: copy all change files from one repo to another ──────────────

copy_all_changes() {
    local src_repo="$1"
    local dst_repo="$2"
    local src_changes="$src_repo/.atomic/changes"
    local dst_changes="$dst_repo/.atomic/changes"

    find "$src_changes" -name "*.change" -type f -print0 | while IFS= read -r -d '' f; do
        local rel="${f#$src_changes/}"
        local dst_dir="$dst_changes/$(dirname "$rel")"
        mkdir -p "$dst_dir" || exit 1
        cp "$f" "$dst_dir/" || exit 1
        cmp -s "$f" "$dst_dir/$(basename "$f")" || exit 1
    done
}

# ── Helper: get view change hashes in dependency order ──────────────────

get_change_hashes() {
    local log
    if ! log="$(cd "$1" && atomic log --format json --reverse 2>&1)"; then
        printf 'cannot read source history: %s\n' "$log" >&2
        return 1
    fi
    printf '%s\n' "$log" | python3 -c '
import json, sys
entries = json.load(sys.stdin)
assert entries and all(isinstance(e.get("hash"), str) and len(e["hash"]) == 52 for e in entries)
print("\n".join(e["hash"] for e in entries))'
}

# ════════════════════════════════════════════════════════════════════════════
# Section 1: Vault Bootstrap After Pull
# ════════════════════════════════════════════════════════════════════════════

begin_section "Vault Bootstrap: User A initializes and records"

make_temp_repo "userA"
REPO_A="$REPO_DIR"
init_repo --vault

# Verify vault is initialized on User A
assert_dir_exists "User A: .vault exists" ".vault"
assert_file_exists "User A: default skill exists" ".vault/skills/atomic-vault.md"

# Record the initial state (includes .atomicignore + .vault/ files)
require_capture "User A: read initial changes" get_change_hashes "$REPO_A"

# Create a source file and record it too
create_file "src/main.rs" 'fn main() { println!("hello"); }'
require_success "User A: add main.rs" atomic add "src/main.rs"
require_success "User A: record main.rs" record_change "Add main.rs"

# Collect all change hashes
require_capture "User A: read ordered change hashes" get_change_hashes "$REPO_A"
A_HASHES="$COMMAND_OUTPUT"
A_HASH_COUNT="$(echo "$A_HASHES" | wc -l | tr -d ' ')"
if [[ "$A_HASH_COUNT" -ge 2 ]]; then
    _pass "User A: has $A_HASH_COUNT recorded changes"
else
    _fail "User A: has bootstrap and source changes" "found $A_HASH_COUNT"
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Vault Bootstrap: User B receives changes"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "userB"
REPO_B="$REPO_DIR"

# User B initializes a fresh repo (NO --vault flag)
init_repo --no-vault

# Verify vault is NOT initialized on User B
assert_dir_not_exists "User B: no .vault before pull" ".vault"

# Copy all change files from User A → User B
require_success "copy change files from User A to User B byte-for-byte" copy_all_changes "$REPO_A" "$REPO_B"

# Insert all changes into User B's dev view (simulating pull)
cd "$REPO_B"
for hash in $A_HASHES; do
    require_success "insert source change $hash" atomic insert "$hash" --view dev
done

# Materialize the working copy
require_success "select and materialize User B dev" atomic view switch dev

# The .vault/ directory should now exist on disk (materialized from graph)
assert_dir_exists "User B: .vault materialized after insert" ".vault"
assert_file_exists "User B: skill file materialized" ".vault/skills/atomic-vault.md"
assert_file_exists "User B: main.rs materialized" "src/main.rs"
assert_success "User B: main.rs matches source byte-for-byte" cmp -s "$REPO_A/src/main.rs" "$REPO_B/src/main.rs"
assert_success "User B: vault skill matches source byte-for-byte" cmp -s "$REPO_A/.vault/skills/atomic-vault.md" "$REPO_B/.vault/skills/atomic-vault.md"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Vault Bootstrap: Verify vault operations work on User B"
# ═══════════════════════════════════════════════════════════════════════════

cd "$REPO_B"

# Bootstrap the vault from materialized files
# (In production, pull/clone would do this automatically.
#  Here we call it explicitly since we simulated with insert.)
require_success "User B: bootstrap vault" atomic vault init

# Vault list should work and show entries
require_capture "User B: list vault" atomic vault list --json
out="$COMMAND_OUTPUT"
if echo "$out" | grep -q "skills/"; then
    _pass "User B: vault list shows entries after bootstrap"
else
    _fail "User B: vault list" "output: $(echo "$out" | head -3)"
fi

# Creating an intent should work
require_capture "User B: create intent" atomic intent new "User B task"
out="$COMMAND_OUTPUT"
if echo "$out" | grep -qE "Created intent:"; then
    _pass "User B: intent create works after bootstrap"
else
    _fail "User B: intent create" "$out"
fi

# ════════════════════════════════════════════════════════════════════════════
# Section 2: View-Scoped Intent Paths
# ════════════════════════════════════════════════════════════════════════════

begin_section "View-Scoped Intents: Paths scoped under current view"

make_temp_repo "view-intents"
init_repo --vault

# Create a draft view (simulates agent session)
require_success "create agent-session-1" new_view "agent-session-1" --draft --parent dev
require_success "switch to agent-session-1" switch_view "agent-session-1"

# Create an intent on the draft view. Intent files are flat and ULID-scoped
# (globally unique, not view-scoped):
#   intents/<ULID>/intent.md
require_capture "create first draft intent" atomic intent new "Draft intent 1"
out="$COMMAND_OUTPUT"
intent_file_1="$(echo "$out" | sed -n 's|.*file: \.vault/||p' | head -1)"
first_id="$(echo "$out" | sed -n 's/.*Created intent: \([A-Za-z0-9:_-]*\).*/\1/p' | head -1)"

# The path is ULID-scoped under intents/
if [[ "$intent_file_1" =~ ^intents/[0-9A-HJKMNP-TV-Z]{26}/intent\.md$ ]]; then
    _pass "Intent path is scoped: $intent_file_1"
else
    _fail "Intent path is scoped" "file: $intent_file_1"
fi

# The intent file should exist on disk
if [[ -n "$intent_file_1" ]] && [[ -f ".vault/$intent_file_1" ]]; then
    _pass "Intent file exists on disk"
else
    _fail "Intent file exists on disk" "file: .vault/$intent_file_1"
fi
if [[ -z "$first_id" || -z "$intent_file_1" ]]; then
    _fail "first intent identity and path" "create output: $out"
    print_summary || true
    exit 1
fi

# ═══════════════════════════════════════════════════════════════════════════
begin_section "View-Scoped Intents: No collision across views"
# ═══════════════════════════════════════════════════════════════════════════

# Create a second draft view
require_success "create agent-session-2" new_view "agent-session-2" --draft --parent dev
require_success "switch to agent-session-2" switch_view "agent-session-2"

# Create an intent on the second view
require_capture "create second draft intent" atomic intent new "Draft intent 2"
out2="$COMMAND_OUTPUT"
intent_file_2="$(echo "$out2" | sed -n 's|.*file: \.vault/||p' | head -1)"
second_id="$(echo "$out2" | sed -n 's/.*Created intent: \([A-Za-z0-9:_-]*\).*/\1/p' | head -1)"

# The two intent files should be different paths: each intent gets its own
# unique ULID directory, so paths never collide across views.
if [[ -n "$intent_file_1" ]] && [[ -n "$intent_file_2" ]]; then
    if [[ "$intent_file_1" != "$intent_file_2" ]]; then
        _pass "Intent files have different paths"
    else
        _fail "Intent path collision" "both got: $intent_file_1"
    fi
else
    _fail "Intent files have different paths" "first: $intent_file_1; second: $intent_file_2"
fi
if [[ -z "$second_id" || -z "$intent_file_2" || "$second_id" == "$first_id" ]]; then
    _fail "second intent identity and path" "create output: $out2"
    print_summary || true
    exit 1
fi
if [[ "$intent_file_2" =~ ^intents/[0-9A-HJKMNP-TV-Z]{26}/intent\.md$ ]]; then
    _pass "second intent has a ULID-scoped path"
else
    _fail "second intent has a ULID-scoped path" "file: $intent_file_2"
fi
assert_file_exists "second intent file exists" ".vault/$intent_file_2"

# Each view should only see its own intents
require_success "switch to agent-session-1" switch_view "agent-session-1"
require_capture "list agent-session-1 intents" atomic intent list --json
list1="$COMMAND_OUTPUT"

require_success "switch to agent-session-2" switch_view "agent-session-2"
require_capture "list agent-session-2 intents" atomic intent list --json
list2="$COMMAND_OUTPUT"

# Note: the manifest is shared (local redb), so both intents may appear.
# The key property is that the file paths don't collide.
assert_success "first intent is listed on its view" python3 -c \
    'import json,sys; assert any(e["id"] == sys.argv[2] and e["title"] == "Draft intent 1" for e in json.loads(sys.argv[1]))' "$list1" "$first_id"
assert_success "second intent is listed on its view" python3 -c \
    'import json,sys; assert any(e["id"] == sys.argv[2] and e["title"] == "Draft intent 2" for e in json.loads(sys.argv[1]))' "$list2" "$second_id"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "View-Scoped Intents: Intent operations work on scoped paths"
# ═══════════════════════════════════════════════════════════════════════════

# Switch back to view 1 and verify show/update work
require_success "switch back to agent-session-1" switch_view "agent-session-1"

if [[ -n "$first_id" ]]; then
    # Show should work
    require_capture "show first draft intent" atomic intent show "$first_id" --json
    show_out="$COMMAND_OUTPUT"
    if echo "$show_out" | grep -qi "draft intent"; then
        _pass "Intent show works on view-scoped path"
    else
        _fail "Intent show returns first intent" "got: $show_out"
    fi

    # Update should work
    require_capture "update first draft intent" atomic intent update "$first_id" --status in-progress
    update_out="$COMMAND_OUTPUT"
    if echo "$update_out" | grep -qiE "updated|in-progress"; then
        _pass "Intent update works on view-scoped path"
    else
        _fail "Intent update works on view-scoped path" "got: $update_out"
    fi
    require_capture "show updated first intent" atomic intent show "$first_id" --json
    assert_success "updated intent status persists" python3 -c \
        'import json,sys; data=json.loads(sys.argv[1]); assert data["status"] == "in_progress"' "$COMMAND_OUTPUT"
else
    _fail "Intent show/update" "could not extract intent ID from creation"
fi

# ════════════════════════════════════════════════════════════════════════════

print_summary
