# Phase 1 Implementation Plan: KG Referential Closure

> Status: **draft plan.** Implements Phase 1 ("Referential closure") of
> [`knowledge-graph-closure-rfc.md`](./knowledge-graph-closure-rfc.md).
> Branch: `feat/semweb-knowledge-graph`. Survey references verified against `dev` at `5ddcf1b`.

## 1. Phase 1 scope (from the RFC)

- Versioned OWL 2 RL ontology, optional OWL 2 DL companion, SHACL shape graph.
- Breaking graph-format cutover to canonical RDF IRIs; no compatibility vocabulary.
- Coverage test: every current predicate / entity type / edge kind has exactly one
  canonical replacement.
- Signed stable `ProjectId`, project-qualified mutable-resource IRIs, algorithm-qualified
  content-object IRIs.
- Intent / session / turn / provenance / change / attestation nodes.
- Required direct shortcut edges (`implements`, `explainedBy`, `attestedBy`).
- Backfill and projection validation.

Explicitly **not** Phase 1: `AIContribution` nodes and cost aggregates (Phase 2),
`query ask` evidence envelopes (Phase 3), review closure digests / `reviewsClosure`
(Phase 4), federation and named-graph catalogs (Phase 5), `Execution` nodes (no
structured execution capture exists yet — see D11).

## 2. Current state (survey findings)

### 2.1 Storage

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

### 2.2 The KG is entirely derived

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

### 2.3 Vocabulary inventory (`atomic-core/src/pristine/ontology.rs`)

- 6 prefix labels (no IRIs), 30 `predicate::*` CURIEs, 16 `entity_type::*` (`vault:*`),
  29 `edge_kind::*` SCREAMING_CASE strings.
- Actually emitted: 19 uppercase kinds + `prov:{wasDerivedFrom,used,wasAssociatedWith,
  hadPlan,hadMember,generated,wasInformedBy}`, `atom:informedBy`, `vault:{explainedBy,hasTodo}`.
- Defined but never emitted: all `dc:*`, `skos:*`, `rdf:type` (as edge), `vault:{status,
  priority,assignedTo,blocks,...}` predicates, `IMPORTS`, `VISIBLE_FROM`, and 12 of 16
  `entity_type` values.
- Typing: `kind` string on every node; `metadata.rdf_type` (prefixed, e.g. `vault:Session`)
  only on the 4 session-ledger node kinds. **No real `rdf:type` anywhere.**

### 2.4 Identity problems

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

### 2.5 Overloaded / misused relations (confirmed in code)

| Current | Uses | Lines |
|---|---|---|
| `DEPENDS_ON` | change→change; intent→intent (fallback for any `:::ref`) | enrich :409,:526; triples :1158 |
| `MODIFIES` | change→file; change→entity **only on record-time path**, not bulk | enrich :436,:554,:624 |
| `ON_VIEW` | change→*working copy's desired view* (not membership!); session→view | enrich :386,:514; session_kg :320 |
| `prov:wasAssociatedWith` | tool_result→goal | triples :1002 |
| `prov:hadPlan` | session→intent **and** turn→intent | session_kg :389-397 |
| `prov:hadMember` | session→turn | session_kg :381 |
| `prov:wasDerivedFrom` | memory→{anything}; session→session (fork) | triples :978; session_kg :456 |

### 2.6 Facts captured but never projected

- `Attestation` (`atomic-core/src/change/attestation.rs:153`): `session_id`,
  `changes_covered`, `signer` (`did:atomic:…` or `session-mac:{sid}`), `signature`,
  `previous_attestation`, `provenance_root`, … Stored as `.attest` files.
  **No KG projection.**
- Canonical intent/memory attestations (signed JSON-LD vault entries) — explicitly skipped
  by the KG (`vault_triples.rs:53`).
- `SessionTurn` (`atomic-core/src/change/session.rs:418`): `plan_id`, `goal`,
  `provenance_hash`, `change_hashes`, `previous_provenance`, `outcome`, boundaries —
  partially projected (no outcome/boundaries).
- AI `Provenance` on changes (model/tokens/cost) — Phase 2.

### 2.7 Consumers that hardcode kind strings

- `atomic-cli/src/commands/query/mod.rs`: `callers` (`to_uppercase()=="CALLS"` :1231,:1255),
  HTML `EDGE_COLORS` (:748-755), `ask` system prompt (:1971-1976), text output.
- `atomic-repository/src/triage/report.rs` (`kind_is` at :35, case-insensitive, ~12 sites),
  `atomic-repository/src/repository/triage.rs:231,305`.
- `atomic-repository/src/repository/vault_intent.rs:1770,1776` (`atom:informedBy`).
- `atomic-cli/src/commands/vault/context.rs:470-473` (filters `node.kind == "memory"`).
- libatomic daemon copies: `services_agent.rs:131,3372-3387`, `services_query.rs:1001-1038,1059`.
- Wire: `libatomic/proto/.../knowledge_messages.proto:123` `KGEdge.relation` (free string).
- Out-of-repo: `../noname` (`atomic.ts:700,1928`, `kgHops.ts:202-245`, `BlastTriage.tsx`,
  `MemoryView.tsx`), `../the-hive/packages/shared/src/types/kg.ts:40-48`.
- ~110 test assertion sites (largest: `vault_triples.rs` ~40, `ontology.rs` ~31,
  `atomic-cli/tests/session_integration_test.rs` ~17).

### 2.8 Existing Semantic Web pieces and constraints

- `atomic-canonical` already emits signed JSON-LD with context
  `https://atomic.dev/ns/ctx.jsonld` mapping `atom:` → `https://atomic.dev/ns#`, and URNs
  `urn:atomic:{intent,memory,change,...}:`. Content hash = `blake3(JCS(node))`, which covers
  the `@context` **URL string** but not the term definitions behind it.
- `atomic provenance` projects PROV JSON-LD on demand (`atomic-canonical/src/prov.rs`);
  not written to the KG.
- **No RDF/OWL/SHACL crates** in the workspace; no `.ttl` files.
- **Standing decision (2026-07-06, `docs/shacl-oxirs-spike-findings.md`):** no SHACL engine
  ships in `atomic`; SHACL runs as a Circuit Breaker (CB) workflow; atomic keeps hand-coded
  gates and ships shapes `.ttl` as data. oxirs rejected (~432 crates, silently drops
  `sh:node`/`sh:sparql`). `oxttl` (~20 crates) noted as acceptable.
- Schema/upgrade machinery: `SCHEMA_VERSION` only rejects newer DBs (`schema.rs`);
  **capability fences** (`capability.rs`, `required-capability/<id>`) make older binaries
  fail closed; `kg_index_meta` holds lazy per-index markers (`fts_reverse_schema` = 2 is the
  precedent for a KG rebuild-on-write).

## 3. Target model for Phase 1

### 3.1 Namespaces

| Prefix | IRI | Role |
|---|---|---|
| `atomic:` | `https://atomic.dev/ns/workflow#` | the one workflow ontology |
| `prov:` | `http://www.w3.org/ns/prov#` | reused directly |
| `dcterms:` | `http://purl.org/dc/terms/` | `created`, `modified`, `description`, `subject` |
| `skos:` | `http://www.w3.org/2004/02/skos/core#` | `Concept`, `broader`, `narrower`, `related` |
| `rdf:`/`rdfs:`/`owl:`/`xsd:` | standard | |

Ontology IRI `https://atomic.dev/ns/workflow`, version IRI
`https://atomic.dev/ns/workflow/1.0.0`.

### 3.2 Node identity (replaces `kind:local` IDs)

| Kind | IRI | Notes |
|---|---|---|
| Project | `urn:atomic:project:<pid>` | `<pid>` from the project identity object (D5) |
| Change | `urn:atomic:change:blake3-256:<base32>` | full hash, canonical uppercase base32 per D4; kills `truncate_hash` |
| Provenance graph | `urn:atomic:provenance:blake3-256:<base32>` | |
| Attestation | `urn:atomic:attestation:blake3-256:<base32>` | hash of `.attest` object |
| Intent (incl. review) | `urn:atomic:project:<pid>:intent:<ulid>` | ULID lowercase canonical |
| Goal / Memory / Skill / ToolResult | `urn:atomic:project:<pid>:{goal,memory,skill,tool-result}:<encoded-name>` | |
| AC / Task / ScopeItem / Constraint | `urn:atomic:project:<pid>:intent:<ulid>:{ac,task,scope,constraint}:<id>` | qualified by parent intent |
| Session / Turn / Todo | `urn:atomic:project:<pid>:session:<sid>[:turn:<n> \| :todo:<id>]` | session-qualified todos only |
| View | `urn:atomic:project:<pid>:view:<pct-encoded>` | |
| File / Module | `urn:atomic:project:<pid>:{file,module}:<pct-encoded path>` | |
| Source entity | `urn:atomic:project:<pid>:entity:<pct path>#<kind>:<qualified-name>[~<ordinal>]` | per D10; file stays in the ID, line becomes a literal shown beside it |
| Identity | `did:atomic:…` when known; else `urn:atomic:project:<pid>:identity:<encoded-name>` + `atomic:Unknown`-style finding | stops display-name identity merging |
| Concept | `urn:atomic:project:<pid>:concept:<encoded-label>` typed `skos:Concept` | |
| Finding | `urn:atomic:project:<pid>:finding:<code>:<subject-hash>` | e.g. `MissingHistoricalPlanId` |

A single `atomic_core::kg::iri` module owns all minting and parsing (percent-encoding per
RFC 3987 path rules); producers never format IDs by hand. The FTS ranking prefix checks
(`txn/write/triples.rs:268-280`, duplicated in `read.rs`) switch to type lookups.

### 3.3 Edge mapping (complete replacement table)

Every row below is a test fixture for the coverage test (M1).

**Classes** — `entity_type::*` and `kind` values → `rdf:type`:

| Current | Canonical |
|---|---|
| `vault:Goal` / `goal` | `atomic:Goal` |
| `vault:Intent` / `intent` | `atomic:WorkIntent` (+ `FeatureIntent`/`BugIntent`/`ReviewIntent`/… from canonical `kind`; `ManagedIntent` from lifecycle status) |
| `vault:Memory` / `memory` | `atomic:Memory` |
| `vault:Skill` / `skill` | `atomic:Skill` |
| `vault:ToolResult` / `tool_result` | `atomic:ToolResult` |
| `vault:File` / `file` | `atomic:File` |
| `vault:Identity` / `identity` | `atomic:Identity` |
| `vault:Concept` / `concept` | `skos:Concept` |
| `vault:Session` / `session` | `atomic:Session` |
| `vault:SessionTurn` / `session_turn` | `atomic:SessionTurn` (+ `atomic:ManagedTurn` when `plan_id` present) |
| `vault:Todo` / `todo` | `atomic:Todo` |
| `vault:ProvenanceGraph` / `provenance` | `atomic:ProvenanceGraph` |
| `vault:Task` / `task` | `atomic:Task` |
| `vault:AcceptanceCriterion` / `acceptance_criterion` | `atomic:AcceptanceCriterion` |
| `vault:ScopeItem` / `scope` | `atomic:ScopeItem` |
| `vault:Constraint` / `constraint` | `atomic:Constraint` |
| `change` | `atomic:Change` |
| `view` | `atomic:View` |
| `module` | `atomic:Module` |
| `entity` | `atomic:SourceEntity` (language kind stays a literal `atomic:entityKind`) |
| *(new)* | `atomic:Attestation`, `atomic:Finding` |

**Predicates** — split by endpoint class where overloaded:

| Current | Endpoints | Canonical |
|---|---|---|
| `prov:hadMember` | session→turn | `atomic:hasTurn` (inverse `atomic:turnOfSession`) |
| `prov:hadPlan` | turn→intent | `atomic:hasPlan` (inverse `atomic:governsTurn`) |
| `prov:hadPlan` | session→intent | **dropped**; derivable as session `hasTurn`/`hasPlan` |
| `prov:generated` | turn→change | `atomic:generatedChange` ⊑ `prov:generated` (inverse `atomic:generatedBy`) |
| `prov:wasInformedBy` | turn→turn | `prov:wasInformedBy` (keep) |
| `prov:wasDerivedFrom` | memory→source; session→session (fork) | `prov:wasDerivedFrom` (keep; fork metadata stays edge-level) |
| `prov:used` | tool_result→file | `prov:used` (keep) |
| `prov:wasAssociatedWith` | tool_result→goal | `atomic:linkedGoal` |
| `atom:informedBy` | intent→memory/source | `atomic:informedBy` ⊑ `prov:wasDerivedFrom` |
| `vault:explainedBy` | turn→provenance | `atomic:explainedBy` |
| `vault:hasTodo` | turn→todo | `atomic:hasTodo` |
| `BLOCKS` / `BLOCKED_BY` | intent↔intent | `atomic:blocks` / `atomic:blockedBy` (inverses; emit one, infer the other) |
| `ASSIGNED_TO` | intent→identity | `atomic:assignedTo` |
| `LINKED_INTENT` | goal→intent | `atomic:linkedIntent` (target now manifest-resolved) |
| `LINKED_GOAL` | intent→goal | `atomic:linkedGoal` |
| `REFERENCES` | entry→memory, entry→file | `atomic:references` |
| `HAS_LABEL` | intent→concept | `atomic:hasLabel` |
| `HAS_TASK` | intent→task | `atomic:hasTask` |
| `HAS_ACCEPTANCE_CRITERION` | intent→AC | `atomic:hasAcceptanceCriterion` |
| `HAS_SCOPE_IN` / `HAS_SCOPE_OUT` | intent→scope | `atomic:hasScopeIn` / `atomic:hasScopeOut` ⊑ `atomic:hasScopeItem` |
| `HAS_CONSTRAINT` | intent→constraint | `atomic:hasConstraint` |
| `SATISFIES` | task→AC | `atomic:satisfies` |
| `TOUCHES` | task→file | `atomic:touches` |
| `SCOPE_OUT_FILE` | intent→file | `atomic:scopeOutFile` |
| `REMEDIATES` | intent→intent | `atomic:remediates` |
| `REVIEWS` | review intent→intent | `atomic:reviews` (inverse `atomic:reviewedBy`) |
| `DEPENDS_ON` | change→change | `atomic:directlyDependsOnChange` ⊑ `atomic:dependsOn` |
| `DEPENDS_ON` | intent→intent | `atomic:dependsOnIntent` ⊑ `atomic:dependsOn` |
| `MODIFIES` | change→file | `atomic:modifiesFile` ⊑ `atomic:modifies` |
| `MODIFIES` | change→entity | `atomic:modifiesEntity` ⊑ `atomic:modifies` (emitted on **both** bulk and record paths) |
| `ON_VIEW` | change→view | `atomic:directlyOnView` — **recomputed from `VIEW_CHANGES[view]` own entries**, not the working copy's desired view |
| `ON_VIEW` | session→view | `atomic:sessionViewContext` |
| `CHILD_OF` | view→view | `atomic:childViewOf` |
| `VISIBLE_FROM` | (never emitted) | `atomic:visibleFrom` — query-time operator only in Phase 1 |
| `DEFINES` | file→entity | `atomic:defines` |
| `EXPORTS` | file→entity | `atomic:exports` ⊑ `atomic:defines` |
| `INCLUDES` | file→file | `atomic:includes` |
| `IMPORTS` | (never emitted) | `atomic:imports` ⊑ `atomic:includes` (declared; extraction split deferred) |
| `CALLS` | entity→entity | `atomic:calls` |
| `PART_OF` | file→module / module→module | `atomic:filePartOfModule` / `atomic:modulePartOfModule` ⊑ `atomic:partOf` |
| `AUTHORED_BY` | change/goal/intent→identity | `atomic:authoredBy` ⊑ `prov:wasAttributedTo` (intents now emit it from `author`/`created_by`) |
| `vault:status`, `vault:priority` | literal | `atomic:status`, `atomic:priority` (datatype properties) |
| `vault:appliesTo` | entity→concept | `atomic:appliesTo` |
| `vault:blocks`… duplicate predicate constants | — | removed (uppercase twins already mapped above) |
| `dc:type` | — | removed (never emitted) |
| `dc:creator` | — | `atomic:authoredBy` |
| `dc:date` | — | `dcterms:created` / `dcterms:modified` from typed source fields |
| `dc:description` | — | `dcterms:description` |
| `skos:broader`/`narrower`/`related` | — | kept |
| `skos:subject` | — | removed; `dcterms:subject` |
| `rdf:type` | — | real type assertions (§3.4) |

**New Phase 1 edges** (no current equivalent):

| Edge | Source fact |
|---|---|
| `Attestation atomic:coversChange Change` | `Attestation.changes_covered` |
| `Attestation atomic:attests SessionTurn` | attestation `session_id` + covered change ∈ turn `change_hashes` (D8: exact once the turn is recorded; interim inference only when unique) |
| `Attestation atomic:signedBy Identity` | `Attestation.signer` (`did:atomic` or MAC signer) |
| `Attestation atomic:previousAttestation Attestation` | `previous_attestation` |
| `Attestation atomic:verificationResult atomic:{Verified,Unverifiable,Failed}` | asserted **only** by the verifier pass (never inferred) |
| `Change atomic:implements Intent` | inferred: `generatedBy ∘ hasPlan` |
| `Change atomic:explainedBy ProvenanceGraph` | inferred: `generatedBy ∘ explainedBy` |
| `Change atomic:attestedBy Attestation` | inferred: inverse of `coversChange` |
| `Finding atomic:aboutResource X`, `atomic:findingCode` | historical gaps (`MissingHistoricalPlanId`, `MissingAttestation`, …) |

### 3.4 Storage shape

Keep the property-graph tables (they already serve FTS, neighbors, and UI) but make them
RDF-faithful:

- `KgEdge.kind` → absolute predicate IRI. The write API takes a typed
  `Predicate` (ontology constant), not `&str`; unknown IRIs are unrepresentable on write and
  rejected on read.
- New field `KgEdge.graph: Asserted | Inferred` (serde default `Asserted`), with
  `metadata.derivation = { rule, ontology_version, premises: [edge keys] }` on inferred edges.
- **Types in a dedicated index**, not as `rdf:type` edges: new multimap tables
  `kg_types` (node IRI → class IRI) and `kg_types_rev` (class → node). Reason: storing
  classes as edge targets would make every node a 2-hop neighbor of every same-typed node
  under the kind-blind `kg_neighbors` BFS. RDF export emits them as `rdf:type` triples.
  Entailed superclasses are stored with `graph = Inferred`.
- `KgNode.kind` is **removed**; display category derives from the most specific type.
  `metadata.rdf_type` is removed. `KgNode.source` stays (it drives `kg_clear_derived`).
- `kg_index_meta["kg_generation"]` records `{ontology_version, projector_version,
  inference_profile, built_at}` per the RFC's "projection generation".
- Register new tables in `schema.rs::visit_tables`; retire nothing (same table names, new
  contents after rebuild).

## 4. Milestones

The RFC forbids a dual-write window, so M3, M4 and M7 land together on this branch and
merge as one release. M1, M2, M5 and M6 are additive and can be reviewed independently.

### M1 — Ontology artifacts and term registry (additive, no behavior change)

- `atomic-core/ontology/workflow-1.0.0.ttl`: OWL 2 RL ontology — classes (§3.3), object
  and datatype properties with domain/range, inverses, subproperty hierarchy, the two
  property chains, `atomic:dependsOn` as `owl:TransitiveProperty`, safe disjointness
  (`AIContribution`/`WorkIntent` deferred with Phase 2 classes), and `atomic:Execution` /
  `atomic:partOfTurn` declared but not projected (D11).
- `atomic-core/ontology/workflow-1.0.0-dl.ttl`: optional DL companion (can slip).
- Rewrite `atomic-core/src/pristine/ontology.rs` as `atomic_core::kg::ontology`:
  `Class` and `Predicate` enums with `iri()`, `curie()`, domain/range metadata, and a
  `PREFIXES` table for display compaction. Embed the `.ttl` via `include_str!`.
- `LEGACY_TERMS` table (test-only): every old constant → `Replacement::{One(term),
  SplitByEndpoint([(domain, range, term)]), Removed(reason)}`.
- Tests:
  - coverage: each of the 30 + 16 + 29 legacy constants appears exactly once in
    `LEGACY_TERMS`, and every `One`/`Split` target exists in the TTL;
  - TTL ↔ Rust consistency: every Rust term is declared in the TTL with matching
    domain/range and vice versa (parse with `oxttl`/`oxrdf` as **dev-dependencies only**);
  - no emitted string starts with `vault:`, `atom:`, or is SCREAMING_CASE (enforced again in
    M4 over a rebuilt fixture repo).
- Doc: update the AGENTS.md table section; amend the RFC to state that frozen historical
  document formats are read-only inputs, not a compatibility vocabulary (D3); publish
  dereferenceable docs at the namespace (ops task, tracked separately).

### M2 — Project identity and IRI minting

- `atomic-canonical` ctx v2 on `https://atomic.dev/ns/workflow#` with an object
  schema-version bump; v1 frozen for verification only; all new writes are v2 (D3).
- `atomic-canonical`: new `ProjectIdentity` node (`issuer` DID, `createdAt`, 256-bit random
  id, schema version, proof), gate rules, JCS hash. Project ID = base32 of the random id.
- Storage and transport per D5: its own pristine node type plus a
  `PROJECT_IDENTITY` table, carried by clone/push/pull as a repository-level object).
  `atomic init` mints it; upgrading an existing repo mints it on the first writable open by
  a signing identity, otherwise fails with `KgNeedsProjectIdentity` and an actionable message.
- `atomic project identity show|fork` (fork records `prov:wasDerivedFrom` origin). Rotation
  chain deferred, but the object schema reserves `previousIdentity`.
- Import collision check: same id + different verified object → reject.
- `atomic_core::kg::iri`: mint/parse for every row of §3.2; property tests for round trip and
  percent-encoding of paths with spaces, `#`, `:`, non-ASCII.
- Strict canonical base32 parser for content-addressed URNs (D4): rejects lowercase,
  padding, wrong length and nonzero trailing bits; tested against `Hash::to_base32`.

### M3 — Storage generation 2 (lands with M4/M7)

- `KgEdge.graph`, typed `Predicate` write API, `kg_types`/`kg_types_rev` tables, removal of
  `KgNode.kind`/`metadata.rdf_type`, read-side rejection of non-IRI kinds.
- Capability fence `kg-semweb-v1` in `capability.rs` so older binaries fail closed.
- `KG_GENERATION` constant plus `kg_index_meta["kg_generation"]`. The upgrade rebuild runs
  automatically on the first writable open and via `atomic query reindex --full`,
  regardless of repository size. The marker is written in the same transaction as the final
  rebuild batch, so the rebuild happens exactly once per generation (D6). The upgrade path
  clears all `kg_*` tables and runs M4's reconcile.
- `kg_neighbors` gains an optional predicate filter and direction; unfiltered behavior kept
  for `vault context`/RAG (they now filter by type, not `kind`).

### M4 — Projector rewrite and unified reconcile (lands with M3/M7)

- New module `atomic-repository/src/kg/` with one projector per family, all emitting via
  the typed API:
  - `vault.rs` (from `vault_triples.rs`): intents (canonical `kind` → intent subclass;
    lifecycle → `ManagedIntent`), memories, goals, skills, tool results, decomposition
    children, `:::ref` edges split by edge name, author edges for intents.
  - `vcs.rs` (from `vault_kg_enrich.rs`): views + `childViewOf`, `directlyOnView` from
    `VIEW_CHANGES`, changes (full hash), `directlyDependsOnChange`, `modifiesFile`,
    `authoredBy`, modules/files.
  - `ast.rs`: entities, `defines`/`exports`/`includes`/`calls`, `modifiesEntity` on both
    paths.
  - Scope-qualified entity IDs (D10): add `Entity.scope` (enclosing containers) to all
    seven `atomic-semantic` parsers plus normalized parameter types for overloading languages;
    update `query callers` display and `extract_path_from_id` to read node data; print
    `path:startLine-endLine` beside every entity ID; accept `path:line` and
    `path#kind:scope::name` as CLI entity arguments.
  - `session.rs`: move the vocabulary out of `atomic-core/.../session_kg.rs` (keep the
    record-time hook in core, but it calls the shared emitter), sessions, turns, todos,
    provenance graphs, `ManagedTurn` typing.
  - `attestation.rs` (**new**): load `.attest` objects, emit Attestation node and §3.3 edges;
    run signature verification (reuse `provenance_gate` verification + MAC provider) and
    assert `verificationResult`.
  - `findings.rs` (**new**): `MissingHistoricalPlanId`, `MissingProvenance`,
    `MissingAttestation`, `UnverifiableAttestation` — never guessed links.
- `reconcile(scope)` pipeline (RFC "Sync and enrichment contract"): ingest vault → discover
  VCS/session/provenance/attestation objects → project → verify → infer (M5) → validate
  (M6) → publish generation + stats. `vault sync`, `query enrich`, `query reindex`, clone,
  pull and import all call it (full or incremental scope). Record-time incremental
  enrichment calls the same projectors for the new objects.
- Determinism: projectors sort inputs; output digest = blake3 over sorted canonical N-Triples.

### M5 — OWL 2 RL inference (additive engine; wired in M4)

- `atomic_core::kg::reason`: forward-chaining to fixpoint over rules **generated from the
  TTL** (so the ontology is data): `rdfs:subClassOf`, `rdfs:subPropertyOf`,
  `owl:inverseOf`, `rdfs:domain`/`rdfs:range` typing, 2-step `owl:propertyChainAxiom`.
- Transitive properties (`atomic:dependsOn`, `atomic:ancestorView`) are **not**
  materialized; query operators compute closure from an explicit seed set.
- Every inferred edge/type carries a derivation record; inferred edges never overwrite an
  asserted edge with the same key (asserted wins, derivation is appended).
- `verificationResult` and any proof-validity term are excluded from rule heads.
- Tests: golden fixtures per rule; idempotence (re-running adds nothing); order-independence.

### M6 — Validation (SHACL as data + hand-coded checks)

Per D1:

- `atomic-core/ontology/shapes/projection-1.0.0.ttl` and `publication-1.0.0.ttl`
  (normative). Projection profile: shortcut agreement, referential existence, full-hash IDs,
  every generated change has `implements`/`explainedBy`/`attestedBy`. Publication profile:
  managed turn has exactly one `hasPlan`; managed intent governs ≥1 turn; turn has exactly
  one provenance graph; every generated change is covered by the session attestation;
  attestation has signer + verified proof.
- Rust `kg::validate` implements each shape rule (one function per `sh:` rule ID, as in
  `atomic-canonical/src/gate.rs`) over a small graph-access trait, and produces a report keyed
  by shape/path IRIs. If it grows into a general SHACL evaluator, extract it into a
  dedicated `atomic-core`-free crate that CB can consume or that can be published (D1).
- **Publication profile hooks into `provenance_gate.rs`** (the existing boundary for
  shared-view insert, triage promotion, push, Git export) behind policy
  `[provenance] require_plan = true` (default off). Absent `plan_id` fails publication.
- Projection failure → deterministic repair (re-run projectors for the offending subjects)
  → revalidate → if still failing, generation is marked non-conforming and query output says
  so.
- `atomic query export --format nquads|turtle [--inferred]` (hand-rolled serializer, no
  deps) so the CB workflow can run real SHACL; differential fixture corpus checked into
  `atomic-core/ontology/shapes/fixtures/` with expected conforms/violations for both the
  Rust checks and CB.

### M7 — Consumer cutover (lands with M3/M4)

- `query_plan.rs`: new steps `Relation { predicate, direction }`, `DirectViewMembers`,
  `ImplementingChanges`, `DependencyClosure { seeds }`, `TypeFilter`; `Filter` on `kind`
  becomes type-based; plan JSON schema version bump.
- CLI `query`: `callers` → `atomic:calls`; graph colour map keyed by IRI; DOT/text output
  compacts IRIs to CURIEs via `PREFIXES`; JSON emits absolute IRIs; `ask` prompt lists
  canonical predicates and IRI forms.
- Triage (`atomic-repository/src/triage/report.rs` `kind_is`, `repository/triage.rs`), `vault_intent.rs` informedBy checks,
  `vault/context.rs` memory filter (type `atomic:Memory`).
- libatomic daemon duplicates (`services_agent.rs`, `services_query.rs`); `rpc.rs`.
- Proto: document `KGEdge.relation` as an absolute IRI; add `types` repeated field to the node
  message; bump knowledge service API version.
- No compatibility shim for JSON/RPC consumers; the release is announced as breaking (D7).
- Update ~110 test assertion sites; add a lint test that greps the workspace for legacy
  kind literals outside `LEGACY_TERMS`.

## 5. Acceptance tests for Phase 1

Mapped to RFC acceptance criteria 1-4, 7, 8:

1. **Closure path (AC1):** integration test drives a managed agent turn with `plan_id` →
   record → attest; asserts the change reaches intent, provenance graph, attestation,
   session and turn via asserted edges, and that `implements`/`explainedBy`/`attestedBy`
   shortcuts exist and agree.
2. **Repair (AC2):** delete all `kg_*` contents → `reconcile --full` → graph digest equals
   the incrementally built graph's digest.
3. **Inference (AC3):** golden test for inverses, hierarchy and both property chains;
   rerun is a no-op.
4. **Validation (AC4):** fixtures for each shape rule (positive + negative); publication
   gate refuses push when `require_plan` and a managed turn lacks `plan_id`.
5. **Historical gaps (AC7):** pre-`plan_id` turn yields `MissingHistoricalPlanId` finding and
   **no** `implements` edge.
6. **Coverage (AC8):** M1 coverage test + workspace lint for legacy literals.
7. **Membership:** `directlyOnView` for a change inserted into `dev` from `feature` lists it on
   both views (both have it in `VIEW_CHANGES`) and never on the working copy's desired view
   unless it is a member.

## 6. Suggested order of work

1. M1 (ontology + registry + coverage test) — establishes the vocabulary everyone reviews.
2. M2 (project identity + IRIs) — unblocks every projector.
3. M5 (reasoner) in parallel with M3 (storage) — independent.
4. M4 (projectors + reconcile) on top of M2/M3/M5.
5. M6 (validation + export + gate).
6. M7 (consumers), then the end-to-end acceptance suite; merge M3+M4+M6+M7 as one release.

## 7. Risks

- **Rebuild cost on large repos** (AST + CALLS fan-out). Mitigation: D6, progress output,
  incremental reconcile by changed objects.
- **Edge-key growth** from absolute IRIs (~40-80 bytes per key). Acceptable for redb; can
  intern predicate IRIs to `u16` later if measured.
- **Attestation verification on receiving remotes**: MAC attestations are unverifiable
  without the key (per `provenance_gate`); they must project as `Unverifiable`, never
  `Verified`, and the projection profile must treat that as a reported state, not a repair loop.
- **Out-of-repo clients** break on release day by design (D7); the API version bump makes the failure loud.
- **`atom:` vs `atomic:` confusion** in reviews: write D3 (v1 frozen and read-only, v2 for new objects) into AGENTS.md.

## 8. Decisions and open questions

### 8.1 Resolved

**D1 — SHACL enforcement location: option (a).** Shapes ship as `.ttl` data and are the
normative source. Atomic implements hand-coded checks that mirror them and exports RDF for
the CB workflow. Implementation rule:
- Start with hand-coded checks in `kg::validate`, one function per shape rule (the
  `atomic-canonical/src/gate.rs` pattern).
- Write the checks against a small graph-access trait (subjects, objects, types,
  literals), not against redb or `KgNode` directly, so they can move later without a
  rewrite.
- **Extraction trigger:** if the checks start to look like a real SHACL implementation
  (generic `sh:` constraint dispatch, property-path evaluation, `sh:node`/`sh:sparql`
  semantics, a shapes loader), move them into a dedicated crate (working name
  `atomic-shacl`). That crate depends only on an RDF term model, never on `atomic-core`, so
  CB can pull it in, it can be relocated, or it can be published separately.

**D2 — OWL 2 RL implementation (RFC open question 1): option (a), written in-house.** Options considered:

| Option | What it is | Cons |
|---|---|---|
| **(a) In-house rule engine** (chosen) | Forward-chaining over rules generated from our TTL: subClassOf/subPropertyOf, inverseOf, domain/range, 2-step property chains | We own correctness and must test every rule. It covers only the RL rules we implement, so the TTL loader must **reject** unsupported axioms (fail closed, the oxirs lesson) rather than silently ignore them |
| (b) `reasonable` crate | OWL 2 RL reasoner (Brick Schema project), Datalog on `datafrog` + `oxrdf` | Latest release is a prerelease (`0.5.0-a1`) from a small research-driven project. It materializes the full RL rule set, which produces many trivial triples (`owl:Thing`, reflexive `sameAs`, …) we'd filter out. It works on its own in-memory graph, so every reconcile copies the KG in and out of redb. It keeps no per-triple derivation records (rule + premises), which the RFC requires |
| (c) Datalog library (`datafrog` 2.0, `ascent` 0.8, `crepe`) + our rules | We write the RL rules; the library evaluates them to a fixpoint | Fixpoint evaluation is the easy part, and we still write and test every rule. `ascent`/`crepe` compile rules from macros, so the ontology stops being data. Derivation tracking is still ours. Worth it only if (a) turns out too slow |
| (d) DL/EL reasoners (`rustdl` OWL 2 DL on `horned-owl`, `whelk-el` OWL 2 EL) | Description-logic classification | Wrong job: they classify the ontology's class hierarchy rather than deriving instance edges, so they don't scale to deriving `implements` across a large KG. The RFC deliberately keeps DL off the conformance path |
| (e) RDF store + rules (`oxigraph` 0.5) | SPARQL store; rules written as `CONSTRUCT` queries looped to a fixpoint | No OWL reasoning built in. A second database next to redb (two copies to keep in sync), a large dependency tree, and we still write the rules by hand |
| (f) External (Jena, RDFox, GraphDB, or inference inside CB like SHACL) | Mature reasoners outside the binary | JVM or service dependency. Local queries need the derived shortcuts (`implements`, `attestedBy`) in the local KG, so offline repos would lose them, and query correctness would depend on an external workflow |

Decision: (a). The TTL loader rejects any axiom outside the supported rule set. Option (c)
is the fallback only if profiling shows the fixpoint loop is a bottleneck.

**D3 — Canonical document namespace: option (b), applied narrowly.**
- New signed objects use a **ctx v2** built on the canonical `atomic:` workflow namespace
  (`https://atomic.dev/ns/workflow#`), under a bumped object schema version.
- Existing signed objects stay byte-for-byte unchanged.
- ctx v1 is **frozen** as a read-only historical input format. The KG projector translates
  it to canonical terms through a fixed, tested mapping, so `atom:` terms never reach KG
  storage, queries, APIs, or new objects.
- `https://atomic.dev/ns#` stays published with permanently fixed meaning (option (c) is
  ruled out).
- Why (b) over (a): the RFC gives `atomic:` sole ownership of workflow concepts, requires
  every producer to emit canonical terms, and rejects `atom:*` variants after migration.
  Byte stability protects only *existing* objects, not future ones. Both options read v1
  forever; the difference is that (a) keeps producing the legacy format, while narrow (b)
  closes it off.
- Consequences:
  - `atomic-canonical` gains ctx v2 plus a schema-version bump for Intent, Memory,
    delegation, PROV graph and `ProjectIdentity` objects.
  - The gate accepts v1 only to verify existing objects. All new writes are v2, including a
    revision of a v1 object (its `previousRevision` points at the v1 hash).
  - Work lands in M2 (documents) and M4 (the v1 → canonical mapping in the vault projector).
- **RFC clarification required** (M1 doc task): frozen historical document formats are
  read-only inputs, not a compatibility vocabulary.

**D4 — Content-hash URN encoding: base32, algorithm-qualified, exactly specified.**
`urn:atomic:change:blake3-256:<base32>`. The RFC doesn't choose an encoding, so this is
a judgment call.
- Why base32:
  - It's already used in `urn:atomic:change:`, so hex would add a second identity break on
    top of the full-hash migration.
  - It's shorter: 52 characters versus 64 for hex.
  - RFC 4648 base32 is a multibase encoding, which fits the RFC's "multihash-compatible" goal.
- Normative encoding (the RFC's acceptance criteria depend on byte-identical IDs across
  implementations):
  1. **Alphabet:** standard RFC 4648 §6 (`A–Z2–7`), not base32hex, Crockford, or a custom
     variant.
  2. **Case:** uppercase is canonical (confirmed: keep current behavior). This is exactly what `Hash::to_base32` emits today
     (`atomic-core/src/types/hash.rs:260`) and matches every existing change ID on disk and
     in the CLI. The multibase equivalent is prefix `B`.
  3. **No padding:** 52 characters, and the final 4 bits must be zero.
  4. **Strict parsing:** reject any non-canonical form rather than normalizing it. The
     existing `from_base32` is case-insensitive and doesn't check the trailing bits, so URN
     parsing gets a new strict parser. Lenient prefix lookup in the CLI is unaffected.
  5. A test proves `Hash::to_base32` output round-trips through the strict parser.
- Scope: applies to every content-addressed URN (change, provenance, attestation,
  project identity, file-version) and to `blake3-256:` evidence digests.
- Cost: standard BLAKE3 tools (`b3sum`) print hex, so external verifiers get a documented
  conversion step.

**D5 — Where the signed `ProjectId` lives: option (a), its own pristine node type.** The
first recommendation (a tracked `.vault/project/identity.jsonld` file) assumed snapshot
semantics. In atomic a `.vault/` file is content recorded *as a change* in the graph, so:
- its existence and bytes depend on which view you're on, because of the view filter;
- it can be unrecorded, edited by a later change, or put in conflict;
- it can diverge between views.

Project identity is a repository-level fact that must be the same on every view and
survive insert/unrecord. It also can't be derived from changes, because changes are global
content-addressed objects: the same change can belong to many projects, and nothing
guarantees a unique genesis change.

| Option | Cons |
|---|---|
| **(a) Dedicated content-addressed, signed object registered as its own pristine node type** (`node_type::PROJECT = 4`, next to CHANGE/TAG/ATTESTATION/PROVENANCE) (chosen). Bytes in the change store; a singleton pristine table `PROJECT_IDENTITY` holds the current hash plus the rotation chain. It is not in any view's `VIEW_CHANGES`, so no view filters, inserts or unrecords it | Needs a new transport object class. Today the push/pull contract excludes sidecars (`atomic-remote/src/sync.rs:476`), so clone/push/pull must learn to carry it, and the server should bind the identity hash to its `ProjectInfo.id` |
| (b) Metadata on the root view's `ViewState` | Still view-scoped: it travels and changes with one view, and other views can't verify it independently |
| (c) Server-assigned `ProjectInfo.id` (UUID) | Requires a remote, so local-only repos have no identity; it's not signed by the user and can't be verified offline |
| ~~(d) Tracked `.vault/` file~~ | Rejected: view-dependent, as explained above |

Under (a), fork is an explicit command that mints a new object with `prov:wasDerivedFrom`
pointing at the origin. The issuer DID binds the project to an identity now; org binding
comes later through delegation (`ResourceRef`).

**D6 — KG rebuild on upgrade: both triggers, regardless of repository size.** The rebuild
runs automatically on the first writable open, and `atomic query reindex --full` runs it
explicitly. It must happen **exactly once** per KG generation:
- A compiled `KG_GENERATION` constant. A rebuild runs only when the stored
  `kg_index_meta["kg_generation"]` is lower.
- The generation marker is written in the **same write transaction as the final rebuild
  batch**. An interrupted rebuild leaves no marker and restarts cleanly (clear, then
  rebuild) on the next writable open. It never leaves a half-marked KG.
- Repositories created by the new binary (`init`, `clone`) are stamped with the current
  generation at creation and never trigger an upgrade rebuild.
- Pull, import, incremental reconcile and `reindex --full` never lower or clear the marker;
  `--full` re-stamps the same generation.
- `KG_GENERATION` bumps only on breaking ontology or projector changes, not on every
  release. Routine upgrades don't rebuild.
- Read-only opens on an old-generation KG return `KgNeedsRebuild`, naming the command to run.
- Tests: a second open after the upgrade performs zero rebuild work; a rebuild killed
  midway completes on the next open; a fresh clone never rebuilds.

**D7 — External JSON/RPC clients:** this release is announced loudly as breaking, so there
is no compatibility shim and no coordination gate on `noname`/`the-hive`. The API version
bump stays, so stale clients fail loudly rather than misreading data.

**D8 — Attestation → turn mapping: option (b), with (a) as the interim.** Record the turn
number in the attestation format at capture time, because the RFC classes turn identity as
a capture-time fact. Until the format change lands, infer the turn only when exactly one
turn's `change_hashes` ⊆ `changes_covered`. Otherwise emit an `AmbiguousAttestationTurn`
finding and no `attests` edge.

**D9 — Domain separation: provided by project identity, used later by federation.**
Each atomic repository is its own domain; federation is how domains are kept apart in
cross-project search (e.g. `testing/hello-world` vs `production/hello-world`, and later
scoping by identity/org so a caller never searches someone else's projects). Content
hashes stay global: the same change bytes hash the same in every project, which insert,
pull and clone depend on.
- Phase 1 obligation: a durable, signed project identity (D5) that later phases can read,
  plus project-qualified IRIs for mutable resources (§3.2).
- Deferred to Phase 5: the catalog, named project graphs, explicit query scope and
  authorization that use that identity to keep projects separate in federated queries.
- No change-format or hash-construction change in Phase 1.

**D10 — Source-entity identity: option (b), scope-qualified IDs.** Where it comes from:
- The RFC's node model lists Entity as a "project-qualified **stable** entity URN" from
  the syntax/semantic index, and goal 6 requires portable identities.
- Today's entity IDs are `entity:{file}:{name}:{line}` (`vault_kg_enrich.rs:587`), so they
  contain the line number. Adding one line above a function changes its ID.
- As a result, `modifiesEntity` edges from older changes point at IDs that no longer
  exist, one function can end up with several IDs over time, and a full rebuild produces
  different IDs than incremental enrichment. That breaks the "full rebuild equals
  incremental" acceptance test and blame-style queries.
- The line number is also doing a hidden job: the extractor (`atomic-semantic`, `Entity`
  at `entity.rs:123`) emits methods by bare name (`fmt`, `find`) with no enclosing type, so
  `impl Display for User { fn fmt }` and `impl Display for Order { fn fmt }` in one file are
  told apart only by their lines.

**Decision: a scope-qualified name instead of the line.**

```text
urn:atomic:project:<pid>:entity:<pct-path>#<kind>:<scope>::<name>[(<param-types>)][~<n>]
```

| Part | Meaning |
|---|---|
| `<pct-path>` | file path, percent-encoded (as today) |
| `<kind>` | `EntityKind` (`function`, `method`, `class`, `interface`, `type-alias`, `const`, `enum`, `module`, …) |
| `<scope>::<name>` | the chain of enclosing containers plus the name, joined by `::` in every language, each segment percent-encoded |
| `(<param-types>)` | only in languages with overloading (Java, C++, Swift, TypeScript overload signatures): normalized parameter types from the signature |
| `~<n>` | last-resort ordinal, by order of appearance, when everything above still collides (e.g. two `#[cfg]` variants of the same `fn`) |

Line, end line, column and signature become literals on the node (`atomic:startLine`, …).

The file stays in the ID. The examples below show only the fragment after `#`; the full ID
for the `fmt` example is
`urn:atomic:project:<pid>:entity:src%2Flib.rs#method:<User%20as%20Display>::fmt`.

**Finding code still works, and line numbers stay visible.** Current consumers:

| Consumer | Where it gets the line today | Change needed |
|---|---|---|
| `query entities <file>` | parses the file live with tree-sitter; never reads KG IDs (`query/mod.rs:1292-1342`) | none |
| `query neighbors` / `search` text and JSON | node metadata `line`/`end_line` (`query/mod.rs:585, 648-665`) | none: the line stays on the node |
| `query callers` display | **parses the ID**, stripping the trailing `:line` (`query/mod.rs:1267-1277`) | format `path:line` from node data |
| Search ranking `extract_path_from_id` | **parses the ID** with `rsplitn(3, ':')` (`vault_triples.rs:629-645`) | read the path from node data |
| Agents following the code-intelligence skill | read the line straight out of the copied ID | every result line prints `path:startLine-endLine` beside the ID |

Display rule: anywhere an entity is shown to a person or an agent, print
`path:startLine-endLine` with the ID. That line is always current, because it is refreshed
on every enrichment, whereas a line embedded in an ID is frozen at the moment the ID was
copied.

Lookup by location: entity arguments in the CLI also accept `path:line` (for example
`atomic query neighbors src/lib.rs:55`) and the shorthand `path#kind:scope::name`. Both
resolve to the full IRI within the current project, so nobody has to type the URN.

Examples:

| Source | Today | Proposed fragment |
|---|---|---|
| `pub fn greet()` in `src/lib.rs` | `entity:src/lib.rs:greet:12` | `#function:greet` |
| `impl User { fn new() }` | `entity:src/lib.rs:new:40` | `#method:User::new` |
| `impl Display for User { fn fmt }` | `entity:src/lib.rs:fmt:55` | `#method:<User as Display>::fmt` |
| `mod tests { fn it_works() }` | `entity:src/lib.rs:it_works:90` | `#function:tests::it_works` |
| Python `class Repo: def get(self)` | `entity:repo.py:get:8` | `#method:Repo::get` |
| Java `void put(int)` / `void put(String)` | two line-based IDs | `#method:Cache::put(int)` / `#method:Cache::put(String)` |
| `#[cfg(unix)] fn open` / `#[cfg(windows)] fn open` | two line-based IDs | `#function:open` / `#function:open~2` |

How it behaves compared with line-based IDs:

| Edit | Line-based ID | Scope-qualified ID |
|---|---|---|
| Add or remove lines above, edit the body, reformat | changes | **stable** |
| Rename the function | changes | changes (a real semantic change; the semantic diff's `ChangeType::Renamed` can later link old → new with `atomic:renamedFrom`) |
| Move it to another file, or into/out of a type | changes | changes |
| Change parameter types (overloading languages only) | stable unless lines move | changes |

The payoff is that `modifiesEntity` edges from every change that ever touched a function
converge on one node, so "history of this function" becomes a direct query, and full
rebuild and incremental enrichment produce identical IDs.

Cost: each of the seven parsers in `atomic-semantic/src/parsers/` (rust, typescript,
python, go, java, cpp, swift) must record its enclosing scope chain (a new
`Entity.scope: Vec<String>` field) and, for overloading languages, normalized parameter
types. `CALLS` resolution stays name-based as today.

| Option | Cons |
|---|---|
| (a) Keep line-based IDs | Breaks rebuild determinism and history queries, as described |
| **(b) Scope-qualified IDs, as above** (chosen) | Extractor work in all seven parsers; the ordinal can shift if a new same-named duplicate appears earlier in the file (rare) |
| (c) CRDT `LeafId` / syntax-index stable IDs | Survives renames too, but depends on the semantic layer being finished (AGENTS.md: "in progress"), which would put Phase 1 behind it |

Work lands in M4 (extractor scope chains, ID minting, `callers` display, search ranking,
`path:line` and shorthand lookup).

**D11 — `Execution` nodes: option (a).** Declare `atomic:Execution` and `atomic:partOfTurn`
in the ontology only. Project nothing until capture records structured executions
(`ProvActivityInput.used` is always empty today).

All Phase 1 decisions are resolved; there are no open questions.
