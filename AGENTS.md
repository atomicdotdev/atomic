# AGENTS.md - Atomic Development Guide

## Project Overview

Atomic is a mathematically sound distributed version control system built in Rust. It uses **patch theory** to represent changes as composable, commutative operations on a directed graph, enabling conflict-free merges when changes are truly independent.

### Design Philosophy

1. **Mathematical Soundness**: Changes are algebraic operations with well-defined composition rules
2. **Content-Addressed**: All data is identified by cryptographic hashes (Blake3)
3. **Graph-Based**: Files are DAGs of vertices and edges, not linear sequences
4. **Views, Not Forks**: Stacks are perspectives on the same graph, not divergent histories

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

### 3. Stacks (Two-Tier Graph Model)

**Critical Concept**: Stacks use a **two-tier graph model** where edge storage
depends on whether a stack is **Shared** or **Local**.

| Aspect | Git Branches | Atomic Stacks |
|--------|--------------|---------------|
| Data Model | Pointer to commit | Ordered sequence of applied changes |
| Storage | Duplicates history | Two-tier: global GRAPH + per-stack STACK_GRAPH |
| "Merging" | 3-way merge | Apply changes (with dependency closure) |
| State | HEAD commit hash | Merkle hash of sequence |
| Cleanup | Manual branch delete + GC | Cascade delete STACK_GRAPH (zero orphans) |

#### Stack Kinds

```rust
pub enum StackKind {
    /// Edges stored in STACK_GRAPH[(stack_id, vertex)].
    /// Cascade-deleted when the stack is removed. Zero orphans.
    Local,  // feature, bug, service-auth, experiment

    /// Edges stored in the global GRAPH[vertex].
    /// Permanent promoted history. Deletion is restricted.
    Shared,    // dev, release, main
}
```

#### Parent Chains and the Overlay Model

Every stack has a parent (except the root). The parent relationship defines
the **overlay chain** for graph traversal:

```
  main  (Shared, parent=None — the only true root)
    │
  release  (Shared, parent=main)
    │
  dev  (Shared, parent=release)
    │
    ├── service-auth  (Local, parent=dev)
    │     ├── feature-login   (Local, parent=service-auth)
    │     └── feature-logout  (Local, parent=service-auth)
    │
    └── service-payments  (Local, parent=dev)
```

An local workspace's **effective view** is the union of its own `STACK_GRAPH`,
each isolated ancestor's `STACK_GRAPH`, and the global `GRAPH` (reached when
a Shared ancestor is encountered):

```
feature-login view = STACK_GRAPH[feature-login]
                   ∪ STACK_GRAPH[service-auth]   (parent, Local)
                   ∪ GRAPH                        (dev is Shared → stop)
```

#### Creating Stacks

```rust
// Backward compatible: defaults to Shared, no parent
let stack = txn.open_or_create_stack("main")?;

// Explicit kind and parent
let dev = txn.create_stack("dev", StackKind::Shared, Some(main.id))?;
let feature = txn.create_stack("feature", StackKind::Local, Some(dev.id))?;

// Stacked isolated on isolated
let login = txn.create_stack("feature-login", StackKind::Local, Some(service_auth.id))?;
```

#### Apply = Change + Dependency Closure (Not Cherry-Pick)

`apply` moves a change **and all of its transitive dependencies** to the
target stack. A change cannot be applied without every change it depends on
already present on the target. The system computes the missing dependency
closure automatically — the user picks the change, and apply pulls in
everything required for correctness.

```rust
// Apply a change from "feature" to "dev"
// 1. Compute transitive deps of the change
// 2. Filter out deps already on the target stack
// 3. Apply missing deps in dependency order, then the change itself
//
// Local → Shared target: edges go to GRAPH
// Local → Local target: edges go to target's STACK_GRAPH
// Source stack is NOT modified
```

#### Deleting Stacks

```rust
// Local: cascade-delete STACK_GRAPH[stack_id] → zero orphans
// Changes previously applied to a Shared stack survive in GRAPH
txn.del_stack_graph_prefix(feature.id)?;

// Shared: restricted (permanent history)
```

### 4. Merkle State

Incremental hash representing the complete state of a stack:

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
├── current_stack          # Active stack name
└── working_copy_id        # Working copy state
```

### Two-Tier Edge Storage

```
┌─────────────────────────────────────────────────────────────────────────┐
│                    Two-Tier Graph Architecture                            │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Global GRAPH (Shared stacks: dev, release, main)                       │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ Key: GraphNode (24 bytes)  → Value: [SerializedGraphEdge]      │    │
│  │ Permanent. Visible to ALL stacks. Written by Shared stacks.    │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                                                                         │
│  Per-Stack STACK_GRAPH (Local workspaces: feature, bug, service-*)       │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ Key: (stack_id, GraphNode) (32 bytes) → Value: [Edge] (24 b)  │    │
│  │ Ephemeral. Visible only through the overlay chain.             │    │
│  │ Prefix scan on stack_id → O(n) cascade deletion.              │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                                                                         │
│  Effective view for Local workspace:                                      │
│    STACK_GRAPH[this] ∪ STACK_GRAPH[parent] ∪ ... ∪ GRAPH               │
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
│  │ ID Mappings │  │    Graph    │  │        Stacks           │  │
│  │             │  │             │  │                         │  │
│  │ EXTERNAL    │  │ GRAPH       │  │ STACKS                  │  │
│  │ INTERNAL    │  │ INODE_GRAPH │  │ STACK_CHANGES           │  │
│  │ NODE_TYPES  │  │ STACK_GRAPH │  │ REV_STACK_CHANGES       │  │
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
| `GRAPH` | GraphNode ([u8; 24]) | [GraphEdge] | Main graph — Shared stack edges (multimap) |
| `INODE_GRAPH` | (Inode, GraphNode) ([u8; 32]) | [GraphEdge] | File-scoped index |
| `STACK_GRAPH` | (stack_id, GraphNode) ([u8; 32]) | [GraphEdge] | Local workspace edges (multimap) |
| `STACKS` | name (str) | StackState (var) | Stack metadata (kind, parent, merkle) |
| `STACK_CHANGES` | (stack_id, seq) ([u8; 16]) | change_id (u64) | Change log |
| `REV_STACK_CHANGES` | (stack_id, change_id) ([u8; 16]) | seq (u64) | Reverse log |
| `TREE` | path (str) | inode (u64) | Path → inode |
| `REV_TREE` | inode (u64) | path (str) | Inode → path |
| `INODES` | inode (u64) | Position ([u8; 16]) | Inode → graph |
| `REV_INODES` | Position ([u8; 16]) | inode (u64) | Graph → inode |
| `DEPS` | change_id (u64) | [dep_id] (u64) | Dependencies |
| `REV_DEPS` | dep_id (u64) | [change_id] (u64) | Reverse deps |
| `STATES` | (stack_id, merkle) ([u8; 40]) | seq (u64) | State → sequence |
| `TAGS` | (stack_id, seq) ([u8; 16]) | merkle ([u8; 32]) | Tagged states |

### Transaction Model

```rust
// Read-only (multiple concurrent)
let txn = pristine.read_txn()?;
let stack = txn.get_stack("main")?;

// Read-write (exclusive)
let mut txn = pristine.write_txn()?;

// Backward-compatible: defaults to Shared, no parent
let mut stack = txn.open_or_create_stack("feature")?;

// Explicit kind and parent
let dev = txn.create_stack("dev", StackKind::Shared, Some(main.id))?;
let feature = txn.create_stack("feature", StackKind::Local, Some(dev.id))?;

txn.put_change(&mut stack, change_id, &hash)?;
txn.update_stack(&stack)?;
txn.commit()?;  // or txn.abort()?
```

### Trait Hierarchy

```
                    MutTxnT
        (Full read-write access)
        (put_stack_graph, del_stack_graph,
         del_stack_graph_prefix, create_stack)
                     │
        ┌────────────┼────────────┐
        ▼            ▼            ▼
    StackTxnT    TreeTxnT    GraphTxnT
   (Stack ops)  (File ops)  (Graph queries)
   (get_stack_by_id,          │
    resolve_overlay_chain,    │
    iter_stack_graph_adjacent)│
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

### Block Finding Methods (Critical for Apply)

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

**Symptom**: When applying changes, edges from inode to content weren't created correctly.

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
dual B-tree indexing strategy. This is critical for performance when outputting
file contents from the repository graph.

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
│  - Global operations                - Output/retrieve operations        │
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
| List stacks | O(s) | s = number of stacks |

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

### Two-Tier Stack Graph Model (Phase 1 complete, Phases 2-6 in progress)

The two-tier graph model enables Local workspaces (feature, bug, service-*)
to own their edges in `STACK_GRAPH` while Shared stacks (dev, release, main)
write to the global `GRAPH`. Deleting an Local workspace cascade-deletes its
edges with zero orphans.

**Phase 1 (complete)**: Foundation types and storage schema.
- `StackKind` enum (Local, Shared)
- `parent: Option<u64>` field on `StackState`
- `STACK_GRAPH` table with `(stack_id, vertex)` composite key
- `create_stack(name, kind, parent)` for explicit stack creation
- `put_stack_graph`, `del_stack_graph`, `del_stack_graph_prefix` on `MutTxnT`
- `get_stack_by_id`, `resolve_overlay_chain`, `iter_stack_graph_adjacent` on `StackTxnT`
- Backward-compatible serialization (v1 data reads as Shared, no parent)
- 37 integration tests covering all new functionality

**Phase 2 (planned)**: Apply path branching.
- `add_edge_with_reverse` branches on `StackKind`: Local → `put_stack_graph`, Shared → `put_graph`
- Thread `StackKind` through the apply pipeline

**Phase 3 (planned)**: Graph traversal overlay.
- `RetrieveOptions` gains `stack_chain: Vec<u64>` for overlay chain
- `retrieve_graph` reads from STACK_GRAPH chain ∪ GRAPH with deduplication
- `find_block` / `find_block_end` check STACK_GRAPH layers

**Phase 4 (planned)**: Stack lifecycle.
- `del_stack` cascade-deletes `STACK_GRAPH` prefix for Local workspaces
- Parent cycle detection, child reparenting

**Phase 5 (complete)**: Apply between stacks + cross-stack diff.
- `apply_from_stack`, `cherry_pick`, `apply_change_rec` already work with the
  two-tier model (Phase 2's `ApplyTarget` routes edges correctly per `StackKind`)
- `get_file_content_via_overlay`: reads file content through `OverlayTxn` so
  local workspaces see their `STACK_GRAPH` edges + global `GRAPH`
- `diff_stacks(a, b)`: change-level diff (only_in_a, only_in_b, common)
- `OverlayTxn` implements `TreeTxnT` + `StackTxnT` (pass-through to inner)
  so it can be used anywhere `GraphTxnT + TreeTxnT` is required

**Phase 6 (complete)**: CLI and UX.
- `stack new --local --parent dev` creates local workspaces with explicit parent
- `stack new` without flags preserves backward-compatible behavior (shared, fork from current)
- `stack list --verbose` shows `[shared]`/`[isolated]` tags and parent name
- `StackInfo` gains `kind: StackKind` and `parent_name: Option<String>` fields
- `Repository::get_stack_info` resolves parent ID → name for display

Relevant code: `atomic-core/src/pristine/traits.rs` (StackKind, StackState),
`atomic-core/src/pristine/tables.rs` (STACK_GRAPH), `atomic-core/src/pristine/txn/`,
`atomic-core/src/pristine/overlay.rs` (OverlayTxn), `atomic-core/src/apply/mod.rs`
(ApplyTarget), `atomic-repository/src/repository/content.rs` (get_file_content_via_overlay,
diff_stacks), `atomic-repository/src/repository/mod.rs` (StackInfo with kind/parent),
`atomic-cli/src/commands/stack/new.rs` (--local, --parent flags),
`atomic-cli/src/commands/stack/list.rs` (kind/parent in verbose output).

### Stack Workflow Commands (planned)

Advanced stack manipulation: `unrecord`, `reinsert`, `revise`, cross-stack `apply`,
per-stack `tag`. These build on the existing `Repository::unrecord()` and
`Repository::reinsert_change()` methods.

Key design: a change reference syntax (`@`, `@~1`, `@~N`, `stack@`) for
addressing changes within stacks. See the Phase 13 section of the CRDT
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
│   │       └── write/      # Split by trait: graph, stack, tree, mutate
│   ├── change/             # Change representation, headers, provenance
│   ├── record/
│   │   └── workflow/
│   │       ├── globalize/  # Position resolution, vertex creation, pipeline
│   │       ├── crdt/       # CRDT operation generation
│   │       └── ...
│   ├── apply/              # Graph application, conflict detection
│   └── output/             # Working copy output, alive graph traversal
│       ├── repo/
│       └── alive/

atomic-repository/
├── src/
│   ├── repository/         # Split by domain:
│   │   ├── mod.rs          # Struct, init, open, path helpers
│   │   ├── stacks.rs       # Stack CRUD
│   │   ├── changes.rs      # Change save/load/delete
│   │   ├── record.rs       # Record workflow
│   │   ├── apply.rs        # Apply changes
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
│   │   ├── push/, pull/, clone/, stack/, tag/, ...
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
cargo test test_stack_operations

# Check documentation
cargo doc --open
```

### CLI Usage

```bash
# Show help
cargo run -p atomic -- --help

# Initialize a new repository
cargo run -p atomic -- init

# Initialize with a specific stack name
cargo run -p atomic -- init --stack main

# Initialize with project-specific ignore patterns
cargo run -p atomic -- init --kind rust

# Show status (stub - not yet implemented)
cargo run -p atomic -- status

# Add files (stub - not yet implemented)
cargo run -p atomic -- add src/main.rs

# Record changes (stub - not yet implemented)
cargo run -p atomic -- record -m "Initial commit"

# View history (stub - not yet implemented)
cargo run -p atomic -- log

# Manage stacks (stub - not yet implemented)
cargo run -p atomic -- stack list
cargo run -p atomic -- stack new feature
cargo run -p atomic -- stack switch main
```

## License

Dual-licensed under MIT and Apache 2.0.
