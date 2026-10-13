# Lossless element replication — self-contained engineer work packet

**Status:** discovery baseline completed; implementation gaps and bounded coverage
work remain. This is the single work document for replication, geo delivery,
server queries, local/remote comparison and lifecycle precision. Its evidence and
requirements are embedded below; no companion discovery document is required.

## 1. Requirement and boundaries

**Export the complete delta of persisted `atomic.redb` state as file-scoped Atomic
element revisions. Push those files across geos. The server stores/queries the
files without needing redb or a serialized database. A fresh local rebuild and
remote queries must preserve the state with complete precision.**

Vault, provenance, tags and changes are examples, not an exhaustive whitelist.
Account for every persisted datum, including metadata, relationships, pending
work and operational records. A reconstructible index may be rebuilt only when
its complete source and rebuild semantics are preserved. No implicit exclusions.
A deliberately redacted/subset export must be called partial, not lossless.

Constraints already decided:

- One ambient graph per repository. Views are effective change-set filters with
  dependency closure and live inheritance, not Git snapshots or private graph copies.
- Local storage remains one `atomic.redb`; keep redb until v5 is evaluated separately.
- Local applications use embedded libatomic/nodeatomic. No mandatory atomicd,
  CLI subprocess, socket probing or automatic service startup.
- Atomic, nodeatomic and consumers remain separate projects. Atomic builds the
  native addon; nodeatomic installs an explicit binary and never invokes Cargo.
- Canonical protobufs stay in libatomic; generate messages/types from its descriptor.
- Remote calls use the existing hosted deployment, not a service per project or
  another provenance/eventing engine. The hosted repository representation must
  not require a redb file or a checked-out working tree.
- Cleanup means **edge cleanup and correct metadata/lifecycle handling**. There is
  no snapshot garbage-collection workstream. Do not delete historical change or
  evidence files merely because a view was removed.
- Preserve opaque bytes, field names, numeric precision, timestamp strings,
  source formatting, signed bytes, reference identity and required revision history.
- Normalizing source-local identities must retain enough origin/mapping information
  for lossless reconstruction. Don't drop persistent fields as presumed local-only.
- Physical database page allocation and live OS locks are not a portable repository
  format. Persistent coordination/runtime records inside the DB still need mapping.
  Private identity-store files outside the DB aren't automatically part of export.

Cost is one query over this state, not the replication model or primary deliverable.
A general C ABI, database migration, Java/Go SDKs and Marketplace publication are
not prerequisites.

## 2. Plain-language explanation: namespaces and 16-way Merkle buckets

Think of Atomic as a school with a big filing cabinet.

### atomic.redb stays the local filing cabinet

It holds your work: code, vault entries, provenance, views, sessions and other
persisted information. We aren't replacing it. We're adding a way to send its
changes as individual element files, without shipping the whole cabinet.

An element file must contain all the information needed to preserve that element,
not just its body text.

### Domain envelopes are the cabinet's big sections

We call each domain container an **envelope**:

```text
Repository
├── Code envelope
├── Vault envelope
├── Provenance envelope
├── Session envelope
├── View envelope
└── Additional envelopes
```

These are organizational containers, not separate databases or an exhaustive list.
Each envelope has two parts:

```text
Envelope
├── Metadata
│   ├── Identity and type
│   ├── Schema version
│   ├── Origin and causal references
│   └── Other shared metadata
└── Content
    └── Merkle tree of element revisions
```

Shared metadata belongs on the domain envelope. An individual intent's title,
frontmatter and proof belong with that intent's revision, not on the entire vault
envelope. The same envelope pattern can be used for individual elements where
useful, without requiring a redundant physical wrapper around every existing file.

### Each envelope's content tree has 16 drawers

The drawers are labeled:

```text
0 1 2 3 4 5 6 7 8 9 A B C D E F
```

We calculate a fingerprint of an element's permanent identity. Its first character
tells us which drawer to use. If a drawer gets more than 128 entries, we divide it
into another set of 16 drawers using the next fingerprint character.

Editing an element doesn't change its permanent identity, so it generally stays
in the same drawer. The 128-entry limit is the evaluated starting point, not a
permanently fixed setting.

### Each file revision gets its own fingerprint

There are two different questions:

- **Identity fingerprint:** Where does this element belong?
- **Content fingerprint:** Which exact version of its bytes is this?

Changing an intent's title—even if its body stays the same—creates a new content
fingerprint. That also catches updates to signatures or unhashed metadata that
might not change Atomic's existing change hash.

### Merkle labels tell us which drawers changed

Each drawer gets a fingerprint based on its entries. Its parent gets a fingerprint
based on its drawers, continuing up to the repository root.

When one file changes:

```text
New file fingerprint
    → new bucket and parent fingerprints
    → new envelope content-tree fingerprint
    → new envelope revision fingerprint (metadata + content-tree fingerprint)
    → new repository root fingerprint
```

The other drawers keep their fingerprints. The server compares labels:

- Matching labels mean: we already have this entire part.
- Different labels mean: look inside this part.

Instead of sending a list of 100,000 elements again, we send the changed file and
a few changed tree nodes/envelope records.

### Envelope metadata changes are changes too

An envelope's revision fingerprint covers **both its metadata and its content-tree
fingerprint**. A content update changes it, and a metadata-only update also changes
it. If only shared metadata changes, send the new envelope record without resending
unchanged content blobs or tree nodes.

Use the technical name **ReplicationEnvelope** to distinguish this container from
Atomic's existing SessionEnvelope and provenance journal envelopes. This is a new
replication structure, not a rename or change to those existing types.

### Envelope lifetimes: a view references work; it does not own it

Some envelope/item state is ephemeral, while other state must persist. Lifetime
belongs to the item or relationship—not automatically to everything referenced
by its container. An envelope can supply a default lifecycle, but mixed contents
need item-level rules. Persistent does not mean undeletable; it means deletion
requires an explicit policy rather than following a temporary container's expiry.

A view is the concrete example:

```text
Changes envelope
└── Change C2 — stored once

Views envelope
├── dev       → C1
├── draft-a   → C2, parent: dev
└── draft-b   → C2, parent: dev
```

Both drafts reference the same change in the ambient graph. Insert C2 into dev:

```text
dev       → C1, C2
draft-a   → C2, parent: dev
draft-b   → C2, parent: dev
```

No change is copied; the target gains membership and the source is unchanged.
Deleting draft-a removes its view element/filter/membership, **not C2**:

- dev and draft-b still expose C2 through their effective filters.
- C2's stored element and persistent provenance/signed evidence remain independent.
- The views envelope/tree changes; unchanged change blobs need not be resent.
- Edge cleanup must preserve edges needed by surviving effective views/dependencies.

A draft's active lifetime can be temporary while its referenced work persists.
The deletion revision/tombstone may itself need to persist to prevent an offline
geo from resurrecting that draft. Even if no active view references a change,
container deletion is not an instruction to erase its retained history/evidence.

A session may likewise contain ephemeral pending processing state and persistent
completed-turn evidence/checkpoints. These are policy examples, not permission
to classify all session/journal data as disposable. Deleting/closing the session
container must not erase persistent evidence; retain durable references for it.

There are three distinct operations: logical envelope deletion, explicit cleanup
of eligible ephemeral items, and eligible ambient graph edge cleanup. No snapshot
GC or automatic cascading blob/evidence deletion follows from the content tree.
Expiration also needs an explicit authority/clock/causal policy across geos, not
each region silently dropping data based only on its wall clock.

Persisted ephemeral state remains part of lossless replication until a valid
expiration/deletion transition changes it. The exporter cannot omit it merely
because it is temporary.

### Shared S3/MinIO storage: two different meanings of bucket

Use a shared object-store namespace as the storage foundation. An **S3/MinIO
bucket** is the storage container. A **16-way Merkle bucket** is a logical
partition represented by an immutable tree file inside that container. They are
not the same thing, and a new fingerprint does not require creating a new S3 bucket.

Illustrative object keys:

```text
s3://atomic-store/<repository>/
├── blobs/<content-hash>
├── trees/<tree-node-hash>
├── envelopes/<envelope-revision-hash>
└── publications/<publication-hash>
```

Two geos do not overwrite the same hash-addressed revision. Both may start with
one logical envelope at E0:

```text
Geo A edits it → E1
Geo B edits it → E2

Shared store retains:
envelopes/E0
envelopes/E1
envelopes/E2
```

Different bytes produce different revision keys; all revisions can coexist.
Identical bytes produce the same key, making repeated upload idempotent. Editing
a Merkle bucket likewise creates a new tree-node hash, not an in-place overwrite.

Coordination is about **publication**, not writing those immutable files: which
accepted revisions represent the repository's current work? Immutable publication
records can name causal predecessors. If E1 and E2 both descend from E0, preserve
both until Atomic's domain rules determine whether their work composes or conflicts.
Fingerprints don't choose a winner or implement those rules.

A mutable current-head reference is a separate design choice. If used, update it
conditionally to prevent silent overwrite; it is not required by hash-addressed
storage. Shared object storage alone neither merges competing publications nor
provides an atomic transaction across all their keys. Upload required files first,
verify closure, then publish a complete frontier.

One authoritative shared bucket/endpoint is also different from asynchronously
replicated regional buckets. Validate the chosen S3/MinIO deployment's consistency,
durability and visibility guarantees; do not assume the word global settles them.
The existing S3/conditional-write infrastructure is reusable. NATS notifications
remain discovery hints, not a replacement for durable element/publication files.

### How this fits Atomic

**This tree organizes replication. It does not replace the ambient graph.**

- Atomic still has one graph.
- Views still filter that graph.
- A child view still follows its live parent.
- Insert still adds references.
- Edge cleanup still works on graph edges, not on these filing-cabinet labels.

The server can store the replicated files without redb. Across geos, identical
files have identical fingerprints, making retries straightforward.

Two regions can store different immutable revisions of the same logical element
without a storage-key collision. Publication must preserve concurrent revisions
until Atomic determines composition/conflict; a shared store is not an automatic
last-write-wins policy.

The remaining essential job is ensuring every persisted mutation updates an
element file. This cabinet-label system cannot detect information the exporter
forgot to include.

## 3. Engineer instructions: reuse the completed discovery

Do not restart the generic inventory, format survey or push/pull investigation.
Use the source map, probes and acceptance facts below. Read exact implementations
for edits and confirm changes made since the baseline, rather than rediscovering
known behavior. Use Atomic code intelligence (`atomic query code/entities/callers`)
for any genuinely missing source trace; callers returning no results isn't proof
that a function is unused.

ER-01 and ER-02 are **not fresh research projects**:

| Work | Already completed | Remaining |
|---|---|---|
| ER-01 inventory baseline | Domain/table/writer/artifact mapping below, actual transport and receiver observations, concrete missing-data cases | Map only still-unaccounted tables/fields/writers and mark exact coverage; an exhaustive field-level mapping is not yet complete |
| ER-02 format baseline | V3/sidecar/manifest inspection, real serialization and record/load probes, full-file versus change-hash distinction | Implement format/revision fidelity fixes and formats for missing elements; do not reopen whether existing formats exist |
| Acceptance baseline | Real CLI HTTP push/pull, fresh receiver queries, insert/delete/refusal/re-pull, physical row diagnostics | Extend/fix the existing harness, then run an actual file-backed server and multi-geo tests |
| Local Node integration | Native exports in libatomic; generated codecs; six local read methods; independent consumer build and public-key verification | Complete file-backed hosted adapter, remote client and logical comparison |

The implementation/configuration is not frozen. Discovery assertions describe the
measured baseline, not a claim that subsequent code can never change.

## 4. Existing implementation map

Paths below are relative to the Atomic checkout unless a project is named.

| Responsibility | Existing code to use |
|---|---|
| Change fields/serialize/deserialize/hash | `atomic-core/src/change/change.rs`, especially `HashedChange`, `Change::serialize`, `Change::deserialize` |
| Header fields | `atomic-core/src/change/header.rs::ChangeHeader` |
| Inline provenance/tokens/cost | `atomic-core/src/change/provenance/{mod.rs,types.rs,builder.rs}`; public re-exports in `change/mod.rs` |
| V3 sections/hash coverage | `atomic-core/src/change/format_v3/types/section.rs`, `writer/state_machine.rs`, `writer/helpers.rs`, `reader/` |
| Turn envelope/record inputs | `atomic-agent/src/envelope.rs`, `record/mod.rs`, `record/provenance.rs`; `atomic-repository/src/record/options.rs` |
| Redb change import/export | `atomic-repository/src/redb_change_store/{mod.rs,queries.rs,batch.rs}` |
| Vault mutations/query projection | `atomic-repository/src/repository/{vault.rs,vault_intent.rs,vault_goal.rs,vault_triples.rs}` |
| Provenance/session publication | `atomic-repository/src/repository/changes.rs`; `atomic-agent/src/turn/orchestrator/{provenance.rs,turn.rs}` |
| Pending journal | `atomic-repository/src/redb_change_store/provenance.rs` |
| Portable object/wire primitives | `atomic-objects/src/{sync.rs,view_snapshot.rs}` |
| Actual current HTTP client | `atomic-remote/src/http/code_sync.rs` (`sync_push`, `sync_pull`); legacy helpers in `upload.rs`, `download.rs`, `queries.rs` |
| Service push/pull selection/ingestion | `libatomic/src/daemon/services_sync_remote.rs`, including `load_change_data`, `import_sidecars`, push/pull implementations |
| CLI push/pull paths | `atomic-cli/src/commands/{push,pull}/`; push implementation in `push/command.rs` |
| Views/filter composition | `atomic-repository/src/repository/{views.rs,filter.rs,content.rs,status.rs}` |
| Delete primitive | `atomic-core/src/pristine/txn/write/mod.rs::del_view` |
| Local dispatch/Node exports | `libatomic/src/{local.rs,node.rs,lib.rs}`, `libatomic/build.rs` |
| Vault/attestation handlers | `libatomic/src/daemon/services_agent.rs` |
| Local native consumer | `~/Projects/nodeatomic/ts/{native.ts,repository.ts,index.ts}`, `scripts/{install-addon.mjs,generate-proto.mjs}` |
| Hosted baseline | `~/Projects/atomic-storage/crates/atomic-storage-routes/src/projects/{protocol/,provenance.rs}` and `crates/atomic-storage-storage/src/repository.rs` |
| Executable diagnostic acceptance | `libatomic/examples/replication_acceptance/{main.rs,fixture.rs,peer.rs,observe.rs,checks.rs}` |

### Known persisted-state mapping

| State/table families | Current representation | Gap or reuse instruction |
|---|---|---|
| GRAPH/INODE_GRAPH, TREE/INODES/reverse mappings, CRDT | Recorded GraphOps/FileOps/content plus local indexes/identity allocation | Keep existing change machinery; cover full file graph/semantic state and portable identity mapping, not only working-file bytes |
| CHANGE_META/GRAPH/SEMANTIC/CHUNKS/UNHASHED/SIGNATURES | V3 sections stored/exported by redb change store | Fix missing opaque metadata and full-file revision handling; use selective section reads where possible |
| VIEWS, VIEW_CHANGES, REV_VIEW_CHANGES, STATES, MERKLE_CHAIN, tags/conflicts | View objects/ref updates plus local metadata | Preserve live parent relationships; fix deletion metadata and add published deletions |
| VAULT_ENTRIES/VAULT_MANIFEST | Direct entry/frontmatter/body/manifest updates; recorded `.vault` files in some caller flows | DB writes don't automatically create changes; complete entry revision must include frontmatter and other persisted fields |
| KG nodes/edges/FTS/reverse indexes/embeddings | Vault/VCS projections plus independently authored or non-reproducible facts | Authored DB-only facts need files; don't call everything derived. Map index inputs, versions and remaining fields explicitly |
| Provenance/evidence | `.provenance` and `.attest` objects, explained-change and predecessor links | Existing formats available; repair discovery of independent/delayed/no-code revisions |
| SESSIONS/SESSION_TURNS/SESSION_PROVENANCE/SESSION_MANIFESTS/SESSION_HEADS/events/todos/phases/intents/logs | Durable indexes/head state plus immutable graphs and some mutable runtime state | Some ledger state reconstructs from graphs; pending/log/runtime records aren't covered merely by transferred graphs |
| PROVENANCE_TURNS/JOURNAL_EVENTS/event indexes/final hashes | Durable pending journal and fenced checkpoint state | Export independently of new VCS changes; retain origin-qualified local identity/coordination information |
| ID/dependency/working-copy/cache/operational tables not fully enumerated above | Existing persisted state | Bounded remaining coverage, not approved omissions; map every field to an element or lossless reconstructible representation |

This baseline is substantial but is **not** a completed every-row/every-field audit.
Close the residual coverage cells; do not repeat the verified discoveries below.

## 5. Completed format and writer findings

### Change coverage

`HashedChange` has header, direct dependencies, extra-known context, opaque metadata,
inline provenance, GraphOps, FileOps and contents_hash. Change also has content bytes,
optional signature and optional unhashed JSON. The generic V3 writer emits header,
dependencies, provenance, graph/semantic sections, content, signature and unhashed
payload; provenance contributes to the change hash, signature/unhashed do not.

**Confirmed defect:** `hashed.metadata` isn't emitted by this writer; the decoder
sets it to an empty vector. The redb save path serializes to V3 first and inherits
the loss; StoredChangeMeta has no opaque-metadata field. Comments saying the complete
SessionEnvelope is hash-bound don't establish actual persistence. Some session/turn
facts do survive in Provenance fields/key-value metadata.

The disposable probe `/tmp/atomic-inflight-research/` exercised both a handcrafted
serialization and a real temporary Repository record/load. It produced:

```json
{
  "metadata_changes_identity": false,
  "metadata_round_trip_len": 0,
  "provenance_changes_identity": true,
  "recorded_change_cost_micro_usd": 12345,
  "recorded_change_metadata_len": 0,
  "serialized_cost_micro_usd": 12345,
  "unhashed_changes_identity": false,
  "vault_writes_advanced_change_history": false,
  "frontmatter_only_changes_vault_content_hash": false,
  "latest_vault_frontmatter": "{\"title\":\"second\"}"
}
```

Extra-known context is an explicit remaining fidelity check: source inspection
found the decoder initializes it empty, but no dedicated runtime round-trip gate
was completed for that field. Don't mark it preserved without a test.

### Vault mutation identity

`vault_store` writes the entry, updates VAULT_MANIFEST and indexes it, without
recording a Change. `vault_record_working_copy`/`vault_record_files` are inflate /
deflate helpers; don't infer that their names guarantee a VCS publication.
The probe changed frontmatter with the same body: latest persisted data changed,
but history didn't advance and the returned body hash stayed unchanged.
`VaultEntry.content_hash` identifies body bytes, not the complete vault revision.

### Artifact ordering and mutable attachments

Keep existing formats where complete. A sealed provenance graph references its
explained change hash. Embedding that graph in the same change's hashed bytes can
create `change hash -> graph bytes -> change hash`; keep dependencies acyclic.

A logical change hash and a hash of the **complete replicated file revision** are
different identities. A new signature/unhashed payload may produce different file
bytes under the same change identity. Presence negotiation must not suppress that
revision. Define update/history/deletion semantics rather than overwriting evidence
or assuming first received bytes always represent current persisted state.

## 6. Completed transport/publication findings

Current sync already sends multiple families: Change, Tag, Provenance, Attest, View.
`sync_push` POSTs a compressed SyncPack; `sync_pull` GETs with a compressed SyncWants
body at `/code`, using the shared protocol header/media type. Pack contains objects
and ref moves; wants contains refs/haves and refs_only. Do not create a duplicate
sync surface just to send additional element files.

View objects hold scope, parent **name**, own changes/set ID/Merkle and predecessor
links. Inheritance resolves the parent's current head, not a frozen Git branch.
RefRecord has name, expect_old and a non-null new_target string: no deletion operation.

Push sidecars are largely discovered via hashes selected for a view's change upload.
A no-op view can therefore suppress independently changing provenance. No-code turns
are explicitly checkpointed with an empty explained-change list; they cannot be
found by following a newly uploaded change that doesn't exist.

`save_provenance_graph` registers explained-change/predecessor dependencies, populates
session tables, indexes a turn and publishes a session manifest. Transferring a graph
can reconstruct a session ledger; that does not prove all independent session/head /
log/pending fields are represented. These require complete element coverage.

## 7. Executed acceptance evidence — do not rerun discovery to learn this

Baseline: **11 PASS, 12 FAIL, one edge-cleanup classification pending, one production
server check BLOCKED.** The old raw report called aggregate retention an observation;
the harness now uses `NOT_YET_CLASSIFIED`, not snapshot GC/reclamation terminology.

What was executed:

- Freshly built stock CLI, real HTTP push/pull to an isolated loopback protocol fixture.
- Fresh repositories reconstructed using stock pull; no source redb copy.
- Actual Repository and libatomic vault/verification queries, insert/delete/re-pull.
- Recorded code, intent, memory, goal/link, signed JSON-LD, provenance, attestation,
  tag, session, DB-only vault/KG and a pending journal turn/envelope.
- Delayed provenance, no-code work, an unpromoted orphan and synthetic shelved artifacts.

First push: **2 changes, 2 view objects, 1 provenance, 1 attest, 1 tag**.
Inline provenance, five FileOps, initial unhashed display payload, related evidence,
plan backlink, tag and one-turn session ledger survived transfer.

| Confirmed finding | Consequence / starting point for implementation |
|---|---|
| Opaque metadata was already empty on source and receiver | Fix producer format/persistence; HTTP alone cannot recover it |
| DB-only vault entry and authored KG node absent on receiver | Need complete element representation plus mutation/delta discovery |
| Source had one pending turn/event; receiver had no pending journal data | Pending state isn't covered by current families/discovery |
| No-op push omitted late linked provenance and no-code graph | Discover independently changing elements, not only new change bodies |
| Promotion later transferred linked graph, not no-code graph | Source ledger had three turns; receiver two; incidental rediscovery isn't complete replication |
| Inactive-view pull transferred artifacts but path queries failed via current tree index | Headless/file-backed queries cannot depend on checkout/tree projection |
| Ordinary checkout refused a dirty receiver; forced checkout enabled file queries but not vault/KG queries | Do not treat materialized files as initialized structured server state |
| Later shared-parent ingestion populated intent/memory/goal/signed rows and derived KG search | Existing rebuild behavior can be reused, but must become complete and headless |
| Signed bytes/proof remained valid, but source freshness became false | Reconstructed intent lost `created_at` and gained one newline; preserve exact source state |
| DB-only source facts remained missing after rebuild | Deriving indexes from received files cannot recover unpublished facts |
| Insert increased view-change rows 2 -> 3, graph edges stayed 228 -> 228 | Membership operation, not graph copy; repeated insert/pull didn't duplicate rows |
| Draft delete removed VIEWS/forward logs/Merkle/state/tag listing/shelved directory | Useful baseline; not complete lifecycle or edge cleanup |
| Each deleted draft retained one REV_VIEW_CHANGES row with zero forward rows | Delete primitive clears forward rows before using them to remove reverse rows |
| Numeric IDs were reused; deleted/recreated view re-pull failed `Change already applied` | Test/fix stale reverse membership and ID reuse before expanding lifecycle |
| Parent deletion refused for children still deleted shelved artifacts | Wrapper removes directory before primitive guards: data-loss risk |
| Orphan vault row remained globally queryable after graph visibility disappeared | Current vault lookup is not view filtered; lifecycle projection needs work |
| Local delete/push left remote refs advertised; cold receiver fetched them | Need explicit replicated deletion/tombstone; omission isn't deletion |
| Aggregate edges stayed 248 after deletion, change/evidence files retained | Not by itself an edge-cleanup failure; classify exclusive versus required edges |

### Test boundary and retained evidence

Loopback fixture: trusted sender, CAS checks, first-write object policy, generous
pull inventory, no production authorization, geo storage or edge-cleanup algorithm.
The mutable-unhashed replacement result depends on that declared policy; production
duplicate-key behavior was not tested. Server queries were actual backend queries
on pull-reconstructed repositories, **not** the production HTTP query service.

Raw baseline report and retained fixtures:

```text
/var/folders/hl/fwbg_jc93tvgz43rj7t25x240000gn/T/atomic-replication-acceptance-lxREbW/report.json
```

The temporary location is optional historical evidence, not a required companion
work document. Its source/receiver/cold repositories and packet/object files can
be regenerated with the checked-in harness. Isolated fixture signing credentials
are outside every replicated repository; don't publish that directory as evidence.

Re-run for **regression validation after edits**, not repeated broad discovery:

```sh
cargo build --locked -p atomic-cli
cargo run --locked -p libatomic --example replication_acceptance -- \
  --atomic-bin "$PWD/target/debug/atomic"
```

It retains `report.json`, packets and repositories; exit **2** means unmet acceptance.
`--out /tmp/new-directory` requires a new directory. Loopback only; no production
remote or real credential. Existing 44 libatomic tests and harness Clippy passed
at this baseline. That doesn't turn failing replication acceptance green.

### Production server blocker already verified

`cargo metadata --locked --offline` in atomic-storage failed because its local
atomic-identity requirement was `^0.16.1`, but this checkout provides `0.19.1`.
Docker's daemon wasn't running; current storage tests need disposable Postgres.
Do not repeat that investigation: align a deliberate server/client pair and provide
a safe test environment. No production server or multi-geo acceptance was executed.

## 8. Existing query/client behavior and server findings

Local Node exports live in libatomic's optional `node` feature. nodeatomic has no
Rust source/Cargo manifest. `install:addon` consumes a prebuilt binary; its build
only generates descriptor-based codecs/types and TypeScript. Its build was tested
with no Cargo on PATH and no sibling Atomic source checkout.

The local read API is already implemented:

```ts
const repo = await openRepository('/path/to/repository');
await repo.vault.get({ path: 'memory/example.md' });
await repo.vault.listIntents();
await repo.vault.getIntent(intentId);
await repo.vault.context({ query: ['topic'] });
const signed = await repo.vault.getSignedIntent(intentId);
```

Local dispatch supports ResolveRepository, Status, GetVaultEntry, ListVaultEntries,
GetVaultContext and VerifyAttestation. Mutations/other methods aren't exposed here.
Current vault operations are repository-wide; explicit ViewRef is rejected and
responses have no ViewSnapshot. Unsupported continuation cursors are rejected.

Verification was repaired to use a read-only repository handle and public identity
material only, with optional explicit 32-byte public key. It doesn't load secret keys
or initialize an identity store. Result separates `signatureValid`, `sourceFresh`,
exact stored `signedDocument`, signer DID and signed-revision hash. Unknown keys
leave signature validity absent; stale source can retain a valid historical signature.
Malformed or wrong-intent signed documents fail closed. SourceFresh concerns stored
vault source, **not** unsaved/unsynced editor text; signature validity isn't signer trust.

Remaining known query limits:

- Current path/vault queries can require tree/materialization/index initialization.
- View fields in protobuf don't implement vault revision filtering by themselves.
- Intent-filtered ListAttestations isn't implemented as its shape suggests;
  unsupported attestation-ID verification fails explicitly.
- Capability discovery isn't yet exposed by local dispatch; full descriptor isn't
  an implemented-method list. Build/schema identity matters: an older addon carried
  99 methods while newer source/probe had 101; counts have continued evolving.
- Common `domain_status` encodes then discards ErrorInfo bytes and uses a text prefix;
  compaction attaches structured details. Normalize errors, don't parse prose.
- Existing readonly opens can perform upgrade/repair; strict read purity needs an
  explicit open/maintenance policy.
- Some handlers unwrap optional fields; validate requests before storage/panic.
- Workspace release uses `panic="abort"`; embedding release hardening is separate
  from working debug tests. Never promise catch_unwind contains aborting panics.

Hosted source already has `/changes/{hash}/attribution` and provenance-by-hash /
by-change endpoints under `projects/provenance.rs`. Attribution reads only the
first inline provenance entry, returns no cost for non-positive USD and loads a
whole change. This is a source finding, not a live deployment result. Preserve
all entries/precision; use cheap element/section projections. Unknown cost isn't
known zero; don't double-count shared invocation/turn totals, revisions, views or
live observations plus sealed summaries. Signatures authenticate claims, not invoices.

The hosted baseline caches Pristine and uses Repository for ingest/queries. The
required target is **file-backed**: it must not require that redb/checkout path.
Use a shared domain read/projection boundary with local-redb and hosted-element
adapters, not two unrelated business implementations.

Remote addressing/auth constraints to preserve: authority + authorized repository /
workspace/view IDs, never client filesystem paths. Local IDs currently derive from
canonical paths and can't prove remote replica identity. Protocol/capabilities,
caller class, permissions and effects are distinct checks; trusted embedded local
context isn't remote authorization. Recorded comparisons must not invent remote
working-copy Status/cwd. Working-copy capabilities remain local unless registered.

Catalyst's badge/view is a downstream consumer. Its existing intent still says
existing gRPC endpoint; transport criteria need an explicit update. Native code
belongs in the extension host, not webview; signed view must freeze stored signed
bytes, disable scripts/edits, and discard stale document-version responses. Its
Bun CommonJS bundling and VSIX `--no-dependencies`/node_modules exclusion need deliberate
runtime packaging. Target the actual extension host, including SSH/WSL; browser-only
hosts report unavailable. UI/VSIX native integration hasn't been claimed complete.

## 9. Required implementation contract — decided direction, not existing features

An element is a domain-owned logical **file** with stable identity and versioned,
complete bytes; not necessarily a source-code file and never a redb table dump.
Repository/session/view/cross-file data needs explicit element files/references too.

**ReplicationEnvelope model:** domain namespaces are represented by envelope records
with metadata and a content-tree reference. Metadata includes the envelope's stable
identity/type, schema, origin and causal/reference information, plus metadata genuinely
shared by its contents. Preserve element-specific metadata with its owning element.
The exact schema remains implementation work; the metadata/content split and joint
revision coverage are required.

An immutable envelope revision commits to its canonical metadata bytes **and** the
content-tree root. Delta tracking must detect envelope-only metadata updates. They
must not be hidden in an unhashed side channel or skipped when the content root is
unchanged. Merkle parent references must include the envelope revision identity,
not just its content-tree identity.

Envelope metadata must distinguish content ownership from references and declare
default lifecycle/deletion rules, with item-level overrides where contents differ.
Deleting an envelope cannot cascade through shared references. Replicated deletion /
expiration revisions must retain enough causality to prevent stale resurrection;
their own lifetime may outlast the deleted envelope. Persistent evidence must
remain independently addressable when its temporary container closes. Define
these policies without excluding persisted ephemeral records from export.

The view envelope describes live filter relationships and own membership. It does
not freeze/copy the graph visible through that view or pin the parent's old content
revision. Existing SessionEnvelope/journal types keep their existing meaning; use
ReplicationEnvelope for this new structure. These records supplement complete existing
artifact formats rather than needlessly wrapping/re-encoding their signed bytes.

For every element preserve scope/origin, complete revision identity, schema, all
persisted fields, dependencies, lifecycle/deletion/history, exporter and rebuild
rules. Portable references must retain source mappings where required. Never use
VaultEntry's body hash or a logical change hash as a complete element-file revision.

```text
persisted local transaction
  -> element revision/dirty tracking in that transaction
  -> consistent export frontier of new/updated/deleted files
  -> delta negotiation by complete revision identity
  -> transfer missing files and dependency closure to a permitted geo
  -> durable acceptance and complete causal publication
  -> file-backed queries and precise local reconstruction/comparison
```

Track every writer, not only record/push. Concurrent exporter reads must represent
a state that actually existed; bytes exported from DB state can't rely only on
materialized workspace files. Retrying must neither lose deltas nor repeat effects.

Deletion needs an explicit revision/tombstone or equivalent operation; absence
from a batch doesn't delete. Prior immutable signed/evidence revisions survive
according to their domain contract. A stale push must not erase a deletion silently.

For geo delivery: stable identity/causality across regions, bounded batches, resume,
missing-closure recovery, conflict/merge semantics for concurrent update/delete and
honest received/published/indexed frontiers. Don't apply silent last-write-wins.
Review existing geo publication/ref algorithms; neither a single global head CAS
nor a redb writer should be assumed to solve distributed pushes. Queries can't
claim completeness while required files are missing.

Server reads files/manifests directly; optional indexes are rebuildable, not
substitutes for unpublished facts. Query through effective view filters, without
independent per-view graph/vault copies or checkout-dependent lookup. Metadata
normalization cannot change reconstructed source truth or signed freshness.

Local/remote comparison is read-only and explicitly targeted. Compare full element
revisions, causal links and matched view/frontiers, not local NodeIds/physical rows.
Return equal/left-only/right-only/different/unsupported/incomplete/redacted/unauthorized
honestly. Unknown is not an empty set; comparison doesn't secretly sync or repair.
Keep credentials/TLS, allowlists, bounded streams, public-key verification and trust
separate; privacy omissions must be labeled partial. No second provenance engine.

## 10. Completed evaluation: blob/Merkle-tree delta overlay

**Recommendation:** use immutable exact-byte blobs and a **namespace-partitioned,
16-way hash-radix tree with bounded leaf buckets** as the next implementation
candidate in engine-independent `atomic-objects`. A 128-element leaf bound is a
measured starting point, not a frozen format/performance guarantee. The tree is
an index of replicated element revisions, not Atomic's semantic graph or a view.

### Existing pieces inspected; reuse them

- `atomic-objects::content_key/verify_key` already hash/verify complete bytes with
  Blake3 without redb/atomic-core dependencies. Keep this engine-free boundary.
- Current object families/ref packs exist, but no generic Blob/Tree element family
  or complete-tree-root negotiation is implemented. The current change object key
  is its logical change hash, not the complete file-byte hash implied by generic
  object comments. Introduce complete revision identity without redefining change IDs.
- Existing ViewSnapshot carries own changes and parent **name**, resolved against
  the parent's current head. Keep that live inheritance; don't pin child visibility
  to the replication-tree revision captured when it was created.
- `atomic-storage-geodist` already has opaque S3 storage and NATS pointer discovery
  without the graph engine. S3Client has conditional-create and conditional-match
  methods (`put_create`, `put_file_create_with_view`, `put_if_match`). Its baseline
  discovery lists change/artifact prefixes; a complete element-root protocol isn't
  present. Reuse infrastructure, not its assumption that changes are the whole state.

### Executed prototype and measured results

A disposable Rust prototype used Atomic's actual Blake3 helpers, version-prefixed
canonical JSON tree nodes, exact raw blobs, recursive hash pruning and path-copy
updates. It verified path-copy roots against full rebuilds. Local subtree-count
caches avoided whole-map rescans during updates; production needs equivalent
validated counts/cache/rebuild rules.

Measurements: 8 synthetic namespaces, 10,000/100,000 distinct ~1 KB element files,
one file update. Bytes below include one changed 1,047-byte blob and new tree nodes.
Compressed column uses the existing postcard/zstd codec on a **candidate tuple
batch**, not the production SyncPack schema. No HTTP/S3/NATS traffic benchmark.

| Layout | 10k raw delta | 100k raw delta | 100k compressed candidate | 100k new tree nodes | 100k initial tree objects |
|---|---:|---:|---:|---:|---:|
| One flat manifest | 939,834 B | 9,388,584 B | 3,930,006 B | 1 | 1 |
| One flat directory per namespace | 116,738 B | 1,151,738 B | 488,808 B | 2 | 9 |
| Global 16-way radix, 128-entry leaves | 7,470 B | 6,475 B | 2,793 B | 4 | 4,369 |
| Global 256-way radix, 128-entry leaves | 23,591 B | 34,079 B | 17,583 B | 3 | 51,580 |
| Namespace then 16-way radix, 128-entry leaves | **9,261 B** | **8,780 B** | **3,762 B** | **4** | **2,185** |

The namespace/radix candidate compared 41 node pairs for that 100k update and
skipped equal subtrees; it didn't enumerate 100k revision keys on the wire.
Initial tree bytes were 9,623,730 versus the flat manifest's 9,387,537 (about 2.5%
extra metadata), spread over more objects. Batch node exchange is therefore required;
2,185 individual network requests would be an unacceptable cold-sync assumption.
Global radix saves roughly another 1 KB compressed on this update but loses direct
namespace scoping and doubles initial tree-object count. 256-way oversharded this
case and had substantially higher metadata/object overhead.

A forced 128->129 leaf split produced 18 new tree nodes / 19 objects, 12,920 raw
bytes (5,774 compressed). Removing that added element restored the original root.
Ordinary updates aren't a universal fixed node count; split/collapse boundaries
are tested and remain a batching/latency consideration.

### Fidelity/lifecycle cases that passed

- Imported **11 actual captured artifact files** from the replication acceptance
  run into the overlay, not invented change metadata.
- Modified a real change's unhashed data: logical change hash stayed the same,
  exact-byte blob/revision key changed, and receiver bytes reconstructed exactly.
- Frontmatter-only candidate vault revision changed the root despite unchanged body.
- Pending/no-code candidate element transferred without any new VCS change.
- Equal-subtree pruning selected the same missing closure as full inventory comparison.
- Exact flatten/rebuild equality, retries without extra stored objects, corrupt-blob
  rejection, and root-before-dependency incompleteness detection passed.
- Existing parent-name view composition exposed a new parent's change without
  rewriting child bytes. This was a composition test, not full engine-view validation.

The overlay preserves **supplied element files**. These tests do not implement the
complete redb mapping/exporter, fix already-lost metadata, or prove that an equal
root covers every DB field. Transactional element coverage/dirty tracking is still
ER-01/ER-03 work; a missing element is invisible to any Merkle algorithm.

### Geo publication: candidate simulation, not production proof

An in-memory element revision register with origin/sequence, causal parent revision
IDs and blob-or-tombstone value demonstrated:

- Independent file updates converge regardless of receive order; retries deduplicate.
- Same-file concurrent edits retain two explicit heads rather than dropping one.
- A stale unchanged revision doesn't resurrect a causally deleted element.
- Concurrent delete/update is an explicit conflict, not silent last-write-wins.
- Missing causal predecessors prevent claiming a complete frontier.

This doesn't resolve Atomic domain conflicts or implement a production geo protocol.
Merkle roots identify inventories; **they don't merge concurrent roots**. A single
whole-repository root CAS still makes independent geo writers contend and isn't a
sufficient concurrency design. Publish origin-qualified causal updates/frontiers;
derive the tree over merged known state, retaining unresolved revisions until the
existing domain rules resolve them. Choose and validate that protocol in ER-06.

### Implementation decisions justified by this evaluation

1. Keep exact raw element bytes in immutable blobs; preserve logical change/entity
   identity separately from complete revision identity.
2. Partition the replication namespace by domain/file ownership, then hash stable
   element IDs into small bounded radix buckets. Paths/renames belong in element
   data unless the element's defined identity really is its pathname.
3. Use canonical, versioned typed tree encodings with bounded/validated entries,
   duplicate rejection, depth/count limits and exact hash checks. JSON was a probe
   codec, not a decision to serialize all repository data as JSON.
4. Negotiate tree roots and batched differing-node/missing-blob closure through the
   existing sync surface. Current enum/wire additions require protocol negotiation;
   don't silently send new families to an old sync/1 reader.
5. Publish only verified complete closure, expose regional/frontier incompleteness,
   and retain causal deletion/update facts for geo convergence.
6. Keep tree maintenance local/incremental and derived from a transaction-consistent
   element revision set. Don't rescan/serialize the complete DB on every push.
7. Don't place a root pointer into the very bytes whose hash defines that root.
   Map replication bookkeeping/publication causally without a self-reference cycle
   or an unapproved omission from persisted-state coverage.
8. Leave the ambient graph/view filters/edge cleanup semantics unchanged. This
   index doesn't introduce Git commits, per-view graph copies or snapshot GC.

Prototype source/results: `/tmp/atomic-merkle-overlay-eval/` (optional retained
experiment; all essential evidence is embedded here). It was debug/in-memory work;
no release/network/geo durability or complete-DB fidelity benchmark was performed.
No production implementation was changed by this evaluation.

## 11. Remaining work — use this baseline, implement rather than restart

### [~] ER-01 — Close residual persisted-state coverage

**Done:** inventory/source map, writer/artifact baseline and concrete missing rows.
**Next:** enumerate only unaccounted tables/fields/writers and fill owner/element /
format/reference/rebuild cells. Preserve pending/operational and authored facts;
any exclusions require approval. Do not repeat the generic artifact survey.
**Gate:** every persisted datum has an explicit lossless mapping or documented
unresolved implementation item; no implicit family whitelist.

### [~] ER-02 — Implement known fidelity/revision fixes

**Done:** format/hash/source-path inspection and measured round trips above.
**Next:** implement ReplicationEnvelope metadata/content records and the namespace/radix
candidate in atomic-objects with a versioned codec and production tests. Test envelope
metadata-only revisions with unchanged content roots, ownership/reference boundaries,
and envelope/item lifecycle policies. Fix opaque metadata; add complete vault/file/attachment revision identity;
close remaining fields such as extra-known context; preserve signed and exact
source bytes; retain origin-ID mapping. Version formats and don't rewrite published
identities. Reuse complete existing formats, not one giant cyclic change.
**Gate:** field-level export/import/local rebuild preserves every covered datum.

### [ ] ER-03 — Add transactional delta tracking to every mapped writer

Source-local tracking/outbox/frontier must capture all persisted element mutations,
metadata-only/no-code/pending updates and explicit deletion. Define coherent export
and crash-safe acknowledgement. Start at the known vault/provenance/session writers.
**Gate:** concurrent mutation/export and crash/retry tests lose no deltas or effects.

### [ ] ER-04 — Extend existing sync for complete file revisions/closure

Use SyncPack/SyncWants where compatible; add/version blob/tree roots, batched node
closure and missing element/frontier/deletion semantics. Discover independently changing files, not merely newly pushed change
hashes. Transfer predecessor closure and already-present elements with new revisions.
**Gate:** no-op code push publishes new metadata/evidence; complete frontier can
reconstruct source state from files alone.

### [ ] ER-05 — Implement the file-backed hosted repository/query adapter

Align actual server/client versions and safe test prerequisites. Serve shared RPC
domain semantics over element files, no hosted redb or working checkout. Make
headless, view-aware file/vault/provenance/session/KG reads complete; normalize errors
and advertise only implemented methods. Keep read initialization/repair explicit.
**Gate:** a cold file-only server answers equivalent permitted queries at a declared
complete/indexed frontier, including exact source and signed-revision fidelity.

### [ ] ER-06 — Integrate geo publication and convergence

Use the existing S3/MinIO-style shared object namespace and distributed storage/event
ownership. Distinguish storage buckets from Merkle partitions and immutable revision
uploads from publication coordination. Define origin/causal frontiers,
regional durability/visibility, resumable closure, concurrent independent writes,
same-element conflicts and deletion races. Pending journal batches need stable
identity/gap checks; sealed artifacts reconcile them without duplicate accounting.
**Gate:** multi-geo interrupted/reordered/concurrent pushes preserve all accepted
work and never present incomplete state as complete.

### [ ] ER-07 — Fix lifecycle safety; implement eligible ambient edge cleanup

First fix `views.rs::delete_view` guard ordering and `write/mod.rs::del_view` reverse
membership cleanup. Regression-test ID reuse, refusal safety and re-pull. Publish
explicit remote deletions with permissions/preconditions/stale-client protection.
Test the same change in multiple views: deleting one draft cannot delete shared
work or its persistent evidence. Test mixed ephemeral/persistent envelopes,
replicated expiration, retained deletion causality and stale-geo resurrection.
Then classify eligible edges versus structural/dependency edges needed by surviving
effective views, maintain reverse/index consistency, and test correct reinsertion.
**Gate:** no data loss on refusal, phantom memberships or accidental resurrection;
edge-specific assertions prove cleanup, not aggregate totals or snapshot GC.

### [ ] ER-08 — Prove all-data rebuild, RPC parity and comparison

Extend the existing acceptance fixture to the residual coverage matrix; don't
replace it with another cost-only test. Run a real isolated file-backed server,
then rebuild a fresh local DB from received files. Test both directions and deltas,
all mapped fields, exact signatures/source, metadata-only and pending/no-code updates,
view ancestry/lifecycle, authored relations, retries/closure/conflicts and geos.
Add explicit local/remote nodeatomic backends over canonical messages and logical
comparison. Native build stays producer-owned; remote-only use needn't load native
code unnecessarily. Test tenant/project/view/caller boundaries and no hidden writes.
**Gate:** complete canonical state exports and permitted queries agree at matched
frontiers. No copied DB, mandatory local service, blind normalization or empty-set
fallback. Incomplete/blocked scope cannot produce a green full-lossless verdict.

## 12. Implementation order and genuinely unresolved decisions

Start immediately with known deletion safety and format fidelity failures in
ER-07/ER-02; ER-01 is bounded gap closure. Design ER-03 tracking and ER-04 revision /
publication together, then ER-05 file-backed adapter. ER-06 geo semantics must inform
publication design before release; ER-08 incrementally validates each change.

Still requires a decision, not rediscovery:

- Exact element encoding and full-revision/origin mapping for uncovered data.
- Transactional dirty/outbox/frontier schema and acknowledgements.
- Geo multi-head/CAS/convergence/conflict policy and deletion causality.
- Edge eligibility/reinsertion policy; unchanged totals aren't an algorithm.
- Deployed server revision/transport and safe compatible test environment.
- Sensitive-data policy: full export versus explicitly partial/redacted scope.
- Supported artifact/platform/contract versions and release panic policy.

**Done means:** every persisted source datum is accounted for; a push transfers
its complete changed-element-file delta across geos; a server uses those files,
not redb/database serialization; fresh local reconstruction and permitted local /
remote queries/comparisons preserve complete precision. Existing discovery is the
starting evidence, not a task to spend tokens repeating.
