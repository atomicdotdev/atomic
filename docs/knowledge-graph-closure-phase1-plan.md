# Phase 1 Implementation Plan: KG Referential Closure — Quick Win

> Status: **draft plan.** Implements Phase 1 ("Referential closure") of
> [`knowledge-graph-closure-rfc.md`](./knowledge-graph-closure-rfc.md).
> Branch: `feat/semweb-knowledge-graph`.

## Guiding principle for Phase 1

Phase 1 is about **clean up, defining a common language, and using it**. It ships four
things that every downstream phase depends on:

1. **`atomic:` namespace** — a canonical workflow ontology (`.ttl` + Rust registry).
2. **`ProjectId`** — a stable, unique project identity minted at `atomic init` and cast
   into IRIs so that future cross-project SPARQL queries have a canonical namespace per
   repository.
3. **SHACL integration using PurRDF** — adapt workflow data to RDF and evaluate
   builtin and enterprise-authored shapes with an existing engine, provisionally
   `purrdf-shapes = 3.0.1`. No hand-rolled validator or constraint evaluator.
   Enterprise users add custom shapes in `.atomic/shapes/*.ttl`.
4. **KG cutover** — rewrite the KG producers to emit the canonical vocabulary and
   project-qualified IRIs, and move every consumer onto it, so the common language is
   actually in use by the end of Phase 1 (§1.6).

Inference, the unified `reconcile()` pipeline, and mandatory publication gates come
later. By the end of Phase 1 the IRIs are stable, the KG speaks only the canonical
vocabulary, and validation is real SHACL.

## What we deliberately skip in Phase 1

**No code-block-level mapping.** AST and Tree-Sitter already produce call graphs,
definitions, exports, imports, and entity relationships. Building a parallel system
to emit `atomic:calls`, `atomic:defines`, `atomic:exports`, `atomic:includes`,
`atomic:modifiesEntity`, and scope-qualified source-entity IDs duplicates information
we get from parsers for free. Those edges are deferred until we need them for queries
that the syntax index cannot answer (e.g. federated cross-project search).

| Skipped | Why |
|---|---|
| `atomic:defines`, `atomic:exports`, `atomic:calls`, `atomic:modifiesEntity` | Tree-Sitter / `atomic-semantic` resolves these on demand |
| `atomic:includes`, `atomic:imports` | File inclusion graphs are cheap to compute from source |
| `atomic:modulePartOfModule`, `atomic:filePartOfModule` | Module hierarchy derivable from paths |
| Scope-qualified entity IDs | Deferred to a phase that needs stable entity references |
| Parser changes for scope chains / parameter types | Not needed without entity materialization |
| KG-backed code-structure queries | `query callers` and triage blast radius read KG `CALLS`/`DEFINES`/`PART_OF`/`INCLUDES` edges today (`query/mod.rs:1223-1231`, `triage/report.rs:122-201, 683-691`); the Phase 1 cutover stops emitting those edges, so both move to live `atomic-semantic` parsing in Phase 1 (§1.6) |

We keep **file-level** `atomic:modifiesFile` and `atomic:directlyOnView` because those
are workflow facts, not code-structure facts.

## Current code (starting point)

Verified against `dev` at `5ddcf1b`. Line numbers drift; search for the named symbol if a
reference is off.

### C.1 Storage

- The KG is a **JSON property graph**, not a triple store:
  `KgNode { id, kind, label, summary, source, metadata }` and
  `KgEdge { from_id, to_id, kind, metadata }` (`atomic-core/src/pristine/vault.rs:462,516`).
- Tables (`atomic-core/src/pristine/tables.rs:797-830`): `kg_nodes`, `kg_edges`
  (key `from\0to\0kind`, `encode_edge_key` L856), `kg_edges_from`, `kg_edges_to`,
  `kg_fts`, `kg_fts_by_node`, `kg_index_meta`. No predicate or type index.
- The predicate string is **part of the edge key**, so renaming means rewriting keys.
- `upsert_kg_edge` (`txn/write/triples.rs:380`) accepts any string. `ontology::is_known_predicate`
  has no callers.
- `kg_neighbors` (`traits/triples.rs:18-68`) is kind-blind, bidirectional BFS with depth ≤ 2;
  it is the main read primitive (query plans, `vault context`, RAG, daemon).

### C.2 The KG is entirely derived

Every node/edge comes from one of three producers, all over authoritative objects:

| Producer | File | Source tag | Trigger |
|---|---|---|---|
| Vault projection | `atomic-repository/src/repository/vault_triples.rs` | `vault` | every `vault_store` (intent/memory/goal writes, `vault sync`, record w/ vault sync), `query reindex` |
| VCS/AST enrichment | `atomic-repository/src/repository/vault_kg_enrich.rs` | `vcs`, `ast` | clone, pull, git import, record (`kg_enrich_change`), `query enrich` |
| Session ledger | `atomic-core/src/pristine/txn/write/session_kg.rs` | `session_ledger` | turn append/rewrite, session lifecycle, sidecar pull, `session rebuild` |

**Consequence for the cutover:** nothing in the KG needs in-place rewriting. A storage-
generation bump + full rebuild from canonical objects *is* the migration, and the RFC's
"fail on unclassifiable overloaded edge" rule becomes a projector-level guarantee (each
projector knows its endpoint classes at emit time).

`vault sync` and `query enrich` share **no** code; they meet only on shared ID strings.

### C.3 Vocabulary inventory (`atomic-core/src/pristine/ontology.rs`)

- 6 prefix labels (no IRIs), 30 `predicate::*` CURIEs, 16 `entity_type::*` (`vault:*`),
  29 `edge_kind::*` SCREAMING_CASE strings.
- Actually emitted: 19 uppercase kinds + `prov:{wasDerivedFrom,used,wasAssociatedWith,
  hadPlan,hadMember,generated,wasInformedBy}`, `atom:informedBy`, `vault:{explainedBy,hasTodo}`.
- Defined but never emitted: all `dc:*`, `skos:*`, `rdf:type` (as edge), `vault:{status,
  priority,assignedTo,blocks,...}` predicates, `IMPORTS`, `VISIBLE_FROM`, and 12 of 16
  `entity_type` values.
- Typing: `kind` string on every node; `metadata.rdf_type` (prefixed, e.g. `vault:Session`)
  only on the 4 session-ledger node kinds. **No real `rdf:type` anywhere.**

### C.4 Identity problems

- Change node IDs are **12-char base32 prefixes** in three places
  (`vault_kg_enrich.rs:1089 truncate_hash`, `session_kg.rs:46 change_node_id`,
  `vault_triples.rs:1268 rdf_target_to_kg_id`). Full hash survives only in metadata.
- `identity:` IDs are display names (`identity:{author.name}` vs `identity:{fm.developer}`);
  intents write `author`/`created_by` which the KG ignores, so intents have no author edge.
- `todo:X` from `urn:atomic:todo:X` collides with the legacy session-global todo form.
- `LINKED_INTENT` targets `intent:{raw}` without uppercasing/manifest resolution.
- **No `ProjectId` exists.** Closest: remote `ProjectInfo.id: Uuid` (server-assigned),
  `VaultManifest.intent_prefix` (4-char human code), `repository_identity` DID in
  `provenance_gate.rs:76`.

### C.5 Overloaded / misused relations (confirmed in code)

| Current | Uses | Lines |
|---|---|---|
| `DEPENDS_ON` | change→change; intent→intent (fallback for any `:::ref`) | enrich :409,:526; triples :1158 |
| `MODIFIES` | change→file; change→entity **only on record-time path**, not bulk | enrich :436,:554,:624 |
| `ON_VIEW` | change→*working copy's desired view* (not membership!); session→view | enrich :386,:514; session_kg :320 |
| `prov:wasAssociatedWith` | tool_result→goal | triples :1002 |
| `prov:hadPlan` | session→intent **and** turn→intent | session_kg :389-397 |
| `prov:hadMember` | session→turn | session_kg :381 |
| `prov:wasDerivedFrom` | memory→{anything}; session→session (fork) | triples :978; session_kg :456 |

### C.6 Facts captured but never projected

- `Attestation` (`atomic-core/src/change/attestation.rs:153`): `session_id`,
  `changes_covered`, `signer` (`did:atomic:…` or `session-mac:{sid}`), `signature`,
  `previous_attestation`, `provenance_root`, … Stored as `.attest` files.
  **No KG projection.**
- Canonical intent/memory attestations (signed JSON-LD vault entries) — explicitly skipped
  by the KG (`vault_triples.rs:53`).
- `SessionTurn` (`atomic-core/src/change/session.rs:418`): `plan_id`, `goal`,
  `provenance_hash`, `change_hashes`, `previous_provenance`, `outcome`, boundaries —
  partially projected (no outcome/boundaries).
- AI `Provenance` on changes (model/tokens/cost): not part of Phase 1.

### C.7 Consumers that hardcode kind strings

- `atomic-cli/src/commands/query/mod.rs`: `callers` (`to_uppercase()=="CALLS"` :1231,:1255),
  HTML `EDGE_COLORS` (:748-755), `ask` system prompt (:1971-1976), text output.
- `atomic-repository/src/triage/report.rs` (`kind_is` at :35, case-insensitive, ~12 sites),
  `atomic-repository/src/repository/triage.rs:231,305`.
- `atomic-repository/src/repository/vault_intent.rs:1770,1776` (`atom:informedBy`).
- `atomic-cli/src/commands/vault/context.rs:470-473` (filters `node.kind == "memory"`).
- libatomic daemon copies: `services_agent.rs:131,3372-3387`, `services_query.rs:1001-1038,1059`.
- Wire: `libatomic/proto/.../knowledge_messages.proto:123` `KGEdge.relation` (free string).
- Out-of-repo clients (`../noname`, `../the-hive`) also match kind strings; the release is
  announced as breaking, so they are not updated as part of this plan.
- ~110 test assertion sites (largest: `vault_triples.rs` ~40, `ontology.rs` ~31,
  `atomic-cli/tests/session_integration_test.rs` ~17).

### C.8 Existing Semantic Web pieces and constraints

- `atomic-canonical` emits signed JSON-LD with context `https://atomic.dev/ns/ctx.jsonld`,
  mapping the old `atom:` prefix to `https://atomic.dev/ns#`. Its content hash is
  `blake3(JCS(node))`, which covers the `@context` **URL string** but not the term
  definitions behind it. Signed documents are never rewritten.
- `atomic provenance` projects PROV JSON-LD on demand (`atomic-canonical/src/prov.rs`);
  it is not written to the KG.
- No RDF/OWL/SHACL crates are in the workspace and there are no `.ttl` files yet. SHACL
  engine selection is in §1.4 and `docs/shacl-purrdf-spike-findings.md`.
- Upgrade machinery: `SCHEMA_VERSION` only rejects newer databases (`schema.rs`);
  **capability fences** (`capability.rs`, `required-capability/<id>`) make older binaries
  fail closed; `kg_index_meta` holds per-index markers (`fts_reverse_schema` = 2 is the
  precedent for a KG rebuild).

## 1. Phase 1 scope

### 1.1 Ontology (`atomic-core/ontology/workflow-1.0.0.ttl`)

**Namespace:** `atomic:` → `https://atomic.dev/ns/workflow#`

Ontology IRI: `https://atomic.dev/ns/workflow`
Version IRI: `https://atomic.dev/ns/workflow/1.0.0`

**Classes** (workflow-level only):

| Canonical class | Superclass | Note |
|---|---|---|
| `atomic:WorkIntent` | `owl:Thing` | Base for all intents |
| `atomic:FeatureIntent` | `atomic:WorkIntent` | From canonical `kind` |
| `atomic:BugIntent` | `atomic:WorkIntent` | From canonical `kind` |
| `atomic:ReviewIntent` | `atomic:WorkIntent` | From canonical `kind` |
| `atomic:ManagedIntent` | `atomic:WorkIntent` | Lifecycle status |
| `atomic:Goal` | `owl:Thing` | |
| `atomic:Memory` | `owl:Thing` | |
| `atomic:Skill` | `owl:Thing` | |
| `atomic:ToolResult` | `owl:Thing` | |
| `atomic:Task` | `owl:Thing` | |
| `atomic:AcceptanceCriterion` | `owl:Thing` | |
| `atomic:ScopeItem` | `owl:Thing` | |
| `atomic:Constraint` | `owl:Thing` | |
| `atomic:Todo` | `owl:Thing` | |
| `atomic:Session` | `owl:Thing` | |
| `atomic:SessionTurn` | `owl:Thing` | |
| `atomic:ManagedTurn` | `atomic:SessionTurn` | When `plan_id` present |
| `atomic:ProvenanceGraph` | `owl:Thing` | |
| `atomic:Attestation` | `owl:Thing` | |
| `atomic:Finding` | `owl:Thing` | Historical-gap findings |
| `atomic:Change` | `owl:Thing` | |
| `atomic:View` | `owl:Thing` | |
| `atomic:File` | `owl:Thing` | Workflow artifact, not code entity |
| `atomic:Identity` | `owl:Thing` | |
| `atomic:Concept` | `skos:Concept` | |

**Object properties** (workflow edges only):

| Property | Domain | Range | Inverse | Super |
|---|---|---|---|---|
| `atomic:hasTurn` | `atomic:Session` | `atomic:SessionTurn` | `atomic:turnOfSession` | — |
| `atomic:hasPlan` | `atomic:SessionTurn` | `atomic:WorkIntent` | `atomic:governsTurn` | — |
| `atomic:generatedChange` | `atomic:SessionTurn` | `atomic:Change` | `atomic:generatedBy` | `prov:generated` |
| `atomic:informedBy` | `atomic:WorkIntent` | `atomic:Memory` | — | `prov:wasDerivedFrom` |
| `atomic:explainedBy` | — | `atomic:ProvenanceGraph` | — | — |
| `atomic:turnExplainedBy` | `atomic:SessionTurn` | `atomic:ProvenanceGraph` | — | `atomic:explainedBy` |
| `atomic:changeExplainedBy` | `atomic:Change` | `atomic:ProvenanceGraph` | — | `atomic:explainedBy` |
| `atomic:hasTodo` | `atomic:SessionTurn` | `atomic:Todo` | — | — |
| `atomic:blocks` | `atomic:WorkIntent` | `atomic:WorkIntent` | `atomic:blockedBy` | — |
| `atomic:assignedTo` | `atomic:WorkIntent` | `atomic:Identity` | — | — |
| `atomic:linkedIntent` | `atomic:Goal` | `atomic:WorkIntent` | — | — |
| `atomic:linkedGoal` | `atomic:WorkIntent` | `atomic:Goal` | — | — |
| `atomic:references` | `owl:Thing` | `owl:Thing` | — | — |
| `atomic:hasLabel` | `atomic:WorkIntent` | `skos:Concept` | — | — |
| `atomic:hasTask` | `atomic:WorkIntent` | `atomic:Task` | — | — |
| `atomic:hasAcceptanceCriterion` | `atomic:WorkIntent` | `atomic:AcceptanceCriterion` | — | — |
| `atomic:hasScopeIn` | `atomic:WorkIntent` | `atomic:ScopeItem` | — | `atomic:hasScopeItem` |
| `atomic:hasScopeOut` | `atomic:WorkIntent` | `atomic:ScopeItem` | — | `atomic:hasScopeItem` |
| `atomic:hasConstraint` | `atomic:WorkIntent` | `atomic:Constraint` | — | — |
| `atomic:satisfies` | `atomic:Task` | `atomic:AcceptanceCriterion` | — | — |
| `atomic:touches` | `atomic:Task` | `atomic:File` | — | — |
| `atomic:scopeOutFile` | `atomic:WorkIntent` | `atomic:File` | — | — |
| `atomic:remediates` | `atomic:WorkIntent` | `atomic:WorkIntent` | — | — |
| `atomic:reviews` | `atomic:ReviewIntent` | `atomic:WorkIntent` | `atomic:reviewedBy` | — |
| `atomic:directlyDependsOnChange` | `atomic:Change` | `atomic:Change` | — | `atomic:dependsOn` |
| `atomic:dependsOnIntent` | `atomic:WorkIntent` | `atomic:WorkIntent` | — | `atomic:dependsOn` |
| `atomic:modifiesFile` | `atomic:Change` | `atomic:File` | — | `atomic:modifies` |
| `atomic:directlyOnView` | `atomic:Change` | `atomic:View` | — | — |
| `atomic:sessionViewContext` | `atomic:Session` | `atomic:View` | — | — |
| `atomic:childViewOf` | `atomic:View` | `atomic:View` | — | — |
| `atomic:visibleFrom` | — | — | — | Query-time operator |
| `atomic:authoredBy` | `owl:Thing` | `atomic:Identity` | — | `prov:wasAttributedTo` |
| `atomic:coversChange` | `atomic:Attestation` | `atomic:Change` | — | — |
| `atomic:attests` | `atomic:Attestation` | `atomic:SessionTurn` | — | — |
| `atomic:signedBy` | `atomic:Attestation` | `atomic:Identity` | — | — |
| `atomic:previousAttestation` | `atomic:Attestation` | `atomic:Attestation` | — | — |
| `atomic:implements` | `atomic:Change` | `atomic:WorkIntent` | — | — |
| `atomic:attestedBy` | `atomic:Change` | `atomic:Attestation` | — | — |
| `atomic:aboutResource` | `atomic:Finding` | `owl:Thing` | — | — |

Note: `atomic:dependsOn` and `atomic:ancestorView` are declared `owl:TransitiveProperty`
but are **not** materialized in Phase 1.

**Datatype properties:**

| Property | Domain | Range |
|---|---|---|
| `atomic:status` | `atomic:WorkIntent` | `xsd:string` |
| `atomic:priority` | `atomic:WorkIntent` | `xsd:string` |
| `atomic:findingCode` | `atomic:Finding` | `xsd:string` |

**Property chains** (declared in TTL as `owl:propertyChainAxiom` for Phase 2 inference):

| Chain | Meaning |
|---|---|
| `atomic:generatedBy ∘ atomic:hasPlan` → `atomic:implements` | A change was generated by a turn that had a plan → the change implements that intent. |
| `atomic:generatedBy ∘ atomic:turnExplainedBy` → `atomic:changeExplainedBy` | A change was generated by a turn explained by a provenance graph → the change is explained by that graph. |

These chains use `atomic:generatedBy` (the inverse of `atomic:generatedChange`), so the intermediate node is consistently `SessionTurn`.

`atomic:explainedBy` has no domain of its own. In OWL a domain is an inference rule, not a check: a `SessionTurn` domain on the property that the chain derives for Changes would infer every Change to be a `SessionTurn`, and the publication shapes that target `SessionTurn` would then run against Changes. The two subproperties carry the domains instead; queries on `atomic:explainedBy` still see both kinds of edge. In Phase 1 the chains are **declarative only** — the ontology records the intended meaning so that enterprise SHACL shapes and documentation see a complete model. The OWL 2 RL reasoner in Phase 2 will materialize them from the TTL.

**What the ontology does NOT contain:**

`atomic:defines`, `atomic:exports`, `atomic:calls`, `atomic:includes`,
`atomic:imports`, `atomic:modulePartOfModule`, `atomic:filePartOfModule`,
`atomic:modifiesEntity`, and the `atomic:SourceEntity` class are omitted. They
describe code structure inside files; that is the syntax index's job.

### 1.2 Rust term registry (`atomic_core::kg::ontology`)

Rewrite `atomic-core/src/pristine/ontology.rs`:

```rust
pub enum Class { WorkIntent, FeatureIntent, ... }
pub enum Predicate { HasTurn, HasPlan, ... }

impl Class {
    pub fn iri(&self) -> &'static str;
    pub fn curie(&self) -> &'static str;
    pub fn parents(&self) -> &'static [Class];
}

impl Predicate {
    pub fn iri(&self) -> &'static str;
    pub fn curie(&self) -> &'static str;
    pub fn domain(&self) -> Option<Class>;
    pub fn range(&self) -> Option<Class>;
    pub fn inverse(&self) -> Option<Predicate>;
    pub fn super_predicate(&self) -> Option<Predicate>;
}

pub static PREFIXES: &[(&str, &str)] = &[
    ("atomic", "https://atomic.dev/ns/workflow#"),
    ("prov", "http://www.w3.org/ns/prov#"),
    ("dcterms", "http://purl.org/dc/terms/"),
    ("skos", "http://www.w3.org/2004/02/skos/core#"),
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("owl", "http://www.w3.org/2002/07/owl#"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
];
```

Embed the TTL via `include_str!("../../ontology/workflow-1.0.0.ttl")` so the Rust
source and the TTL cannot drift.

### 1.3 Project identity (`atomic_core::kg::project_id` + `atomic_core::kg::iri`)

**Design goal:** a single stable identifier per project that survives views,
inserts, unrecords, and clone/pull/push, and becomes the `<pid>` component in
project-qualified IRIs. **Every clone of a project carries the origin's identity**; only
an explicit fork (later phase) mints a new one.

**Format:**
- 256-bit random ID generated by the `atomic init` command (not by
  `Repository::init`; see Transport).
- Canonical encoding: uppercase RFC 4648 base32 (52 characters, no padding), same
  alphabet as `Hash::to_base32`.
- Display form: the base32 string.
- URN prefix: `urn:atomic:project:<pid>`.

**Storage:**
- Dedicated table `PROJECT_IDENTITY` in pristine (not `VIEW_CHANGES`, therefore
  view-independent and immune to insert/unrecord).
- Schema: `current_hash` → the content-addressed object hash of the signed identity
  object; `created_at` → timestamp. A `previous` chain column is reserved for rotation
  (deferred), so the schema is forward-compatible.

**Identity object (content-addressed, signed JSON-LD):**

```jsonld
{
  "@context": "https://atomic.dev/ns/ctx.jsonld",
  "@type": "atomic:ProjectIdentity",
  "atomic:projectId": "ABCD...EFGH",   // the 52-char base32
  "atomic:createdAt": "2026-01-15T09:00:00Z",
  "atomic:schemaVersion": 2,
  "atomic:issuer": "did:atomic:..."     // optional in Phase 1; may be unknown
}
```

- Content hash = `blake3(JCS(normalized_bytes))`.
- Signed by the initializing identity if available; unsigned is allowed in Phase 1
  (the object schema reserves the `proof` field).

**Why not `.vault/` or view metadata?**
A `.vault/` file is a change-tracked artifact. It can be absent on some views,
in conflict, or unrecorded. Project identity must be the same on every view and
survive all graph mutations. The only place that guarantees this is a repository-level
pristine table outside the view system.

**Transport:**

> **Phase 1 implementation boundary.** `atomic clone` is HTTP-only (plus a Git bootstrap
> path that never contacts storage), so identity transport over clone, pull and push all
> depends on server support in `atomic-storage`, which is owned by another effort. In
> Phase 1, implement only the local pieces: minting in the `atomic init` command (not in
> `Repository::init`), the `PROJECT_IDENTITY` table, the upgrade rule, and the CLI. **Do
> not change push, pull, clone, `atomic-remote`, or the daemon's sync code yet.** The rules
> below are the target behavior that the later transport work implements.

Clones keep the origin's identity, and pulls never change it. Rules:

- **Where minting happens.** Only the `atomic init` command mints a new identity.
  `Repository::init` does not, because `atomic clone` calls it first
  (`clone/command.rs:992`) and then pulls; minting there would give every clone a fresh
  identity and discard the origin's.
- **Clone** (from Atomic storage or any other remote): the remote supplies its
  identity object as a repository-level sidecar (not a change). The new repository
  verifies it (hash, and signature when present) and stores it in `PROJECT_IDENTITY`.
  The clone is the same project as its origin.
- **Pull**: the local identity never changes. If the remote's identity matches, nothing
  happens. If the local table is empty (a repository created before identities
  existed), the pulled identity is adopted. If it differs, the pull still succeeds
  (changes are global objects and may move between projects) but reports that the
  remote belongs to a different project.
- **Push**: the first push to a storage project with no bound identity binds it (the
  server records the identity hash against its `ProjectInfo.id`). Later pushes carrying
  a different identity are rejected, so two unrelated repositories cannot claim the same
  storage project.
- **Upgrading existing repositories:** a repository with no remote mints on its first
  writable open. A repository with a remote does **not** auto-mint, since that would
  diverge from its origin; it adopts the identity on its next pull. The owner mints once
  with `atomic project identity init` and pushes, and every clone picks it up.
- **Atomic storage work:** the remote protocol (`atomic-remote`) and the storage
  server gain an identity object transfer and the push-time binding check. The current
  push/pull contract excludes sidecars (`atomic-remote/src/sync.rs:476`), so this is a
  new object class, not a reuse of an existing path.
- No fork command in Phase 1; fork semantics (new identity with
  `prov:wasDerivedFrom` pointing at origin) come later.

**API:**

```rust
// atomic-core/src/kg/project_id.rs
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProjectId([u8; 32]);

impl ProjectId {
    pub fn generate() -> Self;           // OsRng → 32 bytes
    pub fn from_base32(s: &str) -> Result<Self, ProjectIdError>;
    pub fn to_base32(&self) -> String;   // 52-char uppercase, no padding
    pub fn to_urn(&self) -> String;      // urn:atomic:project:<base32>
}

// atomic-core/src/kg/iri.rs
pub fn mint_intent_iri(pid: &ProjectId, ulid: &str) -> String;
pub fn mint_goal_iri(pid: &ProjectId, encoded_name: &str) -> String;
pub fn mint_memory_iri(pid: &ProjectId, encoded_name: &str) -> String;
pub fn mint_session_iri(pid: &ProjectId, sid: &str) -> String;
pub fn mint_view_iri(pid: &ProjectId, view_name: &str) -> String;
pub fn mint_file_iri(pid: &ProjectId, path: &str) -> String;
// ... and parser functions for the reverse direction
```

All minting functions percent-encode path/ name segments per RFC 3987 and guarantee
round-trip stability.

**Identifier casing.** Every identifier inside a URN is uppercase: base32 hashes and
project IDs (as `Hash::to_base32` emits) and intent ULIDs (uppercase Crockford, as
`ulid::Ulid::to_string()` emits, `vault_intent.rs:141`). Parsers for URNs are strict: they
reject lowercase, padding, wrong length, and (for base32) nonzero trailing bits. The
existing `Hash::from_base32` is case-insensitive, so URN parsing needs a new strict parser;
lenient prefix lookup in CLI arguments is unaffected.

**Old-layout intents.** Intents created before the ULID layout live at
`intents/<view>/<session-uuid>/<turn>/intent.md` (e.g. `ATOM-2` in this repo's vault) and
carry no ULID. The projector resolves their identity through the vault manifest; if none is
recorded, it emits a finding instead of inventing an ID. Property tests cover paths with spaces, `#`, `:`, non-ASCII,
and the empty string.

**CLI:**

```bash
# Show the project identity
atomic project identity show
# → project: ABCDEFGH...12345678
#   urn: urn:atomic:project:ABCDEFGH...12345678
#   created: 2026-01-15T09:00:00Z
```

**Phase 1 IRI usage:**

The projectors (Phase 2) will emit IRIs like:

```
urn:atomic:project:<pid>:intent:<ulid>
urn:atomic:project:<pid>:goal:<encoded-name>
urn:atomic:project:<pid>:memory:<encoded-name>
urn:atomic:project:<pid>:session:<sid>
urn:atomic:project:<pid>:view:<encoded-name>
urn:atomic:project:<pid>:file:<pct-encoded-path>
```

In Phase 1 we only mint the `ProjectId` and provide the IRI functions; no
projector actually emits project-qualified IRIs yet. This is deliberate:
projectors come in Phase 2, but the identity must exist first so that Phase 2
can use it without a second migration.

### 1.4 SHACL integration (PurRDF, existing engine)

**Decision:** SHACL shapes are the primary workflow-policy definitions. Use an
existing SHACL engine, not hand-coded checks or an Atomic-owned subset evaluator.
The provisional choice is **`purrdf-shapes = "=3.0.1"`** from
[PurRDF](https://github.com/Blackcat-Informatics/purrdf).

The [initial spike](./shacl-purrdf-spike-findings.md) compiled the published crate
without source changes and passed eleven tests. The reproducible standalone package
is [`tools/shacl-purrdf-spike`](../tools/shacl-purrdf-spike/README.md), outside the
Atomic workspace. This is evidence for adoption, not proof of complete conformance.

**Observed in the spike:**

| SHACL behavior | Result |
|---|---|
| Target class + required/exactly-one property | Positive and negative fixtures pass |
| Nested `sh:node` with enum constraints | Enforced; no deferral to Phase 2 |
| Local `sh:sparql` conditional requirements | Enforced; independent of federation |
| Inverse and sequence paths | Enforced; complex report path retained |
| Class, datatype, and `sh:or` | Positive and negative fixtures pass |
| Report focus/path/shape/component/severity/message | Present; separate paths do not collapse |
| Malformed cardinality parameter / malformed SPARQL query | Shape loading returns an error |
| Unsupported SHACL-JS | Returns an error, even with an empty data graph |
| Valid shape with no applicable focus nodes | Conforms vacuously; target coverage must be reported separately |

PurRDF documents broader Core/SPARQL support; additional constructs must be covered
by profile regression tests before we depend on them. `oxttl` and `oxrdf` are
parser/data-model utilities, **not** the validation engine.

**Toolchain/deployment gate:** PurRDF 3.0.1 requires **Rust 1.98**, while Atomic
currently declares **1.90**. The spike uses a separate toolchain and resolves 73
dependency packages (including target-specific packages). M3 must explicitly choose
an approved MSRV upgrade for embedded use or a separately built PurRDF runner.
Do not silently raise Atomic's MSRV. Either deployment runs the same SHACL shapes;
there is no hand-rolled fallback.

**Builtin shape graphs:**

- `atomic-core/ontology/shapes/projection-1.0.0.ttl` — projection profile:
  - every generated change has `atomic:implements`, `atomic:changeExplainedBy`, and
    `atomic:attestedBy` shortcuts;
  - shortcut targets equal the authoritative intermediate path targets;
  - all referenced canonical nodes exist;
  - full hashes (not abbreviated labels) in durable IDs.

- `atomic-core/ontology/shapes/publication-1.0.0.ttl` — publication profile:
  - every managed `SessionTurn` has exactly one `atomic:hasPlan` intent;
  - no managed turn is governed by more than one intent;
  - every turn has exactly one provenance graph;
  - every generated change is covered by the session attestation;
  - every attestation is signed by an atomic identity DID (`did:atomic:…`) and has an
    explicit successful verification result supplied by a separate cryptographic
    verifier. MAC-signed attestations (`session-mac:{sid}`) never satisfy the publication
    profile; agents publish with their atomic identity DID (decided by Lee, 2026-10-14).

SHACL cannot establish signature validity by itself. These shapes define the
intended profiles; fixture tests in Phase 1 prove their behavior, while complete
live projection and mandatory publication enforcement land in Phase 2. Running
shapes without applicable focus nodes must not be presented as complete audit evidence.

**Enterprise custom shapes:**

Repository directory `.atomic/shapes/*.ttl` is loaded on `atomic query validate`.
Automatic `vault sync` validation and publication hooks land in Phase 2. Builtin
and enterprise shapes use the same engine; policy changes do not require Rust code.
Use explicit validation profiles so teams can select applicable workflow rules.

Report `sh:focusNode`, `sh:resultPath`, `sh:sourceShape`,
`sh:sourceConstraintComponent`, `sh:resultSeverity`, and `sh:resultMessage`, retaining
complex path structure and multiple findings. Shape parse/load/execution errors
are distinct from violations and must never yield a successful validation result.

Example enterprise traceability shape (illustrative, not a complete SOX policy):

```turtle
@prefix atomic: <https://atomic.dev/ns/workflow#> .
@prefix sh:     <http://www.w3.org/ns/shacl#> .

<#SoxChangeShape>
  a sh:NodeShape ;
  sh:targetClass atomic:Change ;
  sh:property [
    sh:path atomic:attestedBy ;
    sh:minCount 1 ;
    sh:message "SOX: every change must be attested."
  ] ;
  sh:property [
    sh:path atomic:implements ;
    sh:minCount 1 ;
    sh:message "SOX: every change must trace to an intent."
  ] .
```

**Integration boundary — adaptation, not evaluation:**

1. Adapt a read-only workflow snapshot into RDF with canonical predicates/types,
   full resource IRIs, typed literals, and properly scoped blank nodes. After the
   §1.6 cutover the KG already uses canonical terms and IRIs, so the adapter does no
   legacy-term translation; AST relationships are not in the KG. Signed objects are
   never rewritten.
2. Load builtin and selected enterprise shape bundles with explicit base IRIs and
   controlled imports. Do not fetch arbitrary remote shape imports implicitly.
3. Call PurRDF's existing API. The spike uses
   `purrdf_shapes::engine::validate_graphs(data_nt, shapes_ttl, shapes_base)`;
   assess its prepared-shape/dataset APIs for production snapshot reuse.
4. Preserve its standard validation report; adapt presentation to CLI JSON/text
   without losing source shapes, paths, messages, or severity.

Atomic code owns snapshot adaptation, profile selection, invocation, and report
presentation only. PurRDF owns shape parsing and constraint execution. Missing
projection facts remain missing—no guessed links or bespoke policy evaluator.
Record snapshot/profile identity and target coverage; an empty target set does not
prove workflow completeness. The adapter's fidelity needs integration tests beyond
the synthetic spike. No OWL inference is performed by this boundary in Phase 1.

**CLI:**

```bash
# Validate against all loaded shapes (builtin + .atomic/shapes/)
atomic query validate

# Validate against a specific shape
atomic query validate --shape <iri>

# Validate a specific node
atomic query validate --node <iri>

# Output validation report as JSON or human-readable
atomic query validate --format json|text
```

### 1.5 Legacy term coverage

`LEGACY_TERMS` (test-only, gated by `#[cfg(test)]`):

```rust
pub enum Replacement {
    One { class: Class },
    OnePred { predicate: Predicate },
    SplitByEndpoint(&[(Option<Class>, Option<Class>, Predicate)]),
    Removed { reason: &'static str },
}

static LEGACY_TERMS: &[(/* old */ &'static str, Replacement)] = &[ ... ];
```

Tests:

1. Every legacy `predicate::*`, `entity_type::*`, and `edge_kind::*` constant appears
   exactly once in `LEGACY_TERMS`.
2. Every `One`/`Split` target exists in the TTL.
3. TTL ↔ Rust consistency: parsed with `oxttl` + `oxrdf` (**dev-dependencies only**).
   Every class/property in the TTL has a corresponding Rust enum variant, and vice
   versa, with matching domain/range.
4. No emitted string starts with `vault:`, `atom:`, or is SCREAMING_CASE.

### 1.6 KG cutover: producers and consumers

The KG is entirely derived from canonical objects (vault entries, changes, views, the
session ledger, provenance sidecars, attestations), so the cutover is a rebuild under a
new generation, not an in-place data migration.

**Storage prerequisites (minimal):**
- Typed write API: `upsert_kg_edge` takes a `Predicate` from `kg::ontology`, not a
  `&str`, so unknown terms cannot be written. Reads reject non-canonical kinds.
- `kg_types` / `kg_types_rev` multimap tables (node IRI ↔ class IRI) for `rdf:type`.
  Types are kept out of the edge tables so the kind-blind neighborhood traversal does
  not connect every node to every same-typed node; the RDF adapter emits them as
  `rdf:type` triples. `KgNode.kind` and `metadata.rdf_type` are removed.
- `KG_GENERATION` constant plus `kg_index_meta["kg_generation"]`. The rebuild runs once
  per generation: automatically on the first writable open and via
  `atomic query reindex --full`, regardless of repository size. The marker is written in
  the same transaction as the final rebuild batch, so an interrupted rebuild restarts
  cleanly and a completed one never repeats. Repositories created by the new binary are
  stamped at creation. Read-only opens on an old-generation KG return `KgNeedsRebuild`.
- Capability fence `kg-semweb-v1` (`capability.rs`) so older binaries fail closed.

**Producer rewrite** (canonical predicates, `rdf:type`, project-qualified IRIs via
`kg::iri`, full-hash change IRIs):

| Producer | Today | Phase 1 |
|---|---|---|
| Vault projection | `vault_triples.rs` | intents (subclass from canonical `kind`), memories, goals, skills, tool results, decomposition children, `:::ref` edges split by edge name (`dependsOnIntent`, `blockedBy`, `remediates`, `reviews`). Signed canonical documents keep their old `atom:` terms; the projector translates them through `LEGACY_TERMS` at projection time and never rewrites signed bytes |
| VCS projection | `vault_kg_enrich.rs` (VCS part) | views and `childViewOf`, changes (full hash), `directlyDependsOnChange`, `modifiesFile`, `authoredBy`, and `directlyOnView` recomputed from `VIEW_CHANGES` (today `ON_VIEW` records the working copy's desired view, not membership) |
| Session ledger | `session_kg.rs` | sessions, turns (`ManagedTurn` when `plan_id` is present), todos, provenance graphs; `hasTurn`, `hasPlan`, `generatedChange`, `turnExplainedBy`, `hasTodo`, `sessionViewContext` |
| Attestations (**new**) | not projected | `Attestation` nodes, `coversChange`, `attests`, `signedBy`, `previousAttestation`, and the verifier's recorded result (never inferred). **Turn mapping:** an attestation records `session_id` and `changes_covered` but no turn number, so emit `attests` only when exactly one turn of that session has `change_hashes` ⊆ `changes_covered`; otherwise emit an `AmbiguousAttestationTurn` finding and no `attests` edge. MAC-signed attestations (`session-mac:{sid}`) can't be verified without the key and record `Unverifiable`, never `Verified`; they never satisfy the publication profile, which requires an atomic identity DID signer |
| Findings (**new**) | none | `MissingHistoricalPlanId`, `MissingAttestation`, `AmbiguousAttestationTurn`, `UnverifiableAttestation`; never guessed links |
| AST enrichment | `vault_kg_enrich.rs` (AST part) | **removed** from the KG: no entity nodes, `CALLS`, `DEFINES`, `EXPORTS`, `INCLUDES`, `PART_OF` |

Existing triggers stay as they are (record, vault sync, clone, pull, import, session
hooks); unifying them into one `reconcile()` pipeline is Phase 2.

**Consumer cutover:**
- `query_plan.rs`: `Filter` on `kind` becomes type-based; add a predicate filter and
  direction to neighborhood steps; bump the plan JSON schema version.
- CLI `query`: graph colour map keyed by IRI; text/DOT output compacts IRIs to CURIEs via
  `PREFIXES`; JSON emits absolute IRIs; the `ask` prompt lists canonical predicates.
- `query callers` and the triage report's blast radius / layer ordering
  (`triage/report.rs`) switch from KG code edges to live `atomic-semantic` parsing.
- Triage, `vault_intent.rs`, and `vault/context.rs` match canonical predicates and types.
- libatomic daemon duplicates (`services_agent.rs`, `services_query.rs`), `rpc.rs`, and
  the knowledge proto (`KGEdge.relation` documented as an absolute IRI; node types added;
  API version bumped). The release is announced as breaking; no compatibility shim.
- Update the ~110 test assertion sites.

## 2. Out of scope for Phase 1

| Item | Deferred to |
|---|---|
| `KgEdge.graph` (`Asserted`/`Inferred`) | Phase 2 |
| Unified `reconcile()` pipeline (one entry point for sync/enrich/reindex) | Phase 2 |
| OWL 2 RL inference engine (`kg::reason`) | Phase 2 |
| Cross-project SPARQL endpoints / federation catalogs | Later federation phase |
| Code-structure edges (`calls`, `defines`, `exports`, `includes`, `modifiesEntity`) | When sourced from AST |
| Scope-qualified entity IDs | Same — when code-structure edges are needed |
| Project identity rotation / fork command | Phase 2 |
| DID binding on `ProjectIdentity.issuer` | Phase 2 |

## 3. Acceptance criteria

1. `workflow-1.0.0.ttl` is parseable by `oxttl` and contains no axioms outside the
   supported RL subset.
2. `atomic_core::kg::ontology::Class::iri()` returns the correct absolute IRI for
   every variant.
3. `atomic_core::kg::ontology::Predicate::domain()` and `::range()` match the TTL.
4. `LEGACY_TERMS` coverage test passes: all old constants mapped, no unmapped survivors.
5. Workspace lint: no legacy `vault:*`, `atom:*`, or SCREAMING_CASE kind strings
   outside `LEGACY_TERMS`.
6. **Project identity:** `atomic init` creates a `ProjectId`; `atomic project identity show`
   displays it; the ID round-trips through base32 encoding; IRIs minted via `kg::iri`
   match the canonical format.
   - `Repository::init` (as called by `atomic clone`) does not mint an identity.
   - **Deferred until `atomic-storage` implements identity transport (owned by another
     effort):** a clone has the origin's identity; a pull never changes a populated
     identity and reports a differing one; pushing a different identity to a bound
     storage project is rejected. Do not implement or test these in Phase 1.
7. **SHACL integration:** PurRDF evaluates positive and negative fixtures for both
   builtin profiles, nested shapes, local SPARQL conditions, and property paths.
   No Atomic-owned constraint evaluator. The MSRV/deployment decision is explicit.
8. **Custom shapes:** placing a `.ttl` in `.atomic/shapes/` causes `atomic query validate`
   to load and evaluate it using the same engine as builtin profiles.
9. **Conformance report:** preserve standard focus/path/source/component/severity/
   message fields and complex path structures. Parse/runtime failures are errors,
   never conformance. Report the selected profile and target coverage separately.
10. **Adapter fidelity:** real workflow snapshots preserve resource identity, types,
    typed literals, and missing facts. Empty or partial projections must not be
    described as complete audit evidence.
11. **Regression corpus:** retain the isolated spike, add the existing gate fixtures
    and relevant W3C cases, and compare against an independent SHACL implementation
    before enabling mandatory production gates.
12. **Cutover:** after the one-time rebuild, the KG contains only canonical predicates,
    `rdf:type` entries in `kg_types`, project-qualified IRIs, and full-hash change IRIs;
    no AST nodes or edges remain. A second writable open performs no rebuild work.
13. **Consumers:** every consumer listed in §1.6 works against the canonical KG;
    `query callers` and triage blast radius produce the same results from live parsing.

## 4. Milestones

### M1 — Ontology + registry (one PR)

- `workflow-1.0.0.ttl`
- `atomic_core::kg::ontology` module
- `LEGACY_TERMS` + coverage tests
- TTL ↔ Rust consistency test (`oxttl`/`oxrdf` as dev-deps)

### M2 — Project identity + IRI minting (one PR, depends on M1)

- `atomic_core::kg::project_id` (`ProjectId`, base32, URN)
- `atomic_core::kg::iri` (mint/parse for every node kind)
- `PROJECT_IDENTITY` pristine table
- Identity object (JSON-LD, content-addressed, optionally signed)
- The `atomic init` command mints (not `Repository::init`); upgrading repos without a
  remote mint on first writable open; repos with a remote stay unminted until transport
  lands (or the owner runs `atomic project identity init`)
- **Not in Phase 1:** clone/pull/push identity transport. It waits for `atomic-storage`
  (see the boundary note in §1.3); leave push, pull, clone, `atomic-remote`, and the daemon's
  sync code unchanged
- CLI: `atomic project identity show` and `atomic project identity init`
- Property tests: round-trip, percent-encoding, strict base32 parsing

### M3 — KG cutover (depends on M1/M2)

- Typed predicate write API, `kg_types`/`kg_types_rev`, `KG_GENERATION` one-time
  rebuild, capability fence
- Producer rewrite (vault, VCS, session ledger) plus new attestation and findings
  projectors; AST enrichment removed from the KG
- Consumer cutover (query plan, CLI, triage, vault context, daemon, proto); `query
  callers` and triage blast radius on live parsing
- Test updates; the lint in acceptance criterion 5 passes

Lands as one PR (or a stack merged together): the RFC allows no dual-write window.

### M4 — PurRDF SHACL integration + builtin shapes (depends on M1/M3)

- Retain the standalone `purrdf-shapes = 3.0.1` spike and lockfile.
- Resolve Rust 1.98 versus Atomic's 1.90 MSRV: approved embedded upgrade or separate
  validation runner. Select deployment before adding workspace dependencies.
- Implement the read-only workflow-to-RDF adapter and a thin PurRDF invocation layer;
  no custom SHACL evaluator, policy-rule compilation, or hand-coded mirror checks.
- `projection-1.0.0.ttl` and `publication-1.0.0.ttl`, including local SPARQL rules
  wherever workflow consistency requires them.
- CLI: `atomic query validate`, profile selection, standard reports and error exits.
- Custom shape loader from `.atomic/shapes/*.ttl`, controlled imports, no-target
  diagnostics, and snapshot/profile identification.
- Positive/negative profile fixtures, adapter integration tests, existing gate
  regressions, and independent-validator comparisons before production enforcement.

## 5. Development notes

### Engine selection and historical decision

The [OxiRS spike](./shacl-oxirs-spike-findings.md) tested **0.3.1**, with nested/
SPARQL constraints silently unenforced, lost result paths, and a large dependency
footprint. Do not generalize those findings to every future OxiRS release.

The [PurRDF 3.0.1 spike](./shacl-purrdf-spike-findings.md) passed the initial probes
for those failure categories. It is the provisional Phase 1 engine choice, subject
to the MSRV decision and broader regression coverage. A version number alone is
not a conformance or maturity guarantee.

This plan replaces the historical direction of hand-coded workflow gates / an
in-house SHACL subset with **SHACL shapes executed by an existing engine**. Existing
cryptographic verification and identity/authorization checks remain separate;
SHACL evaluates their recorded results, not signatures themselves.

### Why an RDF snapshot adapter instead of redb inside the engine?

Keep the database and authoritative objects in Atomic. Adapt workflow facts to
PurRDF's RDF input and retain its reports. This allows either embedded validation
or an isolated runner without maintaining a second authoritative store. The engine
integration must not pull source-code relationship extraction into Phase 1.

### Enterprise workflow shapes

The `.atomic/shapes/` convention is intentionally simple: drop a `.ttl` file, run
`atomic query validate`. No registry, no compilation step. A CI pipeline can run
`atomic query validate --format json` and fail on any `sh:Violation` severity.

### Why view-independent storage for project identity?

If project identity lived in a `.vault/` file (a tracked change), it would:
- be invisible on views that don't include that change;
- be lost on `unrecord`;
- diverge across views in a merge.

Cross-project SPARQL assumes every repository in the federation has one stable
`urn:atomic:project:<pid>` that never changes. A repository-level pristine table
outside the view system is the only place that guarantees this.

### Phase 2 expectations

Phase 2 will:
- add the OWL 2 RL inference engine and the `KgEdge.graph` asserted/inferred split, so
  `implements` and `changeExplainedBy` are materialized from the chains;
- unify sync/enrich/reindex into one `reconcile()` pipeline;
- wire SHACL validation into `vault sync` and publication gates.

Phase 1 already mints `ProjectId` and moves the KG to canonical IRIs, so Phase 2 needs
no second migration or ID rewrite.

---

## Appendix: Why we skip code-block mapping now

The existing `vault_kg_enrich.rs` pipeline extracts `CALLS`, `DEFINES`, `EXPORTS`,
`INCLUDES`, `PART_OF`, and `MODIFIES` (entity) by walking source files and emitting
KG edges. This duplicates `atomic-semantic`'s syntax index, produces stale line-based
entity IDs, and generates millions of mostly-unqueried edges. The right time to
materialize code-structure edges in the KG is when we need them for federated
queries that the syntax index cannot answer. Until then, `atomic:File` stays as a
workflow artifact and Tree-Sitter handles everything inside it.
