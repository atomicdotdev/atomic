# SEMANTIC-MERGE.md — Token-Level Conflict Resolution & Push/Pull

> **Status**: Planning  
> **Created**: 2025-07-14  
> **Last Updated**: 2025-07-14  
> **Prerequisites**: REFACTOR-VIEWS.md (complete), REFACTOR-TYPES.md (in progress)  

## Table of Contents

- [Motivation](#motivation)
- [Current State](#current-state)
- [Architecture](#architecture)
- [Part 1: Semantic Merge Engine](#part-1-semantic-merge-engine)
  - [Phase S1: Wire CRDT Layer into Materialization](#phase-s1-wire-crdt-layer-into-materialization)
  - [Phase S2: Token-Level Conflict Detection](#phase-s2-token-level-conflict-detection)
  - [Phase S3: Automatic Token Merge](#phase-s3-automatic-token-merge)
  - [Phase S4: Conflict Markers for True Conflicts](#phase-s4-conflict-markers-for-true-conflicts)
- [Part 2: Push/Pull with Merge](#part-2-pushpull-with-merge)
  - [Phase P1: Server-Side Change Integration](#phase-p1-server-side-change-integration)
  - [Phase P2: Client Pull with Divergence](#phase-p2-client-pull-with-divergence)
  - [Phase P3: Push Rejection and Resolution](#phase-p3-push-rejection-and-resolution)
- [Risk Register](#risk-register)
- [Verification Strategy](#verification-strategy)

---

## Motivation

In the ambient graph model (REFACTOR-VIEWS.md), all edges live in a single
canonical GRAPH.  Views are change-set filters.  Inserts between views are
O(1) metadata operations.  This works perfectly for a **single repository**.

The problem surfaces in two places:

### 1. Two agents modify the same content

Agent A changes `host: "localhost"` to `host: "prod.example.com"`.
Agent B changes `port: 3000` to `port: 8080`.  Same line, different tokens.

At the **graph level**, both agents deleted the original line vertex and
added a replacement.  When both changes are inserted into the same view,
the graph has two competing live vertices at the same position.  The
materialization concatenates both — producing broken output.

At the **token level**, these are independent edits.  The CRDT semantic
layer (Trunk → Branch → Leaf) can see that Agent A touched the `host`
leaf and Agent B touched the `port` leaf.  No conflict.  The merge is:
`{ host: "prod.example.com", port: 8080 }`.

### 2. Push to a server with other developers' changes

Developer A pushes changes to the server.  Developer B has been working
locally and pushes later.  The server's GRAPH now has both sets of edges.
Without semantic merging, the server reports line-level conflicts that
could have been auto-resolved at the token level.

The semantic merge engine turns most "conflicts" into automatic merges,
making multi-developer push/pull feel collaborative rather than adversarial.

---

## Current State

### What exists

| Component | Status | Location |
|-----------|--------|----------|
| **CRDT model** (Trunk/Branch/Leaf) | Defined | `atomic-core/src/crdt/` |
| **FileOps generation** during record | Working | `atomic-core/src/record/workflow/crdt/` |
| **CRDT table population** during apply | Working | `atomic-core/src/apply/file_ops.rs` |
| **Token-level diff** | Working | `atomic-core/src/diff/token/` |
| **Graph-level conflict detection** | Working | `atomic-core/src/output/alive/` (SCC ordering) |
| **Semantic diff display** | Working | `atomic-cli/src/commands/diff/` |
| **Push/pull transport** | Working | `atomic-remote/src/http/` |
| **ViewGraph filtering** | Working | `atomic-core/src/pristine/view_graph.rs` |

### What's missing

| Component | Status | What's needed |
|-----------|--------|---------------|
| **Token-level merge** | Not started | When two vertices conflict, check if their leaf-level edits are independent |
| **CRDT-aware materialization** | Not started | Use Trunk/Branch/Leaf to resolve conflicts during `materialize_view` |
| **Merge outcome types** | Not started | `Merged`, `ConflictMarker`, `AutoResolved` result types |
| **Server-side merge on push** | Not started | When push introduces conflicting changes, run semantic merge |
| **Pull divergence detection** | Not started | Detect when local and remote have diverged, trigger merge |
| **Conflict marker format** | Not started | How to represent unresolvable conflicts in the working copy |

---

## Architecture

### The Three Layers

```
┌─────────────────────────────────────────────────────────────────────┐
│                        Materialization                               │
│                                                                     │
│  materialize_view() → retrieve_graph() → compute_order()            │
│       │                                                             │
│       ▼                                                             │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │ CONFLICT DETECTED: Two vertices at same graph position      │    │
│  │                                                             │    │
│  │  V2: "let x = 2;"  (from Agent A)                          │    │
│  │  V3: "let x = 3;"  (from Agent B)                          │    │
│  └─────────────────────────┬───────────────────────────────────┘    │
│                             │                                       │
│                             ▼                                       │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │ SEMANTIC MERGE ENGINE                                       │    │
│  │                                                             │    │
│  │  1. Look up the Branch (line) that V2 and V3 replaced       │    │
│  │  2. Get the original Branch's Leaf sequence                 │    │
│  │  3. Get V2's Leaf sequence (Agent A's version)              │    │
│  │  4. Get V3's Leaf sequence (Agent B's version)              │    │
│  │  5. Three-way diff at the Leaf (token) level:               │    │
│  │                                                             │    │
│  │     Original: [host, :, "localhost", ,, port, :, 3000]      │    │
│  │     Agent A:  [host, :, "prod.ex..", ,, port, :, 3000]      │    │
│  │     Agent B:  [host, :, "localhost", ,, port, :, 8080]      │    │
│  │                                                             │    │
│  │  6. Token 3 changed by A only → take A's value              │    │
│  │     Token 7 changed by B only → take B's value              │    │
│  │     No token changed by both → AUTO-MERGE                   │    │
│  │                                                             │    │
│  │  Result: [host, :, "prod.ex..", ,, port, :, 8080]           │    │
│  └─────────────────────────┬───────────────────────────────────┘    │
│                             │                                       │
│                             ▼                                       │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │ OUTPUT                                                      │    │
│  │                                                             │    │
│  │  AutoMerged → write merged content to working copy          │    │
│  │  Conflict  → write conflict markers (both alternatives)     │    │
│  │  Clean     → write content as-is (no competing vertices)    │    │
│  └─────────────────────────────────────────────────────────────┘    │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

### Merge Outcome Types

```rust
/// The result of attempting to merge two competing graph vertices.
pub enum MergeOutcome {
    /// No conflict — only one vertex is alive at this position.
    Clean(Vec<u8>),

    /// The semantic layer automatically resolved the conflict.
    /// Both agents' edits were to different tokens on the same line.
    AutoMerged {
        /// The merged content (combining both agents' token edits).
        content: Vec<u8>,
        /// Which changes contributed to the merge.
        sources: Vec<NodeId>,
    },

    /// True conflict — both agents edited the same token(s).
    /// Cannot be auto-resolved.
    Conflict {
        /// The original content (common ancestor).
        base: Vec<u8>,
        /// Agent A's version.
        left: Vec<u8>,
        /// Agent B's version.
        right: Vec<u8>,
        /// Which changes are in conflict.
        left_source: NodeId,
        right_source: NodeId,
    },
}
```

### How Token-Level Merge Works

The CRDT model gives us stable identifiers for every token:

```
Branch B1 (line):  "const CONFIG = { host: \"localhost\", port: 3000 };"

Leaves:
  L0: "const"       (kind: Keyword)
  L1: " "           (kind: Whitespace)
  L2: "CONFIG"      (kind: Identifier)
  L3: " "           (kind: Whitespace)
  L4: "="           (kind: Operator)
  L5: " "           (kind: Whitespace)
  L6: "{"           (kind: Punctuation)
  L7: " "           (kind: Whitespace)
  L8: "host"        (kind: Identifier)
  L9: ":"           (kind: Punctuation)
  L10: " "          (kind: Whitespace)
  L11: "\"localhost\""  (kind: StringLiteral)
  L12: ","          (kind: Punctuation)
  L13: " "          (kind: Whitespace)
  L14: "port"       (kind: Identifier)
  L15: ":"          (kind: Punctuation)
  L16: " "          (kind: Whitespace)
  L17: "3000"       (kind: NumberLiteral)
  L18: " "          (kind: Whitespace)
  L19: "}"          (kind: Punctuation)
  L20: ";"          (kind: Punctuation)
```

Agent A changes L11: `"localhost"` → `"prod.example.com"`.
Agent B changes L17: `3000` → `8080`.

Three-way merge at the leaf level:

| Leaf | Original | Agent A | Agent B | Action |
|------|----------|---------|---------|--------|
| L11 | `"localhost"` | `"prod.example.com"` | `"localhost"` | Take A (only A changed it) |
| L17 | `3000` | `3000` | `8080` | Take B (only B changed it) |
| All others | same | same | same | Keep original |

Result: `const CONFIG = { host: "prod.example.com", port: 8080 };`

**True conflict** = both agents changed the SAME leaf.  Example: A changes
L11 to `"staging.example.com"` and B changes L11 to `"prod.example.com"`.
The merge engine cannot resolve this — it produces a `Conflict` outcome
with both alternatives for the user to choose.

---

## Part 1: Semantic Merge Engine

### Phase S1: Wire CRDT Layer into Materialization

**Goal**: When `materialize_view` encounters competing vertices (graph-level
conflict), look up the CRDT data to get token-level information.

**Key insight**: The CRDT tables (`CRDT_TRUNKS`, `CRDT_BRANCHES`, `CRDT_LEAVES`)
are populated during `apply_file_ops` when a change is written to the graph.
They map `(change_id, index) → content`.  During materialization, we can
query these tables to get the leaf sequence for any vertex.

**Files to create**:

| File | Contents | ~Lines |
|------|----------|-------:|
| `atomic-core/src/merge/mod.rs` | Module declarations, `MergeOutcome` enum, re-exports | ~50 |
| `atomic-core/src/merge/engine.rs` | `SemanticMergeEngine` — the core three-way merge logic | ~300 |
| `atomic-core/src/merge/leaf_diff.rs` | Three-way diff at the leaf (token) level | ~200 |
| `atomic-core/src/merge/tests/` | Test directory | — |

**Files to modify**:

| File | Change |
|------|--------|
| `atomic-core/src/output/alive/retrieve/mod.rs` | When `compute_order` detects competing vertices, collect them as `ConflictGroup` |
| `atomic-core/src/output/repo/repository/mod.rs` | In `materialize_view`, pass conflict groups to the merge engine |
| `atomic-core/src/lib.rs` | Add `pub mod merge;` |

**Checklist**:
- [ ] Define `MergeOutcome` enum
- [ ] Define `ConflictGroup` struct (two or more competing vertices + their common ancestor)
- [ ] Implement `SemanticMergeEngine::try_merge(conflict_group) → MergeOutcome`
- [ ] Wire into `materialize_view`: when outputting a file with conflicts, attempt semantic merge first
- [ ] Unit tests: two non-overlapping token edits → `AutoMerged`
- [ ] Unit tests: same token edited by both → `Conflict`
- [ ] Unit tests: one side adds tokens, other modifies different tokens → `AutoMerged`
- [ ] `cargo test -p atomic-core`

### Phase S2: Token-Level Conflict Detection

**Goal**: Implement the three-way diff at the leaf level.

The algorithm:

```
fn three_way_leaf_merge(
    base: &[Leaf],
    left: &[Leaf],
    right: &[Leaf],
) -> MergeOutcome {
    // 1. Diff base↔left to get left's edits
    let left_edits = diff_leaves(base, left);
    
    // 2. Diff base↔right to get right's edits
    let right_edits = diff_leaves(base, right);
    
    // 3. Check for overlapping edits (same leaf position changed by both)
    let overlaps = find_overlapping_edits(&left_edits, &right_edits);
    
    // 4. If no overlaps → auto-merge by applying both edit sets
    if overlaps.is_empty() {
        let merged = apply_edits(base, &left_edits, &right_edits);
        return MergeOutcome::AutoMerged { content: merged, ... };
    }
    
    // 5. If overlaps exist → true conflict
    MergeOutcome::Conflict { base, left, right, ... }
}
```

The `diff_leaves` function compares leaf sequences by their stable IDs
(LeafId).  Since each leaf has a unique `(change_id, leaf_idx)` identifier,
we can track which leaves were added, removed, or replaced.

**Key types**:

```rust
/// An edit to a single leaf in a branch.
enum LeafEdit {
    /// Leaf was replaced with new content (same position, different bytes).
    Replace { leaf_id: LeafId, old: Vec<u8>, new: Vec<u8> },
    /// Leaf was deleted.
    Delete { leaf_id: LeafId },
    /// New leaf was inserted after the given position.
    Insert { after: Option<LeafId>, content: Vec<u8>, kind: TokenKind },
}
```

**Checklist**:
- [ ] Implement `diff_leaves(base, modified) → Vec<LeafEdit>`
- [ ] Implement `find_overlapping_edits(left, right) → Vec<Overlap>`
- [ ] Implement `apply_edits(base, left_edits, right_edits) → Vec<u8>`
- [ ] Property test: `apply_edits(base, left_edits, []) == left`
- [ ] Property test: `apply_edits(base, [], right_edits) == right`
- [ ] Property test: non-overlapping edits commute (order doesn't matter)
- [ ] Fuzz test: random leaf sequences, random edits, verify no panics

### Phase S3: Automatic Token Merge

**Goal**: End-to-end working: two agents edit different tokens on the same
line, `materialize_view` produces the merged content without conflict markers.

**Integration test scenario**:

```bash
# Setup
atomic init && echo 'const x = { a: 1, b: 2 };' > config.js
atomic add config.js && atomic record -m "init"

# Agent A: change a
atomic view create agent-a --from dev && atomic view switch agent-a
echo 'const x = { a: 100, b: 2 };' > config.js
atomic record -m "update a"

# Agent B: change b
atomic view switch dev
atomic view create agent-b --from dev && atomic view switch agent-b
echo 'const x = { a: 1, b: 200 };' > config.js
atomic record -m "update b"

# Insert both into dev
atomic insert from-view agent-a --to-view dev
atomic insert from-view agent-b --to-view dev
atomic view switch dev

# Expected: auto-merged
cat config.js
# const x = { a: 100, b: 200 };
```

**Checklist**:
- [ ] Integration test: different tokens on same line → auto-merge
- [ ] Integration test: different lines entirely → no conflict (already works)
- [ ] Integration test: same token → conflict markers
- [ ] Integration test: one add + one modify on same line → auto-merge
- [ ] Integration test: both add at same position → conflict
- [ ] Harness test: add to `tests/harness/11_semantic_merge.sh`
- [ ] `cargo test` full workspace
- [ ] `tests/harness/run_all.sh` all suites pass

### Phase S4: Conflict Markers for True Conflicts

**Goal**: When auto-merge fails (same token edited by both), produce clear
conflict markers in the working copy file.

**Format**:

```
<<<<<<< agent-a (change ABCDEF12)
const x = { a: 100, b: 2 };
||||||| base (change 12345678)
const x = { a: 1, b: 2 };
=======
const x = { a: 1, b: 200 };
>>>>>>> agent-b (change 9ABCDEF0)
```

This is the three-way format (with base) so the developer can see what
the original was.  The change hashes are included for traceability.

**Checklist**:
- [ ] Define conflict marker format
- [ ] Implement `write_conflict_markers(base, left, right, writer)`
- [ ] Wire into materialization: when `MergeOutcome::Conflict`, write markers
- [ ] `atomic status` shows files with conflict markers
- [ ] `atomic record` refuses to record files with conflict markers (must resolve first)
- [ ] Integration test: conflict markers appear, can be resolved manually

---

## Part 2: Push/Pull with Merge

### How Push/Pull Works Today

```
Client:                          Server:
┌──────────┐                     ┌──────────┐
│ GRAPH    │                     │ GRAPH    │
│ (local)  │ ── push changes ──▶│ (remote) │
│          │                     │          │
│          │◀── pull changes ── │          │
└──────────┘                     └──────────┘

Push:
  1. Client serializes changes to .change files
  2. Uploads via HTTP POST to server
  3. Server applies to its GRAPH + VIEW_CHANGES

Pull:
  1. Client compares Merkle state with server
  2. Downloads missing .change files
  3. Client applies to its GRAPH + VIEW_CHANGES
```

### The Divergence Problem

```
Time 0: Client and Server are in sync (same Merkle state)

Time 1: Developer A pushes C4 to server
         Server: {C1, C2, C3, C4}
         Client: {C1, C2, C3}

Time 2: Developer B records C5 locally
         Server: {C1, C2, C3, C4}
         Client: {C1, C2, C3, C5}

Time 3: Developer B pushes C5 to server
         Server has C4 that client doesn't have
         Client has C5 that server doesn't have
         → DIVERGENCE
```

### Phase P1: Server-Side Change Integration

**Goal**: When the server receives a push with changes that have
dependencies the server already satisfies, integrate them into the
target view.  If the changes conflict with existing changes, run the
semantic merge engine.

**Server-side flow**:

```
POST /insert?view=dev&hash=ABCDEF12

1. Receive .change file
2. Save to change store
3. Check dependencies (all satisfied? missing?)
4. Write edges to GRAPH (write_change_to_graph)
5. Add change hash to VIEW_CHANGES[dev]
6. Check for conflicts:
   a. Run materialize_view(dev) in memory (not to disk)
   b. If conflicts detected, run semantic merge
   c. If auto-resolved → accept push, return merged state
   d. If true conflict → accept push but flag the conflict
7. Return new Merkle state to client
```

**Key design decision**: The server ALWAYS accepts valid changes.  Even
conflicting changes are accepted — they're part of the graph.  Conflicts
are a state of the graph, not a reason to reject a push.

The server can optionally **auto-record a merge change** if the semantic
engine resolves the conflict:

```rust
// Server-side auto-merge
if let MergeOutcome::AutoMerged { content, sources } = outcome {
    // Record a new change that resolves the conflict
    let merge_change = Change::new()
        .message(format!("Auto-merge: {} + {}", sources[0], sources[1]))
        .with_merged_content(content);
    
    // This merge change's edges will supersede the conflicting edges
    write_change_to_graph(&mut txn, "dev", merge_change_id, ...)?;
}
```

**Checklist**:
- [ ] Server endpoint accepts changes even with graph-level conflicts
- [ ] Server runs semantic merge after graph write
- [ ] If auto-merged, server optionally records a merge change
- [ ] If true conflict, server flags the view as having unresolved conflicts
- [ ] Server returns conflict status in push response
- [ ] `cargo test -p atomic-storage-server` (when server exists)

### Phase P2: Client Pull with Divergence

**Goal**: When a client pulls and discovers divergence (server has changes
the client doesn't, and client has changes the server doesn't), handle
the merge client-side.

**Client-side pull flow**:

```
atomic pull

1. Get server's Merkle state for the view
2. Compare with local Merkle state
3. If server is ahead (no local-only changes):
   → Simple fast-forward: download and apply missing changes
4. If diverged (both have unique changes):
   a. Download server's unique changes
   b. Apply them to local GRAPH (edges are append-only)
   c. Add them to local VIEW_CHANGES
   d. Run semantic merge on any conflicts
   e. If auto-merged → record merge change locally, push it back
   f. If true conflict → show conflict markers, let user resolve
```

**The key insight**: Because edges are append-only and views are filters,
pulling divergent changes NEVER destroys local work.  Both sets of changes
coexist in the graph.  The merge happens at read time (materialization),
not at write time (graph mutation).

**Checklist**:
- [ ] `atomic pull` detects divergence (local-only + remote-only changes)
- [ ] Downloads and applies remote changes to local GRAPH
- [ ] Runs semantic merge on conflicts
- [ ] Auto-merged conflicts are recorded as merge changes
- [ ] True conflicts produce conflict markers in working copy
- [ ] `atomic status` shows "diverged: X local, Y remote, Z conflicts"
- [ ] Integration test: pull with divergence, no token overlap → auto-merge
- [ ] Integration test: pull with divergence, token overlap → conflict markers

### Phase P3: Push Rejection and Resolution

**Goal**: Define the protocol for push rejection and resolution when the
server detects that the client is behind.

**Protocol**:

```
Client: POST /insert?view=dev&hash=C5
Server: 409 Conflict
        { 
          "status": "diverged",
          "server_state": "MERKLE_HASH",
          "missing_changes": ["C4"],
          "message": "Pull before pushing — server has changes you don't have"
        }

Client: GET /changelist?view=dev&from=3  → downloads C4
Client: Applies C4 locally, runs semantic merge
Client: POST /insert?view=dev&hash=C5    → retries push
Server: 200 OK
        {
          "status": "accepted",
          "new_state": "NEW_MERKLE_HASH",
          "auto_merged": true  // or false if clean
        }
```

**The flow**:

```
atomic push
  → Server: "you're behind, pull first"
  → atomic pull (downloads C4, merges)
  → atomic push (retries with C5)
  → Server: "accepted"
```

This is similar to Git's `push rejected, pull first` flow, but with a
critical difference: the merge step is usually automatic because the
semantic layer resolves token-level conflicts without user intervention.

**Checklist**:
- [ ] Server returns 409 when client is behind
- [ ] 409 response includes missing change hashes
- [ ] Client auto-pulls on 409
- [ ] Client runs semantic merge
- [ ] Client auto-pushes after successful merge
- [ ] `atomic push` reports: "Merged X changes from remote, pushed Y changes"
- [ ] Integration test: push/pull/push cycle with auto-merge
- [ ] Integration test: push/pull/push cycle with true conflict

---

## Merge Recording

When the semantic merge engine auto-resolves a conflict, it needs to
record a **merge change** that makes the resolution permanent in the graph.

The merge change:
1. Depends on BOTH conflicting changes
2. Deletes both competing vertices (V2 and V3)
3. Adds a single merged vertex (V4 with the combined content)
4. Contains the merged FileOps (semantic operations)

This means:
- After the merge change, any view that includes all three changes
  (the two conflicting + the merge) sees clean content
- Views that only include one side still see their own content (no impact)
- The merge change is a first-class change with full provenance

```rust
// The merge change contains:
GraphOp::EdgeUpdate {
    // Delete V2 (Agent A's version)
    edges: [NewEdge { flag: BLOCK|DELETED, to: V2, ... }],
}
GraphOp::EdgeUpdate {
    // Delete V3 (Agent B's version)
    edges: [NewEdge { flag: BLOCK|DELETED, to: V3, ... }],
}
GraphOp::Insertion {
    // Add V4 (merged content)
    content: b"const x = { a: 100, b: 200 };\n",
    ...
}
```

---

## CRDT Table Lookup During Merge

The merge engine needs to look up leaf sequences for competing vertices.
The CRDT tables store this information:

```
CRDT_BRANCHES: (change_id, branch_idx) → BranchData { content_hash, leaf_count }
CRDT_LEAVES:   (change_id, leaf_idx)   → LeafData { kind, content }
```

To look up the leaves for a vertex:
1. The vertex knows its `change_id` and byte range `[start, end)`
2. The CRDT tables map `(change_id, index)` → leaf data
3. Walk the leaf table for this change to reconstruct the token sequence

**Helper function**:

```rust
fn get_leaves_for_vertex<T: GraphTxnT>(
    txn: &T,
    vertex: GraphNode<NodeId>,
) -> Result<Vec<Leaf>, MergeError> {
    let change_id = vertex.change;
    let start = vertex.start;
    let end = vertex.end;
    
    // Look up the branch that contains this byte range
    let branch = txn.get_crdt_branch_for_range(change_id, start, end)?;
    
    // Get all leaves for this branch
    let leaves = txn.get_crdt_leaves(change_id, branch.leaf_start, branch.leaf_count)?;
    
    Ok(leaves)
}
```

---

## Risk Register

| Risk | Severity | Mitigation |
|------|----------|------------|
| **CRDT tables not populated for all changes** | High | The `apply_file_ops` step must run for every change. Verify this is always called in `write_change_to_graph`. Add a check: if CRDT data is missing, fall back to byte-level comparison. |
| **Token boundaries differ between languages** | Medium | The tokenizer already handles multiple languages. The merge engine operates on whatever tokens the tokenizer produced — it doesn't need language awareness itself. |
| **Three-way merge produces invalid syntax** | Medium | The merge engine merges at the token level, preserving token boundaries. But merging two independent edits could produce semantically invalid code (e.g., conflicting type signatures). This is the same as Git — syntactic merge, not semantic validation. |
| **Performance of leaf lookup during materialization** | Medium | Leaf lookup is O(k) where k = leaves in the branch. For a typical line, k < 50. Cache branch → leaves mappings for files with many conflicts. |
| **Server auto-merge produces unexpected results** | Medium | Make auto-merge opt-in per view. Shared views (dev, main) auto-merge by default. Draft views never auto-merge (user resolves manually). |
| **Push/pull protocol changes break existing clients** | Low | Version the protocol. New endpoints for merge-aware push/pull alongside existing ones. |
| **Merge change depends on both sides** | Low | This is correct by design — the merge change's dependency closure includes both conflicting changes, ensuring any view that sees the merge also sees both originals. |

---

## Verification Strategy

### Unit Tests (Phase S1-S2)

```rust
#[test]
fn test_non_overlapping_token_edits_auto_merge() {
    let base   = vec![leaf("a"), leaf(":"), leaf("1"), leaf(","), leaf("b"), leaf(":"), leaf("2")];
    let left   = vec![leaf("a"), leaf(":"), leaf("100"), leaf(","), leaf("b"), leaf(":"), leaf("2")];
    let right  = vec![leaf("a"), leaf(":"), leaf("1"), leaf(","), leaf("b"), leaf(":"), leaf("200")];
    
    let result = three_way_leaf_merge(&base, &left, &right);
    
    assert!(matches!(result, MergeOutcome::AutoMerged { .. }));
    assert_eq!(result.content_string(), "a:100,b:200");
}

#[test]
fn test_same_token_edited_by_both_is_conflict() {
    let base  = vec![leaf("x"), leaf("="), leaf("1")];
    let left  = vec![leaf("x"), leaf("="), leaf("2")];
    let right = vec![leaf("x"), leaf("="), leaf("3")];
    
    let result = three_way_leaf_merge(&base, &left, &right);
    
    assert!(matches!(result, MergeOutcome::Conflict { .. }));
}
```

### Integration Tests (Phase S3)

```bash
# tests/harness/11_semantic_merge.sh

# Test 1: Different tokens on same line → auto-merge
# Test 2: Same token → conflict markers
# Test 3: Different lines → no conflict (baseline)
# Test 4: One add + one modify → auto-merge
# Test 5: Three-way with deletions
```

### Push/Pull Tests (Phase P1-P3)

```bash
# tests/harness/12_push_pull_merge.sh

# Test 1: Simple push (no divergence)
# Test 2: Pull with fast-forward
# Test 3: Push rejected, pull, auto-merge, re-push
# Test 4: Push rejected, pull, true conflict, manual resolve, push
```

### Property Tests

```rust
#[quickcheck]
fn non_overlapping_edits_commute(base: Vec<Leaf>, left_edits: Vec<LeafEdit>, right_edits: Vec<LeafEdit>) -> bool {
    // If edits don't overlap, applying left then right == right then left
    if !has_overlaps(&left_edits, &right_edits) {
        let lr = apply_edits(&base, &left_edits, &right_edits);
        let rl = apply_edits(&base, &right_edits, &left_edits);
        lr == rl
    } else {
        true // overlapping edits are conflicts, not tested here
    }
}
```

---

## Execution Order

| Phase | Depends On | Effort | Impact |
|-------|-----------|--------|--------|
| **S1** | REFACTOR-TYPES A3 (done) | 2-3 days | Foundation — wires CRDT into materialization |
| **S2** | S1 | 2-3 days | Core algorithm — three-way leaf diff |
| **S3** | S2 | 1-2 days | End-to-end — agents see auto-merged content |
| **S4** | S3 | 1 day | UX — conflict markers for true conflicts |
| **P1** | S3 | 2-3 days | Server-side merge on push |
| **P2** | P1 | 2-3 days | Client pull with divergence |
| **P3** | P2 | 1-2 days | Push rejection + auto-retry protocol |

**Total estimate**: 12-17 days

**Minimum viable**: S1 + S2 + S3 gives local semantic merging in ~6 days.
P1 + P2 + P3 adds push/pull merge in another ~6 days. S4 (conflict markers)
can happen anytime after S3.