# Phase 1 Plan: Proposals for Approval

> Status: **for review.** Companion to
> [`knowledge-graph-closure-phase1-plan.md`](./knowledge-graph-closure-phase1-plan.md).
> Each section below is a proposed addition to the plan. Approved sections get merged
> into the plan; this file is then deleted.

| # | Proposal | Plan section it fills |
|---|---|---|
| A | The `LEGACY_TERMS` mapping: every old constant → its canonical replacement | §1.5 (the table is currently `&[ ... ]`) |
| B | Ontology terms the plan uses but never declares, plus one domain bug | §1.1 |
| C | `ProjectIdentity` JSON-LD context and hash construction | §1.3 |
| D | Two questions that need a decision (neither blocks M1–M3) | new "Open questions" |

---

## A. `LEGACY_TERMS` mapping

The code has the old terms (`atomic-core/src/pristine/ontology.rs`: 6 prefixes,
30 `predicate::*`, 16 `entity_type::*`, 29 `edge_kind::*`) but no mapping. The M1
coverage test requires each old constant to appear exactly once with one of:
`One`/`OnePred` (one replacement), `SplitByEndpoint` (replacement depends on the
endpoint classes), or `Removed` (with a reason).

Rows marked **(B)** depend on a term proposed in section B.

### A.1 Classes (`entity_type::*` and node `kind` values)

| Old | Replacement |
|---|---|
| `vault:Goal` / `goal` | `atomic:Goal` |
| `vault:Intent` / `intent` | `atomic:WorkIntent`, plus `FeatureIntent` / `BugIntent` / `ReviewIntent` from the canonical `kind`, and `ManagedIntent` from lifecycle status |
| `vault:Memory` / `memory` | `atomic:Memory` |
| `vault:Skill` / `skill` | `atomic:Skill` |
| `vault:ToolResult` / `tool_result` | `atomic:ToolResult` |
| `vault:File` / `file` | `atomic:File` |
| `vault:Identity` / `identity` | `atomic:Identity` |
| `vault:Concept` / `concept` | `atomic:Concept` (⊑ `skos:Concept`, per §1.1) |
| `vault:Session` / `session` | `atomic:Session` |
| `vault:SessionTurn` / `session_turn` | `atomic:SessionTurn`, plus `atomic:ManagedTurn` when `plan_id` is present |
| `vault:Todo` / `todo` | `atomic:Todo` |
| `vault:ProvenanceGraph` / `provenance` | `atomic:ProvenanceGraph` |
| `vault:Task` / `task` | `atomic:Task` |
| `vault:AcceptanceCriterion` / `acceptance_criterion` | `atomic:AcceptanceCriterion` |
| `vault:ScopeItem` / `scope` | `atomic:ScopeItem` |
| `vault:Constraint` / `constraint` | `atomic:Constraint` |
| `change` | `atomic:Change` |
| `view` | `atomic:View` |
| `module` | **Removed**: module hierarchy is derivable from paths (plan "What we skip") |
| `entity` | **Removed**: source entities stay in the syntax index (plan "What we skip") |
| *(new)* | `atomic:Attestation`, `atomic:Finding`; `atomic:Project`, `atomic:ProjectIdentity` **(B)** |

### A.2 Predicates and edge kinds

| Old | Endpoints | Replacement |
|---|---|---|
| `prov:hadMember` | session → turn | `atomic:hasTurn` |
| `prov:hadPlan` | turn → intent | `atomic:hasPlan` |
| `prov:hadPlan` | session → intent | **Removed**: derivable as session `hasTurn` → turn `hasPlan` |
| `prov:generated` | turn → change | `atomic:generatedChange` (⊑ `prov:generated`) |
| `prov:wasInformedBy` | turn → previous turn | `prov:wasInformedBy`, kept as PROV **(B)** |
| `prov:wasDerivedFrom` | memory → source; forked session → parent | `prov:wasDerivedFrom`, kept as PROV **(B)** |
| `prov:used` | tool result → file | `prov:used`, kept as PROV **(B)** |
| `prov:wasAssociatedWith` | tool result → goal | `atomic:linkedGoal` (PROV association is for activity → agent) |
| `prov:wasGeneratedBy` | never emitted | **Removed** |
| `atom:informedBy` | intent → memory/source | `atomic:informedBy` (⊑ `prov:wasDerivedFrom`) |
| `vault:explainedBy` | turn → provenance graph | `atomic:turnExplainedBy` (⊑ `atomic:explainedBy`) |
| `vault:hasTodo` | turn → todo | `atomic:hasTodo` |
| `BLOCKS` / `BLOCKED_BY` | intent ↔ intent | `atomic:blocks` / `atomic:blockedBy` (inverses) |
| `ASSIGNED_TO` | intent → identity | `atomic:assignedTo` |
| `LINKED_INTENT` | goal → intent | `atomic:linkedIntent` (target now resolved through the manifest) |
| `LINKED_GOAL` | intent → goal | `atomic:linkedGoal` |
| `REFERENCES` | entry → memory, entry → file | `atomic:references` |
| `HAS_LABEL` | intent → concept | `atomic:hasLabel` |
| `HAS_TASK` | intent → task | `atomic:hasTask` |
| `HAS_ACCEPTANCE_CRITERION` | intent → criterion | `atomic:hasAcceptanceCriterion` |
| `HAS_SCOPE_IN` / `HAS_SCOPE_OUT` | intent → scope item | `atomic:hasScopeIn` / `atomic:hasScopeOut` (⊑ `atomic:hasScopeItem`) |
| `HAS_CONSTRAINT` | intent → constraint | `atomic:hasConstraint` |
| `SATISFIES` | task → criterion | `atomic:satisfies` |
| `TOUCHES` | task → file | `atomic:touches` |
| `SCOPE_OUT_FILE` | intent → file | `atomic:scopeOutFile` |
| `REMEDIATES` | intent → intent | `atomic:remediates` |
| `REVIEWS` | review intent → intent | `atomic:reviews` |
| `DEPENDS_ON` | change → change | `atomic:directlyDependsOnChange` (⊑ `atomic:dependsOn`) |
| `DEPENDS_ON` | intent → intent | `atomic:dependsOnIntent` (⊑ `atomic:dependsOn`) |
| `MODIFIES` | change → file | `atomic:modifiesFile` (⊑ `atomic:modifies`) |
| `MODIFIES` | change → entity | **Removed**: code structure (plan "What we skip") |
| `ON_VIEW` | change → view | `atomic:directlyOnView`, recomputed from `VIEW_CHANGES` (today it records the working copy's desired view, not membership) |
| `ON_VIEW` | session → view | `atomic:sessionViewContext` |
| `CHILD_OF` | view → parent view | `atomic:childViewOf` |
| `VISIBLE_FROM` | never emitted | `atomic:visibleFrom`, declared as a query-time operator, never stored |
| `AUTHORED_BY` | change/goal/intent → identity | `atomic:authoredBy` (⊑ `prov:wasAttributedTo`); intents now emit it from `author`/`created_by` |
| `DEFINES`, `EXPORTS`, `INCLUDES`, `IMPORTS`, `CALLS` | code structure | **Removed**: syntax index (plan "What we skip") |
| `PART_OF` | file → module, module → module | **Removed**: derivable from paths |
| `vault:status`, `vault:priority` | literal | `atomic:status`, `atomic:priority` (see the domain fix in B.6) |
| `vault:blocks`, `vault:blockedBy`, `vault:assignedTo`, `vault:linkedIntent`, `vault:linkedGoal`, `vault:references`, `vault:hasLabel` | never emitted; duplicates of the uppercase kinds | **Removed**: the uppercase twins are mapped above |
| `vault:appliesTo` | never emitted | **Removed** |
| `dc:type`, `dc:creator`, `dc:date`, `dc:description` | never emitted | **Removed** (creator → `authoredBy`; dates and descriptions come from typed source fields, see B.5) |
| `skos:broader`, `skos:narrower`, `skos:subject`, `skos:related` | never emitted | **Removed** |
| `rdf:type` (as an edge) | never emitted | **Removed**: types are stored in `kg_types` (§1.6) |

### A.3 Prefix constants

| Old | Replacement |
|---|---|
| `prefix::{ATOM, PROV, SKOS, DC, VAULT, RDF}` | **Removed**: replaced by the `PREFIXES` table in `kg::ontology` (§1.2) |

---

## B. Ontology terms to add

The plan's §1.1 tables reference these, or the producers in §1.6 emit them, but the
ontology doesn't declare them. Without them the TTL ↔ Rust consistency test and the
`LEGACY_TERMS` coverage test (which requires every target to exist in the TTL) would fail.

### B.1 Parents and inverses that are referenced but not declared

| Term | Kind | Notes |
|---|---|---|
| `atomic:dependsOn` | object property, `owl:TransitiveProperty` | Parent of `directlyDependsOnChange` / `dependsOnIntent`; never materialized in Phase 1 |
| `atomic:modifies` | object property | Parent of `modifiesFile` |
| `atomic:hasScopeItem` | object property | Parent of `hasScopeIn` / `hasScopeOut` |
| `atomic:ancestorView` | object property, `owl:TransitiveProperty` | Mentioned in §1.1; never materialized |
| `atomic:turnOfSession`, `atomic:governsTurn`, `atomic:generatedBy`, `atomic:blockedBy`, `atomic:reviewedBy` | object properties, `owl:inverseOf` their partners | Named in the "Inverse" column but have no rows; `generatedBy` is used by both property chains |

### B.2 PROV terms the producers keep

Declared as reused terms (`rdfs:isDefinedBy <http://www.w3.org/ns/prov#>`), not redefined:
`prov:wasInformedBy` (turn → previous turn), `prov:wasDerivedFrom` (memory → source,
forked session → parent), `prov:used` (tool result → file), and the two parents the plan
already uses, `prov:generated` and `prov:wasAttributedTo`.

### B.3 Attestation verification

| Term | Kind |
|---|---|
| `atomic:verificationResult` | object property, `Attestation` → `atomic:VerificationStatus` |
| `atomic:VerificationStatus` | class |
| `atomic:Verified`, `atomic:Unverifiable`, `atomic:Failed` | individuals of `VerificationStatus` |

Written only by the cryptographic verifier, never inferred. MAC-signed attestations
record `Unverifiable`.

### B.4 Project identity

| Term | Kind |
|---|---|
| `atomic:Project` | class (the `urn:atomic:project:<pid>` node) |
| `atomic:ProjectIdentity` | class (the signed identity object) |
| `atomic:hasIdentity` | object property, `Project` → `ProjectIdentity` |
| `atomic:projectId` | datatype property, `xsd:string` (52-character uppercase base32) |
| `atomic:schemaVersion` | datatype property, `xsd:nonNegativeInteger` |
| `atomic:issuer` | object property, `ProjectIdentity` → `Identity` |

Creation time uses `dcterms:created` (B.5).

### B.5 Literals the producers carry today

`rdfs:label` (labels), `dcterms:description` (summaries), `dcterms:created` /
`dcterms:modified` (timestamps), and `atomic:turnNumber` (`xsd:nonNegativeInteger`).

### B.6 Domain bug: `atomic:status` and `atomic:priority`

Both are declared with domain `atomic:WorkIntent`, but tasks, acceptance criteria,
todos, and sessions also carry a status (and todos a priority). An OWL domain is an
inference rule, not a check, so every task, criterion, todo, and session would be
inferred to be a `WorkIntent`, and intent shapes would then run against them. This is
the same problem that was fixed for `explainedBy`.

**Proposal:** remove the domain from both properties. They are literals, so no
subproperties are needed; shapes constrain the allowed values per class.

---

## C. `ProjectIdentity` context and hash

**Problem.** §1.3's identity object uses `"@context": "https://atomic.dev/ns/ctx.jsonld"`.
That context defines the old `atom:` prefix but not `atomic:`, so `"atomic:ProjectIdentity"`
does not expand to `https://atomic.dev/ns/workflow#ProjectIdentity`; a JSON-LD processor
reads `atomic:` as an unknown URI scheme. The hash, `blake3(JCS(...))`, has no type
prefix, so it isn't separated from other object types.

**Proposal.** `ProjectIdentity` is new and has no existing signed copies, so fixing it now
costs nothing:

```jsonld
{
  "@context": { "atomic": "https://atomic.dev/ns/workflow#",
                "dcterms": "http://purl.org/dc/terms/" },
  "@type": "atomic:ProjectIdentity",
  "atomic:projectId": "ABCD...EFGH",
  "dcterms:created": "2026-01-15T09:00:00Z",
  "atomic:schemaVersion": 1,
  "atomic:issuer": "did:atomic:..."
}
```

- The context is inline, so the hashed bytes include the term definitions and their
  meaning can never drift.
- Content hash = `blake3("atomic.project-identity.v1" ‖ 0x00 ‖ JCS(object))`.
- Existing object hashes (changes, attestations, provenance, canonical intents and
  memories) are unchanged.

---

## D. Open questions (need a decision; neither blocks M1–M3)

1. **PurRDF needs Rust 1.98; Atomic declares 1.90** (`Cargo.toml:27`). Raise Atomic's
   minimum Rust version, or run validation as a separately built binary? This affects
   CI, packaging, and every contributor. Blocks M4 only.
2. **MAC-signed attestations vs. the publication profile.** The publication shape
   requires a signer identity and a successful verification result. MAC-signed
   attestations (`session-mac:{sid}`) can't be verified without the key, so MAC-signed
   work would fail it. Require DID signers for publication, or allow MAC signers that
   `[git.trust]` explicitly trusts? Phase 1 only records MAC attestations as
   `Unverifiable`; the decision is needed before publication gates are enforced in
   Phase 2.
