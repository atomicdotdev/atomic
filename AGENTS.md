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
├── atomic-identity/      # User identity & Ed25519 signing ✅ Phase 11 Complete
└── atomic-repository/    # High-level repository operations
```

### Related Projects

- **atomic-remote-client** (`atomic-enterprise/atomic-remote`) - Clean-room HTTP client for remote operations (Phase 9.4.1 ✅)
- **atomic-api** (`atomic-enterprise/atomic-api`) - Server-side HTTP API for remote operations
- **atomic** (original) - Reference implementation (protocol behavior only, no code copied)

## Core Concepts

### 1. Repository Graph

Files are represented as directed acyclic graphs (DAGs):

```
┌─────────┐     ┌─────────┐     ┌─────────┐
│ Vertex  │────▶│ Vertex  │────▶│ Vertex  │
│ "Hello" │     │ " "     │     │ "World" │
└─────────┘     └─────────┘     └─────────┘
     │               │               │
     └───────────────┴───────────────┘
              Content of file
```

- **Vertices**: Contiguous chunks of content within a change
- **Edges**: Ordered relationships between vertices (with flags)

### 2. Changes (Patches)

Atomic transformations that add/remove vertices and edges:

```rust
// A change is identified by its content hash
let hash = Hash::of(change_content);

// Changes are registered to get a repository-local ID
let node_id = txn.register_change(&hash)?;
```

### 3. Stacks (Not Branches!)

**Critical Concept**: Stacks are **views** of the graph, not forks.

| Aspect | Git Branches | Atomic Stacks |
|--------|--------------|---------------|
| Data Model | Pointer to commit | Ordered sequence of applied changes |
| Storage | Duplicates history | Shares underlying graph |
| "Merging" | 3-way merge | Apply missing changes |
| State | HEAD commit hash | Merkle hash of sequence |

```rust
// Create a stack - it's just a view, not a fork
let mut stack = txn.open_or_create_stack("feature")?;

// Apply a change to the stack
txn.put_change(&mut stack, change_id, &hash)?;

// The stack's Merkle state uniquely identifies its sequence
println!("State: {}", stack.state);
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
│  │ NODE_TYPES  │  │             │  │ REV_STACK_CHANGES       │  │
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
| `GRAPH` | GraphNode ([u8; 24]) | [GraphEdge] | Main graph (multimap) |
| `INODE_GRAPH` | (Inode, GraphNode) ([u8; 32]) | [GraphEdge] | File-scoped index |
| `STACKS` | name (str) | StackState (var) | Stack metadata |
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
let mut stack = txn.open_or_create_stack("feature")?;
txn.put_change(&mut stack, change_id, &hash)?;
txn.update_stack(&stack)?;
txn.commit()?;  // or txn.abort()?
```

### Trait Hierarchy

```
                    MutTxnT
        (Full read-write access)
                     │
        ┌────────────┼────────────┐
        ▼            ▼            ▼
    StackTxnT    TreeTxnT    GraphTxnT
   (Stack ops)  (File ops)  (Graph queries)
        │            │            │
        └────────────┼────────────┘
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

Every public item should have:

```rust
/// Brief one-line description.
///
/// Longer explanation of what this does, why it exists,
/// and how it fits into the larger system.
///
/// # Arguments
///
/// * `param` - What this parameter is for
///
/// # Returns
///
/// What the return value means.
///
/// # Errors
///
/// When and why this can fail.
///
/// # Examples
///
/// ```
/// let result = function(arg)?;
/// assert!(result.is_valid());
/// ```
pub fn function(param: Type) -> Result<Output, Error> { ... }
```

### Record Style

Use conventional records:
- `feat:` New features
- `fix:` Bug fixes
- `refactor:` Code restructuring
- `docs:` Documentation updates
- `test:` Test additions/changes

## Implementation Roadmap



#### Phase 9.3: Stack Commands
- [ ] **Stack Command Router** (`atomic/src/commands/stack/mod.rs`)
  - [ ] Subcommand routing
- [ ] **`stack new`** (`atomic/src/commands/stack/new.rs`)
  - [ ] CLI arguments: `name`, `--from`, `--switch`
- [ ] **`stack switch`** (`atomic/src/commands/stack/switch.rs`)
  - [ ] CLI arguments: `name`
- [ ] **`stack delete`** (`atomic/src/commands/stack/delete.rs`)
  - [ ] CLI arguments: `name`, `--force`
- [ ] **`stack list`** (`atomic/src/commands/stack/list.rs`)
  - [ ] CLI arguments: `--verbose`


##### Phase 9.4.7: Remote Management Command 🔄 Planned
- [ ] **`remote` Command** (`atomic/atomic-cli/src/commands/remote.rs`)
  - [ ] `remote` (no args) - List all remotes with URLs
  - [ ] `remote add <name> <url>` - Add new remote
  - [ ] `remote remove <name>` - Remove a remote
  - [ ] `remote set-url <name> <url>` - Update remote URL
  - [ ] `remote default <name>` - Set default remote for push/pull

- [ ] **Remote Configuration in `atomic-repository`**
  - [ ] `RemoteConfig` struct in repository config
  - [ ] Serialization to `.atomic/config.toml`
  - [ ] Methods: `add_remote()`, `remove_remote()`, `get_remote()`, `list_remotes()`

##### Phase 9.4.8: Testing Strategy

- [ ] **Unit Tests in `atomic-remote`**
  - [ ] URL parsing and normalization
  - [ ] Changelist parsing (with/without trailing dot)
  - [ ] State response parsing
  - [ ] Error type conversions
  - [ ] Content encoding/decoding

- [ ] **Integration Tests in `atomic-cli`**
  - [ ] Push to `LocalRemote` (mock)
  - [ ] Pull from `LocalRemote` (mock)
  - [ ] Clone from `LocalRemote` (mock)
  - [ ] Round-trip: init → record → push → clone → record → push → pull

- [ ] **End-to-End Tests** (requires running `atomic-api`)
  - [ ] Push to HTTP API
  - [ ] Pull from HTTP API
  - [ ] Clone from HTTP API
  - [ ] Error handling (auth, not found, conflicts)

##### Phase 9.4.9: Documentation
- [ ] `atomic-remote` crate README with usage examples
- [ ] Remote configuration format in atomic docs
- [ ] Troubleshooting guide for common errors
- [ ] API setup guide for local development

##### Architecture Diagram

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                    Clean-Room Remote Implementation                          │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  ┌───────────────────────────────────────────────────────────────────────┐  │
│  │                           atomic-cli                                   │  │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  │  │
│  │  │    push     │  │    pull     │  │    clone    │  │   remote    │  │  │
│  │  │   command   │  │   command   │  │   command   │  │   command   │  │  │
│  │  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘  │  │
│  │         │                │                │                │         │  │
│  │         └────────────────┴────────────────┴────────────────┘         │  │
│  │                                   │                                   │  │
│  │                                   ▼                                   │  │
│  │  ┌─────────────────────────────────────────────────────────────────┐ │  │
│  │  │                    atomic-repository                             │ │  │
│  │  │  Repository, ChangeStore, Pristine, Status, History, etc.       │ │  │
│  │  └─────────────────────────────────────────────────────────────────┘ │  │
│  └───────────────────────────────────────────────────────────────────────┘  │
│                                   │                                         │
│                                   │ uses                                    │
│                                   ▼                                         │
│  ┌───────────────────────────────────────────────────────────────────────┐  │
│  │                       atomic-enterprise                                │  │
│  │  ┌─────────────────────────────────────────────────────────────────┐ │  │
│  │  │                     atomic-remote (NEW)                          │ │  │
│  │  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐ │ │  │
│  │  │  │ RemoteRepo  │  │ HttpRemote  │  │ LocalRemote (testing)   │ │ │  │
│  │  │  │   trait     │  │   struct    │  │       struct            │ │ │  │
│  │  │  └─────────────┘  └──────┬──────┘  └─────────────────────────┘ │ │  │
│  │  └──────────────────────────┼────────────────────────────────────┘ │  │
│  │                             │ HTTP                                  │  │
│  │                             ▼                                       │  │
│  │  ┌─────────────────────────────────────────────────────────────────┐ │  │
│  │  │                       atomic-api                                 │ │  │
│  │  │  POST ?apply={hash}     GET ?change={hash}                      │ │  │
│  │  │  POST ?tagup={state}    GET ?tag={state}                        │ │  │
│  │  │  GET ?changelist=...    GET ?state=...                          │ │  │
│  │  └─────────────────────────────────────────────────────────────────┘ │  │
│  └───────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘

Key Points:
1. atomic-remote is a NEW crate in atomic-enterprise (clean-room implementation)
2. atomic-cli commands use atomic-remote via Cargo dependency
3. No code from original atomic/atomic-remote is used
4. Protocol derived from atomic-api server implementation
5. User-Agent: atomic-{version} enables CLI detection in API
```

### Phase 11: Identity Management ✅ Complete

The `atomic-identity` crate provides comprehensive identity management for Atomic VCS,
supporting multiple identities, agent delegation, and cryptographic signing.

#### Phase 11.1: Core Identity Types ✅ Complete
- [x] **Identity struct** (`identity.rs`) - 26 tests
  - [x] `IdentityId` - Unique identifier derived from public key (Blake3 hash)
  - [x] `Identity` - Complete identity with name, email, keys, type, usage
  - [x] `IdentityType` enum (User, Agent, Delegated)
  - [x] `IdentityMetadata` - Creation time, expiry, description
  - [x] `IdentityBuilder` - Fluent builder pattern for identity construction
  - [x] `Author` struct for change header integration

- [x] **Usage Contexts** (`usage.rs`) - 16 tests
  - [x] `IdentityUsage` enum (Personal, Work, Community, Bot, Custom)
  - [x] Helper methods: `is_personal()`, `is_work()`, `is_community()`, `is_bot()`
  - [x] Parsing from strings with `IdentityUsage::parse()`
  - [x] Serialization/deserialization support

#### Phase 11.2: Cryptographic Operations ✅ Complete
- [x] **Key Pair Management** (`keypair.rs`) - 7 tests
  - [x] `PublicKey` - Ed25519 public key with base32 encoding
  - [x] `SecretKey` - Ed25519 secret key (not Clone/Serialize for security)
  - [x] `KeyPair` - Combined public/secret key for signing
  - [x] Signature verification via `PublicKey::verify()`

- [x] **Signing Module** (`signing.rs`) - 18 tests
  - [x] `Signature` - Ed25519 signature with base32 encoding
  - [x] `Signer` - Creates signatures from secret keys
  - [x] `SignedData` - Data with attached signature
  - [x] `SignatureInfo` - Rich signature metadata (signer, timestamp, reason)
  - [x] `SignatureSet` - Multi-party signing support
  - [x] `VerificationResult` - Detailed verification outcomes

#### Phase 11.3: Delegation Support ✅ Complete
- [x] **Delegation Module** (`delegation.rs`) - 15 tests
  - [x] `DelegationPermission` enum (Read, Record, Push, Pull, ManageStacks, ManageTags, Admin, Full)
  - [x] `DelegationScope` - Permissions + repository/stack patterns
  - [x] `DelegationScopeBuilder` - Fluent builder for scopes
  - [x] `Delegation` - Certificate authorizing agent to act on behalf of user
  - [x] `DelegationId` - Unique delegation identifier
  - [x] Permission checking with `Delegation::allows()`
  - [x] Expiry and revocation support

#### Phase 11.4: Identity Storage ✅ Complete
- [x] **Identity Store** (`store.rs`) - 12 tests
  - [x] `IdentityStore` - Persistent storage for identities
  - [x] `StoreConfig` - Default identity and per-usage defaults
  - [x] `LoadOptions` - Options for loading (public-only, with secret, with password)
  - [x] `IdentityFilter` - Filter identities by usage, type, name
  - [x] Save/load identities with optional encrypted secret keys
  - [x] Default identity management (global and per-usage)
  - [x] List/filter/delete operations

#### Identity Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                        Identity Management Architecture                      │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │                         Identity Types                               │   │
│  │                                                                      │   │
│  │  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────────┐  │   │
│  │  │     User     │  │    Agent     │  │       Delegated          │  │   │
│  │  │  (Human)     │  │  (AI/Bot)    │  │  (Agent on behalf of)    │  │   │
│  │  └──────────────┘  └──────────────┘  └──────────────────────────┘  │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│                                                                             │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │                         Usage Contexts                               │   │
│  │                                                                      │   │
│  │  ┌──────────┐  ┌──────────┐  ┌───────────┐  ┌─────┐  ┌──────────┐ │   │
│  │  │ Personal │  │   Work   │  │ Community │  │ Bot │  │  Custom  │ │   │
│  │  └──────────┘  └──────────┘  └───────────┘  └─────┘  └──────────┘ │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│                                                                             │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │                         Delegation Model                             │   │
│  │                                                                      │   │
│  │  User Identity ──delegates to──▶ Agent Identity                     │   │
│  │        │                              │                              │   │
│  │        │ signs                        │ operates within              │   │
│  │        ▼                              ▼                              │   │
│  │  Delegation Certificate ◀──defines── DelegationScope                │   │
│  │  (expiry, revocation)                 (permissions, patterns)        │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│                                                                             │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │                         Storage Layout                               │   │
│  │                                                                      │   │
│  │  ~/.atomic/identities/                                              │   │
│  │  ├── config.toml              # Store config (defaults)             │   │
│  │  ├── alice-personal-ABCD1234/                                       │   │
│  │  │   ├── identity.toml        # Identity metadata                   │   │
│  │  │   └── secret.key           # Encrypted secret key                │   │
│  │  └── ci-bot-EFGH5678/                                               │   │
│  │      └── identity.toml        # Agent identity (no secret key)      │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

#### Integration with Change Headers

Identities integrate with atomic-core's `Author` type for change attribution:

```rust
use atomic_identity::{Identity, IdentityUsage};

// Create identity
let identity = Identity::builder("alice")
    .email("alice@example.com")
    .usage(IdentityUsage::Work)
    .build()?;

// Convert to Author for change headers
let author = identity.to_author();
// author.name = "alice"
// author.email = Some("alice@example.com")
// author.identity = Some("<base32 public key>")
```

#### Test Coverage (79 tests)
- identity.rs: 26 tests (IdentityId, Identity, IdentityBuilder, IdentityMetadata)
- usage.rs: 16 tests (IdentityUsage enum and methods)
- keypair.rs: 7 tests (PublicKey, SecretKey, KeyPair)
- signing.rs: 18 tests (Signature, Signer, SignedData, SignatureInfo, SignatureSet)
- delegation.rs: 15 tests (DelegationPermission, DelegationScope, Delegation)
- store.rs: 12 tests (IdentityStore, StoreConfig, LoadOptions, IdentityFilter)
- lib.rs: 6 tests (integration tests)
- Doc tests: 8 passing

### Phase 12: Semantic Layer 🔄 In Progress

Implement the **required** semantic layer (`TrunkOp`, `BranchOp`, `LeafOp`) that
makes the graph understandable for code review and developer workflows.

#### Design Philosophy

**Graph = Storage. CRDT = Understanding. Both are required.**

| Layer | Purpose | Types |
|-------|---------|-------|
| **Graph (Storage)** | Persistence, content-addressing, merging | `Hunk`, `Atom`, `NewVertex`, `EdgeMap`, `Vertex`, `Edge` |
| **Semantic (Interpretation)** | Lines, tokens, human-readable diffs | `TrunkOp`, `BranchOp`, `LeafOp`, `FileOps` |

The graph alone cannot support modern code review:
- "Bytes 1024-1089 changed" is useless for review
- "Position 9 in change X" means nothing to developers
- Byte-level blame doesn't answer "who wrote this function?"

Semantic operations are **required** to provide:
- Line-level diffs ("line 42 was modified")
- Token-level diffs (`--word-diff` shows "changed `foo` to `bar`")
- Fine-grained blame (which change introduced each token)
- Semantic conflict resolution

#### Phase 12.1: Semantic Operations Module ✅ Complete
- [x] **Create Semantic Operations Module** (`atomic-core/src/change/ops.rs`) - 19 tests
  - [x] `FileOps` struct (trunk-level operations)
  - [x] `LineOps` struct (branch-level operations)
  - [x] `FileOpsStats` for change statistics
  - [x] Serialization/deserialization with bincode and JSON

- [x] **Add `file_ops` field to `HashedChange`** (`atomic-core/src/change/change.rs`)
  - [x] Add `file_ops: Vec<FileOps>` field alongside `hunks`
  - [x] Keep `hunks: Vec<Hunk<H>>` as the **primary** graph operations
  - [x] `file_ops` is optional semantic metadata for enhanced features

#### Phase 12.2: Record Workflow - Generate Both Formats ✅ Complete
- [x] **Dual-Format Recording** (`atomic-core/src/record/workflow/`)
  - [x] Generate Hunks for graph storage (primary)
  - [x] Generate FileOps for semantic overlay (optional)
  - [x] `RecordedFile` has both `hunks` and `file_ops` fields
  - [x] `from_file_ops()` constructor for CRDT-enhanced recording
  - [x] Statistics track both hunk count and line/token counts

- [x] **Update `atomic-repository` Recording** (`atomic-repository/src/`)
  - [x] Record changes with both Hunks (graph) and FileOps (CRDT)
  - [x] Both formats are generated together during recording
  - [x] Apply populates both graph tables and CRDT tables

#### Phase 12.3: CRDT Apply for Semantic Tables 🔄 In Progress
- [x] **Semantic Apply Module** (`atomic-core/src/apply/crdt.rs`) - 35 tests
  - [x] `apply_file_ops()` populates CRDT tables (TRUNKS, BRANCHES, LEAVES)
  - [x] Also populates GRAPH table for output system compatibility
  - [x] `CrdtApplyStats` tracks apply statistics
  - [x] Trunk/Branch/Leaf operations implemented

- [ ] **Remaining Work**
  - [ ] Debug content retrieval mismatch in integration tests
  - [ ] Verify CRDT tables correctly index into graph content
  - [ ] Ensure semantic layer and graph layer are consistent

**Current Status**: Semantic tables are populated. Need to verify the semantic layer
correctly interprets the graph content for display.

#### Phase 12.4: Semantic Display Features
- [ ] **Token-Level Diff Display** (`atomic/src/commands/diff.rs`)
  - [ ] Use `FileOps` from stored changes for all diffs
  - [ ] Display line numbers and token changes (not byte ranges)
  - [ ] `--word-diff` shows token additions/deletions/replacements

- [ ] **Fine-Grained Blame**
  - [ ] Use Semantic Leaf IDs for token-level blame
  - [ ] Answer "who wrote this specific token?"
  - [ ] Support line-level and token-level granularity

#### Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                         Change Storage Architecture                          │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  HashedChange                                                               │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │  hunks: Vec<Hunk>           ← PRIMARY: Graph operations              │   │
│  │  file_ops: Vec<FileOps>     ← OVERLAY: Semantic metadata (optional)  │   │
│  │  contents: Vec<u8>          ← Content blob (shared by both)          │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│                                                                             │
│  Apply Flow:                                                                │
│  ┌──────────────┐                                                          │
│  │ apply_hunk() │ ──▶ GRAPH table (vertices, edges) ──▶ Output system     │
│  └──────────────┘                                                          │
│  ┌───────────────────┐                                                     │
│  │ apply_file_ops()  │ ──▶ Semantic tables ( index) ──▶ Diff, Blame        │
│  └───────────────────┘                                                     │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

#### Benefits of Two-Layer Architecture
- **Separation of concerns**: Graph handles storage, CRDT handles semantics
- **Efficient storage**: Graph stores bytes compactly
- **Human-readable display**: CRDT provides lines and tokens
- **Modern code review**: Developers see meaningful diffs, not byte offsets
- **Competitive with Git/GitHub**: Line-level and word-level operations

### Phase 13: Stack Workflow Commands 🔄 Planned

Advanced stack manipulation commands for flexible change management workflows.

#### Design Philosophy

**Changes are immutable but repositionable. Stacks are views of the same graph.**

| Command | Level | Purpose |
|---------|-------|---------|
| `unrecord` | Low-level | Remove change from stack (preserves change data) |
| `reinsert` | Low-level | Put change back into stack at position |
| `revise` | High-level | Modify change in-place (unrecord + re-record + re-apply) |
| `apply` | Cross-stack | Apply changes from another stack |
| `tag` | Metadata | Named state snapshots per-stack |

#### Phase 13.1: Change Reference System
- [ ] **Reference Parsing** (`atomic-core/src/reference.rs`)
  - [ ] `@` = current/last change in stack
  - [ ] `@~1` = previous change (parent)
  - [ ] `@~N` = N changes back
  - [ ] `@{hash-prefix}` = specific change by hash
  - [ ] `stack@` = last change in named stack
  - [ ] `stack@~1` = previous change in named stack

- [ ] **Reference Resolution** (`atomic-repository/src/reference.rs`)
  - [ ] `resolve_ref()` → `(StackName, ChangeId, Hash)`
  - [ ] Validate reference exists
  - [ ] Handle ambiguous hash prefixes

#### Phase 13.2: Low-Level Primitives
- [ ] **`unrecord` Command** (`atomic/atomic-cli/src/commands/unrecord.rs`)
  - [ ] CLI: `atomic unrecord <ref>` (default: `@`)
  - [ ] CLI: `atomic unrecord @~1` (previous change)
  - [ ] CLI: `atomic unrecord --to @~3` (unrecord last 3)
  - [ ] Options: `--dry-run`, `--force`
  - [ ] Output: Shows unrecorded change hash for reinsertion
  - [ ] Working copy left with unrecorded changes applied

- [ ] **`reinsert` Command** (`atomic/atomic-cli/src/commands/reinsert.rs`)
  - [ ] CLI: `atomic reinsert <hash>`
  - [ ] CLI: `atomic reinsert <hash> --at @~2`
  - [ ] Options: `--at <position>`, `--dry-run`
  - [ ] Validates dependencies are satisfied

- [ ] **Repository Methods** (`atomic-repository/src/unrecord.rs`)
  - [ ] `Repository::unrecord()` - Remove change from stack
  - [ ] `Repository::reinsert()` - Add change back to stack
  - [ ] `Repository::can_unrecord()` - Check if safe to unrecord
  - [ ] Dependency tracking for safe operations

#### Phase 13.3: High-Level Revise Command
- [ ] **`revise` Command** (`atomic/atomic-cli/src/commands/revise.rs`)
  - [ ] CLI: `atomic revise` (revise last change)
  - [ ] CLI: `atomic revise @~1` (revise previous change)
  - [ ] CLI: `atomic revise @~1 -m "New message"` (with new message)
  - [ ] CLI: `atomic revise @~1 --reword` (only change message)
  - [ ] Options: `-m/--message`, `--reword`, `--no-edit`

- [ ] **Revise Workflow** (`atomic-repository/src/revise.rs`)
  ```
  revise(@~1):
  1. Unrecord @ (save as pending_1)
  2. Unrecord @~1 (this is the target)
  3. Working copy now has target's changes
  4. User edits files (or just message with --reword)
  5. Record new change (replaces target)
  6. Re-apply pending_1
  ```
  - [ ] `ReviseOptions` - target ref, message, reword-only
  - [ ] `ReviseOutcome` - new change hash, re-applied changes
  - [ ] Handle conflicts during re-apply

#### Phase 13.4: Cross-Stack Operations
- [ ] **`apply` Command** (`atomic/atomic-cli/src/commands/apply.rs`)
  - [ ] CLI: `atomic apply feature@` (apply last from feature)
  - [ ] CLI: `atomic apply feature@~1..feature@` (range)
  - [ ] CLI: `atomic apply --from feature --to main:v1.0` (up to tag)
  - [ ] CLI: `atomic apply --cherry-pick <hash1> <hash2>` (specific changes)
  - [ ] Options: `--dry-run`, `--no-deps`

- [ ] **Cross-Stack Apply** (`atomic-repository/src/cross_apply.rs`)
  - [ ] `CrossApplyOptions` - source stack, target stack, range/selection
  - [ ] `CrossApplyOutcome` - applied changes, conflicts
  - [ ] Automatic dependency resolution
  - [ ] Conflict handling strategies

#### Phase 13.5: Per-Stack Tags
- [ ] **Tag Storage** (`.atomic/tags/{stack}/{name}.tag`)
  - [ ] Allows same tag name in different stacks
  - [ ] Lightweight tags (just state hash)
  - [ ] Annotated tags (message, author, timestamp)

- [ ] **`tag` Command** (`atomic/atomic-cli/src/commands/tag.rs`)
  - [ ] CLI: `atomic tag v1.0` (tag current state)
  - [ ] CLI: `atomic tag v1.0 -m "Release 1.0"` (annotated)
  - [ ] CLI: `atomic tag --list` (list tags in current stack)
  - [ ] CLI: `atomic tag --list --all` (all stacks)
  - [ ] CLI: `atomic tag --delete v1.0`
  - [ ] CLI: `atomic tag --stack feature v1.0` (tag other stack)

- [ ] **Tag Methods** (`atomic-repository/src/tags.rs`)
  - [ ] `Repository::create_tag()` - Create tag for stack state
  - [ ] `Repository::get_tag()` - Resolve tag to state
  - [ ] `Repository::list_tags()` - List tags with filters
  - [ ] `Repository::delete_tag()` - Remove tag

#### Phase 13.6: Log with References
- [ ] **Enhanced `log` Command** (`atomic/atomic-cli/src/commands/log.rs`)
  - [ ] Show `@`, `@~1`, `@~2` references in output
  - [ ] Show tags inline with changes
  - [ ] CLI: `atomic log --oneline` (compact)
  - [ ] CLI: `atomic log --graph` (ASCII graph)
  - [ ] CLI: `atomic log feature` (other stack's log)

#### Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                      Stack Workflow Architecture                             │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  Reference System                                                           │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │  @          → Current change (HEAD equivalent)                       │   │
│  │  @~1        → Previous change                                        │   │
│  │  @~N        → N changes back                                         │   │
│  │  feature@   → Last change in 'feature' stack                         │   │
│  │  main:v1.0  → Tag 'v1.0' in 'main' stack                            │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│                                                                             │
│  Command Hierarchy                                                          │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │                                                                      │   │
│  │  High-Level (Workflows)           Low-Level (Primitives)            │   │
│  │  ┌──────────────────────┐        ┌──────────────────────┐          │   │
│  │  │  revise              │        │  unrecord            │          │   │
│  │  │  (modify in-place)   │───────▶│  (remove from stack) │          │   │
│  │  └──────────────────────┘        └──────────────────────┘          │   │
│  │                                           │                         │   │
│  │                                           ▼                         │   │
│  │                                  ┌──────────────────────┐          │   │
│  │                                  │  reinsert            │          │   │
│  │                                  │  (add back to stack) │          │   │
│  │                                  └──────────────────────┘          │   │
│  │                                                                      │   │
│  │  Cross-Stack                      Metadata                          │   │
│  │  ┌──────────────────────┐        ┌──────────────────────┐          │   │
│  │  │  apply               │        │  tag                 │          │   │
│  │  │  (changes between    │        │  (named state        │          │   │
│  │  │   stacks)            │        │   snapshots)         │          │   │
│  │  └──────────────────────┘        └──────────────────────┘          │   │
│  │                                                                      │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│                                                                             │
│  Revise Workflow Example                                                    │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │                                                                      │   │
│  │  Before:  [A] ← [B] ← [C] ← @                                       │   │
│  │                   ↑                                                  │   │
│  │              want to revise                                          │   │
│  │                                                                      │   │
│  │  Step 1: unrecord @ (C saved as pending)                            │   │
│  │           [A] ← [B] ← @                                              │   │
│  │                                                                      │   │
│  │  Step 2: unrecord @ (B is now target)                               │   │
│  │           [A] ← @    working copy has B's changes                   │   │
│  │                                                                      │   │
│  │  Step 3: user edits, record as B'                                   │   │
│  │           [A] ← [B'] ← @                                            │   │
│  │                                                                      │   │
│  │  Step 4: re-apply pending (C)                                       │   │
│  │           [A] ← [B'] ← [C] ← @                                      │   │
│  │                                                                      │   │
│  │  After:   B has been revised to B', C re-applied on top             │   │
│  │                                                                      │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

#### Key Differences from Git

| Aspect | Git | Atomic |
|--------|-----|--------|
| Amend/Revise | Only HEAD, rewrites history | Any change, preserves graph |
| Rebase | Complex, rewrites commits | Natural via unrecord/reinsert |
| Cherry-pick | Copies commit | Applies same change (shared graph) |
| Tags | Global namespace | Per-stack namespacing |
| References | SHA + branch pointers | `@~N` syntax + stack context |

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

### Current Test Coverage

- **CLI tests**: 365+ in atomic crate (error, output, commands)
  - Error: 87 tests (error types, classification, suggestions, exit codes)
  - Colors: 46 tests (ColorMode, status colors, file colors, StatusChar)
  - Progress: 31 tests (spinners, progress bars, multi-progress, finish states)
  - Table: 46 tests (Alignment, Column, Table, KeyValueTable, truncation)
  - Commands/mod: 20 tests (repository discovery, hash formatting, timestamps)
  - Init command: 42 tests (creation, validation, ignore templates, integration)
  - Status command: 58 tests (StatusOutputConfig, status_code, status_description, format tests, integration)
  - Add command: 45 tests (builder pattern, TrackingOptions conversion, AggregateStats, format_count, integration)
  - Record command: 45 tests (builder pattern, algorithm parsing, author parsing, options, integration)
- **Unit tests**: 92 in pristine module (core transactions)
- **CRDT tests**: 133 in crdt module (hierarchical Trunk → Branch → Leaf model)
  - ids.rs: 21 tests (TrunkId, BranchId, LeafId creation, ordering, serialization)
  - trunk.rs: 22 tests (Trunk, TrunkState, TrunkOp)
  - branch.rs: 24 tests (Branch, BranchState, BranchOp)
  - leaf.rs: 24 tests (Leaf, LeafState, LeafOp)
  - tables.rs: 37 tests (ID/value encoding, state encoding, roundtrips, ordering)
  - mod.rs: 5 tests (hierarchy integration, CRDT semantics)
- **Inode graph tests**: 27 in inode_graph module (dual B-tree traversal)
- **Change tests**: 263 in change module (including provenance, credit, and ChangeStore trait)
  - ChangeStore: 33 tests (trait, MemoryChangeStore, content retrieval, thread safety)
- **Diff tests**: 292 in diff module (algorithm, line, ops, split, myers, patience, token, word, inline)
  - Token: 68 tests (tokenization, operators, strings, numbers, comments)
  - Word: 40 tests (word-level diff operations, merging, configuration)
  - Inline: 37 tests (span handling, rendering, gap filling)
- **Record tests**: 433 in record module (error, item types, builder, detect, context, workflow)
  - detect.rs: 59 tests (DetectOptions, FileChange, FileChangeKind, DetectResult, content comparison)
  - context.rs: 32 tests (DetectContext, RecordContext, RecordItem, PristineFileState)
  - workflow/ module: 272 tests (modular workflow implementation)
    - options.rs: 30 tests (WorkflowOptions builder pattern, prefix matching, size limits)
    - collect.rs: 33 tests (TrackedFile, WorkingFile, CollectionResult, file collection, walk_files)
    - compare.rs: 27 tests (CompareResult, encoding detection, content comparison, diffing)
    - hunk.rs: 56 tests (HunkBuildOptions, PendingChange, BuiltHunk, HunkBuilder)
    - detect.rs: 46 tests (DetectionOptions, DetectedFile, DetectionKind, DetectionResult)
    - record.rs: 60 tests (RecordingOptions, RecordingStats, RecordedFile, RecordingResult, CRDT integration)
- **Apply tests**: 247 in apply module (error, workspace, change, position, vertex, edge, conflict)
- **Output tests**: 630 in output module (error, traits, memory, filesystem, alive graph, ordering, repo)
  - FileSystem: 64 tests (read, write, directory ops, permissions, path traversal safety)
  - Repo module: 268 tests (options, outcome, conflicts, error handling, writer, content, file, repository, tree)
    - options.rs: 17 tests (builder pattern, prefix matching, time filtering)
    - outcome.rs: 31 tests (file/directory recording, merge, summary display)
    - conflict.rs: 28 tests (FileConflict, FileConflictType, builder methods)
    - error.rs: 20 tests (error types, conversions, source chain)
    - writer.rs: 21 tests (ConflictWriter, marker output, line tracking)
    - content.rs: 8 tests (ContentChunk, graph content output)
    - file.rs: 46 tests (FileOutputOptions, FileOutputResult, FileOutputError, output functions)
    - repository.rs: 53 tests (RepositoryOutputOptions, RepositoryOutputResult, OutputItem, errors)
    - tree.rs: 44 tests (TreeCollectOptions, TreeItem, TreeCollectResult, hierarchy building)
- **Repository tests**: 421 in atomic-repository (changestore, repository, status, tracking, apply, history, tags, unrecord, archive, content retrieval, ignore)
  - Apply: 19 tests (options, stats, outcome, error handling, dependency ordering, Repository methods, apply_recorded)
  - Ignore: 36 tests (IgnoreRules, pattern matching, whitelist, glob patterns, real-world patterns)
  - Tracking: 2 new tests (should_ignore_with_rules, collect_files_with_ignore_rules)
  - Repository: 6 new tests (ignore_rules, is_ignored, status_respects_atomicignore, include_ignored, add_respects_atomicignore)
- **Globalize tests**: 34 in globalize module (options, errors, caching, position resolution, vertex/edge creation, FileAdd handling)
- **Assembly tests**: 33 in assembly module (options, errors, context, stats, change creation, dependency collection)
  - Changestore: 39 tests (save/load, iteration, caching, error handling)
  - Repository: 63 tests (init, open, stacks, change storage, status, tracking, tags, archive integration)
  - Status: 29 tests (FileStatus, FileStatusEntry, RepositoryStatus, StatusOptions, helpers)
  - Tracking: 30 tests (add, remove, move, list, path normalization, ignore patterns)

  - History: 36 tests (HistoryEntry, HistoryOptions, HistorySummary, PathHistoryEntry, errors)
  - Tags: 34 tests (Tag, TagOptions, TagFilter, file operations, validation)
  - Unrecord: 27 tests (options, outcome, stats, dependency info, errors)
  - Archive: 31 tests (format, options, entry, manifest, outcome, directory archive)
  - Error: 8 tests (error types, detection methods)
- **Integration tests**: 18 in inode_graph_test
- **Type tests**: 87 in types_test
- **Remote client tests**: 61 in atomic-remote-client (atomic-enterprise)
  - error.rs: 15 tests (error variants, classification, suggestions)
  - types.rs: 37 tests (Node, NodeType, ChangelistEntry, StateResponse, PushDelta, PullDelta)
  - http.rs: 14 tests (HttpRemote, HttpRemoteConfig, URL parsing, changelist parsing)
- **Identity tests**: 79 in atomic-identity
  - identity.rs: 26 tests (IdentityId, Identity, IdentityBuilder, IdentityMetadata, Author)
  - usage.rs: 16 tests (IdentityUsage enum, parsing, serialization)
  - keypair.rs: 7 tests (PublicKey, SecretKey, KeyPair, signing)
  - signing.rs: 18 tests (Signature, Signer, SignedData, SignatureInfo, SignatureSet)
  - delegation.rs: 15 tests (DelegationPermission, DelegationScope, Delegation)
  - store.rs: 12 tests (IdentityStore, StoreConfig, LoadOptions, IdentityFilter)
  - lib.rs: 6 tests (integration tests)
- **Total**: 2,526+ library tests passing (2,026 atomic-core + 421 atomic-repository + 79 atomic-identity)
- **CLI tests**: 365 tests passing (307 with --test-threads=1)
- **Remote client**: 61 unit tests + 5 doc tests (atomic-enterprise/atomic-remote-client)
- **Doctests**: 206 passing
- **Grand Total**: 3,014+ passing tests

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
│   ├── lib.rs              # Crate root, re-exports
│   ├── types/              # Core data types
│   │   ├── mod.rs          # Type exports
│   │   ├── hash.rs         # Hash, Merkle, Hasher
│   │   ├── node_id.rs      # L64, NodeId, ChangePosition, Inode
│   │   ├── vertex.rs       # Vertex<H>
│   │   ├── position.rs     # Position<H>
│   │   └── edge.rs         # EdgeFlags, Edge, SerializedEdge
│   ├── diff/               # Diff algorithms
│   │   ├── mod.rs          # Module documentation & main diff() entry point
│   │   ├── algorithm.rs    # Algorithm enum (Myers, Patience)
│   │   ├── line.rs         # Line struct with FNV-1a hashing
│   │   ├── ops.rs          # DiffOp, DiffResult, Replacement
│   │   ├── split.rs        # LineSplit iterator, Separator
│   │   ├── myers.rs        # LCS-based diff implementation
│   │   ├── patience.rs     # LIS-based diff with unique anchors
│   │   ├── token.rs        # Token/word representation for word-level diff
│   │   ├── word.rs         # Word-level diff algorithm (CRDT-style)
│   │   └── inline.rs       # Inline diff display for code reviews
│   ├── crdt/               # Hierarchical CRDT graph model
│   │   ├── mod.rs          # Module exports, overview documentation
│   │   ├── ids.rs          # TrunkId, BranchId, LeafId (globally unique IDs)
│   │   ├── trunk.rs        # Trunk (file), TrunkState, TrunkOp
│   │   ├── branch.rs       # Branch (line), BranchState, BranchOp
│   │   ├── leaf.rs         # Leaf (token), LeafState, LeafOp
│   │   └── tables.rs       # Pristine table definitions & encoding helpers
│   ├── pristine/           # Storage layer
│   │   ├── mod.rs          # Module documentation & exports
│   │   ├── error.rs        # PristineError, PristineResult
│   │   ├── inode_graph.rs  # InodeGraphOps trait & dual B-tree optimization
│   │   ├── tables.rs       # Table definitions, key encoding
│   │   ├── traits.rs       # GraphTxnT, StackTxnT, TreeTxnT, MutTxnT
│   │   └── txn/            # Transaction implementations
│   │       ├── mod.rs      # Submodule exports
│   │       ├── helpers.rs  # Serialization, AdjIterator
│   │       ├── pristine.rs # Pristine database handle
│   │       ├── read.rs     # ReadTxn implementation (+ InodeGraphOps)
│   │       └── write.rs    # WriteTxn implementation (+ InodeGraphOps)
│   ├── change/             # Change representation
│   │   ├── mod.rs          # Module documentation & exports
│   │   ├── atom.rs         # Atom, NewVertex, EdgeMap, NewEdge
│   │   ├── change.rs       # Change, HashedChange, Offsets
│   │   ├── credit.rs       # AI-aware blame (Credit, CreditType, FileCredits)
│   │   ├── encoding.rs     # Encoding detection (UTF-8, Binary, etc.)
│   │   ├── header.rs       # ChangeHeader, Author, builder pattern
│   │   ├── hunk.rs         # Hunk enum (FileAdd, FileDel, Edit, etc.)
│   │   ├── local.rs        # Local, LocalByte (display context)
│   │   ├── provenance.rs   # AI provenance (vendor, model, tokens, cost)
│   │   └── store.rs        # ChangeStore trait, MemoryChangeStore
│   ├── record/             # Recording changes from working copy
│   │   ├── mod.rs          # Module documentation & exports
│   │   ├── error.rs        # RecordError, RecordResult
│   │   ├── item.rs         # InodeUpdate, FileMetadata, RecordItem
│   │   ├── builder.rs      # RecordBuilder, Recorded, RecordStats
│   │   ├── detect.rs       # Change detection (DetectOptions, FileChange, compare_content)
│   │   ├── context.rs      # DetectContext, RecordContext, PristineFileState
│   │   └── workflow/       # Modular workflow implementation
│   │       ├── mod.rs      # Module exports and documentation
│   │       ├── options.rs  # WorkflowOptions configuration with builder pattern
│   │       ├── collect.rs  # TrackedFile, WorkingFile, CollectionResult, file collection
│   │       ├── compare.rs  # CompareResult, encoding detection, content comparison
│   │       ├── hunk.rs     # HunkBuilder, BuiltHunk, PendingChange
│   │       ├── detect.rs   # DetectionOptions, DetectedFile, DetectionKind, DetectionResult
│   │       ├── record.rs   # RecordingOptions, RecordedFile, RecordingResult
│   │       ├── retrieve.rs # Content retrieval from pristine graph
│   │       ├── globalize.rs # GlobalizeContext, position resolution, vertex/edge creation
│   │       ├── assembly.rs  # AssemblyContext, change assembly, dependency collection
│   │       └── crdt/        # CRDT operation generation (Phase 10.3)
│   │           ├── mod.rs      # Module exports, integration documentation
│   │           ├── tokenize.rs # ContentTokenizer, TokenizedLine, TokenizedToken
│   │           ├── line_ops.rs # LineAnalyzer, LineChange, AnalysisOptions
│   │           ├── convert.rs  # HunkConverter, ConvertedOps, ConversionOptions
│   │           └── builder.rs  # CrdtChangeBuilder, FileOps, LineOps, TokenOps
│   ├── apply/              # Applying changes to the graph
│   │   ├── mod.rs          # Module documentation & exports
│   │   ├── error.rs        # ApplyError, LocalApplyError, results
│   │   ├── workspace.rs    # Workspace, PendingEdge, MissingContext, Zombie
│   │   ├── change.rs       # verify_dependencies, compute_new_state, validate_can_apply
│   │   ├── position.rs     # resolve_position, resolve_vertex, resolve_inode
│   │   ├── vertex.rs       # apply_new_vertex, add_edge_with_reverse
│   │   ├── edge.rs         # apply_edge_map, find_source_vertex, find_target_vertex
│   │   └── conflict.rs     # ConflictTracker, ZombieConflict, OrderConflict
│   └── output/             # Working copy output
│       ├── mod.rs          # Module documentation, Conflict, OutputItem, OutputStats
│       ├── error.rs        # OutputError, ContentError, TreeError, ConflictType
│       ├── filesystem.rs   # FileSystem real filesystem implementation
│       ├── memory.rs       # Memory working copy implementation for testing
│       ├── traits.rs       # WorkingCopyRead, WorkingCopy, VertexBuffer, FileMetadata
│       ├── repo/           # Repository output (modular structure)
│       │   ├── mod.rs      # Module exports and documentation
│       │   ├── options.rs  # OutputOptions configuration with builder pattern
│       │   ├── outcome.rs  # OutputOutcome, FileWritten for result tracking
│       │   ├── conflict.rs # FileConflict, FileConflictType
│       │   ├── error.rs    # OutputError, OutputResult
│       │   ├── writer.rs   # ConflictWriter for conflict marker output
│       │   ├── content.rs  # Graph content output, ContentChunk
│       │   ├── file.rs     # Single file output (output_file, FileOutputOptions)
│       │   ├── repository.rs # Full repository output (output_repository, OutputItem)
│       │   └── tree.rs     # Tree traversal (collect_tree, TreeItem, TreeCollectOptions)
│       └── alive/          # Alive graph traversal
│           ├── mod.rs      # Module exports, RedundantEdge
│           ├── vertex.rs   # AliveVertex, VertexId, VertexFlags
│           ├── graph.rs    # AliveGraph, GraphStats
│           ├── retrieve.rs # retrieve_graph, RetrieveOptions, RetrieveResult
│           └── order.rs    # Tarjan SCC, ConflictTree, ConflictPath, PathElement
└── tests/
    ├── types_test.rs       # Comprehensive type tests
    └── inode_graph_test.rs # Graph indexing tests

atomic-identity/
├── src/
│   ├── lib.rs              # Crate root, re-exports all public types
│   ├── error.rs            # IdentityError enum
│   ├── identity.rs         # Identity, IdentityId, IdentityType, IdentityMetadata, Author
│   ├── keypair.rs          # PublicKey, SecretKey, KeyPair (Ed25519)
│   ├── usage.rs            # IdentityUsage (Personal, Work, Community, Bot, Custom)
│   ├── delegation.rs       # Delegation, DelegationScope, DelegationPermission
│   ├── signing.rs          # Signature, Signer, SignedData, SignatureInfo, SignatureSet
│   └── store.rs            # IdentityStore, StoreConfig, LoadOptions, IdentityFilter
└── Cargo.toml              # Identity crate dependencies

atomic-repository/
├── src/
│   ├── lib.rs              # Crate root, re-exports all modules
│   ├── apply.rs            # Apply changes to graph (ApplyOptions, ApplyOutcome, ApplyStats)
│   ├── archive.rs          # Export repository state (ArchiveFormat, ArchiveOptions, Archive trait)
│   ├── changestore.rs      # Filesystem-backed change storage with LRU caching
│   ├── error.rs            # RepositoryError and result types
│   ├── history.rs          # History querying (log, reverse_log, HistoryEntry, HistoryOptions)
│   ├── ignore.rs           # .atomicignore pattern matching (IgnoreRules, gitignore syntax)
│   ├── repository.rs       # Main Repository struct and operations
│   ├── status.rs           # Working copy status (FileStatus, RepositoryStatus, StatusOptions)
│   ├── tags.rs             # Named state snapshots (Tag, TagOptions, TagFilter, TagSort)
│   ├── tracking.rs         # File tracking (add, remove, move, TrackingOptions)
│   └── unrecord.rs         # Undo applied changes (UnrecordOptions, UnrecordOutcome, UnrecordStats)
└── tests/
    └── (integration tests)

atomic/                       # CLI application
├── src/
│   ├── main.rs              # CLI entry point, argument parsing, command dispatch
│   ├── error.rs             # CLI-specific error types (CliError, CliResult)
│   ├── commands/            # Command implementations
│   │   ├── mod.rs           # Command trait, shared utilities, re-exports
│   │   ├── init.rs          # Repository initialization command
│   │   ├── status.rs        # Working copy status command (58 tests)
│   │   ├── add.rs           # File tracking command (45 tests)
│   │   ├── record.rs        # Change recording command (planned)
│   │   ├── log.rs           # History viewing command (planned)
│   │   ├── diff.rs          # Working copy differences command (planned)
│   │   └── stack/           # Stack management subcommands (planned)
│   │       ├── mod.rs       # Stack command routing
│   │       ├── new.rs       # Create stack
│   │       ├── switch.rs    # Switch stack
│   │       ├── delete.rs    # Delete stack
│   │       └── list.rs      # List stacks
│   └── output/              # Output formatting utilities
│       ├── mod.rs           # Re-exports, convenience functions (print_success, etc.)
│       ├── colors.rs        # Terminal color utilities (ColorMode, status colors)
│       ├── progress.rs      # Progress bar helpers (spinners, progress bars)
│       └── table.rs         # Table formatting (Table, KeyValueTable, Alignment)
└── Cargo.toml               # CLI dependencies
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
