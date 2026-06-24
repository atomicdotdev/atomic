#!/usr/bin/env bash
# 20_sandbox.sh — Concurrent agent sandbox tests.
#
# A sandbox is a private, copy-on-write clone of the working tree. Several
# agents can each work in their own sandbox at once — isolated build
# artifacts, no collisions — while sharing the ONE canonical graph.
#
# Key principles ("views, not forks", taken further):
#   - The working tree is cloned (copy-on-write); the graph is NOT.
#   - There is exactly one pristine. A sandbox shares it via a pointer file.
#   - `atomic` commands run inside a sandbox resolve to the canonical graph,
#     so `status` / `record` work as if it were a normal repo.
#   - Recording in a sandbox lands the change in the canonical graph.
#   - Each sandbox's untracked artifacts are independent.
#
# What this verifies:
#   1. `sandbox create` clones the working tree into a private directory
#   2. The canonical `.atomic/` graph is NOT cloned into the sandbox
#   3. Untracked artifacts (node_modules) ARE cloned (per-agent isolation)
#   4. A sandbox pointer file is written
#   5. `atomic status` works INSIDE the sandbox (resolves to canonical graph)
#   6. Editing + recording inside the sandbox lands in the canonical graph
#   7. The canonical working tree is untouched by sandbox edits
#   8. Two sandboxes have independent artifacts

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Sandbox: create clones the working tree, not the graph"
# ═══════════════════════════════════════════════════════════════════════════

make_temp_repo "sandbox-create"
CANON="$REPO_DIR"
init_repo

create_file "src/main.rs" "fn main() {}"
create_file "Cargo.toml" "[package]"
assert_success "add tracked files" atomic add src/main.rs Cargo.toml
record_change "init" >/dev/null 2>&1 || true

# Untracked build artifact (simulates npm install output)
mkdir -p node_modules/lodash
create_file "node_modules/lodash/index.js" "module.exports = {}"

# Sandboxes live OUTSIDE the canonical repo (as the default dest does), so
# provisioning never recurses into them.
SBROOT="$(mktemp -d "${TMPDIR:-/tmp}/atomic-sandboxes-XXXXXX")"
_HARNESS_TMPDIRS+=("$SBROOT")
SB="$SBROOT/agent-1"
assert_success "sandbox create" atomic sandbox create agent-1 --dest "$SB"

assert_file_exists "tracked file cloned into sandbox" "$SB/src/main.rs"
assert_file_exists "Cargo.toml cloned into sandbox" "$SB/Cargo.toml"
assert_file_exists "untracked artifact cloned into sandbox" "$SB/node_modules/lodash/index.js"
assert_dir_not_exists "graph .atomic NOT cloned into sandbox" "$SB/.atomic"
assert_file_exists "sandbox pointer written" "$SB/.atomic-sandbox"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Sandbox: atomic commands inside resolve to the canonical graph"
# ═══════════════════════════════════════════════════════════════════════════

# Running status from inside the sandbox must succeed even though there is no
# local .atomic/ — it resolves the graph from the canonical repo via the pointer.
assert_success "status works inside sandbox" bash -c "cd '$SB' && '$ATOMIC_BIN' status"

# The tracked file should be clean (it was recorded in the canonical graph).
assert_output_not_contains \
    "tracked file is clean inside sandbox" \
    "src/main.rs" \
    bash -c "cd '$SB' && '$ATOMIC_BIN' status --short"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Sandbox: record inside lands in the canonical graph"
# ═══════════════════════════════════════════════════════════════════════════

# Edit a file inside the sandbox and record from there.
overwrite_file "$SB/src/main.rs" "fn main() { println!(\"from sandbox\"); }"

RECORD_OUT="$(cd "$SB" && "$ATOMIC_BIN" record -m "edit from sandbox" 2>&1)" || true

# The change must be visible in the canonical repo's log.
assert_output_contains \
    "sandbox change appears in canonical log" \
    "edit from sandbox" \
    bash -c "cd '$CANON' && '$ATOMIC_BIN' log"

# The canonical working-tree file on disk is untouched (the edit was isolated
# to the sandbox clone; the change lives in the graph until materialized).
assert_file_content \
    "canonical working file unchanged by sandbox edit" \
    "$CANON/src/main.rs" \
    "fn main() {}"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Sandbox: two sandboxes have independent artifacts"
# ═══════════════════════════════════════════════════════════════════════════

SB2="$SBROOT/agent-2"
assert_success "second sandbox create" atomic sandbox create agent-2 --dest "$SB2"

# Agent 1 installs a dep that agent 2 does not.
mkdir -p "$SB/node_modules/only-in-1"
create_file "$SB/node_modules/only-in-1/x.js" "1"

assert_file_exists "agent-1 has its own dep" "$SB/node_modules/only-in-1/x.js"
assert_file_not_exists "agent-2 does NOT see agent-1's dep" "$SB2/node_modules/only-in-1/x.js"

# Both sandboxes still share the same canonical graph (one pristine).
assert_file_exists "canonical pristine is single source" "$CANON/.atomic/pristine.redb"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Sandbox: --from creates a per-agent draft view"
# ═══════════════════════════════════════════════════════════════════════════

SB3="$SBROOT/agent-3"
assert_success "sandbox create --from dev" \
    bash -c "cd '$CANON' && '$ATOMIC_BIN' sandbox create agent-3 --dest '$SB3' --from dev"

# The new draft view exists in the canonical repo.
assert_output_contains \
    "draft view 'agent-3' created" \
    "agent-3" \
    bash -c "cd '$CANON' && '$ATOMIC_BIN' view list"

# Recording in the sandbox lands in the agent-3 draft, not in dev.
overwrite_file "$SB3/src/main.rs" "fn main() { println!(\"draft work\"); }"
(cd "$SB3" && "$ATOMIC_BIN" record -m "work in draft" >/dev/null 2>&1) || true

assert_output_contains \
    "draft change visible on agent-3 view" \
    "work in draft" \
    bash -c "cd '$CANON' && '$ATOMIC_BIN' log --view agent-3"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Sandbox: seal produces a valid OCI image layout"
# ═══════════════════════════════════════════════════════════════════════════

SEAL_OUT="$SBROOT/seal-dev"
assert_success "seal dev view" \
    bash -c "cd '$CANON' && '$ATOMIC_BIN' sandbox seal dev -o '$SEAL_OUT' --entrypoint /app/run --env PORT=8080"

assert_file_exists "oci-layout written" "$SEAL_OUT/oci-layout"
assert_file_exists "index.json written" "$SEAL_OUT/index.json"
assert_dir_exists "blobs/sha256 written" "$SEAL_OUT/blobs/sha256"
assert_output_contains "oci-layout has version" "imageLayoutVersion" cat "$SEAL_OUT/oci-layout"

# Every blob filename must equal the sha256 of its contents (content-addressed).
BLOB_OK=1
for blob in "$SEAL_OUT"/blobs/sha256/*; do
    name="$(basename "$blob")"
    actual="$(shasum -a 256 "$blob" | awk '{print $1}')"
    [ "$name" = "$actual" ] || BLOB_OK=0
done
if [ "$BLOB_OK" = "1" ]; then
    _pass "every blob is content-addressed (filename == sha256)"
else
    _fail "every blob is content-addressed (filename == sha256)" "a blob filename did not match its sha256"
fi

# The manifest the index points to must reference exactly one layer (flattened).
INDEX_MANIFEST="$(grep -o 'sha256:[0-9a-f]\{64\}' "$SEAL_OUT/index.json" | head -1 | cut -d: -f2)"
assert_file_exists "index references a manifest blob" "$SEAL_OUT/blobs/sha256/$INDEX_MANIFEST"
assert_output_contains \
    "sealed image carries provenance annotation" \
    "org.atomic.view" \
    cat "$SEAL_OUT/blobs/sha256/$INDEX_MANIFEST"

# Seal of the SHARED dev view must contain dev's recorded GRAPH content (not
# the working tree on disk). Extract the single layer and inspect it.
SEAL_LAYER="$(python3 -c "import json,sys; m=json.load(open('$SEAL_OUT/blobs/sha256/$INDEX_MANIFEST')); print(m['layers'][0]['digest'].split(':')[1])")"
SEAL_EXTRACT="$SBROOT/seal-dev-extract"
mkdir -p "$SEAL_EXTRACT"
tar -xzf "$SEAL_OUT/blobs/sha256/$SEAL_LAYER" -C "$SEAL_EXTRACT" 2>/dev/null || true

assert_file_exists "sealed dev layer contains src/main.rs" "$SEAL_EXTRACT/src/main.rs"
assert_file_exists "sealed dev layer contains Cargo.toml" "$SEAL_EXTRACT/Cargo.toml"

# dev's graph state for src/main.rs is the change recorded from the agent-1
# sandbox earlier ("from sandbox"), proving seal reads recorded view state
# rather than the canonical working copy (which still has the original).
assert_file_content \
    "sealed dev content is the recorded graph state" \
    "$SEAL_EXTRACT/src/main.rs" \
    'fn main() { println!("from sandbox"); }'

assert_file_content \
    "canonical working copy still has original (graph != disk)" \
    "$CANON/src/main.rs" \
    "fn main() {}"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Sandbox: stage produces a two-layer (base + delta) image"
# ═══════════════════════════════════════════════════════════════════════════

# agent-3 has the "work in draft" change on top of dev — stage it as base+delta.
STAGE_OUT="$SBROOT/stage-agent-3"
assert_success "stage agent-3 over dev" \
    bash -c "cd '$CANON' && '$ATOMIC_BIN' sandbox stage agent-3 --base dev -o '$STAGE_OUT'"

assert_file_exists "staged oci-layout written" "$STAGE_OUT/oci-layout"

# The staged manifest must reference TWO layers (base + delta).
STAGE_MANIFEST="$(grep -o 'sha256:[0-9a-f]\{64\}' "$STAGE_OUT/index.json" | head -1 | cut -d: -f2)"
LAYER_COUNT="$(grep -o 'tar+gzip' "$STAGE_OUT/blobs/sha256/$STAGE_MANIFEST" | wc -l | tr -d ' ')"
if [ "$LAYER_COUNT" = "2" ]; then
    _pass "staged image has two layers (base + delta)"
else
    _fail "staged image has two layers (base + delta)" "expected 2 tar+gzip layers, got $LAYER_COUNT"
fi

assert_output_contains \
    "staged image records base-view provenance" \
    "org.atomic.base-view" \
    cat "$STAGE_OUT/blobs/sha256/$STAGE_MANIFEST"

# ═══════════════════════════════════════════════════════════════════════════

print_summary
