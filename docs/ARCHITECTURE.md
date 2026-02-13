# Atomic VCS Architecture

## Overview

Atomic is a mathematically sound distributed version control system built from first principles. It uses **patch theory** to represent changes as composable, commutative operations on a directed graph, enabling conflict-free merges in the common case and precise conflict detection when true conflicts exist.

Unlike line-based VCS systems (Git, Mercurial), Atomic tracks the **semantic structure** of changes, making operations like cherry-picking, reverting, and merging mathematically well-defined.

---

## Core Concepts

### 1. The Repository Graph

A Atomic repository is fundamentally a **directed acyclic graph (DAG)** where:

- **Vertices** represent content (file chunks, typically lines or semantic units)
- **Edges** represent ordering relationships between vertices
- **Changes** are transformations that add/remove vertices and edges

```
┌─────────────────────────────────────────────────────────────┐
│                    Repository Graph                          │
│                                                              │
│    [ROOT] ──► [file: main.rs] ──► [line 1] ──► [line 2]     │
│                      │                             │         │
│                      ▼                             ▼         │
│              [file: lib.rs]                   [line 3]       │
│                      │                                       │
│                      ▼                                       │
│                  [line 1] ──► [line 2]                       │
│                                                              │
└─────────────────────────────────────────────────────────────┘
```

### 2. Vertices

A vertex represents a contiguous chunk of content within a file:

```
Vertex {
    change: ChangeId,      // Which change introduced this vertex
    start: Position,       // Start position within the change
    end: Position,         // End position within the change
}
```

**Key properties:**
- Vertices are **immutable** once created
- Vertices are identified by (ChangeId, Position) tuples
- The content of a vertex is stored separately in the change file

### 3. Edges

Edges define relationships between vertices:

```
Edge {
    flag: EdgeFlags,       // Type of edge (see below)
    source: Vertex,        // Origin vertex
    dest: Vertex,          // Destination vertex  
    introduced_by: ChangeId, // Change that created this edge
}
```

**Edge Flags:**

| Flag     | Meaning |
|----------|---------|
| `BLOCK`  | Sequential content ordering |
| `PARENT` | Reverse direction (for efficient traversal) |
| `FOLDER` | File system hierarchy edge |
| `DELETED`| Marks deleted content |
| `PSEUDO` | Computed edge for connectivity |

### 4. Changes (Patches)

A change is an atomic transformation of the repository graph:

```
Change {
    header: ChangeHeader,   // Metadata (author, message, timestamp)
    dependencies: [Hash],   // Changes this depends on
    hunks: [Hunk],         // The actual modifications
    contents: Bytes,       // New content introduced
}
```

**Hunk Types:**

- `FileAdd` - Add a new file
- `FileDel` - Delete a file
- `FileMove` - Rename/move a file
- `Edit` - Modify content (add/remove vertices and edges)
- `Replacement` - Replace content (combined delete + add)

### 5. Channels (Branches)

A channel is a named view of the repository at a particular state:

```
Channel {
    name: String,
    graph: Graph,          // Current state of the DAG
    changes: [ChangeId],   // Ordered list of applied changes
    state: Merkle,         // Cryptographic state hash
}
```

**Key insight:** Multiple channels can share the same underlying graph storage, differing only in which changes have been applied.

---

## Data Model

### Identifiers

| Type | Size | Description |
|------|------|-------------|
| `Hash` | 32 bytes | Blake3 hash of change contents |
| `ChangeId` | 8 bytes | Internal identifier (repository-local) |
| `NodeId` | 8 bytes | Internal node reference |
| `Inode` | 8 bytes | File system inode reference |
| `Merkle` | 32 bytes | Incremental hash of channel state |

### Position Addressing

Content is addressed by position within a change:

```
Position {
    change: ChangeId,
    pos: u64,              // Byte offset within change contents
}
```

This allows efficient vertex splitting without copying data.

---

## Storage Architecture

### On-Disk Layout

```
.atomic/
├── pristine/              # Graph database
│   └── db                 # B-tree storage (primary + indexes)
├── changes/               # Change files (content-addressed)
│   ├── AB/
│   │   └── CDEF1234...    # Change file (hash-based path)
│   └── ...
├── working_copy_id        # Current working copy state
└── config.toml            # Repository configuration
```

### Database Tables

| Table | Key | Value | Purpose |
|-------|-----|-------|---------|
| `graph` | Vertex | Edge | Primary graph storage |
| `inode_graph` | (Inode, Vertex) | Edge | File-scoped index (O(n) traversal) |
| `changes` | ChangeId | Timestamp | Change log |
| `revchanges` | Timestamp | ChangeId | Reverse change log |
| `tree` | Path | Inode | File path → inode mapping |
| `revtree` | Inode | Path | Inode → path mapping |
| `inodes` | Inode | Position | Inode → graph position |
| `revinodes` | Position | Inode | Graph position → inode |
| `dep` | ChangeId | ChangeId | Change dependencies |
| `revdep` | ChangeId | ChangeId | Reverse dependencies |
| `internal` | Hash | ChangeId | External → internal ID |
| `external` | ChangeId | Hash | Internal → external hash |
| `states` | Merkle | Timestamp | Channel states |
| `tags` | Timestamp | Merkle | Tagged states |

### Two-Level B-Tree Optimization

**Problem:** Graph traversal for a single file requires O(n × log N) lookups where N is total repository size.

**Solution:** Secondary `inode_graph` index groups edges by file:

```
inode_graph: (Inode, Vertex) → Edge
```

**Result:** O(log N + n) traversal — one seek, then sequential scan.

---

## Operations

### Recording a Change

1. **Diff Detection**
   - Compare working copy against current channel state
   - Identify modified files via mtime/size or content hash
   
2. **Graph Diff**
   - For each modified file, compute diff against pristine state
   - Use Myers or Patience algorithm for line-level diff
   - Map line changes to graph operations (vertex/edge additions/deletions)

3. **Change Construction**
   - Collect all hunks into a change
   - Compute dependencies (changes that introduced modified vertices)
   - Serialize and hash the change

4. **Application**
   - Apply change to the channel graph
   - Update indexes
   - Compute new Merkle state

### Applying a Change

```
apply_change(channel, change):
    1. Verify all dependencies are present
    2. For each hunk in change:
       - If NewVertex: insert vertex and edges into graph
       - If EdgeMap: modify existing edges
    3. Update channel's change log
    4. Recompute Merkle state
    5. Handle pseudo-edges for connectivity
```

### Merging Channels

```
merge(channel_a, channel_b):
    1. Find common ancestor (via Merkle states)
    2. Identify changes in A not in B, and vice versa
    3. Apply missing changes from B to A (or vice versa)
    4. Detect conflicts:
       - Order conflicts: same vertex has multiple successors
       - Name conflicts: same directory has duplicate names
       - Zombie conflicts: deleted content has live dependencies
    5. Create merge change if conflicts exist
```

**Key property:** If no conflicts exist, merge is automatic and commutative.

---

## Conflict Model

### Types of Conflicts

1. **Order Conflict**
   - Two changes insert content at the same position
   - Represented as multiple outgoing edges from same vertex
   
2. **Name Conflict**  
   - Two files with same name in same directory
   - Represented as multiple FOLDER edges to same parent

3. **Zombie Conflict**
   - Content deleted by one change, modified by another
   - Deleted vertices with live children

### Conflict Resolution

Conflicts are **first-class data** in the graph, not merge failures:

```
┌─────────────────────────────────────────┐
│           Order Conflict                │
│                                         │
│        [context line]                   │
│            │                            │
│       ┌────┴────┐                       │
│       ▼         ▼                       │
│  [line from A] [line from B]            │
│       │         │                       │
│       └────┬────┘                       │
│            ▼                            │
│     [next context line]                 │
│                                         │
└─────────────────────────────────────────┘
```

Resolution creates a new change that adds/removes edges to establish the desired order.

---

## Merkle State

Each channel maintains a **Merkle hash** representing its complete state:

```
state = H(H(change_1) || H(change_2) || ... || H(change_n))
```

**Properties:**
- Incrementally computable: `new_state = H(old_state || H(new_change))`
- Uniquely identifies channel state
- Enables efficient sync (compare states, exchange missing changes)

---

## Remote Protocol

### Operations

| Command | Description |
|---------|-------------|
| `state` | Get current channel state (Merkle hash) |
| `changelist` | List changes since a given state |
| `change` | Download a specific change |
| `apply` | Upload and apply a change |
| `tag` | Download tag metadata |
| `tagup` | Upload tag metadata |

### Sync Algorithm

```
push(local, remote):
    1. Get remote state
    2. Find local changes not in remote (via Merkle comparison)
    3. Upload changes in dependency order
    4. Verify final states match

pull(local, remote):
    1. Get remote changelist since local state
    2. Download missing changes
    3. Apply in dependency order
    4. Verify final states match
```

---

## Crate Structure

```
atomic/
├── atomic/               # CLI application
├── atomic-core/          # Core VCS engine
│   ├── src/
│   │   ├── change/        # Change representation & serialization
│   │   ├── diff/          # Diff algorithms
│   │   ├── graph/         # Graph data structures & operations
│   │   ├── pristine/      # Database layer
│   │   ├── record/        # Change recording
│   │   ├── apply/         # Change application
│   │   ├── output/        # Working copy output
│   │   └── merge/         # Merge operations
├── atomic-config/        # Configuration management
├── atomic-identity/      # User identity & signing
├── atomic-remote/        # Remote operations (HTTP, SSH)
└── atomic-repository/    # High-level repository operations
```

---

## Design Principles

1. **Mathematical Soundness**
   - Changes are well-defined transformations
   - Merge is commutative when conflict-free
   - All operations preserve graph invariants

2. **Efficiency**
   - O(n) file operations via inode indexing
   - Incremental state computation
   - Content-addressed storage with deduplication

3. **Correctness Over Speed**
   - Validate invariants aggressively
   - Prefer clear code over micro-optimizations
   - Comprehensive test coverage

4. **Clean Separation**
   - Core engine has minimal dependencies
   - Storage layer is abstracted (can swap backends)
   - CLI is thin wrapper around library

---

## Next Steps

See [THEORY.md](./THEORY.md) for the mathematical foundations.
See [IMPLEMENTATION.md](./IMPLEMENTATION.md) for implementation details.