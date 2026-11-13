# SHACL-backed retrospective publication review

This is an explicit native Atomic workflow for **present-day review of retained
evidence**. It does not manufacture an original session attestation, recover a
session MAC, alter a managed change, or grant reviewer trust automatically.

## Signed scope and executable policy

`atomic.publication-review.v2` pins:

- stable project identity;
- full root change hashes and their complete dependency closure;
- original session identities and available provenance object hashes;
- source-pinned intent lifecycle observations and retained signed revision evidence;
- exact signed/source revisions and authenticated keys of work and review intents;
- source view, ancestor state pins, and effective change-set identity;
- ordered source-view logs, the full effective membership/dependency closure,
  and complete signed intent/source documents in retained evidence;
- the executable ontology, shape content and validator version fingerprint.

The review intent must explicitly `reviews` the covered work intents. A managed
change needs resolved source-backed intent observations or an exact,
evidence-bearing, signed `implementsChange` declaration from a work intent;
a shared filename is not implementation attribution. Observed intent evidence
cannot be excluded by such a declaration. No historical TypeScript intent is blanket-approved merely because
it touches files also used by a restored Rust implementation.

Native code loads and authenticates canonical sources, checks signatures and
key bindings, and supplies current trust/freshness observations. The existing
canonical intent/schema gate remains enforced and its conformance observation is
distinct from cryptographic verification. New review policy is evaluated by the
existing PurRDF SHACL engine, using
`atomic-core/ontology/shapes/review-1.0.0.ttl`:

- nonempty, complete exact-change subjects;
- completed review and met criteria with verification evidence;
- each required reviewer check passing at the **reviewed source state**;
- coverage of observed work intents and dependency closure;
- authenticated signing-key independence, current reviewer authority and freshness.

An empty dataset fails the fixed evaluation target. Production validation rebuilds
the dataset from originals; callers cannot supply RDF `Verified` or `Trusted`
assertions as an authorization input. Different keys do not prove different
humans or meaningful review; substantive review remains the reviewer's duty.

## Retained evidence has no live-view dependency

V2 source view names and scopes are historical provenance labels, not resources
that must stay alive. Preparation captures each view's ordered own log, the
complete effective dependency closure, and the exact signed intent documents
with their source lift inputs. The outer review signature binds that bundle.
Validation recomputes each log Merkle and the membership SetId, checks canonical
dependency-index closure against immutable change objects, and authenticates
every retained intent revision/source/signature pin. It never substitutes an
unchecked `Verified` assertion or simply ignores a missing source view.

Deleting a source view, changing its scope, reusing its name or advancing it or
an ancestor does not change the content already reviewed. The approval still
covers only its exact original change hashes; later changes do not acquire that
approval. A newly prepared review of an expanded context still needs verification
records for that new context. Current intent-source freshness, signing-key
independence, current reviewer trust/revocation and ordinary original-evidence
refusals remain policy requirements; this fix does not freeze or waive them.

No permanent cleanup/review view is needed. Retained source documents and
membership are evidence inside the signed artifact, not another knowledge base
or a mutable workspace. Intent graph display summarizes these bundles rather
than duplicating full source documents into every graph node.

Legacy v1 documents retain their exact serialization and content addresses.
They retain their original live-view semantics and never silently acquire v2
snapshot authority. While the legacy view still exists, prepare/sign a v2
replacement with explicit `--supersedes` links, verify it, then remove the old
workspace through normal native APIs. Old signed artifacts remain retained.

## Operator flow

Use a CLI built from this branch, not a previously installed `atomic` binary.
These new administrative commands currently use the local repository API. They
do not introduce new network RPC methods or bypass a daemon's database ownership;
if another process owns redb, use an isolated checkout/offline administration
window rather than opening a competing writer. RPC/agent-host orchestration is a
follow-up integration.

1. Inspect the actual change closure and perform the content review and tests.
   Use a separate reviewer execution and signing identity. Create a review intent
   referencing the work intent(s); include passing `::verification` records pinned
   to the source view's Merkle for each required kind. Also set each passing
   review record's `ref=urn:atomic:view-set:FULL_SET_ID` using the effective SetId
   shown by `atomic view show SOURCE`. Own-log Merkle alone cannot observe a
   parent changing beneath a draft. Complete and sign the review
   intent under that reviewer identity, without changing its definition afterward.
2. Ensure the review and work signer public keys resolve locally and that the
   reviewer already has explicit authorization in the repository trust policy.
   Revocation wins. Do not add trust simply to make a failing gate pass.
3. Prepare the exact subject, selecting the source view explicitly:

   ```sh
   atomic triage publication-review prepare \
     --review REVIEW_INTENT_ID --view feature \
     --change FULL_CHANGE_HASH > prepared-review.json
   ```

   Repeat `--change` for multiple roots. The JSON contains `document` and base64
   `signingBytes` for external signing. Root hashes are never abbreviated.
   Inspect the unsigned graph and blockers before proceeding (see below).
4. Sign and record locally with the explicitly selected reviewer key:

   ```sh
   atomic triage publication-review sign \
     --file prepared-review.json --identity reviewer
   ```

   Or attach a standard Atomic Data Integrity proof externally over the prepared
   bytes, then submit the signed document without any private-key handoff:

   ```sh
   atomic triage publication-review record --file signed-review.json
   ```

5. Inspect the current validation report and the connected intent graph:

   ```sh
   atomic triage publication-review inspect
   atomic intent graph REVIEW_INTENT_ID --html
   atomic triage review feature --into dev --json
   ```

6. Promote only after all applicable gates pass. The existing native insertion
   interface is `atomic insert from-view feature --to-view dev`. Promotion retains
   original change hashes; it does not create a surrogate squash change.

Signed artifacts live at `.vault/publication-reviews/<artifact-hash>/review.md`
as tracked attestation entries with a JSON body. That artifact is not an original
binary `.attest` sidecar and is never installed as one. Recording the artifact on
a separate review view avoids invalidating the source view's own review pin.
The review evidence connects to the existing intent graph. Its reference
projection is rebuildable over retained documents, not a second knowledge base.

The `review` profile validates the present-day review claim. The existing
repository-wide `publication` diagnostic profile continues to describe original
capture coverage; a retrospective review does not rewrite those historical
facts or turn missing original evidence into an original-session signature.

## Gate behavior and renewal

### Findings feed the original intent

Review is a feedback loop, not a terminal list of blockers. Before promotion,
put an actionable finding on the original work/review intent, leave the relevant
criterion unmet, correct the code, documentation or metadata, run its checks,
update evidence and re-attest the current revision, then prepare and validate
again. Reuse the same intent identities; do not create replacement/adoption
intents merely to sidestep an incomplete checklist. An actual post-promotion
defect instead needs a provenance-linked remediation.

Do not confuse an approved review with an already executed merge. Insertion,
working-copy verification and deletion belong to explicit post-approval tasks;
they cannot also be preconditions for approving that same insertion. Leave those
tasks open until their operations actually succeed. A work intent with an open
execution task stays in progress; the separate content review may be completed.

Only lifecycle observations of the exact selected review are `review-context`.
They remain source-verified evidence but do not require the review to review
itself. Other work still requires explicit coverage and key independence.
Incidental reference nodes do not fail the full-work conformance shape simply
because they are outside the selected scope; missing scope is reported once per
distinct intent/change, with its canonical identity in the finding's value.
Unsigned preview checks and genuine signer-authorization decisions are reported
separately from actionable content/metadata failures. Never turn a failed test
into a passing record, rewrite original signatures or grant trust automatically.

### Active work versus terminal dispositions

A single existing review may cover multiple relevant intents; a missing-review
count is a count of missing relationships, not a demand for that many new review
intents. Reconcile each original intent against actual source and verification.
Repair stale task/status/path metadata only when its work is evidenced. Preserve
dated execution claims as history rather than calling old runs new tests.

When the product decision genuinely supersedes an old objective, its author may
attest a current `icebox` disposition with an explanation and retained originals.
The completed independent review must explicitly reference that intent. The
resolver pins it in `ReviewSubject.set_aside`, separate from `work`, and the
ontology projects `includesSetAside`, not active `includesIntent` coverage.
A reviewed change whose retained receipts observe that set-aside intent is
still complete review scope; only `includesIntent` can supply managed-change
coverage.
SHACL requires fresh, valid, canonically conforming source, the explicit completed
review and different authenticated signing keys for these dispositions too. It
does **not** assert that historical implementation goals hold in today's product.

Setting `icebox` alone never clears canonical or independent-review failures.
Inactive documents remain visible in triage with their source status, but their
criteria are historical: not counted as current met/unmet criteria, not joined
to candidates as satisfaction, and not used as current walkthrough motivation
or active code coverage. A real code candidate with only inactive task links
remains orphaned. Terminal dispositions also cannot supply native managed-change
coverage. The CLI labels their historical criteria without green implementation
checkmarks, preserving evidence without pretending a retired goal shipped.

The optional empty `set_aside` field is omitted canonically, so retained older
documents keep their original content addresses. A changed executable ontology/
shape profile still requires a fresh, explicitly superseding approval; original
signed artifacts remain retained.

The repository publication gate uses a valid review **only when the original
same-session attestation is absent**. It retains the ordinary envelope, provenance,
ledger, incomplete-session, original-signature and signer-trust checks. A corrupt,
unsigned, unverifiable or untrusted existing original cannot be replaced by a
good retrospective review. Strict discovery includes unindexed originals and
refuses registered missing/corrupt evidence instead of treating it as absent.

The verdict exposes exact retrospective artifact/change/session references.
Triage reports those as informational findings, without clearing unrelated intent,
scope, content-review or historical-attribution blockers. Evaluation errors block
shared-target readiness rather than degrading to a warning.

Stale reviews remain retained. For a genuine renewal, complete and freshly sign
the new review intent, prepare the same original project/root/change/session
scope with `--supersedes OLD_ARTIFACT_HASH`, and sign the new document. Supersession
is authenticated and transitive; missing predecessors, changed scope and corrupt
predecessor signatures fail closed. Old artifacts are inspectable, not overwritten
or deleted. A separate invalid active sibling is not hidden by a valid approval.

Final revalidation and the insertion batch share the repository-common operation
boundary. Participating vault, view/membership and evidence writers contend on it.
Metadata writes can alias an existing same-thread boundary without converting a
non-reentrant operation into a reentrant one. The already-checked
batch does not invalidate itself by rechecking its old ancestor pin after inserting
the first dependency. A failed preflight writes no membership. This is not a new
claim that the existing multi-change insertion implementation is globally ACID
against every storage failure, nor protection against privileged out-of-band
database/config/file tampering. Source/ancestor advances after promotion can make
the historical approval stale for future reuse; its signed historical meaning
does not change.

## Compatibility and limits

- `atomic-repository` has an opt-in `review-shacl` feature, requiring Rust 1.98
  through the existing validator. The CLI enables it. Default library consumers
  retain their lower MSRV and cannot accept retrospective exceptions without it.
- No automatic trust changes, historical signatures, owner-exclusion overrides,
  daemon fallback writers, or agent/model-independence claims are introduced.
- Admission is bounded: 1 MiB signed documents, 64 roots, 4096 closure changes,
  256 work intents/ancestor views/supersession ancestors, and bounded strict
  sidecar inventories, with at most 4096 retained review artifacts per inventory.
  Exceeding a budget is an error, not partial conformance.
- Signed objects require their original source revisions and public keys to be
  available. Unavailable evidence blocks validation; a signature is not proof
  that all historical execution evidence exists.
- Existing file/task-overlap triage attribution and independent work-review
  findings remain in force. This feature alone is not a declaration that live
  Reactor promotion is ready. No live Reactor data is used as a test fixture.

## Review ontology in the existing intent graph

The review module is versioned independently at
`https://atomic.dev/ns/review/1.0.0` in
`atomic-core/ontology/review-1.0.0.ttl`. It imports the workflow ontology and
reuses its intents, changes, sessions, turns, identities and attestations.
Both modules are bundled locally; validation does not fetch ontology imports.
`ReviewSubject`, `IntentObservation`, `ReviewEvidence`, `CriterionJudgment`
and `ReviewFinding` describe scope, evidence, judgments and blockers. SHACL
defines readiness, not the OWL domain/range declarations.

Review subjects, observations, judgments, verification records and findings are
nodes and relationships in **`atomic intent graph`**, using its existing
renderer, output controls and text/JSON/DOT/HTML exports. Stored review documents
appear automatically as reference topology connected to the work and review
intents. Those references are explicitly **not revalidated approval verdicts**.
Their presence never grants authority or invents implementation attribution.

After preparing an unsigned document, inspect it **before signing** using the
same command with an optional review context:

```sh
atomic intent graph REVIEW_INTENT_ID --review-file prepared.json
atomic intent graph WORK_INTENT_ID --review-file prepared.json --json
atomic intent graph REVIEW_INTENT_ID --review-file prepared.json --dot > review.dot
atomic intent graph REVIEW_INTENT_ID --review-file prepared.json --html
atomic intent graph REVIEW_INTENT_ID --review-file prepared.json --html --no-open --output review.html
```

All formats use the same intent-rooted selection. JSON includes canonical nodes
and edges; the review evaluation node carries the complete SHACL report, and
findings also appear as graph nodes. HTML retains the existing node inspector
and browser opening behavior. Reviewed changes are labeled separately from
implementation and recording evidence. Shared sessions, identities and files
remain traversal boundaries; `--max-nodes` and truncation diagnostics still apply.

The existing workflow snapshot service endpoint accepts the bounded canonical
review document, rebuilds/authenticates its evidence, evaluates SHACL against
the **exact bounded subject**, and connects the resulting nodes to the intent
graph for display. It does not validate the combined display graph as a review
approval. A document-hash acknowledgement prevents an older Reactor from silently
ignoring the preview request. Unsupported validators fail closed, with no local
database fallback or trust changes. No separate graph command, renderer or
cross-project knowledge base is introduced.

An unsigned review has a failed signature check and no review `signedBy` edge.
Existing original attestations elsewhere in the intent graph retain their own
signer edges. Preview never stores evidence,
grants trust or authorizes publication. An invalid supplied signature is still
an error, not treated as unsigned. Stale signed source documents can be shown
in preview with failed freshness, but cannot pass the publication gate.

The review resolver **does not read `plan_id`** from either provenance or the
session ledger. Null or conflicting legacy values do not create missing-plan
findings, `hasPlan`, or `implements` assertions. It recognizes exact lifecycle
receipt lines (create/update/attest/verify) paired with matching CLI commands
in retained execution nodes. These remain **recorded observations**, not proof
that a command ran successfully or that the intent governed the work. A CLI
validation result without an exact intent receipt is not guessed from nearby
text. Other evidence formats require explicit adapters, not shell execution.

Each observation pins the full provenance hash, source node ID, exact receipt
reference, session ID, matching ledger turn numbers, resolution and diagnostics.
The provenance must explicitly explain the change and a turn must pin that
same provenance hash. Missing or multiple turn bindings remain unresolved or
ambiguous; no filename, timestamp or view-context heuristic substitutes for
them. Parsed observations and verified retained revision hashes are included
in the signed subject and re-resolved on use.

A retained signed intent document is cryptographically verified separately
from current-source freshness. `retained-not-current` means its signature is
valid but it does not describe the current lift. It is visible historical
evidence, **not absent evidence and not authorization for current work**. Work
selected for approval still requires a fresh, independently authenticated
revision; automatic historical-revision selection is not implemented.

The review profile validates evidence paths directly, including explicit
inclusion/review of observed intents, source/turn consistency, exact closure,
criterion verification, freshness, key independence and signer trust. Its
shapes do not inherit the legacy projection profile's governing-plan shortcut
rule. Ordinary projection/publication profiles and original-session signature
requirements are unchanged. A receipt is not automatically a criterion
judgment; reviewer verification must still cover the exact source context.
