# RFC: End-to-End Intent, Provenance, Attestation, Change, and Resource Closure

> Status: **draft / proposal.** This RFC extends
> [`provenance-graph-plan.md`](./provenance-graph-plan.md) and
> [`vault-sync-design.md`](./vault-sync-design.md) with an enforceable projection
> contract for the project knowledge graph.

## Summary

Atomic SHALL expose a complete, queryable path from the work item that authorized a
change to the resources and executions evidenced by that change:

```text
Intent → Session → Turn → Provenance → Change → File/Entity
                       ↘ Attestation ↗          ↘ Execution evidence
```

The content-addressed change, session ledger, provenance graph, attestation, and
canonical vault objects remain authoritative. The knowledge graph (KG) is a
deterministic, rebuildable read model over those objects. Recording must capture facts
that cannot be inferred later; enrichment must project every captured fact and repair
missing derived nodes and edges.

The target user experience includes questions such as:

- What intent introduced a migration or execution that deletes a database table?
- Which model and session produced changes implementing this bug intent?
- Which approved review covered the exact closure promoted into `main`?
- What did the dependency-closed implementation of this feature cost?
- Which migration or execution evidence has no independently attested review?

## Motivation

Atomic already persists most of the required facts, but they are split across several
surfaces:

- Vault entries describe intents, acceptance criteria, tasks, goals, and memories.
- `SessionTurn` records contain `plan_id`, `goal`, `provenance_hash`, and
  `change_hashes`.
- Changes contain authors, dependencies, graph/file operations, and inline AI
  provenance such as model, tokens, cost, and session identifiers.
- Provenance sidecars contain the causal decision ledger.
- Attestations identify sessions, agents, covered changes, and cryptographic proof.
- CRDT and syntax layers can describe human-readable code operations.

Partial enrichment makes these facts individually visible without guaranteeing a
continuous path between them. Generic neighborhood traversal also cannot distinguish
membership from incidental reachability. A complete projection contract is needed
before KG queries can be treated as audit evidence.

## Goals

1. Provide a continuous graph path from an intent to every implementing change, affected
   resource, and recorded execution.
2. Preserve intermediate session, turn, provenance, attestation, review, and change
   nodes rather than replacing them with opaque metadata blobs.
3. Make full enrichment idempotently reconstruct the same KG from canonical objects.
4. Validate projection completeness and consistency with SHACL.
5. Support direct membership and dependency-closure queries without relying on graph
   depth as a proxy for semantics.
6. Remain portable: use hashes, URNs, stable vault IDs, and paths—not repository-local
   `NodeId` values.

## Non-goals

- Treating the KG as the authoritative store.
- Guessing an intent for historical changes that did not record one.
- Persisting domain-specific source operations or inferred effects in the project KG.
- Requiring AI-specific fields for human-authored changes.

## Authoritative facts versus derived facts

### Must be captured at record/publication time

These claims cannot be reconstructed reliably and MUST be persisted before publication:

- governing intent/plan ID;
- session and turn identity;
- exact generated change hashes;
- provenance graph hash;
- exact attestation coverage and signer;
- reviewed change-set digest and verdict;
- model/provider response metadata when AI participated.

If policy requires a governing intent, publication MUST fail when `plan_id` is absent.
The system must not infer authorization from a nearby timestamp, matching view name, or
similar commit message.

### Deterministically derived during enrichment

The following are rebuildable read-model facts:

- direct `change → intent` and `change → provenance` convenience edges;
- dependency-closed intent implementation sets;
- file/entity links derived from recorded changes;
- generic execution and artifact-role links explicitly present in provenance;
- aggregate token and cost values;
- stale-review findings derived from reviewed and current closure digests.

## Canonical node model

| Node kind | Portable identity | Authority |
|---|---|---|
| Intent | `urn:atomic:project:<project-id>:intent:<uid>` | Canonical vault intent |
| Goal | `urn:atomic:project:<project-id>:goal:<uid>` | Canonical vault goal |
| Session | `urn:atomic:project:<project-id>:session:<id>` | Session record |
| Session turn | `urn:atomic:project:<project-id>:session:<id>:turn:<n>` | Session turn record |
| Provenance graph | `urn:atomic:provenance:blake3-256:<hash>` | Content-addressed sidecar |
| Attestation | `urn:atomic:attestation:blake3-256:<hash>` | Signed attestation object |
| Review intent | `urn:atomic:project:<project-id>:intent:<uid>` | Canonical review intent |
| Change | `urn:atomic:change:blake3-256:<hash>` | Content-addressed change |
| File | `urn:atomic:project:<project-id>:file:<encoded-path>` | Repository tree/semantic trunk |
| Entity | project-qualified stable entity URN | Syntax/semantic index |
| Execution | `urn:atomic:project:<project-id>:execution:<provenance-node-id>` | Recorded provenance activity |
| View | `urn:atomic:project:<project-id>:view:<encoded-name>` | View state |

Full hashes are canonical. Abbreviated hashes are display labels only and MUST NOT be
used as durable identities.

## Required edge model

```text
Intent       ──atomic:governsTurn──────────────▶ SessionTurn
Session      ──atomic:hasTurn──────────────────▶ SessionTurn
SessionTurn  ──atomic:hasPlan──────────────────▶ Intent
SessionTurn  ──atomic:explainedBy──────────────▶ ProvenanceGraph
SessionTurn  ──atomic:generatedChange──────────▶ Change
Attestation  ──atomic:attests──────────────────▶ SessionTurn
Attestation  ──atomic:coversChange─────────────▶ Change
Change       ──atomic:implements───────────────▶ Intent
Change       ──atomic:explainedBy──────────────▶ ProvenanceGraph
Change       ──atomic:attestedBy───────────────▶ Attestation
Change       ──atomic:directlyDependsOnChange─▶ Change
Change       ──atomic:directlyOnView───────────▶ View
Change       ──atomic:modifiesFile─────────────▶ File
Change       ──atomic:modifiesEntity───────────▶ Entity
Execution    ──prov:used───────────────────────▶ File/Entity
Execution    ──atomic:partOfTurn────────────────▶ SessionTurn
ReviewIntent ──atomic:reviews──────────────────▶ Intent
ReviewIntent ──atomic:reviewsChange────────────▶ Change
ReviewIntent ──atomic:reviewsClosure───────────▶ ChangeSetDigest
```

`atomic:implements`, direct `atomic:explainedBy`, and direct `atomic:attestedBy` are derived convenience
edges. Their source intermediate nodes remain present, and validation ensures the
shortcuts agree with the authoritative path.

## Membership is not neighborhood

A graph depth is never a membership predicate. Commands and query plans SHOULD expose
named relations or selectors:

- direct changes recorded on a view: `change atomic:directlyOnView view`;
- effective changes visible in a view: `atomic:visibleFrom` or a view-closure operator;
- implementation changes for an intent: `change atomic:implements intent`;
- direct dependencies: `atomic:directlyDependsOnChange`;
- dependency closure: transitive `atomic:dependsOn` from an explicitly selected seed set.

This prevents a depth-two query from counting dependencies, parent-view changes, or
changes connected through a session as if all belonged directly to the selected view.

## AI provenance and attestation projection

Each AI contribution SHOULD be represented as a provenance contribution node rather
than only a JSON field:

```text
Change ──atomic:hasContribution──▶ AIContribution
AIContribution ──atomic:usedModel──▶ Model
AIContribution ──atomic:inSession──▶ Session
```

An `AIContribution` carries typed literals for vendor, model version, tool, suggestion
type, input/output/total tokens, cost and currency, request ID, timestamp, agent mode,
finish reason, and step count. Large reasoning and transcript content remains in
content-addressed objects referenced by hash; it is not duplicated into every KG node.

Change-level `cost` and token totals are materialized aggregates for query convenience.
They MUST equal the sum of linked contributions and MUST not include dependency costs
unless a closure query explicitly requests them.

Attestations remain distinct from AI contribution metadata. An attestation represents a
signed claim and includes signer DID, proof, session, covered changes, prior attestation,
and verification status.

## Evidence-grounded semantic queries with `query ask`

The persistent KG stops at durable, generic facts: an intent governed a turn, the turn
generated a change, the change modified a file/entity, and provenance recorded an
execution that used an artifact. It does not create a node for each SQL statement, API
change, security interpretation, or inferred effect.

Natural-language domain questions are answered by `atomic vault query ask`. The LLM has
two constrained roles—planning and interpretation—and no graph-write authority.

### Runtime contract

For a question such as:

```text
What intent introduced a migration that deletes a database table?
```

`query ask` executes this bounded loop:

1. **Build a result shape.** The model translates the question into required structural
   bindings and semantic fields, including intent, change, file/execution, exact source
   version, evidence range, interpretation, and uncertainty.
2. **Build a constrained query plan.** The model selects allow-listed operators such as
   intent/change traversal, direct view membership, dependency closure, changed-hunk
   retrieval, content search, and execution lookup. It cannot submit arbitrary storage
   commands or mutate the graph.
3. **Execute structure deterministically.** Atomic executes the plan against canonical
   KG relations and returns bounded candidate changes, files, hunks, and executions.
4. **Retrieve exact evidence.** Atomic loads the content-addressed version associated
   with each change—not merely the current working copy—and includes stable byte/line
   ranges and whether text was added, removed, or executed.
5. **Interpret bounded evidence.** The LLM fills the result shape using only supplied
   candidates. Deterministic parsers may be exposed as optional tools, but are not
   required to define persistent ontology terms.
6. **Validate the envelope.** Atomic verifies every returned IRI/hash exists, every file
   is linked to the cited change, every range exists in the cited object, every quoted
   string matches evidence, and every intent/change/provenance edge is present.
7. **Return ephemeral semantics.** Results are labeled `model_interpretation`, include
   model/provider, prompt/query-plan hash, evidence hashes, source graphs, confidence or
   abstention, and completeness/partial-scope status. They do not enter OWL inference or
   SHACL conformance.

The model-produced plan is a versioned typed AST validated against a closed JSON Schema
before authorization or execution. Unknown fields, operators, relation IRIs, unbounded
limits, raw commands, raw storage keys, arbitrary SPARQL, and unsupported schema versions
are rejected. Each operator has typed parameters and hard server-side cardinality, byte,
time, token, cost, and recursion limits that model output cannot raise. Authorization is
computed over the validated plan and every expansion. The executor never interpolates
model strings into shell, SQL, RDF-store, or filesystem commands.

The model may request another bounded retrieval round through the same schema when
evidence is insufficient. The planner reports truncation and denied expansions; it does
not silently claim completeness.

### Result shape

A representative result is:

```json
{
  "status": "model_interpretation",
  "verified": false,
  "intent": "urn:atomic:project:...:intent:...",
  "change": "urn:atomic:change:blake3-256:...",
  "file": "urn:atomic:project:...:file:migrations/042.sql",
  "evidence_object": "urn:atomic:file-version:blake3-256:...",
  "evidence": {
    "side": "post-change",
    "change_role": "added",
    "coordinate_unit": "utf8-byte",
    "interval": "half-open",
    "start": 142,
    "end": 166,
    "bytes_digest": "blake3-256:...",
    "text": "DROP TABLE public.users;"
  },
  "interpretation": {
    "operation": "drop-table",
    "resource": "public.users"
  },
  "model": "...",
  "query_plan_hash": "...",
  "confidence": 0.94
}
```

Evidence citations identify the canonical evidence-object type and hash, pre-change or
post-change side (or execution-record object), hunk role, UTF-8 byte coordinates using a
half-open `[start, end)` interval, and a domain-separated digest of the exact cited bytes.
Line numbers may be displayed but are derived and non-canonical. Removed and replaced
text cites the pre-change object; added text cites the post-change object. Execution
evidence cites the immutable execution record plus its payload span under the record's
canonical encoding.

`verified: false` means the semantic interpretation is not a canonical fact. Machine
validation recomputes the cited byte digest and proves the evidence and structural path
exist; it does not prove the model interpreted them correctly. Model confidence never upgrades epistemic status.

### Virtual result graph

Structured results may be exposed as a request-scoped virtual graph under the
`atomic-query:` namespace so SPARQL/structured-plan consumers can join them with the
canonical KG. This graph disappears with the query envelope and is excluded from OWL and
SHACL inputs.

### Explicit promotion

If an interpretation must become durable, a user or policy explicitly promotes the
result into an attested finding, review observation, remediation intent, or memory. The
durable object references the query-plan hash and exact evidence; it records who accepted
the interpretation. Promotion never rewrites the original answer into an objective code
fact, and there is no automatic promotion based on confidence.

### Example traversal

```text
atomic vault query ask "What intent introduced a migration that deletes a table?"
  → plan: intent ← implements ← change → modifiesFile → exact changed hunks
  → optional: execution → prov:used → file
  → LLM: interpret only bounded evidence
  → validate citations and structural bindings
  → return ephemeral shaped answer
```

This preserves provenance's purpose: Atomic supplies the factual chain and exact lines;
the LLM supplies question-specific semantics with explicit uncertainty.

## Example query: cost of changes directly in a view

```text
view(name)
  → inverse atomic:directlyOnView
  → changes
  → sum(change.cost)
```

For effective visible cost, callers explicitly request dependency and ancestor closure:

```text
view(name)
  → visible-change closure
  → distinct changes
  → sum(change.cost)
```

The query engine MUST deduplicate changes by full hash before aggregation.

## Review linkage

Feature and bug intents MAY be governed by a policy requiring an approved review intent
before completion. A review records:

- reviewed intent;
- exact dependency-closed change-set digest;
- each reviewed change;
- verdict;
- reviewer identity and attestation;
- remediation intents/findings;
- timestamp and policy version.

Adding or replacing an implementation change changes the closure digest and makes the
review stale. A stale review cannot satisfy the completion shape.

## OWL ontology and inference

SHACL alone describes whether a concrete graph satisfies operational requirements. It
does not provide the domain theory needed to classify resources, infer inverse
relationships, or derive a change-to-intent path from authoritative intermediate facts.
Atomic SHALL therefore publish this model as a versioned OWL ontology and use SHACL as
the complementary validation layer.

### Reasoning profile

The normative ontology SHOULD remain within **OWL 2 RL** so enrichment and query systems
can implement deterministic rule-based materialization without a heavyweight tableau
reasoner. A complete OWL 2 DL representation MAY also be published for external tools,
but the Atomic conformance path depends only on the declared RL subset.

The ontology IRI and version IRI are immutable, for example:

```turtle
atomic:provenance-closure
    a owl:Ontology ;
    owl:versionIRI atomic:provenance-closure/1.0.0 ;
    owl:imports <http://www.w3.org/ns/prov-o#> ,
                <https://w3id.org/security/data-integrity/v2> .
```

A KG projection records the ontology version, projector version, and inference profile
used to produce it. Ontology upgrades create a new projection generation; they do not
change canonical historical objects.

### Namespace ownership

The canonical `atomic:` namespace is the **Atomic workflow ontology**. Its permanent base
IRI is `https://atomic.dev/ns/workflow#` and SHALL be dereferenceable with content
negotiation for human-readable documentation and canonical RDF serializations. It owns every concept whose meaning comes from
the Atomic workflow, including Intent, Change, Memory, Attestation, Review, View, Session,
Turn, acceptance criteria, tasks, scopes, constraints, and their workflow-specific
relations.

PROV-O is the shared crossover vocabulary. Atomic reuses `prov:Activity`, `prov:Entity`,
`prov:Agent`, `prov:used`, `prov:generated`, `prov:wasGeneratedBy`,
`prov:wasDerivedFrom`, `prov:wasInformedBy`, and `prov:wasAssociatedWith` wherever their
standard semantics fit. Atomic does not mint equivalent workflow-prefixed copies of PROV
terms. A narrower Atomic property is introduced only when the workflow relation is more
specific than PROV-O—for example, `atomic:generatedChange rdfs:subPropertyOf
prov:generated`—and its narrower domain/range is explicit.

This gives cross-project and external tools one shared provenance bridge while keeping
all Atomic-specific lifecycle semantics in one coherent workflow namespace. Other
external vocabularies are reused only when their semantics match exactly; they do not
split Atomic workflow ownership into separate intent, change, memory, or attestation
namespaces.

### Core classes

```turtle
atomic:WorkIntent          a owl:Class ; rdfs:subClassOf prov:Entity .
atomic:ManagedIntent       a owl:Class ; rdfs:subClassOf atomic:WorkIntent .
atomic:FeatureIntent       a owl:Class ; rdfs:subClassOf atomic:ManagedIntent .
atomic:BugIntent           a owl:Class ; rdfs:subClassOf atomic:ManagedIntent .
atomic:ReviewIntent        a owl:Class ; rdfs:subClassOf atomic:ManagedIntent .
atomic:Session             a owl:Class ; rdfs:subClassOf prov:Activity .
atomic:SessionTurn         a owl:Class ; rdfs:subClassOf prov:Activity .
atomic:ManagedTurn         a owl:Class ; rdfs:subClassOf atomic:SessionTurn .
atomic:Goal                a owl:Class ; rdfs:subClassOf prov:Entity .
atomic:Memory              a owl:Class ; rdfs:subClassOf prov:Entity .
atomic:Identity            a owl:Class ; rdfs:subClassOf prov:Agent .
atomic:File                a owl:Class ; rdfs:subClassOf prov:Entity .
atomic:Change              a owl:Class ; rdfs:subClassOf prov:Entity .
atomic:ProvenanceGraph     a owl:Class ; rdfs:subClassOf prov:Entity .
atomic:Attestation         a owl:Class ; rdfs:subClassOf prov:Entity .
atomic:AIContribution      a owl:Class ; rdfs:subClassOf prov:Entity .
atomic:Execution           a owl:Class ; rdfs:subClassOf prov:Activity .
```

The ontology SHOULD declare clearly impossible overlaps as disjoint, but only where the
claim is stable across future evolution. For example, `AIContribution` and
`WorkIntent` may be disjoint. `FeatureIntent`, `BugIntent`, and `ReviewIntent` SHOULD
not initially be pairwise disjoint because an imported policy may legitimately assign
multiple roles to one intent.

### Object properties

```turtle
atomic:hasPlan a owl:ObjectProperty ;
    rdfs:domain atomic:SessionTurn ;
    rdfs:range atomic:WorkIntent ;
    owl:inverseOf atomic:governsTurn .

atomic:hasTurn a owl:ObjectProperty ;
    rdfs:domain atomic:Session ;
    rdfs:range atomic:SessionTurn ;
    owl:inverseOf atomic:turnOfSession .

atomic:ManagedIntent rdfs:subClassOf
    [ a owl:Restriction ;
      owl:onProperty atomic:governsTurn ;
      owl:minQualifiedCardinality "1"^^xsd:nonNegativeInteger ;
      owl:onClass atomic:SessionTurn ] .

atomic:ManagedTurn rdfs:subClassOf
    [ a owl:Restriction ;
      owl:onProperty atomic:hasPlan ;
      owl:qualifiedCardinality "1"^^xsd:nonNegativeInteger ;
      owl:onClass atomic:WorkIntent ] .

atomic:generatedChange a owl:ObjectProperty ;
    rdfs:subPropertyOf prov:generated ;
    rdfs:domain atomic:SessionTurn ;
    rdfs:range atomic:Change ;
    owl:inverseOf atomic:generatedBy .

atomic:implements a owl:ObjectProperty ;
    rdfs:domain atomic:Change ;
    rdfs:range atomic:WorkIntent ;
    owl:inverseOf atomic:implementedBy .

atomic:modifiesFile a owl:ObjectProperty ;
    rdfs:domain atomic:Change ;
    rdfs:range atomic:File ;
    rdfs:subPropertyOf atomic:modifies .

atomic:modifiesEntity a owl:ObjectProperty ;
    rdfs:domain atomic:Change ;
    rdfs:range atomic:SourceEntity ;
    rdfs:subPropertyOf atomic:modifies .

atomic:partOfTurn a owl:ObjectProperty ;
    rdfs:domain atomic:Execution ;
    rdfs:range atomic:SessionTurn .

atomic:authoredBy a owl:ObjectProperty ;
    rdfs:subPropertyOf prov:wasAttributedTo ;
    rdfs:domain prov:Entity ;
    rdfs:range atomic:Identity .

atomic:attestedBy a owl:ObjectProperty ;
    rdfs:domain atomic:Change ;
    rdfs:range atomic:Attestation ;
    owl:inverseOf atomic:coversChange .

atomic:reviews a owl:ObjectProperty ;
    rdfs:domain atomic:ReviewIntent ;
    rdfs:range atomic:WorkIntent ;
    owl:inverseOf atomic:reviewedBy .
```

The workflow cardinality is **one managed Intent → one or more Turns**, and every managed
Turn → exactly one governing Intent. A turn may generate multiple changes. A backlog
intent may have zero turns before work starts; transition to a managed/active lifecycle
state activates the minimum-one-turn SHACL gate. OWL describes this meaning under the
open-world assumption, while SHACL enforces it on concrete lifecycle transitions.

`dependsOn` MAY be declared transitive for semantic queries, but direct dependency edges
must remain distinguishable. The implementation SHOULD use domain-specific direct properties and an inferred broad
`dependsOn` property rather than materializing transitive closure into the authoritative
edge family:

```turtle
atomic:directlyDependsOnChange rdfs:subPropertyOf atomic:dependsOn .
atomic:dependsOnIntent rdfs:subPropertyOf atomic:dependsOn .
atomic:dependsOn a owl:TransitiveProperty .
```

### Property-chain axioms

OWL property chains derive query-friendly shortcuts from authoritative paths. Given:

```text
SessionTurn ──generatedChange──▶ Change
SessionTurn ──hasPlan──────────▶ Intent
```

The OWL 2 DL companion can express the inverse traversal directly. The normative RL
projection instead emits the explicit inverse `atomic:generatedBy` and infers
`Change implements Intent` with a regular property chain:

```turtle
atomic:generatedBy owl:inverseOf atomic:generatedChange .
atomic:implements owl:propertyChainAxiom (
    atomic:generatedBy
    atomic:hasPlan
) .
```

Attestation and provenance shortcuts MAY be inferred through equivalent chains when the
intermediate relationship is unambiguous. Property chains MUST NOT manufacture
cryptographic validity: `verifiedAttestation` is asserted only by signature
verification, never inferred merely because a proof node exists.

### Datatype properties

Token counts use non-negative integers; money uses exact decimal values plus an explicit
currency rather than binary floating point:

```turtle
atomic:inputTokens a owl:DatatypeProperty ; rdfs:range xsd:nonNegativeInteger .
atomic:outputTokens a owl:DatatypeProperty ; rdfs:range xsd:nonNegativeInteger .
atomic:totalTokens a owl:DatatypeProperty ; rdfs:range xsd:nonNegativeInteger .
atomic:costAmount a owl:DatatypeProperty ; rdfs:range xsd:decimal .
atomic:currency a owl:DatatypeProperty ; rdfs:range xsd:string .
```

OWL range declarations infer types; they do not reject malformed literals. SHACL remains
responsible for datatype, bounds, cardinality, and currency constraints.

### Current KG replacement and coverage matrix

This matrix is normative for a **breaking Semantic Web-native cutover**. It covers the
terms currently defined in `atomic-core/src/pristine/ontology.rs` and the relations
emitted by vault, VCS, AST, and session-ledger enrichment. Current strings are inventory
inputs, not supported aliases. Every producer, stored edge, query, and consumer SHALL be
rewritten to the canonical RDF/OWL term; old spellings are rejected after migration.

Atomic intentionally provides no `atomic-legacy` ontology, `owl:equivalentProperty`
bridge, uppercase compatibility API, or dual-write period. Repository format and query
API versions advance atomically with this change.

#### Existing class coverage

| Current representation | Observed resources | Required canonical replacement |
|---|---|---|
| `vault:Goal`, `kind=goal` | `goal:*` | Replace with `rdf:type atomic:Goal`; remove class string and kind-as-type semantics |
| `vault:Intent`, `kind=intent` | `intent:*` | Replace with `rdf:type atomic:WorkIntent` |
| `vault:Memory`, `kind=memory` | `memory:*` | Replace with `rdf:type atomic:Memory` |
| `vault:Skill`, `kind=skill` | `skill:*` | Replace with `rdf:type atomic:Skill` |
| `vault:ToolResult`, `kind=tool_result` | `tool:*` | Replace with `rdf:type atomic:ToolResult` |
| `vault:File`, `kind=file` | `file:<path>` | Replace with `rdf:type atomic:File` |
| `vault:Identity`, `kind=identity` | `identity:*` | Replace with `rdf:type atomic:Identity`; DID resources are individuals of this class |
| `vault:Concept`, `kind=concept` | `concept:*` | Replace with `rdf:type skos:Concept`; do not mint a duplicate Atomic class |
| `vault:Session`, `kind=session` | `session:*` | Replace with `rdf:type atomic:Session` (`rdfs:subClassOf prov:Activity`) |
| `vault:SessionTurn`, `kind=session_turn` | `session:*/turn:*` | Replace with `rdf:type atomic:SessionTurn` (`rdfs:subClassOf prov:Activity`) |
| `vault:Todo`, `kind=todo` | session-qualified todo IDs | Replace with `rdf:type atomic:Todo`; reject unqualified `todo:*` identities |
| `vault:ProvenanceGraph`, `kind=provenance` | `provenance:<hash>` | Replace with `rdf:type atomic:ProvenanceGraph` (`rdfs:subClassOf prov:Entity`) |
| `vault:Task`, `kind=task` | intent task directives | Replace with `rdf:type atomic:Task` |
| `vault:AcceptanceCriterion`, `kind=acceptance_criterion` | intent AC directives | Replace with `rdf:type atomic:AcceptanceCriterion` |
| `vault:ScopeItem`, `kind=scope` | in/out scope directives | Replace with `rdf:type atomic:ScopeItem`; role follows the containing property |
| `vault:Constraint`, `kind=constraint` | constraint directives | Replace with `rdf:type atomic:Constraint` |
| `kind=change` | `change:<hash>` | `atomic:Change`; `kind` is metadata, not class assertion |
| `kind=view` | `view:<name>` | `atomic:View`; `kind` is metadata |
| `kind=module` | `module:<path>` | `atomic:Module`; `kind` is metadata |
| `kind=entity` | parsed source symbols | `atomic:SourceEntity`; language-specific kinds become subclasses or metadata |

All projectors MUST emit actual `rdf:type` assertions. `KgNode.kind` and
`metadata.rdf_type` cease to represent class membership and SHOULD be removed where they
duplicate RDF. UI indexes derive compact display categories from entailed `rdf:type`.

#### PROV-O and metadata predicate coverage

| Existing term | Observed domain → range | Canonical OWL disposition |
|---|---|---|
| `prov:wasAssociatedWith` | session/activity → identity; current tool result → goal misuse | Keep PROV-O only for activity → agent; replace tool-result → goal with `atomic:linkedGoal` |
| `prov:used` | activity → entity | Reuse canonical PROV-O property |
| `prov:wasGeneratedBy` | entity → activity | Reuse canonical; inverse of `prov:generated` |
| `prov:generated` | current session turn → change use | Replace with `atomic:generatedChange`, a subproperty of `prov:generated`; do not narrow the imported PROV property globally |
| `prov:wasInformedBy` | turn → preceding turn | Reuse canonical PROV-O property |
| `prov:wasDerivedFrom` | memory/session/entity → source entity | Reuse canonical PROV-O property |
| `atom:informedBy` | intent → memory/source | Replace with `atomic:informedBy`, `rdfs:subPropertyOf prov:wasDerivedFrom`; reject the old prefix form |
| `prov:hadPlan` | current session/turn → intent misuse | Replace with `atomic:hasPlan`; PROV `hadPlan` is reserved for qualified Association → Plan and is not declared equivalent |
| `prov:hadMember` | current session → turn misuse | Replace with `atomic:hasTurn`; Session remains a `prov:Activity`, not a `prov:Collection`, and no equivalence is declared |
| `dc:type` | resource → descriptive type | Retain Dublin Core metadata; never equivalent to `rdf:type` |
| `dc:creator` | resource → creator value | Replace identity-valued edges with object property `atomic:authoredBy`; do not emit literal creators |
| `dc:date` | resource → date literal | Delete stored edges and reproject `dcterms:created`/`dcterms:modified` from authoritative typed source fields; an orphan edge with no role provenance is a migration error |
| `dc:description` | resource → literal | Replace with `dcterms:description`; derive UI summaries from RDF |
| `rdf:type` | resource → class | Canonical class assertion |

#### Vault workflow predicate coverage

| Current namespaced predicate | Current uppercase kind | Observed domain → range | Required canonical replacement |
|---|---|---|---|
| `vault:blocks` | `BLOCKS` | intent → intent | Replace both with `atomic:blocks` |
| `vault:blockedBy` | `BLOCKED_BY` | intent → intent | Replace both with inverse `atomic:blockedBy` |
| `vault:assignedTo` | `ASSIGNED_TO` | intent → identity | Replace both with `atomic:assignedTo` |
| `vault:linkedIntent` | `LINKED_INTENT` | goal/change/entity → intent | Replace with typed relations: `atomic:implements` for change → intent and `atomic:linkedIntent` otherwise |
| `vault:linkedGoal` | `LINKED_GOAL` | intent/entity → goal | Replace both with `atomic:linkedGoal` |
| `vault:references` | `REFERENCES` | entity → entity | Replace both with `atomic:references` |
| `vault:appliesTo` | — | entity → concept/domain | Replace with `atomic:appliesTo` |
| `vault:hasLabel` | `HAS_LABEL` | intent/entity → concept | Replace both with `atomic:hasLabel` |
| `vault:status` | metadata `status` | entity → status literal/concept | `atomic:status`; metadata is application-only serialization |
| `vault:priority` | metadata `priority` | entity → priority literal/concept | `atomic:priority`; metadata is application-only serialization |
| `vault:explainedBy` | — | session turn/change → provenance graph | Replace with `atomic:explainedBy` |
| `vault:hasTodo` | — | session turn/intent → todo | Replace with `atomic:hasTodo` |

#### Intent decomposition and review edge coverage

| Uppercase edge kind | Observed domain → range | Canonical OWL property and relation |
|---|---|---|
| `HAS_TASK` | intent → task | Replace with `atomic:hasTask` |
| `HAS_ACCEPTANCE_CRITERION` | intent → acceptance criterion | Replace with `atomic:hasAcceptanceCriterion` |
| `HAS_SCOPE_IN` | intent → scope item | Replace with `atomic:hasScopeIn`, a subproperty of `atomic:hasScopeItem` |
| `HAS_SCOPE_OUT` | intent → scope item | Replace with `atomic:hasScopeOut`, a subproperty of `atomic:hasScopeItem` |
| `HAS_CONSTRAINT` | intent → constraint | Replace with `atomic:hasConstraint` |
| `SATISFIES` | task → acceptance criterion | Replace with `atomic:satisfies`; evidence uses separate narrower properties |
| `TOUCHES` | task → file | Replace with `atomic:touches` |
| `DEPENDS_ON` | intent → intent and change → change | Overloaded current kind; replace by endpoint class with `atomic:directlyDependsOnChange` or `atomic:dependsOnIntent`, both subproperties of `atomic:dependsOn` |
| `REMEDIATES` | intent → intent | Replace with `atomic:remediates` |
| `REVIEWS` | review intent → work intent | Replace with `atomic:reviews`; inverse `atomic:reviewedBy` |
| `SCOPE_OUT_FILE` | intent → file | Replace with `atomic:scopeOutFile`; MAY also be inferred through an out-scope item |

#### VCS, view, and source-code edge coverage

| Uppercase edge kind | Observed domain → range | Canonical OWL property and relation |
|---|---|---|
| `MODIFIES` | change → file or source entity | Replace by range with `atomic:modifiesFile` or `atomic:modifiesEntity`, both below `atomic:modifies` |
| `DEFINES` | file → source entity | Replace with `atomic:defines` |
| `ON_VIEW` | change/session → view | Overloaded current kind; replace by subject class with `atomic:directlyOnView` or `atomic:sessionViewContext`, both optionally below broad `atomic:onView` |
| `CHILD_OF` | view → parent view | Replace with `atomic:childViewOf`; derive transitive `atomic:ancestorView` separately |
| `VISIBLE_FROM` | not emitted in scoped projectors; intended change/entity → view | Replace with derived `atomic:visibleFrom`, never direct membership |
| `CALLS` | callable source entity → callable source entity | Replace with `atomic:calls` |
| `IMPORTS` | not emitted; intended file → file | Replace with `atomic:imports`, a subproperty of `atomic:includes` |
| `INCLUDES` | file → file | Replace with `atomic:includes` |
| `EXPORTS` | file → exported source entity | Replace with `atomic:exports`, a subproperty of `atomic:defines` |
| `PART_OF` | file → module; module → parent module | Replace with typed `atomic:filePartOfModule` or `atomic:modulePartOfModule`, below `atomic:partOf` |
| `AUTHORED_BY` | goal/intent/change → identity | Replace with object property `atomic:authoredBy`; do not emit a second creator predicate |

#### SKOS taxonomy coverage

| Existing term | Observed/intended domain → range | Canonical disposition |
|---|---|---|
| `skos:broader` | concept → broader concept | Reuse SKOS canonical property |
| `skos:narrower` | concept → narrower concept | Reuse SKOS; inverse of `skos:broader` |
| `skos:subject` | not emitted; intended resource → concept | Do not introduce it; use `dcterms:subject` directly |
| `skos:related` | concept → concept | Reuse SKOS canonical symmetric property |

#### Breaking normalization rules

The following require type-aware destructive rewrites rather than aliases:

1. Current `ToolResult prov:wasAssociatedWith Goal` misuses PROV association. Replace it
   with `atomic:linkedGoal`; no PROV equivalence is asserted.
2. `DEPENDS_ON` combines intent planning dependencies and VCS change dependencies.
   Existing edges are class-aware migrated directly to narrower properties; the broad
   transitive property is inferred and never used to represent directness.
3. `MODIFIES` has a union range of files and source entities. Narrower properties retain
   query precision while the old broad relation is removed.
4. `ON_VIEW` combines change membership and session workspace context. It is not
   equivalent to either canonical property; migrate by subject class to
   `directlyOnView` or `sessionViewContext`. Only direct change membership participates
   in view-cost queries.
5. `IMPORTS` is currently defined but import extraction often emits `INCLUDES`. New
   projection distinguishes language import from textual include and places both in a
   declared hierarchy.
6. Session class identity currently appears in `metadata.rdf_type`, while most classes
   appear only as `kind`. Migration emits true `rdf:type` triples and removes semantic
   dependence on both fields.
7. Session-global `todo:<id>` identities are replaced by
   `session:<id>/todo:<id>`; ambiguous old IDs fail migration rather than receiving an
   `owl:sameAs` assertion.

#### Breaking cutover requirements

Stored uppercase `KgEdge.kind` strings are not RDF IRIs and SHALL be replaced in storage
with canonical, absolute predicate IRIs. The cutover SHALL:

- rewrite every persisted edge to its canonical RDF predicate, using endpoint types to
  split overloaded relations;
- rewrite every class representation to an actual `rdf:type` assertion;
- update producers, query planners, CLI JSON, tests, and consumers in the same repository
  format/API version;
- reject old uppercase kinds, `vault:*` duplicates, `atom:*` prefix variants, and
  metadata-only typing after migration;
- fail with an actionable migration error when an overloaded edge cannot be classified;
- carry tests proving every current `predicate`, `entity_type`, and `edge_kind` constant
  has exactly one canonical replacement and that no compatibility vocabulary is emitted.

There is one ontology module family rooted at the canonical Atomic namespace. No legacy
OWL artifact is published or loaded during reasoning.

### Storage-minimization boundary

The ontology intentionally stops at durable resource and activity classes. It does not
contain language-, framework-, SQL-, API-, or security-specific operation/effect classes.
Those interpretations belong to virtual query graphs or separately attested findings.
This keeps persistent growth proportional to canonical project objects and provenance,
not to the number of possible analyses over each source span.

### Open-world boundary

OWL uses the open-world assumption: absence of `hasPlan`, `attestedBy`, or
`reviews` does not mean the relationship does not exist. OWL cardinality restrictions
therefore describe meaning but cannot serve as publication gates. Atomic MUST NOT use an
OWL consistency result to claim that a record is complete, reviewed, or cryptographically
verified.

The division of responsibility is normative:

| Concern | OWL | SHACL |
|---|---:|---:|
| Class and property meaning | Yes | Optional |
| Subclass/subproperty inference | Yes | No |
| Inverse and property-chain inference | Yes | No |
| Detect missing required edges | No | Yes |
| Enforce exact cardinality for publication | No | Yes |
| Validate datatypes and numeric bounds | No | Yes |
| Verify signatures or hashes | No | External verifier + SHACL result |
| Check aggregate equality and review freshness | No | SHACL-SPARQL |

### Reasoning and validation order

Reconciliation SHALL execute in this order:

1. project asserted triples from canonical objects;
2. cryptographically verify hashes and attestations and assert verification results;
3. apply the declared OWL 2 RL inference rules;
4. run SHACL over the asserted-plus-inferred graph;
5. materialize approved convenience edges with provenance identifying the rule and
   ontology version;
6. publish the projection generation and conformance report.

SHACL shapes may rely on OWL-inferred triples only when the validation report records the
reasoning profile. Implementations that validate asserted triples alone must use shapes
that traverse the full authoritative path explicitly.

## SHACL enforcement

OWL defines meaning and derives entailments; SHACL decides whether a concrete projection
is operationally complete. Validation has two profiles.

### Publication profile

Applied before publishing managed agent work:

- every managed `SessionTurn` has exactly one `atomic:hasPlan` intent;
- every managed/active intent governs at least one turn through `atomic:governsTurn`;
- no managed turn is governed by more than one intent;
- backlog intents may have zero turns until their transition into managed work;
- every turn has exactly one provenance graph;
- every generated change is covered by the session attestation;
- every attestation has a signer DID and valid proof;
- every required review covers the exact implementation closure.

Failure blocks publication because these facts cannot be safely inferred later.

### Projection profile

Applied after sync/enrichment:

- every generated change has matching `atomic:implements`, `atomic:explainedBy`, and
  `atomic:attestedBy`
  shortcuts;
- shortcut targets equal the authoritative intermediate path targets;
- all referenced canonical nodes exist;
- aggregate cost/tokens equal linked contribution sums;
- recorded executions point through `prov:used` to existing canonical artifacts and
  through provenance to the responsible turn/change;
- full hashes, not abbreviated labels, are used in durable IDs;
- an approved review's closure digest equals the current implementation closure.

Projection failure triggers deterministic repair and revalidation. If repair still
fails, the KG is marked non-conforming and must not be presented as complete audit
evidence.

Repositories are upgraded through a breaking format migration before the new graph is
opened. After cutover, one strict shape family applies to the canonical graph. Historical
objects with genuinely absent facts may carry explicit `atomic:Unknown`/finding nodes,
but they do not retain an alternate vocabulary.

## Sync and enrichment contract

`atomic vault sync` and `atomic vault query enrich` currently have different scopes.
The user-facing contract SHOULD become a single reconciliation operation:

1. ingest/materialize canonical vault objects;
2. discover canonical VCS, session, provenance, and attestation objects;
3. project all node and edge families;
4. index exact resource versions and execution evidence needed by authorized `query ask`
   retrieval;
5. apply the versioned OWL 2 RL entailment rules;
6. run projection SHACL validation over the asserted-plus-inferred graph;
7. repair stale derived projections;
8. publish ontology version, reasoning profile, conformance status, and statistics.

Implementations may retain separate internal phases, but users should not need to know
which subsystem owns a missing edge.

Incremental record-time enrichment runs the same projectors for newly published objects.
A full rebuild must produce an equivalent graph.

## Historical migration

1. Replace abbreviated change-node IDs with full-hash canonical IDs; old IDs cease to
   resolve after migration.
2. Replay all stored session turns to restore session/turn/intent/change/provenance
   edges.
3. Load attestations and emit signer, coverage, chain, and verification nodes/edges.
4. Project AI contribution nodes and recompute change aggregates.
5. Parse provenance ledgers into typed causal nodes and edges.
6. Index exact content-addressed file versions and recorded execution evidence for
   `query ask`; do not derive persistent domain operations or effects.
7. Apply the selected OWL profile and materialize derived relations with rule provenance.
8. Run the strict canonical SHACL profile over the entailed graph.
9. Report unrecoverable gaps rather than inventing links.

An unrecoverable record receives a finding such as `MissingHistoricalPlanId`; it does
not receive a guessed `atomic:implements` edge.

## Cross-project resource querying

OWL terms make separately produced project graphs semantically composable, but
federation MUST NOT flatten repositories into one unscoped graph. Atomic models a query
surface as an RDF Dataset: each project projection is a named graph with its own
identity, authorization boundary, ontology version, projection generation, and SHACL
conformance report.

### Project identity and named graphs

Every repository receives a stable `ProjectId` from a signed, content-addressed project
identity object containing an issuer DID, creation time, random 256-bit identifier,
schema version, and proof. Ordinary clones transfer that object unchanged. An explicit
fork operation creates and signs a new identity object with `prov:wasDerivedFrom` the
origin project. Import collision is rejected when the same identifier has a different
verified identity object; lost signing authority is handled by an attested rotation
chain, never silent ID replacement. The identity survives relocation and remote URL
changes. A project graph IRI is derived from that identity and projection generation:

```text
urn:atomic:project:<project-id>
urn:atomic:graph:<project-id>:projection:<generation-hash>
```

Mutable names such as a filesystem path, organization slug, or remote URL are labels and
locators, not identity. A fork may intentionally preserve an origin relationship while
receiving a distinct `ProjectId`:

```text
project:fork ──prov:wasDerivedFrom──▶ project:origin
```

The active dataset separates graph roles explicitly:

```text
urn:atomic:catalog:<scope-hash>                         project/graph catalog
urn:atomic:graph:<project-id>:asserted:<generation>     canonical projections
urn:atomic:graph:<project-id>:inferred:<generation>     OWL entailments
urn:atomic:graph:<project-id>:assertions:<generation>   reified assertion records
urn:atomic:graph:federation:<scope-hash>:<generation>   authorized cross-project links
```

Only authorized project graphs enter the active dataset. Catalog triples map projects to
all graph IRIs and projection manifests; the default graph is empty. Asserted project
triples remain in their source graph. Inferred triples are written to the project
inferred graph and linked through deterministic derivation records to source triple IDs,
rule IDs, ontology version, and asserted generation. These records and graph digests are
covered by the signed projection manifest.

### Portable resource identity

Resources fall into two identity classes:

1. **Globally content-addressed objects** — changes, provenance graphs, attestations,
   reviewed closure digests, and immutable vault objects use full cryptographic hashes.
   The same verified object hash denotes the same immutable bytes across projects.
2. **Project-scoped resources** — views, paths, modules, source entities, mutable intent
   names, and local sequence numbers are qualified by `ProjectId`.

Canonical examples:

```text
urn:atomic:change:blake3-256:<domain-separated-canonical-hash>
urn:atomic:provenance:blake3-256:<domain-separated-canonical-hash>
urn:atomic:attestation:blake3-256:<domain-separated-canonical-hash>
urn:atomic:project:<project-id>:view:<encoded-name>
urn:atomic:project:<project-id>:intent:<canonical-intent-uid>
urn:atomic:project:<project-id>:file:<normalized-path>
```

Each object specification defines canonical bytes, schema version, hash algorithm, and a
distinct domain-separation prefix before an IRI can be treated as global identity.
Multihash-compatible algorithm qualification permits migration without collisions across
algorithms or object families. A human key such as `ATOM::lee::42`, a 12-character
change prefix, or `src/lib.rs` is never globally unique. Query parsers resolve shorthand inside an explicit project scope
before evaluating the federated plan.

Cross-project identity uses `atomic:verifiedEquivalent`, backed by an identity-proof
assertion and policy-controlled substitution. `owl:sameAs` entailment is disabled across
project authorization boundaries because standard replacement semantics can disclose
private properties even when identity itself is proven. It MAY be enabled only inside an
authorization-equivalent graph set whose policy permits full fact substitution. Similar
paths, matching labels, mirrored intents, equivalent APIs, or copied source files use
weaker relations such as `atomic:correspondsTo`, `prov:wasDerivedFrom`,
`dcterms:hasVersion`, or a domain-specific compatibility relation.

### Cross-project relations

The federation graph may contain portable links including:

```text
Project ──atomic:dependsOnProject────▶ Project
Project ──prov:wasDerivedFrom────────▶ Project
Change  ──atomic:patchEquivalent─────▶ Change
Intent  ──atomic:relatedIntent───────▶ Intent
Intent  ──atomic:remediates──────────▶ Intent
Project ──atomic:usesArtifact────────▶ ContentAddressedArtifact
Execution ──prov:used────────────────▶ ContentAddressedArtifact
```

Cross-project assertions are first-class assertion nodes, not unannotated RDF edges:

```text
CrossProjectAssertion
  subject, predicate, object
  assertingProject, assertingIdentity
  sourceObject, timestamp, trustPolicy
  attestation, verificationResult
```

The federation graph contains the relation only when its assertion node verifies and
policy authorizes its use; the assertion graph retains the complete evidence. A
canonical N-Quads statement identifier or deterministic skolem IRI identifies every
assertion. Blank nodes are forbidden in signed slices, evidence paths, reports, and
federated join keys. Inferred links are never promoted to asserted links without a
canonical object recording that assertion.

Project dependencies are not automatically change dependencies. A package manifest may
assert `Project A dependsOnProject Project B`; only lockfile/object evidence can identify
the exact version, view state, or change closure consumed.

### Federation modes

The query planner supports three compatible execution modes:

1. **Local catalog** — query named graphs for projects cloned or indexed locally.
2. **Object-backed federation** — fetch signed project projection manifests and required
   graph slices from Atomic object storage.
3. **Remote endpoint federation** — dispatch authorized subplans to remote Atomic query
   endpoints and combine signed result envelopes.

A logical query has the same semantics in every mode. The planner pushes filters and
projection down to source graphs, requests only necessary graph slices, and performs
cross-project joins on portable IRIs.

Queries MUST declare scope explicitly:

```text
--project <id>                 one project
--projects <id-a>,<id-b>       selected projects
--workspace                    configured local project set
--organization <id>           all authorized organization projects
--federated                    allow remote subplans
```

There is no implicit “all projects visible to the server” mode. This avoids accidental
information disclosure and unstable results as unrelated repositories are indexed.

### Ontology and reasoning negotiation

Each project graph advertises:

- canonical ontology version;
- canonical repository graph-format version;
- OWL reasoning profile;
- available `query ask` model/tool capabilities;
- projection generation;
- SHACL conformance report hash.

Reasoning is an explicit multi-pass operation. For each authorized project, the reasoner
forms a temporary controlled union of that project's asserted graph plus the exact
versioned canonical ontology graphs declared by its manifest. It
writes entailments only to that project's inferred graph. A second federation pass reads
only authorized asserted/inferred graphs, verified assertion nodes, the scoped federation
graph, and explicit version-bridge axioms selected by the planner; its results go to a
separate federation-inferred graph. These bridges relate released versions of the one
canonical Semantic Web ontology; they never expose removed storage strings, uppercase
predicates, old prefixes, or a compatibility query API. Every inferred statement has a
derivation record. The planner computes the newest mutually compatible ontology contract
and MUST NOT union
projects under an arbitrary latest ontology, because that can change historical meaning
or create invalid identity entailments.

Cross-project inference is limited to bridge-safe rules declared by the federation
profile. In particular:

- subclass and subproperty mappings may cross graphs;
- verified content-addressed identity may be shared;
- project-local functional or inverse-functional properties do not become globally
  functional;
- no cross-project `owl:sameAs` is inferred unless all participating graphs share an
  authorization-equivalent policy and an identity proof explicitly permits full OWL
  substitution;
- ephemeral `query ask` interpretations do not participate in persistent OWL entailment
  unless separately promoted and attested as canonical findings.

### Authorization and privacy

Authorization is evaluated before graph selection, reasoning, joins, inference,
aggregation, and result assembly. Unauthorized triples never enter a controlled union or
intermediate binding table. Predicate- and resource-level labels propagate to derived
triples as the intersection of source permissions; a derived statement is visible only
when every supporting statement is visible to the caller. A caller may have access to a request-scoped `query ask` result, or to a separately
attested durable finding, without access to the underlying reasoning text, prompt, source
file, cost, or private intent.

Result filtering MUST not reveal hidden projects through counts, inferred edges,
aggregate differences, or “redacted” placeholders unless policy explicitly permits
existence disclosure. Timing resistance has an explicit threat profile: untrusted remote
callers receive allow-listed bounded plans, isolated execution, cached/padded negative
responses, and coarse resource limits. The RFC does not claim timing non-interference for
arbitrary local administrators or unrestricted SPARQL endpoints.

Remote results are returned in signed envelopes containing signer DID, source project,
projection and graph hashes, canonical query-plan hash and parameters, requested scope,
authorization-policy/version, ontology/reasoning versions, canonical result digest,
execution timestamp, nonce/request ID, expiry, applicable SHACL report, and explicit
complete/partial source lists with failure reasons. Signatures prove authenticity and
integrity; completeness is claimed only relative to the signed requested scope and
source manifest.

### Aggregation and deduplication

Federated aggregation declares its counting identity:

- **Object identity** deduplicates the same full content-addressed object across projects.
- **Contribution identity** deduplicates generation cost by a canonical generation-event
  ID, not by change or attestation hash.
- **Project membership identity** counts `(ProjectId, object hash)` once per project.
- **Occurrence identity** counts every selected view or deployment occurrence.

A generation event has canonical identity, typed cost/token measurements, and explicit
`generatedChange` edges. Attestations independently use `attestsContribution`; one
attestation may cover several contributions and one contribution may have several
attestations. When one generation event produces multiple changes, its cost is counted
once for a closure containing any of them unless an explicit, canonical allocation record
assigns portions to changes.

Cost defaults to contribution identity because a change hash may identify hashed change
content without uniquely identifying every attached provenance payload. Reusing the same
verified contribution in several projects does not incur generation cost again, while
independently generated equivalent changes may. Deployment-risk and adoption queries
often use project membership identity. The API MUST require or report the selected mode
rather than silently choosing based on graph layout.

### Federated query examples

Find every intent, in authorized projects, that introduced a table deletion affecting a
shared database resource:

```sparql
SELECT DISTINCT ?project ?projectGraph ?projection ?ontology ?shaclReport
                ?intent ?change ?file ?execution ?analysis ?attestation
WHERE {
  # These graph IRIs come from the verified active-dataset configuration.
  VALUES (?catalogGraph ?federationGraph ?assertionGraph) {
    (<urn:atomic:catalog:verified-scope>
     <urn:atomic:graph:federation:verified-scope:42>
     <urn:atomic:graph:federation:verified-scope:assertions:42>)
  }
  VALUES ?project { project:payments project:identity project:analytics }
  GRAPH ?catalogGraph {
    ?project atomic:hasProjection ?projection .
    ?projection atomic:assertedGraph ?projectGraph ;
                atomic:ontologyVersion ?ontology ;
                atomic:shaclReport ?shaclReport ;
                atomic:conforms true .
  }
  GRAPH ?projectGraph {
    ?turn atomic:generatedChange ?change ;
          atomic:hasPlan ?intent .
    ?change atomic:modifiesFile ?file .
    OPTIONAL {
      ?execution a atomic:Execution ;
                 prov:used ?file ;
                 atomic:partOfTurn ?turn .
    }
    ?attestation atomic:coversChange ?change ;
                 atomic:verificationResult atomic:Verified .
  }
  SERVICE atomic-query:askResults {
    ?analysis atomic-query:source ?file ;
              atomic-query:status atomic-query:ModelInterpretation ;
              atomic-query:operation "drop-table" ;
              atomic-query:evidenceDigest ?evidenceDigest .
  }
}
```

Trace a migration or recorded execution across authorized projects:

```text
projects(workspace)
  → changes → modified migration files / provenance executions
  → ephemeral query-ask result(operation = drop-table)
  → change → intent → attestation
```

The `query ask` result is request-scoped. Only the project graph's durable file, execution,
change, intent, and attestation relations participate in persistent federation.

Find projects consuming an artifact produced by a particular intent:

```text
intent(project A, uid)
  → implementedBy changes
  → produced content-addressed artifacts
  → inverse atomic:usesArtifact / lockfile or execution provenance
  → consuming projects and pinned artifact versions
```

If the caller asks whether that artifact is an API contract or whether use is compatible,
`query ask` interprets the exact artifact/manifests and returns shaped ephemeral evidence.

Every row includes source project and named graph. Query results that combine conforming
and non-conforming graphs distinguish them explicitly instead of presenting a single
undifferentiated confidence level.

### Federated SHACL validation

Project SHACL shapes validate each project graph independently. A custom dataset
validation runner then executes SHACL-SPARQL/SHACL-AF checks over declared named graphs
and invokes external cryptographic and authorization-policy verifiers. SHACL validates
the recorded verifier results; SHACL itself does not verify signatures, hashes, remote
envelopes, or authorization. Dataset checks validate cross-project assertions:

- both endpoints use portable canonical IRIs;
- the asserting identity is authorized to make the link;
- referenced projection/object hashes exist and verify;
- ontology bridge versions are compatible;
- `verifiedEquivalent` carries the required identity proof and policy result;
- externally computed remote-envelope and attestation verification results are valid;
- a cross-project review names the exact reviewed project and closure digest.

An unavailable remote graph is **unknown**, not conforming and not violating. Query
results report partial federation separately from SHACL violations. All joinable and
signed resources use canonical or deterministic skolem IRIs; blank nodes are rejected.

## Query and API requirements

The query layer SHOULD provide first-class operators for:

- direct view membership;
- effective view visibility;
- dependency closure;
- implementing changes for an intent;
- provenance and attestation traversal;
- evidence-grounded `query ask` over exact file versions and execution records;
- review freshness;
- distinct aggregate cost and token usage;
- explicit project/workspace/organization federation scope;
- named-graph and source-project provenance;
- object, project-membership, and occurrence deduplication modes;
- ontology-version negotiation and partial-federation reporting.

Results SHOULD include a projection generation, ontology version, reasoning profile,
model/tool versions and query-plan hash when semantic interpretation is requested, and
SHACL conformance state so callers can distinguish complete
audit answers from incomplete or non-conforming results.

## Security and privacy

- Reasoning text, prompts, transcripts, and tool output may contain secrets. Store them
  in access-controlled content-addressed objects and expose hashes/approved summaries in
  the general KG.
- Cost and token totals can reveal operational information and follow repository access
  controls.
- `query ask` interpretations are ephemeral claims; results include model/tool
  provenance, query-plan hash, exact source/execution evidence, and uncertainty.
- Attestation validity is computed from signatures, not trusted from a mutable boolean.
- Cross-project queries enforce authorization at every source and do not leak hidden
  project existence through joins, inference, or aggregates.
- Remote graph slices and result envelopes are verified before their claims participate
  in reasoning or SHACL validation.

## Rollout

### Phase 1: Referential closure

- Versioned OWL 2 RL ontology, optional OWL 2 DL companion, and SHACL shape graph.
- Breaking graph-format migration to canonical RDF IRIs with no compatibility ontology.
- Automated replacement coverage test for every current predicate, entity type, and
  edge-kind constant.
- Signed stable `ProjectId` objects, project-qualified mutable-resource IRIs, and
  algorithm-qualified content-object IRIs.
- Intent/session/turn/provenance/change/attestation nodes.
- Required direct shortcut edges.
- Backfill and projection SHACL validation.

### Phase 2: Contribution normalization

- AI contribution and model nodes.
- Typed token/cost fields and validated aggregates.
- Privacy-aware references to large provenance content.

### Phase 3: Evidence-grounded `query ask`

- Constrained natural-language-to-query-plan and result-shape generation.
- Bounded retrieval over exact content-addressed files, changed hunks, and executions.
- Structured LLM interpretation with machine-validated citations and abstention.
- Request-scoped virtual result graphs with no persistent statement/effect nodes.
- Optional deterministic tools callable by the planner without defining stored semantics.

### Phase 4: Review policy

- Exact reviewed closure digests.
- Review freshness and independent-attestation shapes.
- Promotion/completion gates.

### Phase 5: Cross-project federation

- Federation transport and catalogs over the stable project identities and
  project-qualified resource IRIs established in Phase 1.
- Named project graphs and signed projection manifests.
- Local, object-backed, and remote endpoint federation.
- Canonical ontology-version bridge negotiation—with no old vocabulary exposure—and
  dataset-level SHACL.
- Authorization-safe joins, result envelopes, and explicit aggregate identity modes.

## Acceptance criteria

The RFC is implemented when:

1. A newly recorded managed change has a continuous graph path to its governing intent,
   provenance graph, attestation, session, and turn.
2. Full enrichment repairs all derivable links from existing canonical objects.
3. OWL 2 RL reasoning derives the specified inverse, hierarchy, and provenance
   property-chain relations deterministically.
4. SHACL rejects missing or inconsistent required links after the strict cutover.
5. A query over a SQL migration containing `DROP TABLE users`, or equivalent recorded
   execution evidence, returns ephemeral `query ask` evidence linked to the responsible
   file, change, intent, provenance, attestation, and review without persisting a table or
   effect node.
6. Direct-view and effective-view cost queries return distinct, explicitly scoped
   change sets.
7. Historical missing facts are reported and never guessed.
8. Every current `predicate`, `entity_type`, and `edge_kind` constant has exactly one
   tested canonical replacement; no old term or compatibility ontology remains after the
   breaking migration.
9. A query can join authorized named graphs from multiple projects using portable
    identities while reporting source graph, ontology version, conformance, completeness,
    and explicit deduplication semantics.
10. Cross-project identity and review assertions require verifiable evidence and never
    infer `owl:sameAs` from labels, paths, or similarity.

## Resolved decisions

- Domain semantics are supplied by evidence-grounded `atomic vault query ask`, not by
  persistent semantic-effect relations.
- The LLM builds an allow-listed query plan and result shape; Atomic executes traversal,
  retrieves exact evidence, and validates all citations and structural bindings.
- Deterministic parsers are optional tools, not a required ontology or persistence layer.
- Semantic answers are ephemeral and excluded from OWL/SHACL unless explicitly promoted
  into a separately attested durable artifact.
- `atomic vault sync` and `atomic vault query enrich` call one shared graph-reconciliation
  pipeline internally.
- OWL 2 RL entailments are materialized into versioned inferred named graphs with
  derivation records.
- Local, object-backed, and remote query transports sit behind one logical planner.
- One managed intent governs one or more turns; every managed turn has exactly one
  governing intent, while backlog intents may have zero turns before work begins.
- `atomic:` is the single Atomic workflow ontology namespace, covering Intent, Change,
  Memory, Attestation, and every other workflow-owned concept; PROV-O is the shared
  crossover vocabulary and is reused directly.

## Open questions

1. Which OWL 2 RL implementation should provide local deterministic entailment?
2. Beyond the settled PROV-O boundary, which exact external terms from SIOC, DOAP, SPDX,
   Dublin Core, SKOS, and the Data Integrity vocabulary match without weakening workflow
   ownership?
3. Which ontology-version differences are safe for automatic bridge reasoning across
   projects?
