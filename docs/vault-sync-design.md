# Vault as Objects: Server Sync & WebUI Design

> Status: **draft / proposal.** Companion to
> [`view-snapshot-sync-design.md`](./view-snapshot-sync-design.md); it reuses the
> same bare object+ref transport and demand-scoped read model. This doc covers
> getting **vault** data (sessions, intents, memories, goals, skills) onto the
> server and rendering it to a WebUI.

## Overview

The vault holds the durable knowledge Atomic produces alongside code: session
turns, intents, memories, goals, skills. Today it is **100% local** — a content
search for `vault` across `atomic-remote` and the push command returns no
matches. `atomic vault sync` ingests `.vault/` files into redb; `atomic vault
materialize` writes them back to disk; nothing ever reaches a server. So none of
this shows up on the remote, and there is no surface to render it to a WebUI.

The good news from digging through the subsystem: **the vault is already in the
exact shape the bare object+ref model wants** — content-addressed entries indexed
by a merkled manifest. It doesn't need re-architecting; it needs the *same
transport* views are getting, plus a read API. It becomes a second object family
and one ref riding the identical wire protocol as changes, tags, provenance, and
view-snapshots.

## What the vault already is

- **Content objects** — `VaultEntry` (`atomic-core/src/pristine/vault.rs:75`):
  `{ entry_type, content_hash: [u8;32] (Blake3 of body), content_bytes,
  frontmatter_json, created_at, updated_at, introduced_by }`, stored in
  `VAULT_ENTRIES` keyed by vault path (`sessions/…`, `intents/…`, `memory/…`,
  `skills/…`). A content-addressed object, structurally like a `.change`.
- **A manifest = index + merkle** — `VaultManifest`
  (`atomic-core/src/pristine/vault.rs:250`): a singleton with summary maps for
  `goals`, `memory`, `intents`, `skills`, plus `index` stats, totals, intent-id
  counters, and an incremental **`merkle`** (`Hash(prev_merkle || change_hash)`).
  `Repository::vault_manifest_merkle` (`atomic-repository/src/repository/vault.rs:448`):
  *two repos with the same manifest merkle have identical vault state* — i.e. a
  snapshot with lineage already.
- **Derived read-models** — `sync` also builds a knowledge graph (`KgMutTxnT`),
  embeddings (`EmbeddingsMutTxnT`), and triples (`vault_triples.rs`). The vault's
  "graph," derived from the entries.
- **sync vs materialize:**
  - `atomic vault sync` → `Repository::vault_record_working_copy`
    (`atomic-repository/src/repository/vault.rs:654`): scan `.vault/` → `vault_store`
    each into redb (entries + manifest + KG). Ingest disk → store.
  - `atomic vault materialize` → `Repository::vault_materialize_all`
    (`atomic-repository/src/repository/vault.rs:495`): redb → `.vault/` markdown +
    `_manifest.json`. Store → disk.

## Core principle

**The vault is a second content-addressed object family plus one `vault` ref,
riding the same bare object+ref transport. The server stores entry objects and
the manifest object and CASes the ref — no apply, no server-side KG build. The
WebUI renders on demand: the manifest for the index, entry objects for detail,
the derived KG/embeddings for search.**

## It maps 1:1 onto the object+ref model

| view / change world | vault world |
|---|---|
| `.change` / `.tag` object (content-addressed) | **`VaultEntry` object** (already Blake3-addressed) |
| view-snapshot (membership + merkle) | **`VaultManifest` object** (summaries + merkle) |
| `views/{name}` ref → snapshot | **`vault` ref → manifest** (one per project) |
| redb graph (derived, demand-built) | **KG + embeddings** (derived, demand-built) |
| materialize graph → files | `vault_materialize` redb → `.vault/` files |

A `.vault` push is therefore: **store the new entry objects + the manifest
object, then CAS the `vault` ref** — the same bare store+CAS as a view. Bare
server, trivially convergent, serverless-friendly for exactly the reasons in the
companion doc.

## WebUI: the two-plane read model

- **Index / structure** (list intents with status, memories, sessions, goals,
  counts) → read the `vault` ref → **manifest** → its summary maps. One object
  read, no bodies, no KG. *The manifest is the WebUI index.*
- **Detail** (full intent body, session transcript, memory text) → fetch that one
  **entry object** by key. On demand.
- **Search / relationships** (intents touching X, semantic memory search) → the
  derived **KG / embeddings** read-model, built lazily and query-scoped — the
  same demand-scoped graph-slice pattern as code content reads.

If no WebUI/search endpoint is exercised, the KG/embeddings are never built —
storage holds only the entry objects, the manifest, and the ref.

## Preserving the vault graph

An intent is not a flat blob: it decomposes into **acceptance-criterion and task
nodes** with edges, memories link into it (`wasDerivedFrom`), ACs cite the
changes that satisfy them, and tasks name the files they modify. That graph must
stay intact on the server. It does — because it is **derived, not applied**.

**Derived, not applied.** `Repository::vault_extract_kg`
(`atomic-repository/src/repository/vault_triples.rs:35`) parses a single
`VaultEntry` into `(Vec<KgNode>, Vec<KgEdge>)` via `atomic_canonical::lift`. The
server never "introduces" AC/task nodes on push; it re-derives the whole KG on
demand by running that **same deterministic lift** over the pushed entry objects.
Same objects + same extractor (both live in `atomic-canonical`/`atomic-repository`,
linked by the server) ⇒ identical nodes and edges. The graph stays intact because
it is *reconstructed*, not transported.

**Two edge kinds, handled differently:**
- **Intra-object (ACs, tasks inside an intent):** parsed from the intent body
  (`:::acceptance-criterion{#…}`, `:::task{#… criteria=…}`), so they **travel
  inside the intent object** — they cannot go missing and there are no separate
  AC/task objects to dangle.
- **Inter-object (memory → `wasDerivedFrom` → AC/task/intent; AC → evidence
  change; task → file):** encoded as references in the *referencing* object, so
  they require the target present → the push must transfer the **reference
  closure**, and the rebuilder **defers a dangling reference** until its target
  lands (self-healing, like change reconcile).

**The load-bearing rule — encode edges by portable identity, never a local
`NodeId`.** A pristine `NodeId` (u64) is a **repo-local index** allocated at
`register_change`; the same change gets a *different* NodeId in another repo or on
the server, so a transmitted NodeId identifies nothing there. The **edge is
preserved; its target is referenced portably** and re-resolved on each side:

| target | portable form | resolved locally via |
|---|---|---|
| a change | Hash / `urn:atomic:change:…` | `INTERNAL` (Hash → NodeId) |
| a file | path | `TREE` (path → inode) |
| an AC / task / memory | `urn:atomic:…` | the canonical lift |

The vault format **already does this** where it counts: an AC cites its change as
`:::acceptance-criterion{#ac-1 … evidence=urn:atomic:change:01J8}` and a task
names its file as `::file-ref{path=…}` — both portable. When the server rebuilds
the KG it resolves those URNs/paths to *its own* NodeIds/inodes, exactly as change
apply resolves dependency hashes to local NodeIds. `NodeId` never travels; the
Hash/URN/path travels and is re-bound locally.

**Closure spans both object families.** These edges cross out of the vault into
the code graph (intent → AC → evidence change; task → file). So an intent's push
must pull in the **changes/files its ACs and tasks reference** — uniform with
change dependency closure, just crossing vault↔code on one bare store.

**The one leak to fix:** `VaultEntry.introduced_by: u64`
(`atomic-core/src/pristine/vault.rs:96`) is a raw local NodeId. It is bookkeeping,
not a KG edge (the KG expresses that link as `urn:atomic:change:…`), so the
transport/rebuild must **not** depend on it — re-derive the linkage from the URN.
Auditing every portable field for stray NodeIds is the concrete correctness task.

## Decisions / nuances

- **Scope is per-project, not per-view.** The manifest is a singleton, so it's
  **one `vault` ref per project** (like a special branch), not one per view.
  Per-view intents/memories could come later; today the vault is global to the
  repo.
- **Object addressing.** The entry's `content_hash` covers only the body — for the
  *transported object*, hash the canonical serialization of the whole `VaultEntry`
  (type + body + frontmatter) so frontmatter edits are captured. Derive from redb
  on push (the client-truth-now model, D1(a) in the companion doc).
- **Manifest representation — do NOT push one ever-growing object.** The redb
  `VaultManifest` is a singleton that is rewritten wholesale on every change;
  serialized as *one* pushed object, that reintroduces the quadratic
  (index-size × transitions) problem view D3 solved — a new memory would
  re-serialize summaries of *all* intents/memories/sessions/skills. Fix it the
  same two-axis way:
  - **Shard by kind (hierarchical):** the `vault` ref points at a tiny **root**
    object referencing per-kind sub-manifest objects (`goals`, `memory`,
    `intents`, `skills`). A new intent rewrites only the *intents* sub-manifest +
    the small root; other shards are shared by hash. Bonus: the WebUI lists
    per-kind, so shards are also the natural read unit.
  - **Delta + periodic full (temporal), per shard:** if one kind gets large,
    encode its sub-manifest as `{added/updated/removed}` vs prev with a periodic
    full; reconstruct by a short walk. Mirrors view D3 exactly.
  Entry *objects* never have this problem — they are immutable, deduped leaves;
  only the index needed sharding.
- **Ancestry / CAS.** The manifest's incremental `merkle` already gives O(1)
  "same state?" and a prev-chain fold — the CAS-on-`prev` lineage for the `vault`
  ref is essentially already present; make the predecessor explicit so
  fast-forward vs. divergence is decidable exactly as for view refs.
- **Derived layers never push.** KG, embeddings, and triples are rebuildable
  read-models, built on demand server-side (embeddings the heavier, lazier one).
  Never on the push path.
- **Client truth stays redb.** `VAULT_ENTRIES` + manifest remain the client's
  source of truth; objects are serialized on push. `materialize` stays the local
  disk step, off the push path — same split as views.
- **Divergence is a set-union.** Two divergent vault states reconcile by unioning
  their entry sets and folding a new manifest (the intent-id counters merge via
  the existing per-author `intent_seq`, which is already collision-safe offline) —
  patch-style, not a 3-way merge, matching the companion doc's transport/merge
  split.

## Relationship to the view-snapshot design

This rides entirely on the companion doc's machinery:
- Same **bare object store + refs** and **wire protocol** (advertise ref →
  negotiate missing objects → transfer → CAS ref).
- Same **demand-scoped read-model** discipline (manifest = structure plane,
  entry objects = content plane, KG/embeddings = derived slice).
- Same **deployment story** (S3/R2 objects, a conditional-write/DO ref, serverless
  compute, managed replication — no NATS).

The only vault-specific pieces are: the `VaultEntry`/`VaultManifest` object
encodings, the single per-project `vault` ref, and the WebUI read API.

## Phased plan

1. **Object encodings.** Canonical serialization + Blake3 key for `VaultEntry`
   and `VaultManifest`; derive from redb (no client re-architecture).
2. **`vault` ref + client push/pull.** Advertise `vault` ref → transfer missing
   entry objects + manifest → CAS ref. Reuse the view transport; add a `vault`
   ref alongside `views/{name}`.
3. **Server read API (WebUI).** `vault` ref + manifest → index; entry object
   fetch → detail. Both pure object reads, no graph.
4. **Search endpoints.** Demand-built KG/embeddings slice for semantic memory
   search and relationship queries.
5. **Pull/materialize parity.** `atomic pull` brings vault objects down; existing
   `vault_materialize_all` writes `.vault/` — unchanged, off the sync path.
