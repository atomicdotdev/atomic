# Intent Knowledge Graph

Intents are authored as directive markdown, lifted into a typed canonical node,
and **projected into the vault knowledge graph** so their structure — tasks,
acceptance criteria, scope, constraints, and the links between them — is
queryable alongside changes, files, and sessions.

## From directives to a graph

```
intent.md (directives)
        │  lift  (atomic-canonical)
        ▼
CanonicalNode  (typed: acceptance criteria, tasks, scope, constraints, refs)
        ├── gate    (SHACL-style shapes: conform before attest / done / trust)
        └── project (atomic-repository::vault_extract_kg)
                 ▼
         Knowledge graph nodes + edges  (redb KG tables)
```

The **gate** enforces a common, conformant shape (see
[Conformance](#conformance-the-shacl-style-gate) below); the **projection**
writes the structure into the queryable graph.

The projection runs automatically whenever an intent is written or synced
(`vault_store` → `vault_index_kg`). A body that does not lift (legacy prose, a
missing id, malformed directives) is skipped gracefully — it never fails
indexing.

## What the graph contains

For an intent `intent:<ULID>`, the projection emits a node per child and typed
edges linking them:

```
                    ┌───────────────────────────┐
                    │   intent:<ULID>           │
                    └───────────────────────────┘
        HAS_ACCEPTANCE_CRITERION │   │ HAS_TASK
             ┌───────────────────┘   └───────────────────┐
             ▼                                            ▼
   ┌────────────────────┐                     ┌────────────────────┐
   │ ac:<ulid>-ac-1    │◀────  SATISFIES  ────│ task:<ulid>-1     │
   │ status, verified  │                     │ status            │
   └────────────────────┘                     └─────────┬──────────┘
                                                         │ TOUCHES
                                                         ▼
                                              ┌────────────────────┐
                                              │ file:src/index.ts │  (shared)
                                              └────────────────────┘
```

| Node kind | Example id | Source |
|-----------|-----------|--------|
| `intent` | `intent:<ULID>` | the intent |
| `acceptance_criterion` | `ac:<ulid>-ac-1` | `:::acceptance-criterion` |
| `task` | `task:<ulid>-1` | `:::task` |
| `scope` | `scope:<ulid>-scope-in-1` | `:::scope-in` / `:::scope-out` |
| `constraint` | `constraint:<ulid>-constraint-1` | `:::constraint` |
| `file` | `file:src/index.ts` | `::file-ref` (shared with changes) |

| Edge | From → To | Meaning |
|------|-----------|---------|
| `HAS_ACCEPTANCE_CRITERION` | intent → ac | the intent declares this criterion |
| `HAS_TASK` | intent → task | the intent decomposes into this task |
| `HAS_SCOPE_IN` / `HAS_SCOPE_OUT` | intent → scope | scope boundaries |
| `HAS_CONSTRAINT` | intent → constraint | a rule to respect |
| `SATISFIES` | task → ac | the task fulfills the criterion |
| `TOUCHES` | task → file | the task changes this file |

A task's `TOUCHES` edge points at the **same** `file:<path>` node that changes
reference via `MODIFIES`, so "which intents touch this file" comes for free.

Because a task may satisfy more than one criterion, `criteria=` (and the
`satisfies=` alias) accept a comma-separated list — each entry becomes its own
`SATISFIES` edge.

## Lifecycle

The projection stays correct across edits and deletes. When an intent is
re-indexed or removed, its previously projected child nodes (and their
`SATISFIES`/`TOUCHES` edges) are garbage-collected first, so a task removed from
the body does not orphan its node. Shared `file:` nodes are never deleted.

## Conformance: the SHACL-style gate

Before an intent is trusted (attested, advanced to `done`, or relied on for
queries), it is validated against a **SHACL-style policy engine** — the *gate*.
This is what guarantees every intent shares a common, well-formed shape, so the
projected graph is consistent and the queries above are reliable.

M0 hand-codes the shapes in Rust (`atomic-canonical/src/gate.rs`); a full SHACL
evaluator over Turtle shapes is a later milestone (see
[shacl-oxirs-spike-findings.md](./shacl-oxirs-spike-findings.md)). The semantics
are fixed regardless of implementation.

### Shapes

| Shape | Applies to | Enforces |
|-------|-----------|----------|
| `IntentShape` | the intent | closed `status`, author + proof present, a `why` exists, scope-out present when scope-in is |
| `AcceptanceCriterionShape` | each `:::acceptance-criterion` | closed `acStatus`; a `met` criterion must carry `verifiedBy` + `evidence` |
| `TaskShape` | each `:::task` | every `satisfies` target is a criterion the intent actually declares |
| `MemoryShape` | memory nodes | closed `memoryKind` + `status`, author + proof present, non-empty text |

### Governing principles

- **Presence is enforced; content is left honest.** The gate requires that a
  reason (`why`) *exists*, that authorship (`attributedTo`, a DID) is present,
  and that a Data Integrity `proof` is present — it never grades the prose.
- **Closed world.** Status and kind values come from fixed sets
  (`backlog`/`todo`/`in_progress`/`done`/`icebox`; `unmet`/`met`; memory
  kinds/statuses). Anything outside the set is rejected.
- **`status: done` is granted, not written.** Advancing an intent to `done`
  must pass the gate first.
- **A checked box needs proof.** `acStatus = met` without `verifiedBy` +
  `evidence` fails.
- **Referential integrity.** A task's `satisfies`/`criteria` must point at a
  criterion the intent declares; a dangling or mistyped reference is a
  violation, not a silent broken edge.
- **Never auto-fixes.** The gate only *reports* — it returns a
  `ValidationReport` of `Violation`s (each with the focus node, the shape, the
  property path, and a message), leaving the load-bearing facts to the author.

### Running it

```bash
# Validate a stored intent (by reference) or a markdown file directly.
atomic intent validate 3
atomic intent validate PIMO::lee-faus::3 --json
atomic intent validate path/to/intent.md
```

`atomic intent attest` runs the same gate before signing, so an intent cannot
be attested (or its criteria marked `met`) unless it conforms. A conforming
report reads `conforms: yes`; otherwise each violation is listed:

```text
conforms: no (1 violation(s))
  ✗ [TaskShape] urn:atomic:task:01j8ze…-1 (satisfies): task satisfies
    'urn:atomic:ac:01j8ze…-ac-9' which is not an acceptance criterion on this intent
```

## Querying it

All of this is reachable through `atomic vault query`. Get an intent's ULID from
`atomic intent show` or a search, then traverse.

### Search

```bash
atomic vault query search "authentication"
atomic vault query search "readline" -k 20 --json
```

### Neighbors (traverse the structure)

```bash
# The intent and everything it declares (tasks, criteria, scope, constraints).
atomic vault query neighbors intent:01J8ZE7G2WABCDEFGHJKMNPQRS -d 1

# Depth 2 reaches the files tasks touch and the criteria they satisfy.
atomic vault query neighbors intent:01J8ZE7G2WABCDEFGHJKMNPQRS -d 2 --json

# Which intents/tasks touch a given file (reverse traversal via the shared node).
atomic vault query neighbors file:src/index.ts -d 1
```

### Natural-language (RAG)

```bash
# Uses the graph as context; requires ANTHROPIC_API_KEY or OPENAI_API_KEY.
atomic vault query ask "which tasks still need to satisfy the strict-build criterion?"
atomic vault query ask "what files does intent PIMO::lee-faus::3 touch?"
```

### Structured query plans

```bash
echo '{"steps":[
  {"type":"kg_search","query":"interactive cli","bind":"hits"},
  {"type":"kg_neighbors","node_id":"$hits","depth":2,"bind":"ctx"}
]}' | atomic vault query plan --json
```

### Reindex

The KG is maintained on every vault write. To rebuild from scratch:

```bash
atomic vault query reindex
```

## Relevant code

- `atomic-canonical/src/{directive,lift,node}.rs` — parse, lift, and the typed
  canonical node.
- `atomic-canonical/src/gate.rs` — the SHACL-style gate: `validate_intent`,
  `validate_memory`, `ValidationReport`, `Violation`, and the shape rules.
- `atomic-canonical/src/vocab.rs` — the closed value sets (`INTENT_STATUS`,
  `AC_STATUS`, `MEMORY_KIND`, `MEMORY_STATUS`, `DEPENDENCY_EDGES`).
- `atomic-cli/src/commands/intent/{validate,attest}.rs` — CLI entry points
  that run the gate.
- `atomic-repository/src/repository/vault_triples.rs` — `project_intent_semantics`
  (nodes + edges), `delete_intent_child_nodes` (lifecycle GC), `vault_extract_kg`.
- `atomic-core/src/pristine/ontology.rs` — `HAS_TASK`, `HAS_ACCEPTANCE_CRITERION`,
  `HAS_SCOPE_IN`/`OUT`, `HAS_CONSTRAINT`, `SATISFIES`, `TOUCHES`, and the
  `TASK`/`ACCEPTANCE_CRITERION`/`SCOPE_ITEM`/`CONSTRAINT` entity types.
- `atomic-cli/src/commands/query/mod.rs` — the `atomic vault query` subcommands.

See also [Intent Identity](./intent-identity.md) for how `intent:<ULID>` node
ids and the `PROJECT::author::seq` references are formed.
