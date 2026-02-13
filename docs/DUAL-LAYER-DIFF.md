# Dual-Layer Diff Architecture in Atomic VCS

This document explains how Atomic VCS represents and stores changes using a dual-layer architecture: the **Graph Layer** for storage and merging, and the **Semantic Layer** for human understanding.

## Table of Contents

1. [Overview](#overview)
2. [The Unified Diff Header](#the-unified-diff-header)
3. [Graph Layer (Storage)](#graph-layer-storage)
4. [Semantic Layer (Understanding)](#semantic-layer-understanding)
5. [The Dual-Layer Architecture](#the-dual-layer-architecture)
6. [Recording Pipeline](#recording-pipeline)
7. [Why Two Layers?](#why-two-layers)
8. [Code References](#code-references)
9. [Terminology Reference](#terminology-reference)

---

## Overview

When you run `atomic diff` and see output like:

```
@@ -1,3 +1,9 @@
 fn main() {
-    println!("Hello");
+    println!("Hello");
+    println!("World");
 }
```

This familiar unified diff format is **not** how Atomic stores changes internally. Instead, Atomic maintains two parallel representations:

| Layer | Purpose | Speaks In |
|-------|---------|-----------|
| **Graph Layer** | Storage, merging, sync | Spans, edges, byte positions |
| **Semantic Layer** | Display, review, blame | Files, lines, tokens |

Both layers are stored in every change and share the same content blob.

---

## The Unified Diff Header

The `@@ -1,3 +1,9 @@` line is called a **hunk header** in unified diff format. This is standard terminology from the unified diff specification (used by `diff -u`, Git, patch, etc.) - not to be confused with Atomic's internal `GraphOp` type. The hunk header is purely for human consumption:

```
@@ -1,3 +1,9 @@
    │  │   │  └── 9 lines in new file starting at line 1
    │  │   └───── Starting line in new file
    │  └───────── 3 lines in old file starting at line 1
    └──────────── Starting line in old file
```

**Key insight**: Atomic doesn't store line numbers. This header is *generated* from the semantic layer at display time.

See: [`atomic-cli/src/commands/diff.rs`](../atomic-cli/src/commands/diff.rs)

---

## Graph Layer (Storage)

The graph layer represents changes as **graph nodes** (content chunks) connected by **edges** (ordering relationships) in a directed acyclic graph (DAG).

### Core Concept: Context, Not Position

Instead of saying "insert at line 10," Atomic says "insert **after** these nodes and **before** those nodes":

```
OLD FILE (in graph):                    NEW CONTENT being inserted:
┌──────────────────────────────────────┐    
│  Node A: "fn main() {"           │ ◄── PREDECESSORS: "Insert AFTER this"
│  Position: (Change#1, 0:13)      │     
└──────────────────────────────────────┘     
                │                            │
                │                     ┌──────▼────────────────────┐
                │                     │  NEW Node: "   // new"    │
                │                     │  Content at bytes [50:60] │
                │                     │  in the change blob       │
                │                     └──────┬────────────────────┘
                ▼                            │
┌──────────────────────────────────────┐         │
│  Node B: "}"                     │ ◄───────┘
│  Position: (Change#1, 13:14)     │    SUCCESSORS: "Insert BEFORE this"
└──────────────────────────────────────┘
```

### The Insertion Structure

When inserting content, Atomic creates an `Insertion`:

```rust
pub struct Insertion<H> {
    /// Vertices that should come BEFORE this new content
    pub predecessors: Vec<Position<H>>,

    /// Vertices that should come AFTER this new content
    pub successors: Vec<Position<H>>,

    /// Edge flags (BLOCK for content, FOLDER for directories)
    pub flag: EdgeFlags,

    /// Byte range in the change's content blob
    pub start: ChangePosition,
    pub end: ChangePosition,

    /// The file (inode) this node belongs to
    pub inode: Position<H>,
}
```

See: [`atomic-core/src/change/atom.rs` - `Insertion`](../atomic-core/src/change/atom.rs)

### Context Resolution

- **Predecessors**: References the **end** of nodes that come before
- **Successors**: References the **start** of nodes that come after

This is handled by two different lookup methods:

| Method | Use Case | Matching Logic |
|--------|----------|----------------|
| `find_block_end(pos)` | Predecessors | Find node that **ends** at position |
| `find_block(pos)` | Successors | Find node **containing** position |

See: [`atomic-core/src/apply/position.rs` - `resolve_position()`](../atomic-core/src/apply/position.rs)

### Why This Design?

1. **No line numbers to conflict** - Two people can edit "line 10" but if they're editing different graph nodes, no conflict
2. **Content-addressed** - Same content = same hash, always
3. **Commutative merges** - Independent node changes can be applied in any order
4. **Renames don't break history** - Inodes track files, not paths

---

## Semantic Layer (Understanding)

The semantic layer provides a **human-readable interpretation** of graph operations using the Trunk → Branch → Leaf hierarchy:

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                     Trunk → Branch → Leaf Architecture                       │
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

### Hierarchy

| Level | Represents | Operation Types | Example |
|-------|------------|-----------------|---------|
| **Trunk** | File | Create, Delete, Move, Undelete | `TrunkOp::Create { path: "main.rs" }` |
| **Branch** | Line | Insert, Delete, Restore | `BranchOp::Insert { after: line_2 }` |
| **Leaf** | Token | Insert, Delete, Replace | `LeafOp::Insert { kind: Word, content: "fn" }` |

### FileOps Structure

```rust
pub struct FileOps {
    trunk_id: TrunkId,           // Unique file identifier
    path: String,                // Human-readable path
    trunk_op: Option<TrunkOp>,   // File-level operation (if any)
    line_ops: Vec<LineOps>,      // All line operations
}

pub struct LineOps {
    branch_id: BranchId,         // Unique line identifier
    operation: BranchOp,         // Insert/Delete/Restore with tokens
    old_line_num: Option<usize>, // Line number in old file
    new_line_num: Option<usize>, // Line number in new file
}
```

See: [`atomic-core/src/change/ops.rs` - `FileOps`, `LineOps`](../atomic-core/src/change/ops.rs)

### Token Kinds

The semantic layer tokenizes content into meaningful units:

| TokenKind | Example | Use Case |
|-----------|---------|----------|
| `Word` | `fn`, `main`, `println` | Identifiers, keywords |
| `Whitespace` | `    `, ` ` | Indentation, spacing |
| `Punctuation` | `(`, `)`, `{`, `}` | Delimiters |
| `Operator` | `+`, `->`, `::` | Operators |
| `String` | `"Hello"` | String literals |
| `Number` | `42`, `3.14` | Numeric literals |
| `Comment` | `// todo` | Comments |

See: [`atomic-core/src/diff/token.rs`](../atomic-core/src/diff/token.rs)

---

## The Dual-Layer Architecture

Both layers are stored together in every change:

```rust
pub struct HashedChange {
    // ... metadata ...

    /// Graph operations (storage layer)
    pub hunks: Vec<GraphOp<Option<Hash>>>,

    /// Semantic operations (understanding layer)
    pub file_ops: Vec<FileOps>,

    /// Hash of the shared content blob (both layers reference this)
    pub contents_hash: Hash,
}
```

See: [`atomic-core/src/change/change.rs` - `HashedChange`](../atomic-core/src/change/change.rs)

### The Mapping

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                      Graph ↔ Semantic Mapping                                │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  GRAPH LAYER                           SEMANTIC LAYER                       │
│  (Storage)                             (Understanding)                      │
│                                                                             │
│  GraphNode + Inode                ←→   Trunk (File)                         │
│  Position(Change, byte)                TrunkId(Change, file_idx)            │
│                                                                             │
│  GraphNode byte range             ←→   Branch (Line)                        │
│  predecessors/successors               BranchId(Change, branch_idx)         │
│  (relative to graph)                   old_line_num / new_line_num          │
│                                                                             │
│  Content bytes                    ←→   Leaf (Token)                         │
│  start..end in content blob            LeafId(Change, leaf_idx)             │
│                                        TokenKind (Word, Punct, WS, etc.)    │
│                                                                             │
│  SHARED:                                                                    │
│  └── contents: Vec<u8>  ←── Both layers reference the same content blob    │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## Recording Pipeline

When you run `atomic record`, both layers are generated together:

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                         Recording Pipeline                                   │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  1. DETECT CHANGES                                                          │
│     Working copy ──diff──▶ Pristine state                                   │
│     Result: DiffOp (Equal/Insert/Delete/Replace)                            │
│                                                                             │
│  2. BUILD GRAPH LAYER                                                       │
│     DiffOp ──▶ GraphOpBuilder ──▶ GraphOp::Edit { Insertion { pred/succ }}  │
│                                                                             │
│  3. BUILD SEMANTIC LAYER (parallel)                                         │
│     a. Tokenize: content bytes ──▶ TokenizedLine ──▶ TokenizedToken         │
│     b. Analyze:  old + new ──▶ LineAnalyzer ──▶ LineChange                  │
│     c. Generate: LineChange ──▶ CrdtChangeBuilder ──▶ FileOps/LineOps       │
│                                                                             │
│  4. STORE BOTH                                                              │
│     HashedChange { graph_ops: [...], file_ops: [...], contents: [...] }     │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

Key modules in the pipeline:

| Step | Module | Purpose |
|------|--------|---------|
| Diff | [`atomic-core/src/diff/`](../atomic-core/src/diff/) | Myers/Patience diff algorithms |
| Graph Build | [`atomic-core/src/record/workflow/graph_op.rs`](../atomic-core/src/record/workflow/graph_op.rs) | Convert diffs to graph operations |
| Tokenize | [`atomic-core/src/record/workflow/crdt/tokenize.rs`](../atomic-core/src/record/workflow/crdt/tokenize.rs) | Split content into tokens |
| Line Analysis | [`atomic-core/src/record/workflow/crdt/line_ops.rs`](../atomic-core/src/record/workflow/crdt/line_ops.rs) | Analyze line-level changes |
| CRDT Build | [`atomic-core/src/record/workflow/crdt/builder.rs`](../atomic-core/src/record/workflow/crdt/builder.rs) | Generate semantic operations |
| Assembly | [`atomic-core/src/record/workflow/assembly.rs`](../atomic-core/src/record/workflow/assembly.rs) | Assemble final change with dependencies |
| Globalize | [`atomic-core/src/record/workflow/globalize.rs`](../atomic-core/src/record/workflow/globalize.rs) | Resolve file paths to graph positions |

---

## Why Two Layers?

### Use Case Comparison

| Use Case | Graph Layer | Semantic Layer |
|----------|-------------|----------------|
| **Storage** | ✅ Primary | Index/metadata |
| **Apply to graph** | ✅ Required | Optional |
| **Sync/merge** | ✅ Required | Helps conflict resolution |
| **Display diff** | ❌ "bytes 35-59 changed" | ✅ "line 3: added `println`" |
| **Code review** | ❌ Unreadable | ✅ Human-friendly |
| **Blame** | ❌ Node-level | ✅ Token-level |
| **Word diff** | ❌ Must recompute | ✅ Direct from LeafOp |

### Graph Layer Alone is Insufficient

The graph speaks in byte positions:

```
"Insert bytes 0-24 after position 35 in change ABC123"
```

A developer asks:
- "What line is that?"
- "What word changed?"
- "Who wrote this function name?"

**Answer: Unknown without semantic layer.**

### Semantic Layer Alone is Insufficient

The semantic layer provides human understanding but:
- Cannot be applied directly to the graph
- Doesn't capture the precise content-addressed relationships
- Can't support conflict-free merging without the graph structure

### Together: Best of Both Worlds

```
Graph Layer                          Semantic Layer
─────────────                        ──────────────
Efficient storage          +         Human-readable display
Content-addressed          +         Line/token granularity  
Conflict-free merging      +         Meaningful code review
Rename-resilient           +         Accurate blame
```

---

## Code References

### Core Structures

| Type | File | Description |
|------|------|-------------|
| `HashedChange` | [`change/change.rs`](../atomic-core/src/change/change.rs) | Both layers stored together (`hunks` + `file_ops`) |
| `GraphOp` | [`change/graph_op.rs`](../atomic-core/src/change/graph_op.rs) | Graph operations (FileAdd, Edit, etc.) |
| `Insertion` | [`change/atom.rs`](../atomic-core/src/change/atom.rs) | Insert content with context (predecessors/successors) |
| `EdgeUpdate` | [`change/atom.rs`](../atomic-core/src/change/atom.rs) | Modify edges (for deletions) |
| `NewEdge` | [`change/atom.rs`](../atomic-core/src/change/atom.rs) | Individual edge modification within an `EdgeUpdate` |
| `FileOps` | [`change/ops.rs`](../atomic-core/src/change/ops.rs) | Semantic file operations |
| `LineOps` | [`change/ops.rs`](../atomic-core/src/change/ops.rs) | Semantic line operations |

### Graph Types

| Type | File | Description |
|------|------|-------------|
| `GraphNode` | [`types/graph_node.rs`](../atomic-core/src/types/graph_node.rs) | Content range in a change (change, start, end) |
| `Position` | [`types/position.rs`](../atomic-core/src/types/position.rs) | Point in the graph (change, pos) |
| `GraphEdge` | [`types/graph_edge.rs`](../atomic-core/src/types/graph_edge.rs) | Relationship between nodes (flag, dest, introduced_by) |
| `SerializedGraphEdge` | [`types/graph_edge.rs`](../atomic-core/src/types/graph_edge.rs) | Compact 24-byte edge for storage |
| `EdgeFlags` | [`types/graph_edge.rs`](../atomic-core/src/types/graph_edge.rs) | Bitflags: BLOCK, PSEUDO, FOLDER, PARENT, DELETED |

### CRDT Hierarchy

| Type | File | Description |
|------|------|-------------|
| `Trunk` | [`crdt/trunk.rs`](../atomic-core/src/crdt/trunk.rs) | File representation |
| `Branch` | [`crdt/branch.rs`](../atomic-core/src/crdt/branch.rs) | Line representation |
| `Leaf` | [`crdt/leaf.rs`](../atomic-core/src/crdt/leaf.rs) | Token representation |
| `TrunkOp/BranchOp/LeafOp` | [`crdt/`](../atomic-core/src/crdt/) | Operations per level |

### Apply Logic

| Function | File | Description |
|----------|------|-------------|
| `resolve_position` | [`apply/position.rs`](../atomic-core/src/apply/position.rs) | Resolve hash-based position to internal NodeId |
| `apply_insertion` | [`apply/insertion.rs`](../atomic-core/src/apply/insertion.rs) | Apply new content with predecessor/successor context |
| `find_block` | [`pristine/traits.rs`](../atomic-core/src/pristine/traits.rs) | Find node containing position |
| `find_block_end` | [`pristine/traits.rs`](../atomic-core/src/pristine/traits.rs) | Find node ending at position |
| `apply_file_ops` | [`apply/file_ops.rs`](../atomic-core/src/apply/file_ops.rs) | Apply semantic layer to CRDT tables |

### Recording Workflow

| Module | File | Description |
|--------|------|-------------|
| `workflow/` | [`record/workflow/`](../atomic-core/src/record/workflow/) | Recording pipeline |
| `crdt/` | [`record/workflow/crdt/`](../atomic-core/src/record/workflow/crdt/) | Semantic layer generation |
| `globalize.rs` | [`record/workflow/globalize.rs`](../atomic-core/src/record/workflow/globalize.rs) | Graph position resolution |
| `assembly.rs` | [`record/workflow/assembly.rs`](../atomic-core/src/record/workflow/assembly.rs) | Change assembly with dependencies |

---

## Terminology Reference

This project uses specific terminology that differs from traditional VCS and other patch-based systems:

| Atomic Term | Traditional Term | Description |
|-------------|------------------|-------------|
| **GraphNode** | Vertex/Hunk | A contiguous range of bytes in the graph (change, start, end) |
| **GraphOp** | Hunk | A high-level operation (FileAdd, Edit, etc.) |
| **Insertion** | NewVertex | Insert new content with context |
| **EdgeUpdate** | EdgeMap | Modify existing edges |
| **NewEdge** | — | Individual edge modification (previous → new flags) |
| **predecessors** | up_context | Nodes that come before new content |
| **successors** | down_context | Nodes that come after new content |
| **Trunk** | File | File-level CRDT entity |
| **Branch** | Line | Line-level CRDT entity |
| **Leaf** | Token | Token-level CRDT entity |

---

## Summary

1. **Graph layer** = How Atomic *stores* and *merges* changes
   - GraphNodes, edges, byte positions
   - Content-addressed, conflict-free

2. **Semantic layer** = How *humans understand* changes
   - Files (Trunk), Lines (Branch), Tokens (Leaf)
   - Line numbers, word diffs, blame

3. **Both are stored** in every change (`hunks` + `file_ops`)

4. **Both reference** the same `contents` blob

5. **Unified diff format** (`@@ -1,3 +1,9 @@`) is *generated from* the semantic layer for display

This dual-layer architecture is what enables Atomic to provide:
- Git-compatible workflows (line-based diffs)
- Mathematical soundness (graph-based merging)
- Modern code review (token-level precision)
- Accurate blame (who wrote each token)

---

## Further Reading

- [`AGENTS.md`](../AGENTS.md) - Full development guide with architecture details
- [`atomic-core/src/crdt/mod.rs`](../atomic-core/src/crdt/mod.rs) - CRDT module documentation
- [`atomic-core/src/change/mod.rs`](../atomic-core/src/change/mod.rs) - Change module documentation