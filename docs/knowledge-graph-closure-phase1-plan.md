# Phase 1 Implementation Plan: KG Referential Closure — Quick Win

> Status: **draft plan.** Implements Phase 1 ("Referential closure") of
> [`knowledge-graph-closure-rfc.md`](./knowledge-graph-closure-rfc.md).
> Branch: `feat/semweb-knowledge-graph`.

## Guiding principle for Phase 1

Ship **three artifacts** that every downstream phase depends on:

1. **`atomic:` namespace** — a canonical workflow ontology (`.ttl` + Rust registry).
2. **`ProjectId`** — a stable, unique project identity minted at `atomic init` and cast
   into IRIs so that future cross-project SPARQL queries have a canonical namespace per
   repository.
3. **Native SHACL engine** — parse and evaluate SHACL shapes against the property-graph
   KG. No hand-coded Rust validators. Enterprise users add custom shapes in
   `.atomic/shapes/*.ttl`; the engine evaluates them the same way it evaluates builtin
   shapes.

Everything else — projectors, storage cutover, inference, consumers — comes later.
This keeps Phase 1 reviewable while guaranteeing that IRIs are stable and validation
is real SHACL from day one.

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
| `query callers` KG-backed rewrite | Live-parsed implementation stays |

We keep **file-level** `atomic:modifiesFile` and `atomic:directlyOnView` because those
are workflow facts, not code-structure facts.

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
| `atomic:explainedBy` | `atomic:SessionTurn` | `atomic:ProvenanceGraph` | — | — |
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
| `atomic:generatedBy ∘ atomic:explainedBy` → `atomic:explainedBy` (on Change) | A change was generated by a turn explained by a provenance graph → the change is explained by that graph. |

These chains use `atomic:generatedBy` (the inverse of `atomic:generatedChange`), so the intermediate node is consistently `SessionTurn`. In Phase 1 the chains are **declarative only** — the ontology records the intended meaning so that enterprise SHACL shapes and documentation see a complete model. The OWL 2 RL reasoner in Phase 2 will materialize them from the TTL.

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

**Design goal:** a single stable identifier per repository that survives views,
inserts, unrecords, and clone/pull/push, and becomes the `<pid>` component in
project-qualified IRIs.

**Format:**
- 256-bit random ID generated at `atomic init` time.
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
- Clone / pull / push carry the project identity object as a repository-level
  sidecar (not a change). The receiving side stores it in its own `PROJECT_IDENTITY`
  table if the table is empty; if already populated, the clone keeps its own identity
  (a cloned project is a new project).
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
round-trip stability. Property tests cover paths with spaces, `#`, `:`, non-ASCII,
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

### 1.4 SHACL validation engine (`atomic_core::kg::shacl`)

**Decision:** SHACL is the primary validation mechanism. We do not write hand-coded
Rust checks that mirror SHACL rules. We parse SHACL shapes and evaluate them against
the property-graph KG.

**Dependencies:** `oxttl` (~20 crates) + `oxrdf` for parsing Turtle / RDF. These are
acceptable per the 2026-07-06 decision in `docs/shacl-oxirs-spike-findings.md`.
`oxirs` (~432 crates, silently drops `sh:node`/`sh:sparql`) is still rejected.

**What the engine supports (Phase 1):**

| SHACL feature | Status | Rationale |
|---|---|---|
| `sh:NodeShape`, `sh:PropertyShape` | ✓ | Core shape types |
| `sh:targetClass`, `sh:targetSubjectsOf`, `sh:targetObjectsOf` | ✓ | Target declaration |
| `sh:class`, `sh:datatype`, `sh:minCount`, `sh:maxCount` | ✓ | Cardinality and typing |
| `sh:minExclusive`, `sh:minInclusive`, `sh:maxExclusive`, `sh:maxInclusive` | ✓ | Range checks |
| `sh:minLength`, `sh:maxLength`, `sh:pattern` | ✓ | String constraints |
| `sh:hasValue`, `sh:in` | ✓ | Enumerated values |
| `sh:node` | ✗ | Defer; complex nesting |
| `sh:or`, `sh:and`, `sh:not` | ✓ | Boolean logic |
| `sh:sparql` | ✗ | Requires SPARQL; defer to federation phase |
| `sh:property` paths (`sh:path`) | ✓ | Predicate, inverse, sequence (limited) |

**Builtin shape graphs:**

- `atomic-core/ontology/shapes/projection-1.0.0.ttl` — projection profile:
  - every generated change has `atomic:implements`, `atomic:explainedBy`, and
    `atomic:attestedBy` shortcuts;
  - shortcut targets equal the authoritative intermediate path targets;
  - all referenced canonical nodes exist;
  - full hashes (not abbreviated labels) in durable IDs.

- `atomic-core/ontology/shapes/publication-1.0.0.ttl` — publication profile:
  - every managed `SessionTurn` has exactly one `atomic:hasPlan` intent;
  - no managed turn is governed by more than one intent;
  - every turn has exactly one provenance graph;
  - every generated change is covered by the session attestation;
  - every attestation has a signer DID and valid proof.

**Enterprise custom shapes:**

Repository directory `.atomic/shapes/*.ttl` is loaded automatically on `atomic query
validate` and on every `vault sync`. Failed shapes are reported in the CLI with
`sh:focusNode`, `sh:resultPath`, and `sh:resultMessage`.

Example enterprise shape:

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

**API surface:**

```rust
// atomic-core/src/kg/shacl.rs (new module)
pub struct ShaclValidator { ... }

impl ShaclValidator {
    /// Load a shape graph from a .ttl file.
    pub fn load_shapes(&mut self, ttl: &str) -> Result<(), ShaclError>;

    /// Validate the KG (property-graph read interface) against loaded shapes.
    pub fn validate(&self, kg: &dyn KgRead) -> ValidationReport;

    /// Validate a single focus node.
    pub fn validate_node(&self, kg: &dyn KgRead, node: &NodeId, shape: &ShapeId)
        -> Result<(), Vec<ConstraintViolation>>;
}

pub struct ValidationReport {
    pub conforms: bool,
    pub results: Vec<ConstraintViolation>,
}

pub struct ConstraintViolation {
    pub focus_node: String,     // IRI
    pub result_path: Option<String>,
    pub severity: Severity,     // sh:Violation, sh:Warning, sh:Info
    pub message: String,
    pub shape: String,
}
```

The validator operates over a small **graph-access trait**, not redb directly:

```rust
pub trait KgRead {
    fn subjects(&self) -> Box<dyn Iterator<Item = &str>>;
    fn types(&self, node: &str) -> Vec<&str>;
    fn objects(&self, subject: &str, predicate: &str) -> Vec<&str>;
    fn has_property(&self, subject: &str, predicate: &str, object: &str) -> bool;
    fn literal(&self, subject: &str, predicate: &str) -> Vec<Literal>;
}
```

This trait is implemented for the property-graph tables (`kg_nodes`, `kg_edges`)
and also for an in-memory `oxrdf::Dataset` during tests.

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

## 2. Out of scope for Phase 1

| Item | Deferred to |
|---|---|
| `KgEdge.graph` (`Asserted`/`Inferred`) | Phase 2 |
| `kg_types` / `kg_types_rev` tables | Phase 2 |
| Storage generation 2 / `KG_GENERATION` | Phase 2 |
| Projector rewrite (`vault.rs`, `vcs.rs`, `session.rs`, `attestation.rs`, `findings.rs`) | Phase 2 |
| `reconcile()` pipeline | Phase 2 |
| OWL 2 RL inference engine (`kg::reason`) | Phase 2 |
| `sh:sparql` and advanced path expressions | Federation phase |
| Consumer cutover (`query`, triage, daemon, proto) | Phase 2 |
| Code-structure edges (`calls`, `defines`, `exports`, `includes`, `modifiesEntity`) | When sourced from AST |
| Scope-qualified entity IDs | Same — when code-structure edges are needed |
| `sh:node` (shape nesting) | Phase 2 |
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
7. **SHACL engine:** `projection-1.0.0.ttl` and `publication-1.0.0.ttl` parse and
   evaluate against a test property graph without error.
8. **Custom shapes:** placing a `.ttl` in `.atomic/shapes/` causes `atomic query validate`
   to load and evaluate it.
9. **Conformance report:** `ValidationReport` produces `sh:focusNode`, `sh:resultPath`,
   `sh:resultMessage` for every violation.

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
- `atomic init` mints on creation; upgrading repos mint on first writable open
- Minimal clone/pull/push transport (repository-level sidecar)
- CLI: `atomic project identity show`
- Property tests: round-trip, percent-encoding, strict base32 parsing

### M3 — SHACL engine + builtin shapes (one PR, depends on M1)

- `oxttl` + `oxrdf` dependency
- `atomic_core::kg::shacl` module with `ShaclValidator`, `KgRead` trait,
  `ValidationReport`, `ConstraintViolation`
- `projection-1.0.0.ttl` and `publication-1.0.0.ttl`
- In-memory `oxrdf::Dataset` adapter for `KgRead` (testing)
- CLI: `atomic query validate`
- Custom shape loader from `.atomic/shapes/*.ttl`
- Test: a synthetic property graph passes/fails each builtin shape as expected

## 5. Development notes

### Why not oxirs?

`oxirs` (~432 transitive deps) silently drops `sh:node` and `sh:sparql`. We write
the subset we need (~800–1200 lines) against `oxrdf`/`oxttl`, grow it only when a
shape requires it, and keep the graph-access trait small enough to extract later
into a standalone crate if the engine becomes general-purpose.

### Why `KgRead` trait instead of redb directly?

The trait abstracts over property-graph tables, `oxrdf::Dataset`, and any future
RDF store. Validation tests run against an in-memory dataset without a database.
If the engine ever needs to move into a standalone crate (e.g. for the Circuit
Breaker workflow), the trait goes with it and redb does not.

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
- rewrite projectors to emit project-qualified IRIs using `kg::iri`;
- implement `kg_types`/`kg_types_rev` for type indexing;
- add the OWL 2 RL inference engine;
- wire SHACL validation into `vault sync` and publication gates;
- cut over consumers to use canonical IRIs.

Because Phase 1 already minted `ProjectId`, Phase 2 projectors can use it
immediately without a second migration or ID rewrite.

---

## Appendix: Why we skip code-block mapping now

The existing `vault_kg_enrich.rs` pipeline extracts `CALLS`, `DEFINES`, `EXPORTS`,
`INCLUDES`, `PART_OF`, and `MODIFIES` (entity) by walking source files and emitting
KG edges. This duplicates `atomic-semantic`'s syntax index, produces stale line-based
entity IDs, and generates millions of mostly-unqueried edges. The right time to
materialize code-structure edges in the KG is when we need them for federated
queries that the syntax index cannot answer. Until then, `atomic:File` stays as a
workflow artifact and Tree-Sitter handles everything inside it.
