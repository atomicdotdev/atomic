---
created_by: continuouslee <lee@atomic.dev>
goals: []
id: ATOM-1
priority: medium
status: done
title: Understand B-tree graph building
view: tight-storm-0847
entry_type: intent
content_hash: 7TC3V3WDMX6ZFKXHX7PLECD223TEXS2TAW4KXTNJPAUQXM3GKFBA
created_at: 2026-06-22T13:15:22.910549+00:00
updated_at: 2026-06-22T13:15:22.910549+00:00
---
# Understand B-tree graph building

**ID:** ATOM-1 · **Priority:** medium · **Status:** backlog
**Created by:** continuouslee <lee@atomic.dev> · **Created:** 2026-06-22T12:53:50.551571+00:00

---

## Problem

Explain how Atomic's persistent B-tree storage records are assembled into the
runtime graph model developers see when reading files, diffing, or traversing
history. The answer needs to connect the storage tables, graph node/edge keys,
inode indexing, view filters, and traversal rules clearly enough to be useful
for future implementation work.

## Acceptance Criteria

- [x] Identify the B-tree tables involved in graph construction.
- [x] Explain how vertices and edges are encoded and resolved during traversal.
- [x] Describe how views filter the ambient graph.
- [x] Call out the key ambiguity around positions versus vertices.

## Scope

**In:**
- `atomic-core` pristine storage and graph traversal concepts.
- `atomic-repository` read/diff-facing interpretation where relevant.

**Out:**
- Code changes or behavior modifications.
- A full CRDT semantic-layer implementation walkthrough.

## Constraints

- Use Atomic CLI and vault context only for repository operations.
- Keep the explanation grounded in the project documentation and KG-discovered
  code structure.

## Dependencies

None

## TODOs

- [x] `ATOM-1/1` Inspect graph storage and traversal symbols
  **Files:** `atomic-core/src/pristine/**`, `atomic-core/src/types/**`
  **Criteria:** Relevant graph entities and tables are identified.

- [x] `ATOM-1/2` Provide concise conceptual explanation
  **Files:** No code changes.
  **Criteria:** User receives a clear answer connecting B-tree layout to graph
  construction and traversal.
  **Depends:** `ATOM-1/1`
