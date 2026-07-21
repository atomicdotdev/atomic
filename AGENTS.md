# AGENTS.md - Atomic Development Guide

## Project Overview

Atomic is a mathematically sound distributed version control system built in Rust. It uses **patch theory** to represent changes as composable, commutative operations on a directed graph, enabling conflict-free merges when changes are truly independent.

### Design Philosophy

1. **Mathematical Soundness**: Changes are algebraic operations with well-defined composition rules
2. **Content-Addressed**: All data is identified by cryptographic hashes (Blake3)
3. **Graph-Based**: Files are DAGs of vertices and edges, not linear sequences
4. **Views, Not Forks**: Views are filtered perspectives on the same graph, not divergent histories

## Architecture

### Crate Structure

```
atomic/
├── atomic-cli/           # CLI application
├── atomic-core/          # Core VCS engine
│   ├── types/            # Fundamental data types
│   └── pristine/         # Storage layer (redb)
├── atomic-config/        # Configuration management
├── atomic-identity/      # User identity & Ed25519 signing
└── atomic-repository/    # High-level repository operations
```

### Related Projects

- **atomic-remote-client** (`atomic-enterprise/atomic-remote`) - Clean-room HTTP client for remote operations
- **atomic-api** (`atomic-enterprise/atomic-api`) - Server-side HTTP API for remote operations

## Core Concepts

### 1. Repository Graph

Files are represented as directed acyclic graphs (DAGs). Nodes are opaque
byte ranges (hunks); edges define the ordering between them. The semantic
layer (CRDT) interprets the bytes as human-readable text.

```
  Graph layer (storage):

  ┌──────────────┐     ┌──────────────┐     ┌──────────────┐
  │ Hunk [0:5]   │────▶│ Hunk [5:6]   │────▶│ Hunk [0:5]   │
  │ (5 bytes)    │     │ (1 byte)     │     │ (5 bytes)    │
  └──────────────┘     └──────────────┘     └──────────────┘
  Node (change 1)      Node (change 1)      Node (change 2)

  Semantic layer (interpretation):

  The CRDT reads the hunks and translates them for display:
  [0:5] → "Hello"    [5:6] → " "    [0:5] → "World"
```

- **Inodes** (`Inode`): A stable identifier for a file. Survives renames.
  The `TREE` table maps `path → Inode` and `INODES` maps `Inode → Position`
  (the root node of that file's graph). The Inode *is* the file.
- **Nodes / Vertices** (`GraphNode`): Each node holds a chunk of content (a
  byte range within a change). The codebase uses "node" and "vertex"
  interchangeably — `GraphNode` is the struct name, "vertex" is the graph
  theory term used in traversal code (`AliveVertex`, `find_block`, etc.).
  To read a file, walk the graph and concatenate node content in edge order.
- **Edges** (`SerializedGraphEdge`): Define ordering between nodes. An edge
  from A to B means "A's content comes before B's." Edges carry flags
  (`BLOCK`, `FOLDER`, `PARENT`, `DELETED`) that indicate structure and state.

### 2. Changes (Patches)

Atomic transformations that add/remove vertices and edges:

```rust
// A change is identified by its content hash
let hash = Hash::of(change_content);

// Changes are registered to get a repository-local ID
let node_id = txn.register_change(&hash)?;
```

### 3. Views (Ambient Graph + View Filters)

**Critical Concept**: Views are **change-set filters** on a single canonical
GRAPH. All edges are always written to the global GRAPH immediately. A view
determines which subset of the graph is visible by tracking which changes
belong to it in `VIEW_CHANGES`.

| Aspect | Git Branches | Atomic Views |
|--------|--------------|--------------|
| Data Model | Pointer to commit | Ordered set of change references |
| Storage | Duplicates history | Single GRAPH, per-view change filters |
| "Merging" | 3-way merge | Insert change references (with dependency closure) |
| State | HEAD commit hash | Merkle hash of change sequence |
| Cleanup | Manual branch delete + GC | Delete VIEW_CHANGES entries; GC orphaned edges |
| Collaboration | Isolated snapshots | Filtered perspectives, real-time sharing |

#### View Scopes

```rust
pub enum ViewScope {
    /// Personal workspace. Changes visible through the view's filter.
    /// Deletion removes VIEW_CHANGES entries; orphaned edges cleaned by GC.
    Draft,  // feature, bug, service-auth, experiment

    /// Collaborative view. Visible to all. Deletion is restricted.
    Shared,    // dev, release, main
}
```

#### Parent Chains and View Filters

Every view has a parent (except the root). The parent relationship defines
the **filter chain** for graph traversal:

```
  main  (Shared, parent=None — the only true root)
    │
  release  (Shared, parent=main)
    │
  dev  (Shared, parent=release)
    │
    ├── service-auth  (Draft, parent=dev)
    │     ├── feature-login   (Draft, parent=service-auth)
    │     └── feature-logout  (Draft, parent=service-auth)
    │
    └── service-payments  (Draft, parent=dev)
```

A view's **effective filter** is the union of its own `VIEW_CHANGES` and
all ancestor views' `VIEW_CHANGES`, plus the transitive dependency closure:

```
feature-login filter = VIEW_CHANGES[feature-login]
                     ∪ VIEW_CHANGES[service-auth]   (parent, Draft)
                     ∪ VIEW_CHANGES[dev]             (ancestor, Shared)
                     ∪ VIEW_CHANGES[release]         (ancestor, Shared)
                     ∪ VIEW_CHANGES[main]            (root)
                     ∪ dependency closure
```

During graph traversal, an edge is visible if the change that introduced it
is in the view's effective filter. All edges live in the single GRAPH table;
the view just decides which ones to follow.

#### Creating Views

```rust
// Backward compatible: defaults to Shared, no parent
let view = txn.open_or_create_view("main")?;

// Explicit scope and parent
let dev = txn.create_view("dev", ViewScope::Shared, Some(main.id))?;
let feature = txn.create_view("feature", ViewScope::Draft, Some(dev.id))?;

// Draft on draft
let login = txn.create_view("feature-login", ViewScope::Draft, Some(service_auth.id))?;
```

#### Insert = Change Reference + Dependency Closure (Not Cherry-Pick)

`insert` adds a change reference **and all of its transitive dependencies**
to the target view's `VIEW_CHANGES`. A change cannot be inserted without
every change it depends on already present in the target. The system computes
the missing dependency closure automatically — the user picks the change, and
insert pulls in everything required for correctness.

Because all edges are already in GRAPH, insert is an **O(1) metadata
operation** per change — it only writes entries to `VIEW_CHANGES`. No edge
copying is needed.

```rust
// Insert a change from "feature" into "dev"
// 1. Compute transitive deps of the change
// 2. Filter out deps already in the target view
// 3. Add missing dep refs to VIEW_CHANGES in dependency order, then the change itself
//
// No edge copying — edges are already in GRAPH
// Source view is NOT modified
```

#### Deleting Views

```rust
// Draft: delete VIEW_CHANGES entries for this view.
// Orphaned edges (not referenced by any other view) cleaned by GC.
txn.del_view_changes_prefix(feature.id)?;

// Shared: restricted (permanent collaborative history)
```

### 4. Merkle State

Incremental hash representing the complete state of a view:

```
state_0 = Hash(empty)
state_1 = Hash(state_0 || change_hash_1)
state_2 = Hash(state_1 || change_hash_2)
...
```

This enables:
- Efficient sync (compare Merkle states to find divergence)
- Integrity verification
- Deterministic state identification

### 5. CRDT Semantic Layer (Trunk → Branch → Leaf)

**Critical**: The CRDT model is a **required semantic layer** that makes the graph
understandable for developers. The graph stores bytes; CRDT makes it human-readable.

**Why CRDT is Required (not optional):**

To compete with Git and GitHub, developers need to:
- See "line 42 changed" not "bytes 1024-1089 changed"
- Review code at the token/word level, not byte ranges
- Get blame at the token level ("who wrote this function name?")
- Understand diffs in terms of lines and words

The graph is the **storage layer** (persistence, content-addressing, edges).
CRDT is the **semantic layer** (lines, tokens, human-readable operations).

**Both layers are required. You cannot have one without the other.**

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                  Semantic Overlay Architecture                          │
│                  (On top of core graph vertices/edges)                       │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  TRUNK (File)                                                               │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │  id: TrunkId          path: "src/main.rs"    encoding: UTF-8        │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│       │                                                                     │
│       ├──────────────────┬──────────────────┬─────────────────────┐        │
│       ▼                  ▼                  ▼                     ▼        │
│  BRANCH (Line 1)    BRANCH (Line 2)    BRANCH (Line 3)      BRANCH (...)  │
│  ┌──────────────┐   ┌──────────────┐   ┌──────────────┐                    │
│  │ id: B(ch1,0) │   │ id: B(ch1,1) │   │ id: B(ch2,0) │  ← different       │
│  │ state: alive │   │ state: alive │   │ state: alive │    change!         │
│  └──────────────┘   └──────────────┘   └──────────────┘                    │
│       │                  │                                                  │
│       ▼                  ▼                                                  │
│  ┌────┬────┬────┐   ┌────┬────┬────┬────┐                                  │
│  │ fn │ ░░ │main│   │ ░░ │ ░░ │let │ ░░ │   LEAF (Token)                   │
│  │L0  │L1  │L2  │   │L0  │L1  │L2  │L3  │   id: L(ch_id, leaf_idx)         │
│  └────┴────┴────┘   └────┴────┴────┴────┘                                  │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

**Two-Layer Architecture:**

| Layer | Purpose | Types | Required |
|-------|---------|-------|----------|
| **Graph (Storage)** | Persistence, content-addressing | GraphNode, GraphEdge, GraphOp, Atoms | Yes |
| **Semantic (Interpretation)** | Human-readable operations | Trunk/Branch/Leaf, FileOps | Yes |

The graph stores raw content efficiently. CRDT interprets it for humans.

**Performance Characteristics:**

| Operation | Without CRDT | With CRDT |
|-----------|--------------|-----------|
| Find line N | O(vertices) | O(1) via branch index |
| Find token in line | O(vertices) | O(tokens in line) |
| Insert line | O(vertices) to find position | O(1) branch insert |
| Delete line | O(tokens) edge updates | O(1) mark branch deleted |
| Word-diff line | Reconstruct + diff | Compare leaf sequences |
| Blame token | Traverse graph | Direct: `leaf.change_id` |

**Semantic ID Types:**

| Type | Size | Description |
|------|------|-------------|
| `TrunkId` | 12 bytes | (change_id: u64, file_idx: u32) - File identifier |
| `BranchId` | 12 bytes | (change_id: u64, branch_idx: u32) - Line identifier |
| `LeafId` | 12 bytes | (change_id: u64, leaf_idx: u32) - Token identifier |

**Semantic Operations:**

```rust
// File operations
enum TrunkOp {
    Create { path: String, encoding: Option<Encoding> },
    Delete { trunk: TrunkId },
    Move { trunk: TrunkId, new_path: String },
    Undelete { trunk: TrunkId },
}

// Line operations
enum BranchOp {
    Insert { after: Option<BranchId>, content: Vec<LeafOp> },
    Delete { branch: BranchId },
    Restore { branch: BranchId },
}

// Token operations
enum LeafOp {
    Insert { after: Option<LeafId>, kind: TokenKind, content: Vec<u8> },
    Delete { leaf: LeafId },
    Replace { leaf: LeafId, new_content: Vec<u8> },  // Preserves ID for blame
    Restore { leaf: LeafId },
}
```

**Why Two Layers?**

The graph uses byte positions which are machine-efficient but human-hostile:
```
Graph: "Insert bytes 1024-1089 after position 9 in change X"
Human: "What line is that? What word changed?"
```

Semantic provides stable, interpretation identifiers:
```
CRDT: "Insert 'fn main()' on line 42 after token 'pub'"
Human: "I can review that!"
```

**Both layers work together:**
- Graph handles storage, content-addressing, and merging at the byte level
- Semantic translates graph operations into line/token operations for display
- Changes always have both GraphOps (graph) and FileOps (CRDT)
```

## Key Data Structures

### Core Types (`atomic-core/src/types/`)

| Type | Size | Description |
|------|------|-------------|
| `L64` | 8 bytes | Little-endian u64 for cross-platform consistency |
| `NodeId` | 8 bytes | Internal 64-bit identifier (repository-local) |
| `Hash` / `Merkle` | 32 bytes | Unified Blake3 hash (type alias) |
| `ChangePosition` | 8 bytes | Byte offset within a change's content |
| `Inode` | 8 bytes | Stable file identifier (survives renames) |
| `GraphNode<H>` | 24 bytes | Graph node: (change, start, end) |
| `Position<H>` | 16 bytes | Specific location: (change, pos) |
| `EdgeFlags` | 1 byte | Bitflags: BLOCK, PSEUDO, FOLDER, PARENT, DELETED |
| `SerializedGraphEdge` | 24 bytes | Compact edge: (flags+pos, change, introduced_by) |

### Hash Type Design

Following the original Atomic project, `Hash` is a **type alias** for `Merkle`:

```rust
// Both content hashes and state hashes use the same type
pub type Hash = Merkle;

// Unified API
let content_hash = Hash::of(b"file content");
let next_state = current_state.next(&content_hash);
```

This simplifies the codebase while maintaining semantic clarity.

### Storage Layout

```
.atomic/
├── pristine/              # Graph database (redb)
│   └── data.mdb           # Single database file
├── changes/               # Content-addressed change files
│   └── AB/CDEF...         # Two-level directory structure
├── config.toml            # Repository configuration
├── current_view           # Active view name
├── working_copy_id        # Working copy state
└── workspaces/            # Per-view shelved artifacts
    └── <view_name>/       # Shelved build artifacts for this view
```

### Workspace Shelving

See [README.md § Workspace Shelving](README.md#workspace-shelving) for the
user-facing explanation of how `.atomicignore` vs `[workspace] shelve` work.

Relevant code: `atomic-config/src/lib.rs` (`WorkspaceConfig`, `RepoConfig`),
`atomic-repository/src/repository/switch.rs` (`switch_view`, `collect_ignored_paths_on_disk`),
`atomic-cli/src/commands/init.rs` (`get_shelve_patterns`).

**Summary**: All ignored files are shelved per-view by default. Paths in
`[workspace] expose` are the exception — they persist across all views.
`atomic init` seeds `expose` with common tool configs (`.opencode`, `.vscode`,
`.idea`, `.claude`, `.gemini`, `.agents`). No per-language shelve lists needed.

### Single GRAPH Architecture

All edges live in one canonical GRAPH table. Views determine visibility by
filtering on which changes are referenced in `VIEW_CHANGES`. The
`introduced_by` field on each edge identifies which change created it,
enabling efficient filtering during traversal.

```
┌─────────────────────────────────────────────────────────────────────────┐
│                    Ambient Graph Architecture                            │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Canonical GRAPH (all edges from all views)                             │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ Key: GraphNode (24 bytes)  → Value: [SerializedGraphEdge]      │    │
│  │ All edges written here immediately. Single source of truth.    │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                                                                         │
│  Secondary Index: INODE_GRAPH (file-local traversal optimization)      │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ Key: (Inode, GraphNode) (32 bytes) → Value: [GraphEdge]       │    │
│  │ Same edges, indexed by file for O(m) file-local traversal.    │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                                                                         │
│  View filter for any view:                                              │
│    visible_changes = VIEW_CHANGES[this]                                │
│                    ∪ VIEW_CHANGES[parent]                               │
│                    ∪ ... (up the ancestor chain)                        │
│                    ∪ dependency closure                                  │
│                                                                         │
│  An edge is visible iff edge.introduced_by ∈ visible_changes           │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

## Pristine Storage Layer

### Overview

The pristine is the persistent storage layer using [redb](https://docs.rs/redb):

- **Pure Rust**: No C dependencies
- **ACID Transactions**: Safe concurrent access
- **Copy-on-Write B-trees**: Efficient updates
- **Memory-mapped I/O**: Excellent read performance

### Table Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        Pristine Database                        │
├─────────────────────────────────────────────────────────────────┤
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐  │
│  │ ID Mappings │  │    Graph    │  │         Views           │  │
│  │             │  │             │  │                         │  │
│  │ EXTERNAL    │  │ GRAPH       │  │ VIEWS                   │  │
│  │ INTERNAL    │  │ INODE_GRAPH │  │ VIEW_CHANGES            │  │
│  │ NODE_TYPES  │  │             │  │ REV_VIEW_CHANGES        │  │
│  └─────────────┘  └─────────────┘  └─────────────────────────┘  │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐  │
│  │  File Tree  │  │Dependencies │  │         State           │  │
│  │             │  │             │  │                         │  │
│  │ TREE        │  │ DEPS        │  │ STATES                  │  │
│  │ REV_TREE    │  │ REV_DEPS    │  │ TAGS                    │  │
│  │ INODES      │  │             │  │                         │  │
│  │ REV_INODES  │  │             │  │                         │  │
│  └─────────────┘  └─────────────┘  └─────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

### Table Reference

| Table | Key | Value | Purpose |
|-------|-----|-------|---------|
| `EXTERNAL` | NodeId (u64) | Hash ([u8; 32]) | Internal → external ID |
| `INTERNAL` | Hash ([u8; 32]) | NodeId (u64) | External → internal ID |
| `NODE_TYPES` | NodeId (u64) | u8 | Node type (change=0, tag=1) |
| `GRAPH` | GraphNode ([u8; 24]) | [GraphEdge] | Canonical graph — all edges (multimap) |
| `INODE_GRAPH` | (Inode, GraphNode) ([u8; 32]) | [GraphEdge] | File-scoped index |
| `VIEWS` | name (str) | ViewState (var) | View metadata (scope, parent, merkle) |
| `VIEW_CHANGES` | (view_id, seq) ([u8; 16]) | change_id (u64) | Change log per view |
| `REV_VIEW_CHANGES` | (view_id, change_id) ([u8; 16]) | seq (u64) | Reverse log |
| `TREE` | path (str) | inode (u64) | Path → inode |
| `REV_TREE` | inode (u64) | path (str) | Inode → path |
| `INODES` | inode (u64) | Position ([u8; 16]) | Inode → graph |
| `REV_INODES` | Position ([u8; 16]) | inode (u64) | Graph → inode |
| `DEPS` | change_id (u64) | [dep_id] (u64) | Dependencies |
| `REV_DEPS` | dep_id (u64) | [change_id] (u64) | Reverse deps |
| `STATES` | (view_id, merkle) ([u8; 40]) | seq (u64) | State → sequence |
| `TAGS` | (view_id, seq) ([u8; 16]) | merkle ([u8; 32]) | Tagged states |

### Transaction Model

```rust
// Read-only (multiple concurrent)
let txn = pristine.read_txn()?;
let view = txn.get_view("main")?;

// Read-write (exclusive)
let mut txn = pristine.write_txn()?;

// Backward-compatible: defaults to Shared, no parent
let mut view = txn.open_or_create_view("feature")?;

// Explicit scope and parent
let dev = txn.create_view("dev", ViewScope::Shared, Some(main.id))?;
let feature = txn.create_view("feature", ViewScope::Draft, Some(dev.id))?;

txn.put_change(&mut view, change_id, &hash)?;
txn.update_view(&view)?;
txn.commit()?;  // or txn.abort()?
```

### Trait Hierarchy

```
                    MutTxnT
        (Full read-write access)
        (create_view, put_graph,
         put_inode_graph)
                     │
        ┌────────────┼────────────┐
        ▼            ▼            ▼
    ViewTxnT     TreeTxnT    GraphTxnT
   (View ops)   (File ops)  (Graph queries)
   (get_view_by_id,           │
    resolve_filter_chain,     │
    collect_visible_changes)  │
        │            │        │
        └────────────┼────────┘
                     │
                GraphTxnT
              (Base trait)

              InodeGraphOps
        (File-local graph traversal)
              Implemented by:
           ReadTxn, WriteTxn
```

### Block Finding Methods (Critical for Graph Writes)

The `GraphTxnT` trait provides two methods for finding vertices by position:

| Method | Use Case | Matching Logic |
|--------|----------|----------------|
| `find_block(pos)` | Down-context, general lookup | `start <= pos < end` (half-open range) |
| `find_block_end(pos)` | Up-context resolution | `end == pos` OR empty vertex at `pos` |

**Why Two Methods?**

In Atomic's graph model, context positions have different semantics:

- **Up-context**: References the **end** of a predecessor vertex. Position 12 means
  "find the vertex that ends at position 12" (e.g., vertex [0:12]).
- **Down-context**: References the **start** of a successor vertex. Position 12 means
  "find the vertex containing position 12" (e.g., vertex [10:20]).

```rust
// Up-context: find vertex ENDING at position
let up_vertex = txn.find_block_end(up_pos)?;  // [0:12] matches pos=12

// Down-context: find vertex CONTAINING position  
let down_vertex = txn.find_block(down_pos)?;  // [10:20] matches pos=12
```

**Empty Vertex Handling**:

Both methods handle empty vertices (where `start == end`) specially:
- `find_block`: Matches if `start == pos == end`
- `find_block_end`: Matches if `start == pos == end`

This is crucial for inode vertices which are empty structural markers.

**ROOT Position Handling**:

Both methods return `Vertex::ROOT` when the position's change ID is ROOT.
The ROOT vertex is virtual and doesn't exist in the database.

### Position Ambiguity and Graph Traversal (Critical Lessons Learned)

When a single position can refer to multiple vertices, careful handling is required
to ensure correct graph traversal. This section documents critical bugs discovered
and fixed during the diff command implementation.

#### The Problem: Shared Start Positions

In Atomic's graph model, a file's structure includes:
- **Name vertex**: `V[0:9]` - The filename in the parent directory
- **Inode vertex**: `V[9:9]` - Empty structural marker (start == end)
- **Content vertex**: `V[9:23]` - The actual file content

Notice that the inode vertex `V[9:9]` and content vertex `V[9:23]` share the same
start position (9). This creates ambiguity when resolving edge destinations.

```
┌─────────────────────────────────────────────────────────────────────────┐
│                    Position Ambiguity Example                            │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Position 9 can refer to:                                               │
│                                                                         │
│  ┌─────────────────┐     ┌─────────────────┐                           │
│  │ Inode V[9:9]    │     │ Content V[9:23] │                           │
│  │ (empty marker)  │     │ (actual data)   │                           │
│  │ start=9, end=9  │     │ start=9, end=23 │                           │
│  └─────────────────┘     └─────────────────┘                           │
│                                                                         │
│  Edge destination Pos[9] is AMBIGUOUS without additional context!       │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

#### Bug 1: `find_block_end` Iteration Order

**Symptom**: When writing changes, edges from inode to content weren't created correctly.

**Root Cause**: `find_block_end(9)` iterated through vertices in B-tree order (by start position).
The name vertex `V[0:9]` was encountered first and matched because `end == 9`.

**Fix**: Check for empty vertices at the exact position using **direct lookup first**,
before falling back to iteration:

```rust
fn find_block_end(&self, pos: Position<NodeId>) -> Result<Vertex<NodeId>, _> {
    // FIRST: Direct lookup for empty vertex at exact position
    let empty_key = encode_vertex(change_id, target_pos, target_pos);
    if table.get(&empty_key)?.next().is_some() {
        return Ok(Vertex::new(change_id, target_pos, target_pos));
    }
    
    // SECOND: Fall back to iteration for vertices ending at this position
    // ...
}
```

#### Bug 2: `find_block` Preferring Empty Vertices

**Symptom**: Graph traversal found inode vertex instead of content vertex when
following edges.

**Root Cause**: `find_block(9)` returned `V[9:9]` (the inode) instead of `V[9:23]`
(the content) because empty vertex matching was checked before non-empty range matching.

**Fix**: **Prefer non-empty vertices** over empty vertices when both match:

```rust
fn find_block(&self, pos: Position<NodeId>) -> Result<Vertex<NodeId>, _> {
    let mut empty_vertex_match: Option<Vertex<NodeId>> = None;
    
    for vertex in vertices {
        // Prefer non-empty vertex containing this position
        if v_start != v_end && v_start <= target_pos && target_pos < v_end {
            return Ok(vertex);  // Return immediately
        }
        
        // Track empty vertex as fallback
        if v_start == v_end && v_start == target_pos {
            empty_vertex_match = Some(vertex);
        }
    }
    
    // Only return empty vertex if no non-empty vertex matched
    if let Some(vertex) = empty_vertex_match {
        return Ok(vertex);
    }
    // ...
}
```

#### Bug 3: `retrieve_graph` Cache Key Ambiguity

**Symptom**: Graph traversal only found 2 vertices (dummy + inode) instead of 3
(dummy + inode + content).

**Root Cause**: The traversal used **position** as the cache key. When following
an edge `BLOCK -> Pos[9]`:
1. Cache lookup found existing entry for `Pos[9]` (the inode, added at startup)
2. Traversal assumed destination was already visited
3. Content vertex was never discovered

**Fix**: Use the **resolved vertex** as the cache key, not the position:

```rust
// BAD: Position as cache key (ambiguous)
let mut cache: HashMap<Position<NodeId>, VertexId> = HashMap::new();

// GOOD: Resolved vertex as cache key (unambiguous)
let mut cache: HashMap<Vertex<NodeId>, VertexId> = HashMap::new();

// In traversal loop:
let resolved_vertex = txn.find_block(dest_pos)?;  // Resolve first
if let Some(&existing) = cache.get(&resolved_vertex) {  // Then cache check
    // Already visited this specific vertex
} else {
    // New vertex, add to graph
}
```

#### Key Takeaways

1. **Position ≠ Vertex**: A position can refer to multiple vertices. Always resolve
   to the actual vertex when caching or comparing.

2. **Empty vs Non-Empty Priority**: When both an empty vertex and a non-empty vertex
   match a position, prefer non-empty for `find_block` (edge destinations point to
   content) and empty for `find_block_end` (up-context references structural markers).

3. **Direct Lookup vs Iteration**: For specific cases like empty vertex lookup,
   direct B-tree lookup is more reliable than iteration order.

4. **Test End-to-End**: The integration test that caught these bugs exercised the
   full workflow: record → modify → status → diff. Unit tests of individual
   functions didn't reveal the interaction bugs.

### Inode Graph Operations (Dual B-Tree Optimization)

The `InodeGraphOps` trait provides efficient file-local graph traversal using a
dual B-tree indexing strategy. This is critical for performance when
materializing file contents from the repository graph.

**Problem**: Standard graph storage uses `Vertex<NodeId>` as the key, storing all
vertices from all files in a single B-tree. This leads to O(n × log N) traversal
complexity when iterating edges for a file, where N is the total number of
vertices across ALL files.

**Solution**: By using `(Inode, Vertex<NodeId>)` as a composite key in the
`INODE_GRAPH` secondary index:
- All edges for a single file are stored contiguously
- Cursor-based iteration within a file becomes O(m) where m is vertices in that file
- Cross-file queries remain possible via the primary `GRAPH` index

```
┌─────────────────────────────────────────────────────────────────────────┐
│                      Dual B-Tree Index Architecture                      │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Primary Index (GRAPH)              Secondary Index (INODE_GRAPH)       │
│  Key: Vertex<NodeId>                Key: (Inode, Vertex<NodeId>)        │
│  ┌─────────────────────┐            ┌─────────────────────────────┐    │
│  │ V(1, 0:10)  → edges │            │ (Inode(42), V(1,0:10)) → e │    │
│  │ V(1, 10:20) → edges │            │ (Inode(42), V(1,10:20))→ e │    │
│  │ V(2, 0:5)   → edges │            │ (Inode(42), V(2,0:5))  → e │    │
│  │ V(3, 0:100) → edges │            │ (Inode(99), V(3,0:100))→ e │    │
│  │ ...         → ...   │            │ ...                        │    │
│  └─────────────────────┘            └─────────────────────────────┘    │
│                                                                         │
│  Use for:                           Use for:                            │
│  - Cross-file queries               - File-local traversal              │
│  - Global operations                - Materialize operations            │
│  - Backward compatibility           - O(m) instead of O(m × log N)     │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

**Key Types**:
- `InodeVertex` - Composite key: `(Inode, Vertex<NodeId>)` for B-tree ordering
- `InodeAdjState` - Cursor state for adjacency iteration within an inode
- `InodeGraphStats` - Performance metrics (vertices visited, edges traversed, cache hits)
- `InodeEdgeIter` - Iterator over edges within an inode scope

**Trait Methods**:
```rust
pub trait InodeGraphOps {
    // Initialize adjacency iteration for a vertex within an inode scope
    fn init_inode_adj(&self, inode: Inode, vertex: Vertex<NodeId>,
                      min_flag: EdgeFlags, max_flag: EdgeFlags) -> Result<InodeAdjState, _>;
    
    // Get next adjacent edge (with flag filtering)
    fn next_inode_adj(&self, adj: &mut InodeAdjState) -> Option<Result<SerializedEdge, _>>;
    
    // Find block containing position within inode scope
    fn find_block_in_inode(&self, inode: Inode, pos: Position<NodeId>) -> Result<Option<Vertex<NodeId>>, _>;
    
    // Count vertices for an inode
    fn count_inode_vertices(&self, inode: Inode) -> Result<usize, _>;
    
    // Check if inode has entries in the secondary index
    fn inode_graph_is_populated(&self, inode: Inode) -> Result<bool, _>;
    
    // Convenience iterator over all edges for an inode
    fn iter_inode_edges(&self, inode: Inode, min_flag: EdgeFlags, max_flag: EdgeFlags) -> Result<InodeEdgeIter<'_, Self>, _>;
}
```

**Expected Performance Improvement**:

| Changes | Before (O(n log N)) | After (O(n)) | Improvement |
|---------|---------------------|--------------|-------------|
| 1,000   | ~230ms              | ~50ms        | ~5x         |
| 10,000  | ~2s                 | ~200ms       | ~10x        |
| 100,000 | ~20s                | ~2s          | ~10x        |

### Performance Characteristics

| Operation | Complexity | Notes |
|-----------|------------|-------|
| Get vertex edges | O(k) | k = number of edges |
| Find block | O(log n) | B-tree binary search |
| Register change | O(log n) | Two table insertions |
| Iterate file | O(m) | m = file vertices (via INODE_GRAPH) |
| List views | O(v) | v = number of views |
| Insert change to view | O(1) | Metadata write to VIEW_CHANGES |

The `INODE_GRAPH` secondary index enables **O(n) file traversal** where n is proportional to file size, rather than O(N) where N is total graph size.

## Development Guidelines

### Code Style

1. **Error Handling**: Use `thiserror` for error types, `anyhow` for application code
2. **Serialization**: Use `serde` with bincode for binary, JSON/TOML for human-readable
3. **Testing**: Write unit tests inline, integration tests in `tests/` directory
4. **Documentation**: Document all public APIs with examples

### Naming Conventions

- Types: `PascalCase`
- Functions/methods: `snake_case`
- Constants: `SCREAMING_SNAKE_CASE`
- Crates: `atomic-{name}`
- Tables: `SCREAMING_SNAKE_CASE`

### Documentation Standards

- Document **public** items that aren't self-explanatory from the type signature
- Skip docs on trivial getters, obvious fields, and internal helpers
- Use `# Examples` on public APIs that have non-obvious usage
- Use `# Errors` only when the failure modes aren't clear from the return type
- Avoid restating the function name or field name in the doc comment

### Getter Convention

Follow Rust naming conventions for accessors:

```rust
// Good: getter matches field name
pub fn algorithm(&self) -> Algorithm { self.algorithm }

// Good: builder uses with_ prefix
pub fn with_algorithm(mut self, alg: Algorithm) -> Self { ... }

// Bad: Java-style get_ prefix
pub fn get_algorithm(&self) -> Algorithm { self.algorithm }
```

### Record Style

Use conventional records:
- `feat:` New features
- `fix:` Bug fixes
- `refactor:` Code restructuring
- `docs:` Documentation updates
- `test:` Test additions/changes

## Roadmap

### Identity Management (complete)

The `atomic-identity` crate provides Ed25519-based identity management with
multiple identities per user, agent delegation, and cryptographic signing.
Identities are stored under `~/.atomic/identities/` and integrate with
change headers via `identity.to_author()`.

Key types: `Identity`, `IdentityId`, `KeyPair`, `Delegation`, `DelegationScope`,
`Signature`, `Signer`, `IdentityStore`.

### Semantic Layer (in progress)

The semantic layer translates raw graph operations (byte positions) into
human-readable line/token operations for code review, blame, and diffs.

**Status**: FileOps generation and CRDT table population work. Remaining:
verify content retrieval consistency and wire up token-level diff display.

Relevant code: `atomic-core/src/change/ops.rs`, `atomic-core/src/apply/crdt.rs`,
`atomic-core/src/record/workflow/crdt/`.

### Ambient Graph + View Filter Model (Phases 1-8 complete)

The ambient graph model stores all edges in a single canonical GRAPH. Views
are change-set filters that determine which subset of the graph is visible.
This replaces the earlier two-tier model (STACK_GRAPH + GRAPH) with a simpler,
more performant architecture.

**Phase 1 (complete)**: Foundation types and storage schema.
- `ViewScope` enum (Draft, Shared)
- `parent: Option<u64>` field on `ViewState`
- `create_view(name, scope, parent)` for explicit view creation
- Backward-compatible serialization (v1 data reads as Shared, no parent)
- 37 integration tests covering all new functionality

**Phase 2 (complete)**: Graph write path.
- All edges written directly to GRAPH + INODE_GRAPH (no two-tier routing)
- `write_new_vertex` writes edges to GRAPH unconditionally
- `write_edge_map` writes edges to GRAPH unconditionally
- `add_edge_with_reverse` in both `edge.rs` and `insertion.rs` always targets GRAPH
- `del_edge_with_reverse` always targets GRAPH
- `write_change_to_graph` writes all edges to canonical GRAPH regardless of view scope
- `materialize_view` uses view's change filter to determine visible files
- `materialize_view_prefix` also uses change filter for file visibility
- `collect_visible_changes(view)` collects change NodeIds from
  the full parent chain (current view + all ancestors)
- `get_file_content`, `get_file_content_on_view` use change filter
  for consistent view-aware retrieval

**Phase 3 (complete)**: Graph traversal with change filters.
- `iter_adjacent` filters edges by `introduced_by ∈ visible_changes`
- `find_block` operates on canonical GRAPH with change filter
- `find_block_end` operates on canonical GRAPH with change filter
- `has_vertex` checks GRAPH directly
- `is_file_alive(pos)` checks whether an inode vertex has at least one alive
  (non-PARENT, non-DELETED) forward edge whose introducing change is visible
- `iter_tree` filters entries by graph aliveness through the change filter —
  files whose edges were introduced by changes not in the view are excluded
- `status` method uses change filter for `iter_tree`, `get_inode`,
  `is_directory`, and `inode_position` calls, ensuring files from other
  views do not appear as tracked
- TREE/INODES tables remain global (they are a superset index); the change
  filter determines file visibility at the graph level

**Phase 4 (complete)**: View lifecycle.
- `del_view` deletes `VIEW_CHANGES` entries for Draft views;
  orphaned edges cleaned by background GC
- `del_view` blocks deletion of Shared views (`CannotDeleteSharedView`)
- `del_view` blocks deletion of views with children (`ViewHasChildren`)
- `del_view` cleans up: VIEW_CHANGES, REV_VIEW_CHANGES, STATES, TAGS
- Parent cycle detection in `create_view`: walks proposed parent chain with
  a visited set; returns `ViewCycleDetected` if a cycle is found
- Parent validation: `create_view` returns `ViewNotFound` if parent ID
  references a non-existent view

**Phase 5 (complete)**: Insert between views + cross-view diff.
- `insert_from_view` adds change references to target view's VIEW_CHANGES
- `insert_change_rec` computes and inserts transitive dependency closure
- No edge copying needed — edges are already in GRAPH
- `get_file_content_via_filter`: reads file content using change filter so
  views see only edges introduced by their visible changes
- `diff_views(a, b)`: change-level diff (only_in_a, only_in_b, common)

**Phase 6 (complete)**: CLI and UX.
- `view create --draft --parent dev` creates draft views with explicit parent
- `view create` without flags preserves backward-compatible behavior (shared, fork from current)
- `view list --verbose` shows `[shared]`/`[draft]` tags and parent name
- `ViewInfo` has `scope: ViewScope` and `parent_name: Option<String>` fields
- `Repository::get_view_info` resolves parent ID → name for display

**Phase 7 (complete)**: Vocabulary refactoring across codebase.
- Stack → View, StackKind → ViewScope, Local → Draft
- Apply → Insert (user-facing cross-view operation)
- Output working copy → Materialize (write graph state to disk)
- Eliminated: `ApplyTarget`, `OverlayTxn`, `STACK_GRAPH`
- All edges always go to GRAPH; views use change filters

**Phase 8 (complete)**: Documentation update.
- AGENTS.md comprehensively updated to reflect ambient graph + view filter model
- All code examples, type references, and architectural diagrams updated

Relevant code: `atomic-core/src/pristine/traits.rs` (ViewScope, ViewState),
`atomic-core/src/pristine/tables.rs` (GRAPH, INODE_GRAPH, VIEWS, VIEW_CHANGES),
`atomic-core/src/pristine/txn/`,
`atomic-core/src/apply/mod.rs` (write_change_to_graph),
`atomic-core/src/apply/edge.rs` (add_edge_with_reverse, del_edge_with_reverse),
`atomic-core/src/apply/insertion.rs` (write_new_vertex, add_edge_with_reverse),
`atomic-repository/src/apply.rs` (write_change_to_graph),
`atomic-repository/src/repository/content.rs` (get_file_content_via_filter,
diff_views), `atomic-repository/src/repository/mod.rs` (materialize_view,
materialize_view_prefix, switch_view, ViewInfo with scope/parent),
`atomic-repository/src/repository/status.rs` (change-filter-aware status),
`atomic-cli/src/commands/view/create.rs` (--draft, --parent flags),
`atomic-cli/src/commands/view/list.rs` (scope/parent in verbose output).

### View Workflow Commands (planned)

Advanced view manipulation: `unrecord`, `reinsert`, `revise`, cross-view `insert`,
per-view `tag`. These build on the existing `Repository::unrecord()` and
`Repository::reinsert_change()` methods.

Key design: a change reference syntax (`@`, `@~1`, `@~N`, `view@`) for
addressing changes within views. See the Phase 13 section of the CRDT
Semantic Layer design doc for the full spec.




## Testing Strategy

### Unit Tests (Inline)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_specific_behavior() {
        let result = function_under_test();
        assert_eq!(result, expected);
    }
}
```

### Integration Tests (`tests/` directory)

```rust
// tests/pristine_test.rs
use atomic_core::pristine::{Pristine, MutTxnT};

#[test]
fn test_full_workflow() {
    let pristine = Pristine::open(temp_path)?;
    let mut txn = pristine.write_txn()?;
    // ... test complete workflow
    txn.commit()?;
}
```

### Property Tests (QuickCheck)

```rust
use quickcheck_macros::quickcheck;

#[quickcheck]
fn hash_roundtrip(data: Vec<u8>) -> bool {
    let hash = Hash::of(&data);
    let base32 = hash.to_base32();
    Hash::from_base32(base32.as_bytes()) == Some(hash)
}
```

### Testing

Run the full test suite with:

```bash
cargo test                        # all crates
cargo test -p atomic-core         # core engine
cargo test -p atomic-repository   # repository operations
cargo test -p atomic-identity     # identity management
```

Tests are colocated with the code they exercise (inline `#[cfg(test)]` modules)
and integration tests live under each crate's `tests/` directory. Doctests on
public APIs serve as both documentation and regression tests.

## Performance Considerations

1. **Inode Index**: Secondary B-tree index (INODE_GRAPH) for O(n) file traversal
2. **Lazy Loading**: Load change contents on demand from change files
3. **Atomic Counters**: Thread-safe ID allocation with AtomicU64
4. **Key Encoding**: Efficient fixed-size byte arrays for table keys
5. **Copy-on-Write**: redb uses COW B-trees for efficient updates
6. **O(1) Insert**: Cross-view insert is metadata-only (no edge copying)

## File Organization

```
atomic-core/
├── src/
│   ├── lib.rs
│   ├── types/              # L64, NodeId, Hash, Merkle, Vertex, Edge, etc.
│   ├── diff/               # Myers + Patience diff algorithms
│   │   ├── token/          # Token-level diff (word diff for code review)
│   │   ├── semantic/       # Line + token semantic diff
│   │   └── ...
│   ├── crdt/               # Trunk → Branch → Leaf semantic model
│   ├── pristine/           # redb storage layer
│   │   └── txn/
│   │       ├── read.rs
│   │       └── write/      # Split by trait: graph, view, tree, mutate
│   ├── change/             # Change representation, headers, provenance
│   ├── record/
│   │   └── workflow/
│   │       ├── globalize/  # Position resolution, vertex creation, pipeline
│   │       ├── crdt/       # CRDT operation generation
│   │       └── ...
│   ├── apply/              # Graph writes, conflict detection
│   └── materialize/        # View materialization, alive graph traversal
│       ├── repo/
│       └── alive/

atomic-repository/
├── src/
│   ├── repository/         # Split by domain:
│   │   ├── mod.rs          # Struct, init, open, path helpers
│   │   ├── views.rs        # View CRUD
│   │   ├── changes.rs      # Change save/load/delete
│   │   ├── record.rs       # Record workflow
│   │   ├── apply.rs        # Write changes to graph
│   │   ├── status.rs       # Working copy status
│   │   ├── tracking.rs     # File tracking
│   │   ├── history.rs      # Log, unrecord, reinsert
│   │   ├── tags.rs         # Tag CRUD
│   │   ├── archive.rs      # Export
│   │   ├── content.rs      # File content retrieval
│   │   └── remotes.rs      # Remote configuration
│   └── ...

atomic-cli/
├── src/
│   ├── main.rs
│   ├── commands/
│   │   ├── diff/           # types, command, output
│   │   ├── log/            # types, command
│   │   ├── change/         # types, command
│   │   ├── record/         # builder, provenance, format, command
│   │   ├── push/, pull/, clone/, view/, tag/, ...
│   │   └── ...
│   └── output/             # colors, progress, table
```

## Getting Started

```bash
# Build the project
cargo build

# Build the CLI specifically
cargo build -p atomic

# Run all tests
cargo test

# Run CLI tests only
cargo test -p atomic

# Run tests with output
cargo test -- --nocapture

# Run specific test
cargo test test_view_operations

# Check documentation
cargo doc --open
```

### CLI Usage

```bash
# Show help
cargo run -p atomic -- --help

# Initialize a new repository
cargo run -p atomic -- init

# Initialize with a specific view name
cargo run -p atomic -- init --view main

# Initialize with project-specific ignore patterns
cargo run -p atomic -- init --kind rust

# Show status
cargo run -p atomic -- status

# Add files
cargo run -p atomic -- add src/main.rs

# Record changes
cargo run -p atomic -- record -m "Initial commit"

# View history
cargo run -p atomic -- log

# Manage views
cargo run -p atomic -- view list
cargo run -p atomic -- view create feature
cargo run -p atomic -- view switch main

# Insert changes between views
cargo run -p atomic -- insert from-view feature --to-view dev
```

## License

Dual-licensed under MIT and Apache 2.0.