# Cross-View Merge Duplication — Root Cause Analysis

**Scope:** the user-reported bug where inserting changes from a draft view into
a shared view (e.g. `feature → dev`) produced files with duplicated content,
fork conflicts at unexpected positions, and — in compound scenarios —
truncated output or post-merge records that silently dropped one side's
edits.

**Outcome:** root-caused to a stack of nine interacting issues that all
traced to the same architectural mismatch — a record/output pipeline still
half-written for a *single-hunk-per-inode, materialize-then-diff* model
running against a graph that had been migrated to *per-line vertices on an
additive DAG*.

This document explains the architecture, walks each defect, and points at
the files where each fix lives.

---

## 1 · Preamble — Terms and Definitions

Atomic stores three coordinated structures: a **B-tree** for raw storage,
a **graph** of vertices and edges built on top of it, and a parallel
**CRDT hierarchy** (Trunk → Branch → Leaf). All three describe the same
files; they differ in granularity and purpose.

The terminology below is what the rest of this document uses. The examples
follow one tiny file through all three representations so the
interconnect is concrete.

### Inode

A **stable identifier for a file or directory**. It is independent of the
file's path or content — if you move `src/main.rs` to `lib/main.rs`, the
inode stays the same. Inodes are how Atomic keeps file identity
attached across renames.

Inodes are also the *anchor point* for graph traversal: every file has an
"inode vertex" (an empty span at a fixed position), and all of that
file's content vertices hang off it.

```text
Inode(42)  ──corresponds to──▶  "src/main.rs"
                                (path is mutable; the inode is not)
```

### Vertex (Graph Node)

A **span of bytes in some change's content blob**. Internally a vertex is
a 3-tuple `(change_id, start, end)` where `change_id` identifies which
change introduced the content, and `[start, end)` is a byte range in
that change's serialized content buffer.

Vertices are immutable. To "edit" content, you don't modify a vertex;
you add a new one and mark the old one's incoming edges deleted.

```text
V[change=C1, start=11, end=20]   ←  the bytes "<!DOCTYPE>\n" stored
                                    in change C1's content blob
```

A few special vertices:
- **Inode vertex:** `[c, p, p)` — start == end, an empty span sitting at
  the inode's position. The traversal entry point for the file.
- **Root vertex:** `change=ROOT` — the virtual repository root, parent of
  all top-level files.

### Edge

A directed connection between two vertices in the graph. Edges carry a
*flag set* and an *introduced_by* — the change that recorded that edge.

Two orthogonal flag dimensions matter for this RCA:

| Direction | Flag    | Meaning                                                |
|-----------|---------|--------------------------------------------------------|
| Forward   | `BLOCK` | "the destination is the next structural block"         |
| Forward   | `FOLDER`| "the destination is a child in the directory tree"    |
| Forward   | `PSEUDO`| computed connectivity, never recorded in change files |
| Reverse   | `PARENT`| bit set on the reverse edge — same target/source pair |

Orthogonally:

| Flag      | Meaning                                                          |
|-----------|------------------------------------------------------------------|
| (none)    | the edge is live                                                 |
| `DELETED` | a later change wants this edge to no longer apply                |

Critically — **edges are additive**. When change `C2` deletes the edge
`V_A → V_B` (originally introduced by `C1`), we do **not** remove the
`BLOCK(C1)` edge from the B-tree. We add a new `BLOCK | DELETED(C2)`
edge alongside it. Both rows coexist forever; the *view filter*
(see below) decides which is in effect.

```text
inode ──BLOCK(C1)──▶ V[server]      (live edge: C1 introduced it)
inode ──BLOCK|DELETED(C3)──▶ V[server]   (deletion edge: C3 wants it gone)
```

If a view's filter contains `C3`, the deletion has happened in that
view's perspective and `V[server]` is dead. If the filter excludes `C3`,
the deletion has not happened and `V[server]` is alive.

### Trunk, Branch, Leaf — the CRDT Layer

Parallel to the byte-range graph, Atomic maintains a CRDT hierarchy
optimized for semantic merges:

```text
TRUNK (file)
  ├── BRANCH (line)
  │     ├── LEAF (token)
  │     ├── LEAF (token)
  │     └── …
  └── BRANCH (line)
        └── …
```

- **Trunk** — one per file. Holds metadata: inode, path, encoding,
  alive/deleted state.
- **Branch** — one per line. Has a stable `BranchId = (change_id, branch_idx)`
  so the line keeps its identity across edits. Stores a hash of the line
  for fast equality.
- **Leaf** — one per token (word, whitespace, operator, …). Stable
  `LeafId = (change_id, leaf_idx)`. Stores a `TokenKind` and the
  content's byte range.

The CRDT layer is what enables **token-level three-way merges**. When
two agents edit the same line but touch different tokens, the merge
engine can see that the leaves they changed are disjoint and compose
both edits cleanly instead of emitting conflict markers.

### View, View Changes, Change Filter

A **view** is a named set of changes. `dev` is a shared view; a draft
view like `feature-auth` is a private overlay on top of `dev`.

The view's set of change IDs is its `VIEW_CHANGES`. When draft views
inherit, we walk the parent chain and union the parent's changes in to
form the **change filter** — the set of changes that this view "sees".

The change filter is the *only* thing that distinguishes one view from
another at read time. The underlying graph is global (the "ambient graph"
model): every change ever applied lives in the same B-tree. View
isolation is purely a *read-time filter* operation.

### B-tree (Storage Layer)

Atomic uses redb (a persistent B-tree) for all on-disk state. The graph
isn't a separate data structure — it's a **logical view over B-tree
tables**. The relevant tables for this RCA:

| Table                  | Key                          | Value                                | Purpose                                              |
|------------------------|------------------------------|--------------------------------------|------------------------------------------------------|
| `GRAPH`                | `(change, start, end)` (24 B)| serialized edge `(flag, dest, by)`   | every edge, including DELETED variants and PARENT mirrors |
| `INODE_GRAPH`          | scoped to one inode          | mirror of edges for that inode       | secondary index for fast per-file traversal          |
| `VIEW_CHANGES`         | `(view_id, change_id)`       | (presence)                           | which changes are visible from a view                |
| `CHANGE_DEPS`          | `change_id → [dep_hash]`     | indexed dependency list              | "what was this change recorded knowing about?"       |
| `crdt_trunks`          | `TrunkId`                    | path/encoding/state                  | CRDT file table                                      |
| `crdt_branches`        | `BranchId`                   | trunk_id/state/line_hash             | CRDT line table                                      |
| `crdt_trunk_branches`  | `TrunkId → [BranchId]`       | (multimap)                           | ordered line list within a file                      |
| `crdt_leaves`          | `LeafId`                     | branch_id/kind/state/byte range      | CRDT token table                                     |
| `crdt_vertex_branch`   | graph node → BranchId        | bridge                               | walk *into* the CRDT layer from a graph vertex       |

### Worked example — one file in all three views

Take a one-line file `config.ts` whose contents are `const x = 5;\n` and
which was added by the initial change `C1`. Inode is `42`, path is
`config.ts`.

**B-tree rows:**

```text
GRAPH:
  key=(C1, 13, 13) (inode marker)
    → edge BLOCK, dest=(C1, 13), by=C1   ← forward to content
    → edge BLOCK|PARENT, dest=(0, 0), by=C1   (reverse to root)
  key=(C1, 13, 27) (the "const x = 5;\n" span)
    → edge BLOCK|PARENT, dest=(C1, 13), by=C1   (reverse to inode)

INODE_GRAPH[42]: same edges, indexed by inode

VIEW_CHANGES[dev]: {C1}
```

**Graph view:**

```text
ROOT ──FOLDER(C1)──▶ Inode-vertex V[13:13]  ──BLOCK(C1)──▶  V[13:27] "const x = 5;\n"
```

**CRDT view:**

```text
Trunk(C1,0) "config.ts" inode=42
  └── Branch(C1,0) line_hash=…
        ├── Leaf(C1,0) kind=Word     "const"
        ├── Leaf(C1,1) kind=Whitespace " "
        ├── Leaf(C1,2) kind=Word     "x"
        ├── Leaf(C1,3) kind=Whitespace " "
        ├── Leaf(C1,4) kind=Operator "="
        ├── Leaf(C1,5) kind=Whitespace " "
        ├── Leaf(C1,6) kind=Number   "5"
        ├── Leaf(C1,7) kind=Punctuation ";"
        └── Leaf(C1,8) kind=Newline  "\n"
```

Now an agent records change `C2`, which renames `x` to `count`. The
**delta** applied to all three layers:

```text
GRAPH gains rows:
  (C1, 13, 27) → edge BLOCK|DELETED, dest=(C1,13), by=C2   ← deletion
  (C1, 13, 13) → edge BLOCK, dest=(C2,0), by=C2            ← new line
  (C2, 0, 14) → (new vertex's reverse + content edges)

VIEW_CHANGES[dev] becomes {C1, C2}

Graph view (filter={C1,C2}):
  ROOT ──FOLDER(C1)──▶ V[13:13] ──BLOCK(C2)──▶ V[(C2,0,14)] "const count = 5;\n"
                                  ╲                ╱
                                   ╲─BLOCK|DELETED(C2)─▶ V[13:27] (now dead)

CRDT view:
  Trunk(C1,0)
    ├── Branch(C1,0) ← marked Deleted
    └── Branch(C2,0) ← alive, new line
          └── Leaves(C2, …) ← new token sequence
```

This is the world the rest of the document operates in.

---

## 2 · How the layers interconnect

For someone tracing a read end-to-end:

```text
┌────────────────────────────────────────────────────────────────────┐
│ user: "show me dev's config.ts"                                   │
└────────────────────────────────────────────────────────────────────┘
                              │
              ┌───────────────┴───────────────┐
              ▼                               ▼
   collect_visible_change_ids        inode_position(inode)
   walks parent chain →               B-tree lookup →
   change_filter HashSet              Position { change, pos }
              │                               │
              └───────────────┬───────────────┘
                              ▼
           retrieve_graph(txn, position, opts.with_change_filter)
              │
              │  Stack-based DFS over GRAPH B-tree edges.
              │  At each vertex:
              │    – call iter_forward → ForwardEdge[]
              │    – options.passes_filter(edge.introduced_by) ?
              │    – options.is_edge_alive(edge) ?
              │    – options.is_vertex_alive(txn, dest) ?
              │    – If dest is dead → walk_through_dead
              │  Result: AliveGraph (vid → vertex, with children lists)
              ▼
           compute_order(AliveGraph)
              │
              │  Tarjan's SCC, producing reverse-topological order.
              ▼
           resolve_conflicts_semantically
              │
              │  Detect forks at multi-child parents.
              │  For each fork:
              │    – supersedor_in_fork (change-DAG closure check)
              │    – engine.try_merge (CRDT three-way merge)
              │    – or insert_unresolved_fork (markers)
              ▼
           output_graph_content_resolved → bytes
```

Conceptually:

- The **B-tree** holds every edge ever recorded.
- The **graph** is a logical view of the B-tree, filtered at runtime by
  the view's change filter and a vertex-aliveness rule.
- The **CRDT layer** mirrors the graph at line/token granularity and is
  consulted by `SemanticMergeEngine` whenever a fork needs more than
  byte-level reasoning.
- The **view filter** is the bridge: the *same* B-tree produces different
  graphs for different views by changing which edges pass.

---

## 3 · The Bug, as Reported

> Inserting a draft view's changes into the shared `dev` view produces
> **content duplication** in the materialized file. Both the original
> and the inserted version of every line appear.

Reproductions ranged from "every section header appears 3 times" through
"the working copy has stray conflict markers from a clean merge" all
the way to "post-merge records silently drop one side's edits". They all
turned out to be facets of the same underlying mismatch.

---

## 4 · Root Cause — Architectural Mismatch

The codebase was carrying two competing models simultaneously:

| Layer            | Original model                                           | New model                                                |
|------------------|----------------------------------------------------------|----------------------------------------------------------|
| Storage          | One content vertex per file ("the file's bytes")         | Per-line vertices chained by BLOCK edges                |
| Deletion         | Physically remove the edge row from the B-tree           | Add a parallel `BLOCK \| DELETED` row (additive)         |
| Materialization  | Reconstruct file by writing the single vertex's bytes    | DFS the graph filtered by view, emit alive vertices     |
| Record           | Diff working copy against materialized text, write a Replace hunk that *swaps the whole file vertex* | Diff against materialized text, write *targeted* per-line Replace hunks |
| Cross-view merge | Snapshot text on draft view, snapshot on shared view, 3-way merge the text, auto-record back | Add the draft's change IDs to the shared view's `VIEW_CHANGES` (pure metadata) |

The new model was already most of the way in: per-line vertices were
being created, the additive edge model existed, the cross-view-insert was
already metadata-only. But the *interaction surfaces* — the diff path,
the walker, the apply path, the merge engine fallbacks — still embedded
assumptions from the single-vertex / materialize-then-diff world. Every
defect below is one of those surfaces.

### 4.1 · How the mismatch got there — an LLM-assisted-development failure mode

It is worth saying plainly: a significant fraction of these interaction
surfaces were written by an LLM-assisted pair-programming workflow, and
the LLM was hallucinating **"this is a git-like system"** when it should
have been writing **"this is a patch-theory graph system"**.

The two domains look superficially similar from the user's perspective —
both have records / commits, both have branches / views, both have
inserts / merges — but their *internal* operations are opposites:

| Concept            | Git (LLM's prior)                                              | Atomic (actual requirement)                                                        |
|--------------------|----------------------------------------------------------------|------------------------------------------------------------------------------------|
| What a commit owns | A new snapshot of files                                        | A delta applied to a shared graph                                                  |
| How edits work     | Write the new file content; diff against parent for display    | Compute the algebraic graph delta and emit it as vertex/edge operations            |
| How merges work    | 3-way text merge between file snapshots, write merged file     | Add the other side's change IDs to this view's filter; the graph is already current |
| Materialise        | Check the snapshot out of the object store                     | DFS the global graph filtered by the view's change set                             |
| "Conflict"         | Text regions that 3-way merge can't reconcile                  | Two alive vertices the walker can't topologically order                            |

The pattern was clear in retrospect. Whenever code needed to handle a
modification, the path of least resistance for the LLM was to reach for
*the operations git would do*: snapshot the working file, run a textual
diff, write a `Replace` hunk that swallows the whole file's content,
auto-record the merged text back into the graph. Each of those moves
**looks correct line-by-line** — the diff is computed correctly, the
slice is extracted correctly, the apply faithfully applies the recorded
ops — and that's what makes the bug class so insidious. The defects
aren't in any one function; they're in the *implied data flow between
functions* and that implied flow is the one a git-trained model defaults
to.

Specific instances visible in the nine defects below:

- **5.1** — the Replace-hunk pipeline passing the *entire file* content
  to the globalizer is the canonical git move. The comment that
  justified it ("Replace hunks need the full file content because they
  delete all existing vertices and re-insert the complete new file")
  reads like a perfectly reasonable description of git's `git checkout
  --theirs file.txt` behaviour, applied wholesale to a system that
  needed targeted graph surgery.
- **5.6** — `HunkBuilder::combine_with` summing two `new_len`s while
  taking the *outer* old-line range is correct for a unified-diff
  display (the canonical git mental model: hunks are presentation
  artifacts) and destructive in a system where hunks are graph
  operations.
- **5.9** — falling back to a "whole-file replace" path that scans the
  *unfiltered* `INODE_GRAPH` is "just re-read the file's edges from
  disk" thinking — sensible if edges were a per-file index of the
  current state, catastrophic when the index is a global merged graph
  whose semantics depend on a view's change filter.

The pattern is general, not specific to this project. When asking an
LLM to work in a domain where one paradigm dominates its training data
(git, in this case) and the task requires a different paradigm (patch
theory / additive DAG operations), the model will repeatedly default
to the dominant paradigm and the code it produces will fail in *exactly
this way*: each function correct in isolation, the inter-function
contracts silently wrong. Three guardrails that would have caught it
earlier:

1. **Make the contract explicit at every boundary.** A `Replace` hunk's
   contract should have said in code (not just comments) that it
   targets *specific vertices* and carries *only their replacement
   bytes*, not the whole file. A typed `ReplacementSlice` newtype
   wrapping the byte range, distinct from `FileContent`, would have
   made `pipeline.rs:302` impossible to write.
2. **Forbid the cross-view escape hatch at the type level.** Every read
   path in record / output should require a `&ViewGraph`, never a raw
   `&impl GraphTxnT`. The `find_content_vertices` → `INODE_GRAPH`
   fast-path was reachable because the function accepted any graph
   transaction; making it require a view-aware wrapper turns 5.9 into
   a compile error.
3. **Run patch-theory invariant tests during record, not just during
   merge.** "After recording a single-line edit, the change file must
   list exactly the changes whose vertices the new change *targeted*"
   is an invariant the LLM would not have violated if it had been a
   `debug_assert!` in the record path. The phantom dependency bug
   would have failed loudly the first time it was introduced.

These three are the highest-leverage follow-ups beyond the defect fixes
themselves.

### 4.2 · The recurring meta-principle

One principle ran through the entire fix:

> **Don't iterate the materialized text to make graph decisions.**
> The materialized text is *output*. Every place that loops back to it
> as *input* is text-first thinking creeping back into a patch-theory
> system. When the graph is genuinely ambiguous, delegate to the
> semantic layer (CRDT branches/leaves, change DAG, three-way merge
> engine) instead.

---

## 5 · The Nine Defects, by Layer

Each subsection: symptom → root cause → fix → reference file.

---

### 5.1 Replace hunks rewrote the whole file

**Symptom**
A one-line edit (e.g. `level = "info"` → `level = "debug"`) caused the
recorded change to contain a new vertex for *every* line of the file,
turning a 1-line edit into a 9-vertex insert + 9-edge delete. The
merged view showed the entire file twice — once from each side's
reconstruction.

**Root cause**
For a `Replace` hunk, the globalize pipeline passed the **whole new
file content** to `globalize_replace`, which called
`create_content_vertices_per_line` on that content. Result: per-line
vertices were created for every line in the new file, not just the
replacement lines.

The comment that justified this was still describing the old "delete all
existing vertices and re-insert the complete new file" model:

```rust
// Replace hunks need the full file content because they delete
// all existing vertices and re-insert the complete new file.
```

With per-line vertex surgery, "all existing vertices" should be just the
targeted ones, and the content slice should be just the replacement
lines.

**Fix**
Added a `slice_lines(content, start, len)` helper that extracts only the
byte range covering the replacement lines, and use it for Replace
hunks.

```rust
let replace_slice;
let hunk_content: &[u8] = match built.kind {
    BuiltHunkKind::Replace => {
        replace_slice = slice_lines(content, built.new_start, built.new_len);
        replace_slice
    }
    BuiltHunkKind::Insert => { /* existing per-hunk slice */ }
    BuiltHunkKind::Delete => &[],
};
```

**Reference**
`atomic-core/src/record/workflow/globalize/pipeline.rs` (the modification
branch around `built.kind` matching; `slice_lines` helper near
`enrich_file_ops_for_add`).

---

### 5.2 `BlockDeleted` edges followed as alive forward edges

**Symptom**
After feature wrote a `BlockDeleted(C2)` edge from `V[server]` to
`V[host=localhost]`, dev's view (which doesn't include C2) treated the
`BlockDeleted` edge as alive. The same destination `V[host=localhost]`
then appeared in the children list **twice** — once via the original
`BLOCK(C1)` edge, once via the (mis-classified-as-alive) `BLOCK|DELETED(C2)`
edge. Fork detection saw two children at the same vid and emitted
conflict markers around three identical copies of `[server]`.

**Root cause**
`is_edge_alive` had logic that treated a `DELETED` edge as "alive" when
its introducing change was outside the filter. The reasoning was "from
this view, the deletion hasn't happened, so the edge is still live."

The reasoning sounds right but conflates two distinct objects: the
*original* `BLOCK(C1)` edge (which still exists in the B-tree, and *is*
the live edge) and the **deletion marker** `BLOCK|DELETED(C2)` (which is
not itself a forward connection — it's a flag on the C1 edge's
liveness). Treating the latter as a forward edge double-counts.

**Fix**
A `BlockDeleted` / `FolderDeleted` edge is *never* alive as a forward
edge. Reachability comes from the original edge. The "outside the view"
case is correctly handled by `is_vertex_alive`'s parent-edge inspection.

```rust
pub fn is_edge_alive(&self, edge: &ForwardEdge) -> bool {
    !edge.kind.is_deleted()
}
```

**Reference**
`atomic-core/src/output/alive/retrieve/options.rs` (`is_edge_alive`).
Test expectations updated in
`atomic-core/src/output/alive/retrieve/tests.rs`.

---

### 5.3 Empty-flag down edges invisible to typed `iter_forward`

**Symptom**
A new vertex `V_new` recorded as a Replace had its predecessor
properly wired (`inode → V_new`) but its **successor** edge (`V_new →
V_next_line`) was invisible to forward traversal. Materialization would
emit `V_new` and then nothing — the rest of the file disappeared. In
the inverse direction: the new vertex's parent edge from `V_next_line`
was also dropped, so `is_vertex_alive(V_next_line)` returned `false`
and the next line was reported dead.

**Root cause**
Legacy apply code stripped `BLOCK` from down edges:

```rust
let down_flag = if insertion.flag.is_folder() {
    insertion.flag
} else {
    insertion.flag - EdgeFlags::BLOCK
};
```

That produces an edge with **empty** flags. The typed edge model
introduced after this code's original design rejects empty flags as a
valid `EdgeKind`:

```rust
pub fn from_flags(flags: EdgeFlags) -> Option<Self> {
    match flags {
        f if f == EdgeFlags::BLOCK => Some(Self::Block),
        // … no `EdgeFlags::empty()` variant …
        _ => None,
    }
}
```

So `iter_forward` and `iter_parents` silently dropped these edges. The
legacy Pijul design (where these edges *were* meaningfully non-BLOCK)
no longer applied — we wanted both directions to be `BLOCK`.

**Fix**
Use the same flag for predecessor and successor edges.

```rust
let down_flag = insertion.flag | EdgeFlags::BLOCK;
```

**Reference**
`atomic-core/src/apply/insertion.rs` (`apply_insertion` — the
down_flag block).

---

### 5.4 `walk_through_dead` conflated dead-walked with alive-found

**Symptom**
When V_h1_new was wired between dead V[5] and dead V[7], the dead-walk
from V[5] correctly discovered V_h1_new as an alive successor. But the
*same* walk's continuation past V[5]→V[6]→V[7] also discovered
downstream alive vertices, and they got wired as duplicate children of
the upstream alive parent — manifesting as forks at body sections that
should have been chains.

**Root cause**
`walk_through_dead` tracked everything it visited in a single `visited`
HashSet. The `has_alive_alt_parent` and `claimed_by_alive_outsider`
checks consult that set to decide whether a vertex is "claimed by some
other alive path". But `visited` contained both:

- dead vertices the walk had stepped through (correct to exclude — they
  are part of the dead chain)
- alive vertices the walk had *discovered* (incorrect to exclude — they
  are legitimate alternate parents)

A dead vertex with an alive parent we'd just discovered was being
misread as "no alt parent", so its downstream got attached to the
wrong vertex.

**Fix**
Split into `dead_visited` (used in alt-parent and outsider checks) and
`seen` (only for BFS de-duplication). Push to `dead_visited` only when
genuinely walking *through* a dead vertex; never when reaching an
alive one.

**Reference**
`atomic-core/src/output/alive/retrieve/mod.rs` (`walk_through_dead`).

---

### 5.5 Fork detection treated linearly-ordered children as concurrent

**Symptom**
After fixing the duplication bugs above, materialization started
showing **chains** as forks. A parent vertex with two children where
one reaches the other through the alive graph DAG (the additive model
naturally creates these "diamond paths") was reported as a CRDT
conflict.

**Root cause**
`detect_fork_conflicts` checked SCC membership but not reachability.
Two children in different single-vertex SCCs were assumed concurrent.
In an additive edge graph this is too coarse — they can be linearly
ordered through subsequent dead-walk bypasses without sharing an SCC.

**Fix**
After the SCC check, reduce the children list to a **maximal
antichain** via reachability:

```rust
// Drop any child reachable from another child via the alive graph DAG.
// Those are downstream of a sibling and belong to that sibling's
// chain, not to a concurrent fork.
let mut antichain: Vec<VertexId> = Vec::new();
for &c in &content_children {
    let dominated = content_children.iter().any(|&other|
        other != c && reachable(other, c));
    if !dominated { antichain.push(c); }
}
```

Only report a fork when the antichain has more than one element.

**Reference**
`atomic-core/src/output/repo/fork.rs` (`detect_fork_conflicts`).

---

### 5.6 `HunkBuilder` combine threshold destroyed unchanged middle lines

**Symptom**
When the diff produced two `Replace` ops separated by 4 unchanged
lines (e.g. "change title on line 3, change paragraph on line 7"), the
builder merged them into a single `Replace` with `old_len=5,
new_len=2`. The slice extracted for that hunk contained only the 2
replacement lines, but the deletion targeted 5 vertices. The 3
unchanged middle lines were silently dropped.

**Root cause**
`HunkBuilder::add_pending` combined two `PendingChange`s when the gap
between them was below `combine_threshold` (default 6). The combine
math summed `new_len` from both sides but used the *outer* range of
old line indices, losing the unchanged-lines-in-between:

```rust
let new_old_len = other_old_end.saturating_sub(self.old_start);  // 5
let combined_new_len = self.new_len + other.new_len;             // 2
```

This was correct for the *display* use case (where wide combines make
diffs more readable) but destructive in the patch-theory graph context.

**Fix**
Set the default `combine_threshold` to 0. Only directly adjacent
changes (e.g. a Delete immediately followed by an Insert that
collapses into a Replace) combine. Unchanged-line gaps preserve their
existing vertices.

**Reference**
`atomic-core/src/record/workflow/graph_op/options.rs` (`DEFAULT_COMBINE_THRESHOLD`).

---

### 5.7 Bypass children attached to parent instead of last direct alive child

**Symptom**
`V[body]` had two children: `V_h1_new` (direct alive forward edge,
representing "the new `<h1>` line") and `V_<p>_new` (bypass child found
by walking through dead `V[7]`, representing "the new `<p>` line").
They are linearly ordered in the original chain (`<h1>` precedes
`<p>`), but the byte graph wired both as siblings of `V[body]`,
producing a fork.

**Root cause**
When `walk_through_dead` returned live successors via a dead chain,
they were attached as children of the *parent that triggered the
walk*. But that parent may already have direct alive children that
sit *earlier* in the chain — the bypass children belong downstream of
those, not as parallel siblings.

**Fix**
Track bypass children separately during edge iteration. After the
edge loop:

- If the vertex has *no* direct alive children, attach bypass children
  here (the existing behaviour).
- If it *does*, queue them in `pending_bypass[last_direct_child]` so
  they attach to that direct child's children list when it's popped
  off the traversal stack.

```rust
if !bypass_children.is_empty() {
    let direct_child = children_to_add.iter().rev()
        .find_map(|(_, v)| if !v.is_dummy() { Some(*v) } else { None });
    if let Some(direct) = direct_child {
        pending_bypass.entry(direct).or_default()
            .extend(bypass_children.iter().copied());
    } else {
        for vid in &bypass_children { children_to_add.push((None, *vid)); }
    }
}
```

**Reference**
`atomic-core/src/output/alive/retrieve/mod.rs` (the deferred-attach
block in the stack loop).

---

### 5.8 Change-DAG supersession + `ResolvedConflicts::is_empty` fast-path

**Symptom**
A fork between "C2's `<p>` (inserted via merge)" and "C4's `<p>`
(written after the merge)" was emitted as conflict markers, even
though C4 was recorded *with C2 visible* — meaning C4's edit is the
authoritative one.

**Root cause (two layers)**

**(a)** The byte graph alone cannot encode "C4 supersedes C2 because
it was authored knowing about C2". That information lives in the
**change DAG** (`CHANGE_DEPS`).

**(b)** Even after adding a supersession check that marked the loser
via `resolved.insert_skip(vid)`, the loser still emitted. Reason:
`ResolvedConflicts::is_empty()` only checked the `merged` map; an
empty `merged` with a populated `skip` returned `true`. The
output-writer's fast path keyed off `is_empty()` to call the
non-skip-aware `output_graph_content`, bypassing every skip we'd
recorded.

**Fix**

Added a `supersedor_in_fork` helper that walks each child change's
indexed dependency closure and returns the index of the change that
transitively depends on all the others. If found, mark the losers
`skip` and emit the winner via the normal path.

```rust
if let Some(winner_idx) = supersedor_in_fork(txn, &fork.children, graph) {
    for (idx, &vid) in fork.children.iter().enumerate() {
        if idx != winner_idx { resolved.insert_skip(vid); }
    }
    continue;
}
```

And tightened the fast-path predicate:

```rust
pub fn is_empty(&self) -> bool {
    self.merged.is_empty()
        && self.skip.is_empty()
        && self.unresolved_forks.is_empty()
}
```

This is the place where *semantic-layer delegation* actually pays off
— the byte graph stays oblivious; the change DAG (which already
encodes the dependency information) settles the ordering.

**Reference**
- `atomic-core/src/output/repo/content.rs` (`supersedor_in_fork`,
  `resolve_conflicts_semantically`)
- `atomic-core/src/merge/resolved.rs` (`is_empty`)

---

### 5.9 Single-vertex Replace fell back to unfiltered `INODE_GRAPH`

**Symptom (the most insidious one)**
For a one-line file, agent A bumped a constant to `10000`, agent B
bumped it to `30000`. Both edits should produce a fork with markers.
Instead, the merged view silently dropped agent A's value and showed
only agent B's `30000`.

**Root cause**
`globalize_replace` had an over-conservative early-return:

```rust
let sorted = collect_sorted_content_vertices(ctx.txn(), inode, inode_pos)?;
if sorted.len() <= 1 {
    return globalize_replace_whole_file(ctx, inode, inode_pos, content, …);
}
```

The comment justified this as "legacy: file is one big vertex" — but
a *single-line* file in the per-line vertex model legitimately produces
exactly one vertex. The targeted path handles that correctly
(`predecessor=inode`, `successor=None`, delete that one vertex, insert
the replacement). The early-return shouldn't fire.

`globalize_replace_whole_file` then called `find_content_vertices`,
which uses the `INODE_GRAPH` secondary index. That index is **not view-filtered**.

So agent B's record, scanning `INODE_GRAPH` for `config.ts`, saw both
the original `V_5000` *and* agent A's `V_10000` (recorded on a
different view). Both vertices' introducing changes were registered as
**dependencies of agent B's change**.

When the change-DAG supersession check (5.8) ran on the resulting fork
between V_10000 and V_30000, it correctly observed "V_30000's change
depends on V_10000's change" and quietly skipped V_10000 — silently
losing agent A's edit.

In short: the unfiltered `INODE_GRAPH` read leaked vertices from other
views into the recording change's dependency list, which manufactured a
phantom supersession relationship.

**Fix**

```rust
if sorted.is_empty() {
    return globalize_replace_whole_file(ctx, inode, inode_pos, content, …);
}
```

Single-vertex files use the targeted path (which reads through `ViewGraph`,
view-filtered). Whole-file fallback only fires when there's genuinely
nothing to target.

**Reference**
`atomic-core/src/record/workflow/globalize/hunk.rs` (`globalize_replace`).

---

## 6 · Why this took nine fixes

Each defect on its own is small. They piled up because the codebase was
**half-migrated** between two models:

```text
Old model                              New model
─────────                              ─────────
single vertex per file                 per-line vertex chain
diff materialized text → Replace       diff text → targeted line replace
delete-and-rewrite-everything           additive edges (DELETED flag)
text-level 3-way merge of file         token-level CRDT merge of branches
"a view owns its bytes"                "a view is a filter on the global graph"
```

Each of those interaction surfaces — the diff builder's combine logic,
the apply's down_flag stripping, the fallback to unfiltered
`INODE_GRAPH`, the byte-graph's inability to topologically order
linearly-related children — is a place where the LLM-assisted
implementation **reverted to assuming this is git**: snapshot the file,
combine adjacent diff hunks for display, walk the file's edges out of
a per-file index, treat two alive content vertices as "two competing
versions of the same line". Each surface was fine in isolation, but
the combination produced the cascading misreads we kept finding —
because every surface was answering "what would git do here?" while
the system above and below them expected "what does patch theory say
the graph delta is?".

The fix pattern that kept working was the same one: **stop iterating the
materialized text or the unfiltered B-tree to make graph decisions;
delegate to the structure that actually encodes the answer**:

- For *ordering* between non-overlapping edits → the change DAG
  (`CHANGE_DEPS`).
- For *content merge* of competing edits → the CRDT layer
  (`tokenize` + `three_way_merge` over Trunk/Branch/Leaf).
- For *reachability* between fork children → the alive graph DAG itself
  (antichain reduction).
- For *view isolation* during record → the `ViewGraph` wrapper around
  `iter_adjacent` (never the raw `INODE_GRAPH`).

---

## 7 · Results

| Suite                             | Before    | After       |
|-----------------------------------|-----------|-------------|
| `atomic-core` (lib)               | 3338 / 0  | **3338 / 0** |
| `atomic-repository` (lib)         | 759 / 7   | **767 / 0** |
| `tests/harness/17_cross_view_merge.sh` | 58 / 14   | **103 / 0** |

Harness 17 was also extended with four new agent-style scenarios:

- **Case 9** — token-disjoint same-line merge (rename + value change → auto-compose)
- **Case 10** — same-token overlap (markers preserved, no silent loss)
- **Case 11** — structural same-line edits (parameter add + return type)
- **Case 12** — multi-file refactor (rename across files + body change in one file)

---

## 8 · Files Touched

### Storage / record / apply
- `atomic-core/src/apply/insertion.rs` — down_flag uses `BLOCK` (5.3)
- `atomic-core/src/record/workflow/globalize/pipeline.rs` — `slice_lines`, Replace hunk slice (5.1)
- `atomic-core/src/record/workflow/globalize/hunk.rs` — `globalize_replace` early-return tightened (5.9)
- `atomic-core/src/record/workflow/graph_op/options.rs` — `DEFAULT_COMBINE_THRESHOLD = 0` (5.6)
- `atomic-core/src/record/workflow/graph_op/tests.rs` — expectation update for above

### Graph retrieval / output
- `atomic-core/src/output/alive/retrieve/options.rs` — `is_edge_alive` (5.2)
- `atomic-core/src/output/alive/retrieve/mod.rs` — `walk_through_dead` set split (5.4); bypass-child deferred attach (5.7); children dedupe
- `atomic-core/src/output/repo/fork.rs` — antichain reduction (5.5)
- `atomic-core/src/output/repo/content.rs` — `supersedor_in_fork` (5.8)
- `atomic-core/src/merge/resolved.rs` — `is_empty` checks all fields (5.8)

### Tests
- `atomic-core/src/output/alive/retrieve/tests.rs` — updated for new `is_edge_alive`
- `atomic-repository/src/repository/tests/cross_view_merge_tests.rs` — new lib-level cases
- `atomic-repository/src/repository/tests/mod.rs` — register new tests
- `tests/harness/17_cross_view_merge.sh` — Cases 9-12 (agent token merges); two false-positive assert fixes in Cases 2 and 5

---

## 9 · The deeper finding — CRDT scaffolding without CRDT functionality

While investigating the residual hyperfine commit-4 regression (the
multi-hunk Insert case where a vertex has more than one alive forward
edge), an audit of the CRDT layer surfaced a structural debt that
predates this PR and explains why the byte-graph walker keeps
accumulating heuristics:

> **The CRDT redb tables are defined and have writers, but nothing
> in production actually populates them.** A fresh repository will
> never have a single row in `crdt_trunks`, `crdt_branches`, or
> `crdt_leaves`. The Trunk → Branch → Leaf hierarchy that §1's
> preamble describes — and that we advertise externally — is
> currently a schema, not a database.

### What is plumbed vs. what is functional

| Component | Plumbed? | Functional? | Notes |
|---|---|---|---|
| `crdt_trunks` / `crdt_branches` / `crdt_leaves` schema | ✅ | ❌ | Tables exist, never written to |
| `crdt_trunk_branches` / `crdt_branch_leaves` (ordering) | ✅ | ❌ | Same |
| `crdt_inode_trunk` reverse lookup | ✅ | ❌ | Same |
| `BRANCH_VERTEX` / `VERTEX_BRANCH` bridge | ✅ | ❌ | Same |
| `put_crdt_trunk` / `put_crdt_branch` / `put_crdt_leaf` writers | ✅ | ❌ | Methods exist on `MutTxnT`, no production caller |
| `iter_trunk_branches` / `iter_branch_leaves` readers | ✅ | ❌ | Methods exist, no caller |
| FileOps generation in record (TrunkOp/BranchOp/LeafOp) | ✅ | ✅ — but as **change-file metadata only** | Embedded inside the change file; never reaches the redb CRDT tables |
| `atomic diff` token-level highlighting | ✅ | ✅ | Reads FileOps from the change file *in memory*, renders an overlay |
| `SemanticMergeEngine::try_merge` | ✅ | ✅ | Calls `tokenize()` on raw bytes at merge time, three-way merge in memory |
| `output_file_via_crdt` (the doc's Task #1) | ❌ | ❌ | Doesn't exist — would query empty tables anyway |
| `collect_sorted_via_crdt` for record line ordering | ❌ | ❌ | Same |

### Why this matters

The implementation pattern of consuming **change-file FileOps in memory
to render an overlay** is structurally the same shape as `git` reading
a `.pack` to render a diff. We advertise a multi-level CRDT graph
database; what's actually shipping is a byte-range graph (which is
real and persistent) plus a token-level diff renderer that consumes
in-memory metadata from change files. The "CRDT graph" doesn't exist
as a queryable persistent structure today.

This is also why every defect in §5 had to be patched at the byte-graph
layer: the layer above it — the one that would naturally answer
"what is the *alive line at position N for inode I in view V*" — has
the schema but not the data. The byte-graph walker keeps trying to
re-derive that answer from BLOCK edges + vertex-aliveness rules, and
that's exactly the question Insert hunks make ambiguous (multiple
alive forwards from one vertex). The CRDT graph would make it
unambiguous by construction — but only once the data is there.

### The hyperfine commit-4 residual

After the §5 fixes, hyperfine commits 1, 2, and 3 roundtrip byte-exact
(this was the original bug class: content duplication). Commit 4 —
which extracts a `get_progress_bar` helper function (containing a copy
of the existing `progressbar_style` block) and deletes the original
copies from `main()` — produces 4630 bytes vs 4539 expected (~2%
over-count). The duplication comes from the linear `collect_sorted`
walker reaching the original successor via the still-alive `Block(C1)`
edge and missing the C4-inserted helper-function vertices via the
sibling `Block(C4)` edge. Materialize then emits both paths.

This is the structural limit of the byte-graph approach for Insert
hunks — and the case the CRDT layer is *designed* to handle, by
storing line identity (Branch IDs) rather than inferring it from
graph topology.

### What "real" CRDT functionality would mean

| Piece | Effort |
|---|---|
| 1. `apply_change` calls into `crdt::apply` so Trunk/Branch/Leaf rows + bridge tables (`BRANCH_VERTEX`/`VERTEX_BRANCH`) land in redb during the apply step | Most of the work — needs full coverage across FileAdd / Insert / Replace / Delete |
| 2. Read-side accessors moved (or mirrored) onto an immutable trait so output paths can use them without `&mut` | Mechanical |
| 3. `output_file_via_crdt(inode, change_filter)` walking Trunk → branches (filtered) → leaves | New code, but small once data is present |
| 4. `collect_sorted_via_crdt` replacing the byte-graph linear walker in record | Small; closes the hyperfine commit-4 regression |
| 5. CRDT-first git import: tokenize once, hash lines, only deep-compare changed lines, emit BranchOp/LeafOp directly — skipping the diff-then-globalize round trip | The performance and identity-preservation win |
| 6. Backfill: `atomic doctor --rebuild-crdt-index` (or lazy on first read) so existing repos with FileOps-in-changes but no redb CRDT rows don't break after the upgrade | Necessary for forward-compatibility |

Estimated effort: **3–5 focused days**, depending on how complete
`atomic_core::crdt::apply` already is. The module exists with
`branch.rs`, `leaf.rs`, `trunk.rs`, `conflict.rs`, `context.rs`,
`order.rs`, `traits.rs` — much of the write-side logic may already be
implemented but never wired into the main `apply_change` path. The
audit task #20 will clarify which.

### Performance argument

Git import today: per commit, run Myers diff (O(N²) worst case in
file lines) → build `BuiltHunk`s → globalize each hunk into vertex/edge
operations → apply.

CRDT-first git import: per commit, tokenize the file content directly
into Branches/Leaves keyed by stable `BranchId`, compare against the
previous CRDT state (O(N) by line-hash equality, deep token compare
only on changed lines), emit `BranchOp::Modify`/`Insert`/`Delete`
directly → apply writes BOTH the CRDT tables and the byte-graph
indices that retrieve depends on.

This bypasses the diff-then-globalize round-trip entirely and
preserves token identity across reformats — both flagship value props
for the system that the current pipeline cannot deliver.

---

## 10 · What the previous "Task #1 / CRDT-driven output" guidance got wrong

The earlier handoff doc claimed:

> The CRDT tables are already populated during record (`Trunk`,
> `Branch`, `Leaf` rows go into the B-tree alongside graph edges),
> so the option stays open whenever we choose to take it.

This was incorrect. The CRDT *FileOps inside the change file* are
populated (and that's what `atomic diff` consumes). The CRDT
*redb tables* are not. Until they are, neither
`output_file_via_crdt` nor a CRDT-aware `collect_sorted` will work.
This RCA supersedes that earlier note.

---

## 11 · Three layers of CRDT-table-population breakage

Once we started writing the audit test to see *why* the CRDT tables
were empty, three distinct defects in the write path showed up. The
first two are fixed in this PR. The third is the load-bearing reason
the tables can never reach a consistent state and is the next thing
to land.

### 11.1 · Inode mismatch on `TrunkOp::Create` (fixed in this PR)

`apply_trunk_op` for `Create` called `txn.alloc_inode()` and used the
result as the CRDT trunk's inode. The tree layer had already allocated
its *own* inode for the same file during `write_recorded`'s FileAdd
pre-pass. The two inodes were never reconciled: the tree layer
indexed `path → tree_inode → graph position`, while the CRDT layer
indexed `crdt_inode → trunk_id`. A caller asking "what is the CRDT
trunk for the inode the tree returned for this path" would always
miss — even though both rows existed.

**Fix.** `apply_trunk_op::Create` now calls `txn.get_inode(path)` and
reuses the tree's inode (with `alloc_inode` only as a fallback for
CRDT-only callers that have no tree entry).

**Reference.** `atomic-core/src/apply/file_ops.rs`,
`apply_trunk_op` — `TrunkOp::Create` arm.

### 11.2 · `BranchId` placeholder collision (fixed in this PR)

The record path allocates BranchIds with a constant
`placeholder_change_id = NodeId::new(0)` — i.e. `NodeId::ROOT` — so
every change's record produces `BranchId(ROOT, 0)`,
`BranchId(ROOT, 1)`, … The convention is "fill in the real change id
at apply time", analogous to how vertex `Position`s use
`change: None` as a placeholder and `resolve_position` substitutes the
current change id during apply.

But apply did not substitute. `apply_line_ops_with_position` used the
recorded `BranchId(ROOT, N)` verbatim as the BRANCHES table key.
Every commit's first Insert wrote to the same key as commit 1's first
Insert and overwrote it. The audit saw commit 1's 82 branches, commit
2's 82 branches replaced ~half of them, etc.

**Fix.** `apply_line_ops_with_position` now resolves
`BranchId(NodeId::ROOT, idx)` → `BranchId(current_change_id, idx)`
before using it as a key. LeafIds inherit the resolved
`branch_id.change_id()` so the substitution propagates to the leaf
layer automatically.

**Reference.** `atomic-core/src/apply/file_ops.rs`,
`apply_line_ops_with_position` — top of the function.

### 11.3 · Record path never looks up existing branches (FIXED in this PR)

This was the load-bearing one. After 11.1 and 11.2 the BRANCHES table
finally grew with each commit. But the audit showed branches **only
grew — they never got deleted**:

```
Before fix:
CRDT AUDIT commit 1 (a658ab8c): branches=82  alive=82  deleted=0
CRDT AUDIT commit 2 (d4ebdd7b): branches=134 alive=134 deleted=0
CRDT AUDIT commit 3 (197f9fb): branches=156 alive=156 deleted=0
CRDT AUDIT commit 4 (68fdc2c): branches=200 alive=200 deleted=0

After fix:
CRDT AUDIT commit 1 (a658ab8c): expected=82  branches=82  alive=82  deleted=0
CRDT AUDIT commit 2 (d4ebdd7b): expected=122 branches=124 alive=122 deleted=2
CRDT AUDIT commit 3 (197f9fb): expected=127 branches=130 alive=127 deleted=3
CRDT AUDIT commit 4 (68fdc2c): expected=161 branches=165 alive=162 deleted=3
```

Alive counts now track expected line counts (within ~1 line),
deletions actually happen, and branches turn over rather than
accumulating forever.

Every line the recorder ever observed sits alive forever, even when
the file has visibly fewer alive lines than the cumulative count
because lines were deleted or modified across commits.

The cause is upstream in the recorder. Look at
`atomic-core/src/record/workflow/record/crdt.rs`:

```rust
let placeholder_change_id = NodeId::new(0);
…
let mut alloc_branch = || {
    let id = BranchId::new(placeholder_change_id, next_branch_idx);
    next_branch_idx += 1;
    id
};
```

`alloc_branch()` is called for **every** line operation — Insert,
Delete, and Modify alike — and unconditionally returns a fresh
placeholder. There is no lookup that says "for this Modify of old
line N, the existing branch is `BranchId(prior_change_X, idx_Y)`".

Consequences:

- **Insert** ops carry `BranchId(ROOT, k)`. After 11.2's apply
  substitution this becomes `BranchId(current_change, k)` — a new,
  unique branch identifier. Correct.

- **Delete** ops carry `BranchId(ROOT, k)`. After substitution it
  becomes `BranchId(current_change, k)` — a *brand-new* branch
  id, not the one being deleted. `update_branch_state` calls
  `get_crdt_branch(this_new_id)` which returns `None`, so the
  state update is a silent no-op.

- **Modify** ops carry `BranchId(ROOT, k)`. Same outcome: the old
  branch isn't marked Deleted (the id doesn't refer to it), and the
  new branch *replaces* the (non-existent) old one with itself —
  another silent no-op as far as marking the prior content gone.

`atomic diff` keeps working anyway because `diff` reconstructs lines
from the leaf-ops embedded inside the `BranchOp::Modify` / `Delete`
variants — it never queries the BRANCHES table. So a user inspecting
recent commits sees correct token-level highlights and assumes the
CRDT graph is functional. The graph is *not* functional; only the
diff overlay is.

### 11.4 · What "fix the record path" looks like (now implemented)

For the recorder to produce CRDT-consistent ops, it needs CRDT state
to compare against. The implemented shape:

- `iter_trunk_branches_in_file_order(txn, trunk_id)` returns the
  trunk's branches in **file order** (top-of-file first) by walking the
  new `BRANCH_AFTER` chain (RCA §11.6).  TRUNK_BRANCHES alone would have
  given BranchId-sort order, which mis-orders any later commit that
  prepends a line.
- `record_modified_file` gained an `existing_branches:
  Option<&[BranchId]>` parameter.  The repository call site
  (`repository::record`) reads the file's existing branches via the
  helper above and threads them through.
- `build_crdt_ops_for_modified_file` accepts the same parameter and uses
  `existing_branches[old_line_idx]` for **Delete** and **Modify** ops
  (anywhere the old line's identity must be referenced), and continues
  to allocate fresh placeholders for **Insert** (genuinely new lines).
- Other callers (`git import` parallel path, `parallel_record` worker,
  tests) pass `None` — acceptable because the git path overrides CRDT
  ops via `build_crdt_ops_from_git_diff` at write time.

The pre-fix design is preserved below for context:

1. **Load the current CRDT branch list for the file's trunk.** Walk
   `crdt_trunk_branches[trunk_id]` (filtered by view) in order. Build
   a mapping `old_line_index → (existing BranchId, line_hash)`.

2. **For each diff op:**
   - `Equal` → no CRDT op needed. Optional: bump `prev_branch` to the
     last existing branch so subsequent `Insert::after` references it.
   - `Delete(old_pos, len)` → emit `BranchOp::Delete { branch:
     existing_branch_id[old_pos + i] }` for each deleted line, with
     `old_content` snapshot for diff display.
   - `Insert(new_pos, len)` → emit `BranchOp::Insert { after: prev,
     content: leaves }`. `prev` is the existing BranchId at the line
     just before `new_pos`. The new branch is freshly allocated under
     this change.
   - `Replace(old_pos, old_len, new_pos, new_len)` → emit
     `BranchOp::Modify { branch: existing_branch_id[old_pos + i] }`
     when sizes match; otherwise a Delete + Insert combination, each
     with the right existing-branch references.

3. **Allocator becomes hybrid.** `alloc_branch` (for genuinely new
   branches) keeps producing `BranchId(ROOT, next_idx)` for the
   placeholder-resolution-at-apply path. Existing-branch references
   come from the trunk-branch list, with their real change_ids.

This makes the record path **CRDT-aware**. It's the missing piece
that makes the redb CRDT tables a real database instead of an
insert-only log.

### 11.5 · Implication for the broader plan

| Step | Status |
|---|---|
| Inode mismatch (11.1) | Fixed |
| BranchId placeholder collision (11.2) | Fixed |
| TRUNK_BRANCHES preserves file order via `BRANCH_AFTER` (11.6) | Fixed |
| Record-side existing-branch lookup (11.3) | Fixed |
| Read accessors on the immutable trait | Pending (currently using write-txn-as-read window) |
| `output_file_via_crdt` walker | Pending |
| Replace `collect_sorted_content_vertices` with CRDT-driven version | Pending — closes hyperfine commit-4 byte-graph over-count |
| CRDT-first git import | Pending; the performance win |
| Backfill (`atomic doctor --rebuild-crdt-index`) | Pending |

The CRDT layer now correctly tracks branch lifecycle.  The byte-graph
fix (linear-walker problem) is the remaining piece blocking byte-exact
hyperfine roundtrip at commit 4.

### 11.6 · `TRUNK_BRANCHES` did not preserve file order (FIXED in this PR)

A prerequisite that surfaced while wiring 11.3: the `TRUNK_BRANCHES`
multimap stores branch IDs sorted by their natural key
`(change_id, branch_idx)`.  This means a *later* commit that prepends a
line at the top of the file would store its new branch *after* all of
the original commit's branches in iteration order — fine for stable
identity, wrong for presentation, and fatal for "look up the branch
for line N" semantics.

Fix:

- New table `BRANCH_AFTER: BranchId → BranchId` records each branch's
  predecessor at apply time (`[0u8; 12]` sentinel = "start of file").
- `iter_trunk_branches_in_file_order` walks the chain with
  deterministic `BranchId` tie-breaks for concurrent inserts —
  the same rule used by `crdt::apply::order::find_insert_position`.
- `output/crdt.rs::get_file_lines_by_trunk` switched to the new helper
  so line numbering is correct in the presence of prepended lines.

---

## 10 · Quick reference — the architecture in one diagram

```text
                      ┌──────────────────┐
                      │   redb B-tree    │ ← persistent storage
                      │  (GRAPH, INODE_  │
                      │   GRAPH, CRDT_*) │
                      └────────┬─────────┘
                               │
            ┌──────────────────┼──────────────────┐
            ▼                  ▼                  ▼
    ┌───────────────┐  ┌────────────────┐  ┌──────────────┐
    │  Byte-range   │  │  CRDT layer    │  │ Change DAG   │
    │  graph        │  │  Trunk→Branch  │  │ (CHANGE_DEPS │
    │  (vertices +  │  │  →Leaf, with   │  │  index)      │
    │  edges)       │  │  stable IDs    │  │              │
    └──────┬────────┘  └────────┬───────┘  └──────┬───────┘
           │                    │                  │
           │                    │                  │
           └────────────────┬───┴──────────────────┘
                            │
                    ┌───────▼────────┐
                    │ Change filter  │ ← runtime overlay per view
                    │ (HashSet<      │   built from VIEW_CHANGES
                    │  NodeId>)      │   + parent chain
                    └───────┬────────┘
                            │
            ┌───────────────┼───────────────┐
            ▼               ▼               ▼
       retrieve_graph   try_merge      supersedor_in_fork
       (byte walk)      (CRDT 3-way)   (DAG closure)
            │                │               │
            └────────┬───────┴───────────────┘
                     ▼
              output bytes
```

Three coordinated structures. One filter. Each fix in this RCA was
about routing the right question to the right structure.
