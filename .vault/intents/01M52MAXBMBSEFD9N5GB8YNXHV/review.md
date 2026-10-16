# Independent review: Phase 1 KG referential closure

**Verdict: blocked.** Three reproducible application bugs undermine the canonical
read model, and required CI/release files are outside the recorded candidate.
The existing acceptance harness passes, but the author's claim that all five
criteria are met is not supported by the adversarial cases below.

Reviewer: `bradley+reviewer-kg-phase1`,
`did:atomic:2FMHT6HKUEBZDK6OVFZHPS2JNCRVTFXJSSNRQB75O6O44CXCMEUA`.
This is a fresh reviewer context and separate delegated signing key. Both the
candidate provenance and reviewer model identify `openai/gpt-6.1-sol-pro`;
independence does not mean a different model.

## Sources and review boundary

- Work: `urn:atomic:intent:01M50EXZ198HPVF0CQ2Y0AEH4Z`
  (`ATOM::bradley::1`), specifically its why, five criteria, and constraints.
- Implementation: `CHUVT4O4NMAUNV6E3IWU7LP4PQD4CA4ZIF2N2DFYEBBEGS7RLBBA`.
  Message: “I agree with your recommendations. proceed to build it. And I'd recom...”.
  `atomic change ... -f json` reports author `opencode+sese`, OpenCode CLI,
  vendor openrouter, model `openai/gpt-6.1-sol-pro`, session
  `ses_ebf301078ffe87afrIWHEFkq64`, and embedded provenance.
- Author's signed lesson: `memory:01m51gg55garte6ykkvxkqcb38`, derived from the
  work intent and its AC4. `atomic memory show` reports it signed by Bradley's DID.
- Specifications: `docs/knowledge-graph-closure-phase1-plan.md`,
  `docs/knowledge-graph-closure-rfc.md`; upgrade contract:
  `docs/knowledge-graph-phase1-upgrade.md`.
- Requested promotion boundary: `deep-sea-79f7` into `noble-meadow-00f6`, both draft.
  Initial candidates contained only the implementation hash, with no closure
  additions or baggage. On resumption the hook-recorded review scaffold
  `D3ONLYGBANESEKOJIRWOBB6WQNPUT3WBUMYMEORHA4R7PREPQ4GQ` was also present.
  Its inspected diff is only this review intent's initial 45-line scaffold;
  it is not additional application implementation.
- **Observed source-view Merkle:**
  `IXTPRO5JWHQVJ3KFGS3G4TFISJFYWEW6VE2BYFF5QMD2PV5G3IUA`.
  Source: bounded `target/debug/atomic view show deep-sea-79f7`, which reports
  24 changes and SetId `4EA72BYQGQALYXWT5D57MTFUG2NIWDELX34EFZNNF75JNW2YJDVA`.
  This is a source-view pin, **not a triage report Merkle/reference**. Verification
  records use this accurately labeled substitute; no triage reference was obtained.

The full triage command exceeded 120s and 600s in the first attempt. A background
retry blocked the harness provenance hooks and was externally terminated by the
user. No further full triage was attempted. Consequently there is no report
walkthrough, content-addressed triage reference, or exhaustive per-file diff.
The narrative below follows the plan's foundation-to-consumer milestone order,
not an invented walkthrough. The candidate's JSON hunk/path list and provenance
were inspected; bounded `diff -c ... --stat` confirmed `vault_projection.rs` is a
622-line addition. A bounded token diff of the publication shape exceeded 25s
and was killed/reaped. Actual current source was read and exercised directly.

## Narrative: intent → memory → change → judgment

### M1 — common workflow language

The work intent's why asks for a canonical, referentially closed workflow model,
rather than disconnected property-graph labels. The implementation change adds
the versioned ontology, explicit atomic/external class references and datatype
ranges, typed predicates, and the retired-term inventory. In
`atomic-core/src/kg/ontology.rs:22-80,140-207`, absent domains remain distinct
from `owl:Thing`; `status`, `priority`, and the generic `explainedBy` do not
accidentally classify their subjects as intents or turns. The registry/TTL tests
also compare inverses, superproperties and chains. Independent M1 tests pass.
No M1 blocker was found in the inspected registry and consistency tests.
Source: work intent AC1 and implementation hash above.

### M2 — stable identity before projection

To support that why, the same change mints random 256-bit project IDs, strictly
round-trips canonical encodings, and stores the singleton outside view membership.
`project_id.rs:14-46` validates by decoding and re-encoding; `iri.rs:24-44,147-163`
rejects noncanonical percent encodings and intent UIDs. The project identity hash
uses `atomic.project-identity.v1\0` and proof-free JCS;
`project_identity.rs:12-79` separately checks DID/key correspondence, proof suite,
purpose, verification method, hash and signature. Full writable opens mint before
rebuild (`repository/mod.rs:619-623`), while the identity tests exercise lightweight
opens and idempotence. Independent identity/signature tests pass. This establishes
local minting/storage correctness, not the deferred transport/adoption behavior.
Canonical reference *resolution* still has R3 below, in the M3 projector.
Source: work intent AC2 and implementation hash.

### M3 — canonical producers, ingestion, and consumers

The change rebuilds derived tables in one immediate transaction, closes endpoints
before stamping the generation (`kg_rebuild.rs:19-78`), and maintains reciprocal
type indexes (`txn/write/triples.rs:345-373,418-428`). Session projection separates
session view context from change membership, resolves plans through the manifest,
and emits explicit per-change missing-shortcut findings (`session_kg.rs:287-362`).
View projection derives direct membership from each view's own log
(`vault_kg_enrich.rs:244-269`); the independently rerun membership and real-snapshot
tests cover inherited visibility versus direct membership, unrecord and reinsert.
The consumer cutover uses live syntax parsing, including view-aware content for
triage (`syntax_index.rs:138-185`), and the captured caller/layer baseline passes.
Query plans reject unknown schema versions and the service checks KG API version
1 (`query_plan.rs:154-159`; `service/mod.rs:1067-1090`).

The separate cryptographic path is appropriately conservative. Signer refresh
uses authenticated remote-origin configuration, awaits network responses before
cache writes, and checks fingerprint DID/key correspondence
(`libatomic/src/signer_refresh.rs:10-46`). Offline projection rechecks keys and
calls the real verifier (`kg_attestation.rs:110-141`). Mismatched remote keys,
poisoned raw cache rows and payload tampering did not produce `Verified` in the
independent checks. CLI and daemon pull paths refresh signers even after
download-only sidecar ingestion (`pull/command.rs:772,971`;
`services_sync_remote.rs:1384-1418,1495-1530`).

However, R1 destroys projected acceptance evidence, R2 leaves stale late-arrival
references, and R3 redirects exact canonical IRIs. These violate the motivating
read-model contract despite successful happy-path fixtures. The author's memory
explicitly derives from an AC, so references to children are a real workflow,
not a contrived unsupported input. Source: work intent AC3, signed memory
`01m51gg55garte6ykkvxkqcb38`, and implementation hash.

### M4 — real SHACL over asserted data

The author's signed lesson states that PurRDF follows `owl:imports` only from a
shapes-document anchor, `owl:Ontology`, or `sh:ShapesGraph`. The negative import
fixture uses the correct document anchor (`atomic-shacl/tests/profiles.rs:193`).
The implementation pins PurRDF 3.0.1 and supplies explicitly loaded bundles;
Atomic does snapshot adaptation, selectors, target coverage and presentation,
while PurRDF parses/evaluates shapes (`atomic-shacl/src/lib.rs:151-282`). The adapter
adds asserted types and subclass axioms, not inverse/domain/chain materialization
(`adapter.rs:18-46,100-106`); rules/entailment are rejected at the validation boundary.

The projection shapes require matching findings for missing shortcuts and SPARQL
checks for shortcut/path equality. Publication additionally checks managed plans,
provenance, session coverage and `Verified` DID signatures. Real-snapshot and
profile tests, custom shapes, selectors, complex paths, vacuous diagnostics,
blocked imports and error exit tests all passed in the harness. An extra negative
fixture independently rejected an explicit `Failed` signature. The shared-library
Rust 1.90 check passed. The intended CI/release toolchain updates are correct in
the working copy but omitted from candidate coverage (R4).
Sources: work intent AC4/AC5, signed memory above, implementation hash, upgrade doc.

## Actionable findings

### R1 — P1 / high: referencing a child overwrites its authoritative evidence

- **Locations:** `atomic-repository/src/repository/vault_projection.rs:226-234,495-505`;
  persistence consequence `vault_triples.rs:331-334`; rebuild `kg_rebuild.rs:39-48`.
- `link` emits a `vault_reference` placeholder for a resolved target. The final
  metadata loop marks *any* Task/AcceptanceCriterion/ScopeItem/Constraint/Finding
  node as owned by the referring entry, including placeholders belonging to a
  different intent. Persistence then treats that placeholder as an authoritative
  replacement and overwrites the existing node.
- **Reproduction:** store intent AC `target-ac` with status `met`, verifiedBy and
  evidence; store `memory/reference.md` with
  `derivedFrom: ["urn:atomic:ac:target-ac"]`. The AC becomes a `vault_reference`
  labeled with the raw URN, owned by `memory/reference.md`, with no status,
  verifiedBy, evidence, or authoritative text. A forced full rebuild also loses
  those fields. Executed assertions are in `repro.rs::referenced_child_is_overwritten`.
- **Impact:** ordinary memory derivation changes another intent's projected
  acceptance judgment and audit evidence; subsequent cleanup may delete that
  owner's relationships. Canonical bytes remain intact, but the read model is false.
- **Fix:** track genuinely owned nodes explicitly at creation, never infer ownership
  from class. Reference placeholders must not carry the referring entry's ownership
  or replace an existing authoritative node. Add incremental and rebuild tests for
  cross-entry references to ACs/tasks and deletion of the referring entry.

### R2 — P2 / medium: later received changes do not repair existing references

- **Location:** `atomic-repository/src/repository/changes.rs:143-152`.
- `import_change_bytes` projects only the received change. Existing vault projections
  and their unresolved-reference findings are not revisited when the target arrives.
- **Reproduction:** store a memory derived from the full legacy URN of a not-yet-local
  change. Import that exact verified change through `Repository::import_change_bytes`.
  Its Change node exists, but the memory's `prov:wasDerivedFrom` edge is still absent
  and its `UnresolvedReference` finding remains. Calling `vault_reindex_kg` immediately
  repairs both, with no authoritative data change. See
  `repro.rs::late_change_does_not_repair_reference`.
- **Impact:** projection/query results depend on ingestion order; CLI/daemon download
  paths using this repository boundary can keep false historical gaps until an
  unrelated vault write or manual reindex. RFC lines 18-22 require enrichment to
  repair missing derived facts; work AC3 is not genuinely met.
- **Fix:** repair references affected by newly available changes at the repository
  ingestion boundary (and late dependency facts), without changing direct membership.
  Add both arrival orders to repository and sync-sidecar fixtures.

### R3 — P2 / medium: canonical IRIs are resolved through a case-folded alias map

- **Locations:** `atomic-repository/src/repository/vault_projection.rs:101-108,173-179`.
- Canonical subjects and legacy aliases share the same lowercase map. Legacy
  case-insensitivity is required; canonical IRIs remain case-sensitive identities.
- **Reproduction:** store `memory/A.md` and `memory/a.md` as distinct vault rows.
  Reference the exact minted project-qualified `...:memory:A` IRI. The emitted edge
  points to `...:memory:a`, depending on map insertion order. See
  `repro.rs::namespace_identity_case_collision`. The test uses authoritative vault
  rows, so it does not require a case-sensitive host filesystem.
- **Impact:** lineage can silently move to another canonical resource; rebuilds
  preserve the wrong link. Strict mint/parse functions alone do not protect it.
- **Fix:** resolve exact canonical IRIs in a case-sensitive map first. Keep legacy
  aliases separate; reject ambiguous case-folded legacy references with a finding
  rather than selecting one resource by iteration order.

### R4 — P1 / high candidate-coverage blocker: required toolchain changes are untracked

- **Locations:** `.github/workflows/ci.yml:149-155` and
  `.github/workflows/release.yml:128-129` (existing working-copy content).
- Both files show as `??` in Atomic status. A bounded candidate diff-stat restricted
  to those files reports no changes; the later scaffold-only change does not add them.
  These files contain the plan's required 1.90 library / 1.98 CLI split and release
  toolchain update. They will not travel with the reviewed implementation reference.
- **Fix:** have the author include the necessary intended workflow edits through the
  recording hooks, with exact task file references and appropriate review coverage.
  Preserve the other untracked ignore/prepare-release files and determine their
  intended ownership separately. The reviewer has not edited these files.

## Work-intent acceptance judgment

| Author AC | Independent judgment |
|---|---|
| AC1 ontology/registry/legacy dispositions | Inspected and targeted tests pass. |
| AC2 local identity/minting/strict IRI round trips | Inspected and targeted integration/signature tests pass. R3 is in reference resolution, not the mint/parse functions. |
| AC3 canonical producer/consumer cutover and repair | **Not satisfied:** R1, R2, R3. Membership-only and baseline cases do pass. |
| AC4 real SHACL and library MSRV | Runtime/adapter/engine and 1.90 checks pass; candidate delivery remains incomplete under R4. |
| AC5 full workflow harness | Harness exists and passes; its coverage misses R1–R3, so it cannot establish full plan/RFC satisfaction. |

The author attestation is also stale: bounded
`target/debug/atomic intent verify ATOM::bradley::1` exits 2 with
“attestation is stale: the intent changed since it was signed.” This corroborates
the stale listing; the precise historical edit causing it was not established.
The author must re-attest their final corrected state. The reviewer will not sign
on their behalf or mark their intent done.

## Checks independently executed

All resumed Atomic commands were serial. Potentially long subprocesses used an
explicit timeout and a `finally` path that kills/reaps the process group before
returning. No background repository opener was started after resumption.

1. `cargo build -p atomic-cli --locked`: pass; Darwin linker emitted an unwind-table
   size warning. This was a workspace CLI build, not the installed pre-cutover CLI.
2. `ATOMIC_BIN="$PWD/target/debug/atomic" TMPDIR=/var/folders/1h/42fls18s3db5l8nrkffk1nm00000gn/T/opencode bash tests/harness/46_knowledge_graph_closure.sh`:
   pass within the 150s bound. CLI closure fixture and pre-cutover callers/triage
   result-set baseline pass. Includes core kg_iri (3), kg_ontology (3), vocabulary
   lint (1), repository kg_projection (3), project_identity (2), SHACL profiles (6),
   W3C fixture (1), real workflow snapshots (4), libatomic signer_refresh (2).
3. `cargo +1.90.0 check --workspace --exclude atomic-cli --exclude atomic-shacl --locked --quiet`:
   pass against the workspace lockfile.
4. `cargo test -p atomic-repository --lib repository::project_identity::tests::signed_identity_hash_and_proof_are_independently_verified --quiet`:
   1 passed.
5. `cargo test -p atomic-core --lib kg_ --quiet`: 14 passed.
6. Standalone scratch package, using this checkout's path dependencies and
   `.vault/intents/01M52MAXBMBSEFD9N5GB8YNXHV/repro.rs` as its binary:
   `CARGO_TARGET_DIR="$PWD/target" cargo run --offline --manifest-path /var/folders/1h/42fls18s3db5l8nrkffk1nm00000gn/T/opencode/kg-phase1-review/Cargo.toml`:
   final run exits 0 and confirms **all three reported bugs**, rejects a `Failed`
   signature in publication SHACL, rejects a wrong DID/key pair and poisoned raw
   signer cache, verifies the valid key, and rejects signed-payload tampering.
   The scratch package resolves its own offline lockfile; the workspace tests/MSRV
   check above use the workspace lockfile. An initial extension of this fixture
   failed to compile for a missing trait import; that reviewer-only import was
   corrected before the successful final run. No application source was changed.
7. Delegation verification with `--offline`: delegator proof valid, delegate key
   matches, not expired; revocation not checked. `atomic identity list` confirms
   Bradley remains the default identity.

No independent claim is made here that the entire workspace suite, clippy,
rustdoc, every constructor/ingestion interleaving, or a real authenticated remote
server was rerun. The full triage linkage/blast-radius report is unavailable.
These are remaining coverage limits, not substitutes for the concrete bugs above.

## Outcome and next action

Keep this review `in_progress`, with failed correctness/coverage criteria unmet,
and sign the blocked judgment under the delegated reviewer identity. The author
should fix R1–R3 in the draft, add regression cases, include required workflow files,
and re-attest. Re-run independent review with a bounded, functioning full triage
report before promotion. ProjectIdentity transport/adoption, OWL materialization,
mandatory publication gates and persistent source-structure KG remain correctly
deferred by this Phase 1 scope.
