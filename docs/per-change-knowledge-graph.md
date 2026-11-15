# Canonical per-change workflow knowledge graph

**Question:** for one immutable change, which goal motivated it, which intent
governed or explicitly claims it, what was recorded, and which review covers it?
Can we follow the same evidence back from the review to the goal?

The reading order is **goal → intent → change → review**; the reverse reading
order is **review → change → intent → goal**. Neither is a new stored predicate,
an inverse patch operation, or a Git ancestry rule. The actual edges have the
directions below, and some paths can legitimately be missing.

This is a documentation view of the **existing** canonical ontology, not a new
schema, inference engine, CLI command, or approval mechanism. Normative vocabulary:

- [Workflow ontology](../atomic-core/ontology/workflow-1.0.0.ttl):
  `https://atomic.dev/ns/workflow/1.0.0`.
- [Review ontology](../atomic-core/ontology/review-1.0.0.ttl):
  `https://atomic.dev/ns/review/1.0.0`.
- `atomic:` means `https://atomic.dev/ns/workflow#`, including review predicates.

## 1. Nodes and identities

| Node | Meaning | Identity rule |
|---|---|---|
| `atomic:Goal` | A recorded objective or work context | Copy its canonical IRI from graph output. |
| `atomic:WorkIntent` | Requirements, tasks, criteria and constraints | Use its canonical project-qualified IRI; human keys are CLI lookup handles. |
| `atomic:SessionTurn` | A particular captured execution turn | Project/session/turn identity; never select a turn by recency. |
| `atomic:Change` | An immutable recorded patch | `urn:atomic:change:blake3-256:<FULL_HASH>`. |
| `atomic:ReviewIntent` | The reviewer's work intent, criteria and findings | A `WorkIntent` subtype; not the implementation intent or a signature alone. |
| `atomic:PublicationReview` | A present-day signed conclusion over exact scope | A content-addressed review artifact; an `Attestation` subtype. |
| `atomic:ReviewSubject` | Pinned changes, dependency closure, intent revisions and source context | Exact subject identity, not a mutable branch name alone. |
| `atomic:ReviewEvaluation` | Evaluation of a retained review | Keep its evaluation context separate from the immutable reviewed subject. |

Change hashes are not Git commit hashes, view Merkle states, or SetIds. A Git
rebase does not create an Atomic change or establish an intent/review edge.
Paths and matching authors are not substitutes for any of these identities.

## 2. Canonical edges: forward facts and reverse traversal

`^predicate` below means **follow an incoming edge**. It is path notation in this
guide, not another predicate to write into the graph.

| Asserted relationship | What it establishes | Reverse traversal |
|---|---|---|
| Goal `linkedIntent` Intent | Explicit goal-to-intent link | Intent `^linkedIntent` Goal |
| Intent `linkedGoal` Goal | Explicit intent-to-goal link | Goal `^linkedGoal` Intent |
| Intent `governsTurn` Turn | Captured governing turn | Turn `hasPlan` Intent |
| Turn `generatedChange` Change | Captured generation | Change `generatedBy` Turn |
| Change `implements` Intent | Captured implementation association | Intent `^implements` Change |
| Intent `declaresImplementation` Change | Explicit, evidence-bearing author declaration, possibly retrospective | Change `^declaresImplementation` Intent |
| Change `modifiesFile` File | Affected recorded artifact | File `^modifiesFile` Change |
| ReviewIntent `reviews` Intent | Work-intent scope of a review | Intent `reviewedBy` ReviewIntent |
| PublicationReview `reviewIntent` ReviewIntent | Review artifact's governing review intent | ReviewIntent `^reviewIntent` PublicationReview |
| PublicationReview `reviewsClosure` ReviewSubject | Exact pinned review subject | ReviewSubject `^reviewsClosure` PublicationReview |
| ReviewSubject `reviewsChange` Change | This exact change is in the reviewed subject | Change `^reviewsChange` ReviewSubject |
| ReviewSubject `includesIntent` Intent | Intent revision included in exact review scope | Intent `^includesIntent` ReviewSubject |
| PublicationReview `coversChange` Change | Change coverage; also inspect subject and artifact type | Change `attestedBy` PublicationReview |
| PublicationReview `hasJudgment` CriterionJudgment | Reviewer judgment | Judgment `^hasJudgment` PublicationReview |
| CriterionJudgment `judgesCriterion` AcceptanceCriterion | Criterion being judged | Criterion `^judgesCriterion` Judgment |

The ontology declares inverses for `governsTurn`/`hasPlan`,
`generatedChange`/`generatedBy`, `reviews`/`reviewedBy` and
`coversChange`/`attestedBy`. It does **not** declare `linkedIntent` and
`linkedGoal` as OWL inverses. Read whichever explicit goal links exist; do not
manufacture the other one. Likewise, do not invent `implementedBy`, `approvedBy`
or `changeReviewedBy` predicates.

### Forward: goal to intent to change to review

```text
Goal --linkedIntent--> WorkIntent --governsTurn--> SessionTurn
                          ^                           |
                          | implements                | generatedChange
                          |                           v
                          +------------------------ Change
                                                      ^
                                                      | reviewsChange
                                                ReviewSubject
                                                      ^
                                                      | reviewsClosure
                                               PublicationReview
                                                      |
                                                      | reviewIntent
                                                      v
                                                 ReviewIntent
                                                      |
                                                      +--reviews--> WorkIntent
```

An explicit `WorkIntent --linkedGoal--> Goal` can supply the goal connection
instead of `Goal --linkedIntent--> WorkIntent`.

Captured implementation has the property-chain meaning:

```text
Change --generatedBy--> SessionTurn --hasPlan--> WorkIntent
Change --implements--------------------------------> WorkIntent
```

An author's `declaresImplementation` is a **different** path. It can make an
implementation claim without a generating turn, but must not be relabeled as
captured provenance or used to fill a missing `hasPlan` edge.

### Reverse: review to change to intent to goal

```text
PublicationReview --reviewsClosure--> ReviewSubject --reviewsChange--> Change
                                                                        |
                                                                   generatedBy
                                                                        v
                                                                   SessionTurn
                                                                        |
                                                                     hasPlan
                                                                        v
                                                                   WorkIntent
                                                                        |
                                                                    linkedGoal
                                                                        v
                                                                       Goal
```

Where present, `Change --implements--> WorkIntent` is a shortcut for the
captured path. For declarations use incoming `declaresImplementation`; for a
goal with only `linkedIntent`, traverse that edge backwards from the intent.
If no qualifying link exists, the reverse path stops with a reported gap.

## 3. Keep the kinds of evidence separate

- **Captured implementation:** the recorded generating-turn/intent association.
- **Declared implementation:** an explicit author claim, not historical capture.
- **Intent recording:** a change modifies the *exact intent Markdown artifact*.
  `atomic intent graph` joins stored `vault_path` metadata to `modifiesFile` and
  reports `recording_changes` separately. This does not prove implementation.
- **Observed intent:** `Change --hasReviewEvidence--> IntentObservation
  --observedIntent--> Intent` retains an observation or receipt, not governance.
- **Dependency:** `Change --directlyDependsOnChange--> Change` supplies patch
  prerequisites. Dependency-only work does not inherit the root's intent,
  authorship, goal or approval.
- **Explanation:** provenance graphs and memories can explain a decision;
  `prov:wasDerivedFrom` does not replace an implementation or review assertion.

One change can have several roles and implement several intents. Do not discard
that multiplicity to force a single linear chain. Conversely, many changes and
intents may share **one group review**. A per-change report repeats the review
artifact's reference for covered changes; it does not require a new review per
change or count the shared approval as several independent reviews.

### A link to a review is not an approval

Inspect the artifact type: an ordinary session attestation can also
`coversChange`. For a publication review, retain its exact subject, work/review
intent revisions, criterion judgments, evidence, signer and policy context.
Check cryptographic `verificationResult`, current `freshnessResult`,
`authorityResult`, required verifications and blocking findings. Schema
`canonicalConformance`, an unsigned preview, a `done` label, or a signature alone
does not establish substantive approval.

`includesSetAside` describes an independently reviewed terminal disposition; it
is not proof that the historical implementation satisfies its criteria.
An intent-level `reviews` link alone must not be stretched over later changes.
Use the actual triage/publication evaluation for the promotion being considered.

## 4. One Markdown result per change

Use the following as a **reporting template**, not as a claim that a new CLI
returns these field names. Keep canonical IRIs and evidence next to every result.

| Result item | What to retain |
|---|---|
| Change | Full hash/IRI, message and verified local object availability. |
| Snapshot | Project, source view, Merkle/SetId where available, KG generation, ontology version and graph `snapshot_identity`. |
| Goals | Goal/intent pairs and the explicit predicate/direction joining them. |
| Captured intents | Intent IRI and generating turn/path; separate from declarations. |
| Declared intents | Intent IRI, declaration evidence and current signature status. |
| Recording/observation evidence | Exact intent artifact or retained observation; never silently promoted to implementation. |
| Dependencies | Required changes, labeled separately from selected implementation. |
| Reviews | Review intent, artifact and subject IRIs; selected-root/dependency role; pinned scope, judgments, verification, freshness, authority and blockers. |
| Forward result | Proven goal → intent → this change → exact review paths, or the point where each path stops. |
| Inverse result | The same facts traversed review → this change → intent → goal; no additional inferred approval. |
| Limits and gaps | Unavailable objects, missing links, ambiguous claims, stale indexes, truncated exports and unestablished judgments. |

Use **“not established in this snapshot”**, not “never existed,” for an absent
link. A failed or capped query is not an empty successful result. A two-hop
neighborhood is not complete lineage. Shared file, session, author or view
neighbors can lead to unrelated changes and must not expand review authority.

## 5. Commands: start at a change

Run inside the Atomic repository containing the object. These examples use the
published review-skill patch; substitute another full hash for another change.

```sh
CHANGE_HASH=GYA4QZCV4Z4IPG6SC7TXAMAZHAVPOMS33DVOU3R6DTXSXFCR6MUA

# Immutable change details; graph hunks can be empty for a span-backed edit.
atomic change "$CHANGE_HASH" --full-hash --show-deps --show-hunks
atomic change "$CHANGE_HASH" --format json

# Discover the canonical resource rather than guessing a project-qualified ID.
atomic query search "$CHANGE_HASH" --kind 'https://atomic.dev/ns/workflow#Change' --json
```

Copy the exact `id` from search. For this example it is:

```sh
CHANGE_IRI=urn:atomic:change:blake3-256:GYA4QZCV4Z4IPG6SC7TXAMAZHAVPOMS33DVOU3R6DTXSXFCR6MUA

# Neighbors includes incoming AND outgoing edges, with their original direction.
atomic query neighbors "$CHANGE_IRI" --depth 1 --json
atomic query neighbors "$CHANGE_IRI" --depth 2 --json

# Exploratory graph export, NOT a complete canonical lineage/approval report.
atomic query graph "$CHANGE_HASH" --limit 1 --depth 2 --changes-per-seed 0 --json > change-neighborhood.json
```

For captured explanation, when available:

```sh
atomic provenance trace "$CHANGE_HASH"
atomic provenance show "$CHANGE_HASH" > change.prov.jsonld
```

These commands can report that no provenance graph explains the change. Preserve
that result; do not invent a generating turn. The JSON-LD projection is unsigned
by default. Signing an export would be a new present-day assertion, not an
original execution signature. Current `atomic change` renders recorded
provenance in its default tree; legacy `change -p`/`change -a` flags are not
available in the CLI used for this guide.

### Directional per-change queries and immutable patch evidence

`query plan` supports predicate/direction filtering that the simpler `neighbors`
command does not expose. This plan asks only for relevant direct relationships
and bounded original-object evidence:

```sh
atomic query plan --json <<JSON
{
  "schema_version": 1,
  "steps": [
    {"type":"kg_neighbors","node_id":"$CHANGE_IRI","depth":1,"predicate":"https://atomic.dev/ns/workflow#generatedBy","direction":"outgoing"},
    {"type":"kg_neighbors","node_id":"$CHANGE_IRI","depth":1,"predicate":"https://atomic.dev/ns/workflow#implements","direction":"outgoing"},
    {"type":"kg_neighbors","node_id":"$CHANGE_IRI","depth":1,"predicate":"https://atomic.dev/ns/workflow#declaresImplementation","direction":"incoming"},
    {"type":"kg_neighbors","node_id":"$CHANGE_IRI","depth":1,"predicate":"https://atomic.dev/ns/workflow#reviewsChange","direction":"incoming"},
    {"type":"kg_neighbors","node_id":"$CHANGE_IRI","depth":1,"predicate":"https://atomic.dev/ns/workflow#hasReviewEvidence","direction":"outgoing"},
    {"type":"patch_evidence","change_iri":"$CHANGE_IRI"}
  ]
}
JSON
```

Follow returned turn IDs through `hasPlan`; intent IDs through outgoing
`linkedGoal`, incoming `linkedIntent`, or incoming `reviews`; and subject IDs
through incoming `reviewsClosure`. Reuse the plan's `kg_neighbors` step with the
copied node IRI, full predicate IRI and `incoming`/`outgoing` direction.

`patch_evidence` requires a **literal full-hash change IRI**, not a prefix or a
`$variable` query-plan reference. It returns bounded recorded GraphOps, FileOps
and inserted-blob bytes with explicit availability/truncation fields. It does
not reconstruct removed preimages or expose a complete span-rendered file or
all span operations. Empty GraphOps do not mean “no change.”

## 6. Expand the intent, goal and exact review

For each *established* work-intent association, use its copied human key or IRI:

```sh
atomic intent show ATOM::continuouslee::1 --json
atomic intent graph ATOM::continuouslee::1 --json > intent-graph.json
atomic intent graph ATOM::continuouslee::1 --details
atomic intent graph ATOM::continuouslee::1 --dot -o intent.dot
atomic intent graph ATOM::continuouslee::1 --html --no-open -o intent.html
```

Inspect `captured_changes`, `declared_changes`, `implementing_changes`,
`recording_changes`, `dependency_changes`, `reviewed_changes`, `missing_resources`,
`truncated` and `diagnostics`. This is an **intent-centered** asserted-lineage
export, not an implicit per-change graph command. Its `scope` and snapshot
identity qualify any result you derive from it. Follow an observed or
recording-only intent as context without claiming it governed the change.

Use `atomic query neighbors '<copied-goal-IRI>' --json` for a goal and the same
command for copied review intent, subject, artifact or evaluation IRIs.

```sh
# Current candidate-set evaluation; replace SOURCE_VIEW and TARGET_VIEW.
atomic triage review SOURCE_VIEW --into TARGET_VIEW --json

# Revalidate registered exact-scope retrospective publication reviews.
atomic triage publication-review inspect

# Structural workflow checks, not a substitute for inspecting the work.
atomic query validate --profile projection --format json
```

The publication validator profile and the promotion policy have additional
requirements; a projection-profile pass alone does not mean “approved.” For
review authoring, signing, registration and renewal see
[Retrospective Publication Review](./retrospective-publication-review.md).

### Missing or stale derived indexes

Normal inspection/export commands do not author missing governance or approval.
If a command reports `KgNeedsRebuild` or an unprojected resource, an explicit
maintenance action may be needed:

```sh
atomic query reindex --full
```

This **writes derived indexes** from retained canonical objects and vault data;
it is not a read-only inspection command. Re-export the graph afterward and
record the new snapshot identity. Reindexing does not manufacture provenance,
retroactively bind an intent, or approve a review. Investigate schema/repository
errors rather than treating them as evidence that no links exist.

## 7. Observed per-change results: the published review-skill update

Observed on 2026-11-15 using `atomic 0.22.1 (dev 540688f1310d-dirty)` in a fresh
clone of the canonical `atomic-skills` storage **release** view. This is an
explicit historical snapshot, not a claim about future source or approvals.
Derived indexes were explicitly rebuilt in that disposable clone after an
initial `KgNeedsRebuild` response; no historical capture was added.

| Pin | Value |
|---|---|
| Project | `G4YM66Q3RX5TGKH4YRBYDZXA6PRPCN6JF7AB4LAGYTP2NBMWCEUA` |
| View Merkle | `ER2S7UIQAOAAL3I5QISADZAIJ7E6B2HH6TI47QDQIR4KYBSRLGJQ` |
| KG generation / workflow version | `2` / `https://atomic.dev/ns/workflow/1.0.0` |
| Intent graph snapshot | `NHPD6W5UEZLVBZWX5VSQUZWR6FVHRWA2VQ2PXJPCKTW32HQXZZBQ` |
| Intent | `ATOM::continuouslee::1`, UID `01M7AD39HBA6NSJSMQX7ZKB78T` |
| Captured / declared / implementing / reviewed changes in its intent graph | All empty; `truncated: false`, no missing resources reported. |

| Change | Recorded result | Goal/intent result | Review/inverse result |
|---|---|---|---|
| `Y7DBQKHTSS7IHTE4BCTEX4FCA2BZL34TK44WJFAYXBCL5B5WCYDA` | Object available/projected; records the exact intent artifact. | Listed in `recording_changes`, **not** `implementing_changes`; no captured goal/intent path established. | No retained publication review returned by inspection; no established review-to-change-to-goal path. |
| `GYA4QZCV4Z4IPG6SC7TXAMAZHAVPOMS33DVOU3R6DTXSXFCR6MUA` | Object available/projected; `modifiesFile` identifies `skills/triage-review/SKILL.md`; five direct patch dependencies. | No direct `generatedBy`, `implements`, incoming `declaresImplementation` or `hasReviewEvidence` returned by the filtered plan. | No incoming `reviewsChange` returned; inspection returns `[]`. Review approval and its inverse chain are **not established**. |

The projection validator reports `conforms: true` for this snapshot, but its
generated-change shape has **zero applicable/evaluated targets**. That is not
proof of complete execution provenance. The separate `release → dev` candidate
report is `blocked` with eleven findings over the group; it is not eleven
findings attributed to this one skill patch.

For `GYA4QZCV4Z4I`, bounded patch evidence returns `found`, zero GraphOps, one
FileOps entry and an 8,419-byte inserted blob, with content truncation reported.
`atomic provenance trace` reports that no provenance graph explains the change.
The manual record's authorship/AI declarations do not invent a captured turn.

The actual proven paths include:

```text
Y7DBQKHTSS7I --modifiesFile--> .vault/intents/01M7AD39HBA6NSJSMQX7ZKB78T/intent.md
GYA4QZCV4Z4I --modifiesFile--> skills/triage-review/SKILL.md

Goal ? --explicit link--> Intent ? <--implementation evidence-- GYA4QZCV4Z4I
GYA4QZCV4Z4I <--reviewsChange-- Subject ? <--reviewsClosure-- Review ?
```

Question marks mark unestablished links, not placeholder assertions to persist.
The conversation and local intent explain the work to a human, but that is not
permission to fill the canonical graph by matching filenames, guessing a
`plan_id`, creating an adoption intent, or manufacturing an independent review.
Successful recording, publishing and fresh-clone tests establish those outcomes;
they do not establish the missing workflow/review relationships.

## Further reference

- [Intent graph semantics and capture/declaration boundaries](./intent-graph.md).
- [Exact review scope and policy](./retrospective-publication-review.md).
- [Typed predicate registry and inverse declarations](../atomic-core/src/kg/ontology.rs).
- [Intent graph selection and result sets](../atomic-repository/src/intent_graph.rs).
- [Review graph construction](../atomic-repository/src/repository/publication_review/graph.rs).
- [Query-plan schema](../atomic-repository/src/query_plan.rs) and
  [bounded patch evidence](../atomic-repository/src/patch_evidence.rs).
