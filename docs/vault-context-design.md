# Vault Context: Memory Research Before Work

**Status:** Phase A (`atomic vault context`) is implemented in PR #108. Phase B
(derived-index lifecycle) is implemented in this stacked PR. Agent workflow,
Intent lineage, and automatic integrations remain follow-up work.

## Why

Atomic already stores Vault memories, Intents, changes, and provenance, but
stored knowledge only compounds when it is found before a decision is made.
The target workflow is Intent-centered:

```text
memory-based research
  → select relevant source memories
  → create Intent with problem statement, acceptance criteria, and todos
  → human acceptance
  → implement changes mapped to the Intent
  → update todos and complete/sync the Intent
  → create a learning Memory linked to the Intent and its source memories
```

RDF links provide lineage and build an ontology around that taxonomy. Retrieval
is not the same as use: a memory returned as a candidate must not automatically
be recorded as `prov:used`. The Intent workflow should persist links only for
the source memories actually selected.

PR #108 supplies the read-only retrieval primitive. PR #111 makes the derived
KG/FTS/embedding lifecycle safe enough for that retrieval to represent current
Vault state. Neither PR creates Intents, writes lineage links, injects prompts
automatically, or distills new memories.

## What exists already (verified)

- `vault_kg_search` searches KG node IDs, labels, and summaries together with
  the repository source-content index. `vault_kg_search_by_kind` applies a node
  kind before top-N selection for typed callers such as memory retrieval.
- `vault_kg_neighbors` traverses relationships created from wiki links and file
  references in Vault entries.
- `vault_search` is a separate vector-search path. Normal KG search does not use
  Vault embeddings.
- Vault remains the source of truth for full memory bodies.
- The existing `atomic-vault` skill covers Vault operations, but does not yet
  define this memory-research workflow.

## Phase A: `atomic vault context` (PR #108)

```text
atomic vault context [QUERY]... [--intent <id>] [--files <path>]
                     [--limit 5] [--budget-chars 8000]
                     [--candidates-only] [--format md|json]
```

The command is read-only and supports two workflow moments:

1. Before Intent creation, free-text research finds candidate source memories.
2. After human acceptance, `--intent <id>` retrieves implementation context
   from the accepted problem statement, criteria, todos, and relationships.

Candidates come from memory-only KG metadata search, Intent/file graph
neighbors, and a scan of current Vault memory bodies. Ranking combines
metadata rank, exact body-token match, graph adjacency, and recency. Explicit
seeds with no match return no context; only an unseeded request falls back to
recent memories.

JSON returns the prompt-ready Markdown once and records both identities:

- `memory_id`: canonical memory resource identity when present;
- `kg_node_id`: identity used by the current KG;
- `revision_hash`: entry type + frontmatter + body, identifying the exact
  knowledge revision exposed;
- `content_hash`: body-only hash used by body/embedding caches.

This separation avoids treating a body hash as a full knowledge revision or
creating RDF links to a canonical ID that the current graph does not yet know.

For compact push + agent pull, `--candidates-only` returns only ranked metadata:
memory identity, kind, path, exact revision, score, and a short explanation of
why it matched. It omits memory bodies, previews, and prompt-ready context. An
agent must treat the candidate fields as untrusted historical data, then pull
only a selected, unchanged entry:

```text
atomic vault show memory/auth-decision.md --revision <REVISION> --json
```

`vault show --json` identifies its generic response as `vault_entry_body`,
includes the current `revision_hash`, and labels the returned body as
`untrusted_historical_data`; `--revision` fails closed if the entry changed
between candidate selection and body retrieval. Revision-gated pulls require
JSON so the trust marker cannot be silently dropped.
The existing full `vault context` output remains the default for compatibility.

## Phase B: derived-index lifecycle (PR #111)

- KG FTS remains an index of node IDs, labels, and summaries. Memory bodies are
  not copied into it.
- A transactional reverse FTS index lets node replacement/deletion remove old
  metadata tokens. On the next KG initialization/write, a one-time rebuild uses
  current KG nodes and prunes obsolete or posting-only orphan terms.
- Read-only search treats FTS as a candidate index and validates every hit
  against the current node ID/label/summary. Upgraded repositories therefore do
  not serve an obsolete term while waiting for the next write-time migration.
- Vault node updates replace only edges owned by that Vault entry. Edge metadata
  records `derived_from_vault_path`, so incoming RDF/KG links owned by another
  resource survive a target update, while symmetric relationships owned by the
  updated entry are removed correctly.
- Vault deletion removes the source entry, its current path-derived KG node and
  edges, and embedding chunks in one transaction. Entry-type changes remove the
  previous KG identity, including ToolResult identities.
- Working-copy sync detects frontmatter-only changes such as `status`, labels,
  or description without changing JSON types during materialize/parse. Manifest
  type indexes, Goal/Intent summaries, file count, and byte totals follow
  replacement and deletion instead of leaving old entries behind. Removing or
  reclassifying a Goal also removes that Goal from linked Intent sources.
- KG and embedding writers verify that their source Vault entry is still the
  same revision before committing, preventing slow internal indexers from
  resurrecting older data after an update or deletion.
- `vault context` reads current Vault memory bodies directly, so body updates
  and deletions take effect without a separate body index.

### Deliberate limits

- Write-side automatic indexing remains best-effort; this PR does not add a
  durable indexing outbox/retry system.
- The migration repairs FTS postings, not stale full KG nodes or embeddings left
  by older failed lifecycle operations. A source-aware reconciliation command is
  a separate follow-up.
- Embedding model/provider/chunker provenance and forced re-embedding belong to
  the vector-search lifecycle, not this metadata/source cleanup.
- Schema migration still runs on a KG write/init; read-only retrieval uses
  current-node validation instead of acquiring a write lock.

## Follow-ups

- **Direct-agent workflow:** add an `atomic-vault-context` skill linked from the
  existing `atomic-vault` skill. `AGENTS.md`, `CLAUDE.md`, and equivalents say
  *when* to run memory research; the skill says *how*.
- **Intent source lineage:** after source selection, write typed RDF/PROV links
  from the Intent to the selected source Memories. Candidate retrieval alone
  must not create these links.
- **Sherpa/noname:** run free-text research while authoring an Intent, then use
  `--intent` after human acceptance. Prefer compact candidates first and pull
  full bodies only for explicit selections. Inject selected content at
  untrusted user/tool-data authority and record exact exposed revisions
  separately from the smaller set selected as sources.
- **Completion distillation:** after the Intent is complete and synced, create a
  typed learning Memory and link it to the completed Intent and original source
  Memories with RDF. Session transcripts and `agent explain --save` are evidence
  for this step, not the lifecycle trigger by themselves.

## Verification

- PR #108 covers free-text, Intent, and file seeds; memory-only top-N selection;
  inactive-memory exclusion; exact-token matching; identity/revision fields;
  delimiter integrity; budget allocation; and explicit no-match behavior.
- Compact retrieval tests cover body-free Markdown/JSON candidate contracts,
  match explanations, exact revision output, and revision-mismatch rejection.
- Core KG tests cover reverse-FTS migration, metadata-token replacement, and
  deletion.
- Repository tests cover target updates preserving incoming links, owned reverse
  edge replacement, read-side stale-term rejection, frontmatter-only updates,
  lossless frontmatter round-trips, manifest replacement and Goal/Intent
  deletion, entry-type/ToolResult cleanup, source-aware deletion, embedding
  cleanup, and stale-writer rejection for both automatic and public two-step KG
  indexing.

## Known limits

- Vaults have few memories until completion distillation exists; seed a small
  set of high-value memories for initial use.
- Body retrieval scans the memory corpus. Add a specialized body index only
  after corpus-size and latency measurements justify it.
- `--intent` requires the full ID; `--files` expects repo-relative paths.
- Prompt delimiters are an advisory trust boundary. Integrations must preserve
  memory as untrusted data rather than merging it into system instructions.
