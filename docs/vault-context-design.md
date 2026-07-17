# Vault Context: Memory Research Before Work

**Status:** PR #108 implements the read-only retrieval primitive. Derived-index
lifecycle, agent skills, Intent lineage, and automatic integrations are separate
steps.

## Target knowledge flywheel

The workflow agreed with Lee is Intent-centered:

```text
memory-based research
  → select relevant source memories
  → create an Intent with problem statement, acceptance criteria, and todos
  → human acceptance
  → implement changes mapped to the Intent
  → update todos, complete, and sync the Intent
  → write a learning Memory linked to the completed Intent and source memories
```

Those links should be RDF relationships so Atomic can build an ontology around
the taxonomy. Retrieval is not proof of use: a memory returned by a search is a
candidate, and only memories actually selected for the Intent should later be
linked as sources.

## What already exists

- Vault is the source of truth for full memory bodies.
- KG FTS searches node IDs, labels, and summaries.
- KG relationships connect Vault entries, files, changes, and other resources.
- Vault vector search exists as a separate path; normal KG search does not use
  those embeddings.

Atomic was missing a task-oriented way to research current Vault memories
before creating or implementing an Intent.

## What PR #108 adds

```text
atomic vault context [QUERY]... [--intent <id>] [--files <path>]
                     [--limit 5] [--budget-chars 8000] [--format md|json]
```

The command is read-only and supports two moments in the workflow:

1. Before Intent creation, use a free-text problem query to discover candidate
   source memories.
2. After human acceptance, use `--intent <id>` to retrieve implementation
   context from that Intent's title, labels, body, and KG relationships.

Candidates come from:

- memory-only KG metadata search, filtered by kind before top-N selection;
- one/two-hop KG neighbors of the supplied Intent;
- one-hop KG neighbors of supplied repo-relative files;
- exact-token matching against current Vault memory bodies.

Ranking combines metadata rank, body-token match, graph adjacency, and recency.
Index, superseded, retracted, and malformed-status memories are excluded.
Explicit seeds with no match return no context; only an unseeded request falls
back to recent active memories.

Markdown is prompt-ready but remains untrusted historical data. JSON returns
the same Markdown once plus the selected candidates and their identities:

- `memory_id`: canonical memory/RDF identity when present;
- `kg_node_id`: identity used by the current KG;
- `revision_hash`: entry type + stored frontmatter + body, identifying the exact
  knowledge revision returned;
- `content_hash`: existing body-only hash used by body/embedding caches.

This identity split lets a later Intent workflow create RDF links deliberately
without assuming a canonical ID is already the node used by today's KG.

## Example

```json
$ atomic vault context "JWT signing algorithm" --json

{
  "schema_version": 1,
  "memories": [
    {
      "memory_id": "urn:atomic:memory:auth-decision",
      "kg_node_id": "memory:auth-decision",
      "body": "Use RS256 signing, not HS256."
    }
  ]
}
```

An agent or human may select that result while writing the Intent. A later
workflow can then add an RDF source link from the Intent to
`urn:atomic:memory:auth-decision`. Merely appearing in these results must not
create the link.

## Follow-ups (not in this PR)

- PR #111 keeps KG FTS, KG nodes/edges, and embeddings consistent with current
  Vault entries.
- Add a focused skill tied to the existing `atomic-vault` skill, and describe
  the research → Intent workflow in agent context files. This belongs with the
  agent package rather than the CLI retrieval primitive.
- Add Intent commands/schema for explicitly selecting source memory IDs and
  writing RDF lineage after human acceptance.
- Integrate the workflow with Noname/Sherpa only after the direct-agent flow is
  useful and the trust/provenance contract is clear.
- After Intent completion, distill a new learning Memory and link it to both the
  completed Intent/change and the original selected source memories.

## Verification

Tests cover free-text, Intent, and file-seeded retrieval; memory-only top-N
selection; exact body tokens; active-memory fallback; legacy Intent paths;
identity/revision fields; output budgets; and explicit no-match behavior.

## Known limits

- Body retrieval linearly scans the memory corpus. Add a specialized body index
  only after corpus-size and latency measurements justify it.
- `--intent` currently expects the full ID; `--files` expects repo-relative
  forward-slash paths.
- Prompt delimiters are advisory. Integrations must keep memory text in an
  untrusted-data boundary rather than merge it into system instructions.
