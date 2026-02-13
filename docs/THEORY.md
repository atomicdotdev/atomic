# Atomic VCS: Mathematical Foundations

## Abstract

This document describes the mathematical theory underlying Atomic, a distributed version control system based on **patch theory**. Unlike traditional line-based VCS systems, Atomic represents repository history as transformations on a directed graph, enabling mathematically sound merge operations with precise conflict semantics.

---

## 1. Foundational Concepts

### 1.1 The File as a Directed Graph

Traditional VCS systems represent files as sequences of lines. Atomic instead represents each file as a **directed acyclic graph (DAG)** where:

- **Vertices** represent chunks of content (typically lines)
- **Edges** represent sequential ordering between chunks

**Definition 1.1 (Content Graph):**
A content graph G = (V, E) where:
- V is a finite set of vertices
- E ⊆ V × V is a set of directed edges
- G is acyclic
- G has a distinguished ROOT vertex with no incoming edges
- G has a distinguished END vertex with no outgoing edges

**Example:**
```
A file containing:      Is represented as:
    Line 1                  ROOT
    Line 2                    │
    Line 3                    ▼
                           [Line 1]
                              │
                              ▼
                           [Line 2]
                              │
                              ▼
                           [Line 3]
                              │
                              ▼
                             END
```

### 1.2 Why Graphs Instead of Sequences?

Sequences fail to capture the **structure** of changes:

1. **Insertion ambiguity:** "Insert X after line 5" becomes ambiguous after other insertions
2. **Deletion conflicts:** Cannot distinguish "delete line 5" from "delete the content that was line 5"
3. **Move detection:** Moving code is indistinguishable from delete + insert

Graphs solve these by giving each piece of content a **stable identity** independent of position.

---

## 2. Vertices and Positions

### 2.1 Vertex Identity

**Definition 2.1 (Vertex Identifier):**
A vertex is uniquely identified by a triple (c, s, e) where:
- c ∈ ChangeId: the change that introduced this vertex
- s ∈ ℕ: start position within the change's content
- e ∈ ℕ: end position within the change's content (s ≤ e)

**Notation:** We write v = (c, [s, e)) for a vertex spanning bytes [s, e) in change c.

**Property 2.1 (Immutability):**
Once created, a vertex's identity never changes. The content it references is immutable.

### 2.2 Positions

**Definition 2.2 (Position):**
A position p = (c, n) identifies a specific byte offset n within change c.

Positions are used to:
- Reference the start/end of vertices
- Specify insertion points
- Define edge endpoints

### 2.3 Vertex Splitting

**Theorem 2.1 (Splittability):**
Any vertex v = (c, [s, e)) can be split at position m (where s < m < e) into two vertices:
- v₁ = (c, [s, m))
- v₂ = (c, [m, e))

such that all edges to v are redirected to v₁, and all edges from v are redirected from v₂.

**Proof sketch:** The content is stored by change c. Splitting only changes the graph structure, not the underlying data. □

This property is essential for fine-grained edits within existing content.

---

## 3. Edges and Flags

### 3.1 Edge Structure

**Definition 3.1 (Edge):**
An edge e = (f, v₁, v₂, i) where:
- f ∈ EdgeFlags: the edge type
- v₁ ∈ V: source vertex
- v₂ ∈ V: destination vertex
- i ∈ ChangeId: the change that introduced this edge

### 3.2 Edge Flags

Edges are typed to represent different relationships:

| Flag | Symbol | Meaning |
|------|--------|---------|
| BLOCK | B | Sequential content ordering |
| PARENT | P | Reverse direction edge (for efficient traversal) |
| FOLDER | F | File system hierarchy |
| DELETED | D | Soft deletion marker |
| PSEUDO | Ψ | Computed edge for connectivity |

**Invariant 3.1 (Parent Duality):**
For every edge (f, v₁, v₂, i) where f does not contain PARENT, there exists a corresponding edge (f ∪ {PARENT}, v₂, v₁, i).

This invariant enables efficient bidirectional traversal.

### 3.3 Deletion Semantics

**Definition 3.2 (Deletion):**
Deleting content does not remove vertices or edges. Instead:
1. Add DELETED flag to edges pointing to the content
2. The content becomes "dead" but remains in the graph

**Rationale:** This preserves history and enables:
- Undeletion
- Conflict detection when deleted content is modified
- Accurate blame/annotation

**Definition 3.3 (Alive Content):**
A vertex v is **alive** if there exists a path from ROOT to v consisting entirely of non-DELETED edges.

**Definition 3.4 (Zombie):**
A vertex v is a **zombie** if:
- v has at least one incoming DELETED edge
- v has at least one alive descendant

Zombies represent potential conflicts that need resolution.

---

## 4. Changes (Patches)

### 4.1 Change Definition

**Definition 4.1 (Change):**
A change C is a tuple (H, D, K, Δ, B) where:
- H: header metadata (author, message, timestamp)
- D ⊆ Hash: set of dependency hashes
- K ⊆ Hash: set of "extra known" hashes
- Δ: sequence of hunks (atomic modifications)
- B: binary content blob

### 4.2 Hunks

**Definition 4.2 (Hunk):**
A hunk is one of:

1. **NewVertex(up, down, flag, content, inode)**
   - up: list of positions (up-context)
   - down: list of positions (down-context)
   - flag: edge flags for new edges
   - content: byte range in B
   - inode: file this vertex belongs to

2. **EdgeMap(edges, inode)**
   - edges: list of edge modifications
   - inode: file being modified

### 4.3 New Vertex Semantics

**Definition 4.3 (NewVertex Application):**
Applying NewVertex(up, down, f, [s,e), ι) to graph G:

1. Create vertex v = (C, [s, e)) where C is the current change
2. For each position p in up:
   - Find vertex u containing p
   - Add edge (f, u, v, C)
3. For each position p in down:
   - Find vertex d containing p
   - Add edge (f, v, d, C)

**Theorem 4.1 (Context Uniqueness):**
Given contexts (up, down), there is at most one valid position in G to insert the new vertex.

### 4.4 Edge Map Semantics

**Definition 4.4 (EdgeMap Application):**
An EdgeMap modifies existing edges:

```
NewEdge {
    previous: EdgeFlags,    // Flags to remove
    flag: EdgeFlags,        // Flags to add
    from: Position,         // Edge source
    to: Vertex,            // Edge destination
    introduced_by: ChangeId // Original edge's change
}
```

**Application:** Find the edge matching (from, to, introduced_by), remove `previous` flags, add `flag` flags.

**Common uses:**
- Deletion: Add DELETED flag
- Undeletion: Remove DELETED flag
- Conflict resolution: Modify ordering edges

---

## 5. Dependencies

### 5.1 Dependency Definition

**Definition 5.1 (Direct Dependency):**
Change C₂ **directly depends** on change C₁ (written C₁ ≺ C₂) if any hunk in C₂ references a vertex or edge introduced by C₁.

**Definition 5.2 (Dependency Closure):**
The dependency closure of C is the transitive closure of ≺:
```
deps(C) = {C' | C' ≺* C}
```

### 5.2 Dependency Properties

**Theorem 5.1 (Dependency Acyclicity):**
The dependency relation ≺ is acyclic.

**Proof:** Changes are identified by content hash. If C₁ ≺ C₂, then H(C₂) includes H(C₁) in its computation. Hash cycles are impossible. □

**Theorem 5.2 (Application Order):**
Changes can only be applied in an order consistent with ≺.

**Corollary:** Any topological sort of the dependency DAG is a valid application order.

### 5.3 Minimal Dependencies

**Definition 5.3 (Minimal Dependencies):**
The minimal dependencies of C are:
```
min_deps(C) = {C' ∈ deps(C) | ∄C'' : C' ≺ C'' ≺* C}
```

Only minimal dependencies need to be stored; the full closure is computed.

---

## 6. Application and Inverse

### 6.1 Change Application

**Definition 6.1 (Application Function):**
Let A(G, C) denote applying change C to graph G.

**Algorithm:**
```
A(G, C):
    G' ← G
    for each hunk h in C.Δ:
        G' ← apply_hunk(G', h, C)
    return G'
```

### 6.2 Inverse Changes

**Theorem 6.1 (Invertibility):**
Every change C has an inverse C⁻¹ such that A(A(G, C), C⁻¹) ≅ G.

**Construction:**
- NewVertex(up, down, f, content, ι) inverts to EdgeMap that adds DELETED to all edges of that vertex
- EdgeMap(edges, ι) inverts by swapping `previous` and `flag` for each edge

**Note:** The inverse depends on the state at application time, so C⁻¹ must be computed during application.

---

## 7. Merge Theory

### 7.1 The Merge Problem

**Definition 7.1 (Merge):**
Given graphs G_A and G_B that share common ancestor G₀:
- Let Δ_A = changes applied to get G₀ → G_A
- Let Δ_B = changes applied to get G₀ → G_B

Merge produces G_M containing all changes from both branches.

### 7.2 Commutativity

**Theorem 7.1 (Conditional Commutativity):**
If changes C₁ and C₂ are **independent** (neither depends on the other), then:
```
A(A(G, C₁), C₂) = A(A(G, C₂), C₁)
```

**Definition 7.2 (Independence):**
C₁ and C₂ are independent if:
1. C₁ ⊀ C₂ and C₂ ⊀ C₁
2. No hunk in C₁ modifies vertices/edges referenced by C₂, and vice versa

**Corollary:** Merging independent changes is trivial—apply them in any order.

### 7.3 Conflict Detection

**Definition 7.3 (Conflict):**
A conflict occurs when two changes modify the same structure incompatibly:

1. **Order Conflict:** Both C₁ and C₂ insert content with the same up-context
2. **Name Conflict:** Both create files/directories with the same name in the same parent
3. **Zombie Conflict:** C₁ deletes vertex v, C₂ modifies content depending on v

### 7.4 Conflict Representation

**Key insight:** Conflicts are represented **in the graph**, not as merge failures.

**Order Conflict Graph:**
```
    [context]
       │
   ┌───┴───┐
   ▼       ▼
[from C₁] [from C₂]
   │       │
   └───┬───┘
       ▼
  [next context]
```

Both alternatives exist simultaneously. The working copy shows conflict markers; resolution creates a new change that establishes the desired order.

### 7.5 Merge Algorithm

```
merge(G_A, G_B, G₀):
    # Find changes unique to each branch
    Δ_A = changes_since(G₀, G_A)
    Δ_B = changes_since(G₀, G_B)
    
    # Apply all changes to a combined graph
    G_M = G_A
    for C in topological_sort(Δ_B):
        if C not in G_A:
            G_M = A(G_M, C)
    
    # Detect conflicts
    conflicts = detect_conflicts(G_M)
    
    return (G_M, conflicts)
```

---

## 8. Merkle States

### 8.1 State Hashing

**Definition 8.1 (Channel State):**
The state of a channel is a Merkle hash computed incrementally:

```
S₀ = H(∅)
Sₙ = H(Sₙ₋₁ || H(Cₙ))
```

where H is a cryptographic hash function and C₁, ..., Cₙ are the applied changes in order.

### 8.2 State Properties

**Theorem 8.1 (State Uniqueness):**
Two channels have the same state S if and only if they have applied exactly the same set of changes.

**Theorem 8.2 (Efficient Comparison):**
Comparing channel states is O(1)—just compare the Merkle hashes.

### 8.3 Synchronization

**Algorithm (Efficient Sync):**
```
sync(local, remote):
    if local.state == remote.state:
        return  # Already synchronized
    
    # Find divergence point
    common = find_common_ancestor(local.states, remote.states)
    
    # Exchange changes since common ancestor
    local_new = changes_since(common, local)
    remote_new = changes_since(common, remote)
    
    send(remote_new - local_new)
    receive(local_new - remote_new)
```

---

## 9. File System Mapping

### 9.1 Inode Abstraction

**Definition 9.1 (Inode):**
An inode is a stable identifier for a file or directory, independent of its path.

**Mapping:**
- `tree: Path → Inode` (path to inode)
- `revtree: Inode → Path` (inode to path)
- `inodes: Inode → Position` (inode to graph position)

### 9.2 File Operations as Graph Operations

| File Operation | Graph Representation |
|----------------|---------------------|
| Create file | Add vertex + FOLDER edge from parent |
| Delete file | Add DELETED flag to FOLDER edge |
| Rename file | Remove old FOLDER edge, add new one |
| Edit content | Add/modify vertices within file's subgraph |

### 9.3 Path Independence

**Theorem 9.1 (Rename Independence):**
Renaming a file does not affect the identity of its content vertices.

This enables accurate tracking of changes across renames.

---

## 10. Complexity Analysis

### 10.1 Basic Operations

| Operation | Naive | With Inode Index |
|-----------|-------|-----------------|
| Find file vertices | O(N) | O(log N + n) |
| Apply change | O(h × log N) | O(h × log N) |
| Record change | O(n × log N) | O(n) |
| Merge | O((a + b) × log N) | O(a + b) |

Where:
- N = total vertices in repository
- n = vertices in single file
- h = hunks in change
- a, b = changes in each branch

### 10.2 Scaling Properties

**Theorem 10.1 (Sublinear Scaling):**
Operations on a single file scale with file size, not repository size.

**Theorem 10.2 (Logarithmic History):**
Looking up a change is O(log C) where C is total changes.

---

## 11. Invariants

The following invariants must be maintained:

1. **Graph Acyclicity:** The content graph is always a DAG
2. **Parent Duality:** Every forward edge has a reverse PARENT edge
3. **Connectivity:** All alive content is reachable from ROOT
4. **Dependency Consistency:** Applied changes form a valid topological order
5. **State Consistency:** Merkle state accurately reflects applied changes

---

## 12. References

1. Mimram, S., Di Giusto, C. "A Categorical Theory of Patches" (2013)
2. Jacobson, S. "Camp: Commuting Patches" (2009)
3. Roundy, D. "Darcs: Distributed Version Control in Haskell" (2005)
4. The Pijul Manual - Theory Section

---

## Appendix A: Notation Summary

| Symbol | Meaning |
|--------|---------|
| G = (V, E) | Graph with vertices V and edges E |
| v = (c, [s, e)) | Vertex from change c, positions s to e |
| e = (f, v₁, v₂, i) | Edge with flags f, from v₁ to v₂, introduced by i |
| C | A change |
| A(G, C) | Apply change C to graph G |
| C⁻¹ | Inverse of change C |
| C₁ ≺ C₂ | C₁ is a dependency of C₂ |
| deps(C) | Dependency closure of C |
| H(x) | Cryptographic hash of x |
| S | Merkle state |