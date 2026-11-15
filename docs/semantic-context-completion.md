# Proposed patch-context completion operation

**Status: production design for review; separately authorized sandbox experiment.**
The user first selected design-only work, then explicitly authorized an
experimental replayable patch in disposable repositories. The production format
is not finalized. The actual skill edit now records and replays in disposable
sandboxes, including a concrete commutation test; the published source is still
untouched. No ancestry mechanism is proposed. See
[sandbox results](replayable-context-sandbox.md).

A later [native prototype](native-patch-algebra-prototype.md) tests ordinary
inode-based name transport, commutation and an actual inverse where existing
context suffices. It does not implement the operation proposed here or activate
a capability. The newer replayable experiment is a distinct, separately gated
feature; it is not a production implementation of every obligation below.

## Why inode transport alone is insufficient

The inode establishes the file, not every referenced line/token owner. A read-only
name-transport trial resolves the first eleven changes of atomic-skills release
from canonical graph inodes and the operations' declared contexts. It then finds
a context gap in:

- Consumer P: `FWMGY5TBGUV5RJOMN6F4CXR5UT3DX3YNSRAWS2WBZHECZDFU6Y3A`.
- Candidate provider Q: `VXJVNZNCFTBSMOSCGOXA27CACML6YO62GSGWIH6CEROX23IR3CSQ`.

P's `skills/atomic-vault/SKILL.md` operations reference encoded foreign owner 15,
including branch 2. Q declares a self-relative branch 2 for that same file, with
a content range. Q is absent from P's native dependency closure; `extra_known`
and its frontier roots are empty. A separate native closure query confirms this,
independently of the trial's context-membership cache.

This identifies a candidate missing context, not a recovered original-author
attestation. A production validator must still establish every referenced
endpoint against the exact inode, graph ranges and token relationships. Matching
a branch number or text is insufficient. The trial currently refuses the gap;
it does not import Q from arbitrary repository state or rewrite P's dependency
index. The trial is archived outside production source.

## Reconstructed witness for this gap

The missing declaration is not missing data. A read-only reconstruction now
accounts for **all 72 occurrences** of foreign symbol 15 in P, covering **24
distinct branch identities**. Q is the only complete graph-backed provider in
the preserved selected patch set. No chronological ordering or receiving-local
NodeId is used to choose it.

Reproduce the witness with the version-controlled example:

```sh
cargo run -p atomic-repository --example reconstruct_semantic_context --locked -- \
  /path/to/atomic-skills \
  FWMGY5TBGUV5RJOMN6F4CXR5UT3DX3YNSRAWS2WBZHECZDFU6Y3A \
  15 /existing/output-directory/context-witness.json
```

The output must be new and outside the repository. The program opens the source
read-only, verifies stored objects using their original encodings, checks the
canonical file creation/index, and checks each self-relative provider branch
against its graph-owned byte ranges, line boundaries and token bytes. Every
consumer occurrence has an exact role and declaration witness. This bounded
implementation supports newline-terminated branch declarations; trunk/token
completion and unsupported occurrences refuse rather than receive partial proof.

The planner also checks that a proposed provider does not itself require the
consumer, which would make the new prerequisite cyclic. Joint cycles involving
other future completion relations remain an operation-integration obligation.

Twelve example tests cover complete occurrence coverage, missing providers,
additional equally valid providers, scope reorder, conflicting inode identity
despite equal paths/text/indices, incomplete or altered witnesses, unsupported
token occurrences, cyclic prerequisites, duplicate declarations, mismatched
semantic bytes, missing graph support, mid-line ranges and separation of context
proof from consumer operation validity. Strict Clippy for the
example passes. The actual preserved skill edit and database remain unchanged.

The witness audits direct self-parent order operations separately and always
sets `application_validated` to false. In the real consumer the direct audit
finds **zero** such self-parent operations. An earlier inference from grouped
reference lists was incorrect: roles with the same indices occurred at different
operation sites. The synthetic self-parent test ensures a complete endpoint
witness cannot conceal a detected order fault or be presented as proof that a
patch is executable.

An automatic provider choice is unique **within the recorded search scope**,
not a claim about every hypothetical patch or an original author's signature.
Once an explicit positive witness pins Q's hash, adding another candidate does
not silently redirect that witness; the tests distinguish new-planning ambiguity
from validation of an already selected immutable endpoint. `search_scope`,
`search_set_id` and the database digest are audit observations, not dependencies
on a live view or lifetime requirements for an applied relation.

The witness is not yet a native completion operation. It must not be consumed as
an unchecked sidecar override or used to modify P's hashed dependencies or their
index. The remaining step is to express/consume the completion through ordinary
patch algebra under the reviewed operation and compatibility contract below.

## Required abstraction: a graph name, not ancestry

An operation can introduce an explicit relation between a name used by a patch
and a context object that supplies that name. The key is scoped to the exact
immutable patch, not a repository's global numbering:

```text
ContextName = (consumer change hash, encoded foreign-owner symbol)
Provider    = stable provider change hash
EffectRef   = (introducing change hash, operation ordinal)
```

The encoded symbol is a literal namespace slot in the consumer's payload. It is
never looked up as a receiving repository's NodeId. ROOT is not a foreign symbol.
Names in two different consumers remain distinct even if both contain `15`.

The relation is part of ordinary typed graph/semantic state and is introduced by
an ordinary patch operation, analogous to a name-binding or attribute operation.
Any physical index is a derived cache of those operations, not another authority,
another version-control graph, a metadata repair database or a branch lineage.
The design must not be implemented by renaming the rejected JSON overlay or
changing a dictionary on reads without normal operation, conflict and inverse
semantics.

## Proposed operations (names and encoding not finalized)

```text
BindContext {
    consumer: ChangeHash,
    symbol: ForeignOwnerSymbol,
    provider: ChangeHash,
    witnesses: exact typed endpoint proofs
}

ReverseContextEffect {
    observed: EffectRef
}
```

`BindContext` adds a new relation claim. Its containing patch R must explicitly
require P, Q and the objects necessary to validate the witnesses. R does not edit
P or Q, rewrite P's hashed dependencies or modify `CHANGE_DEPS` to disagree with
the stored object. The existing dependency closure already brings P and Q into
any selection containing R. The new relation completes a context name; it is
not an assertion that one patch is a chronological parent of another.

`ReverseContextEffect` reverses exactly the observed relation effect. It requires
that effect in its ordinary patch context. It cannot mean "delete whichever
mapping is current". Reversing a reversal must reactivate the original relation
identity, rather than manufacture a substitute relation and orphan attachments.
Native encoding should reuse established exact edge-transition/inverse machinery
where it genuinely matches this contract; the model below is not a substitute
for that integration proof.

An inverse restores the relation's projected effect, not an erased audit trail
or an automatic release of all prerequisite references. Ordinary dependency-
closed insertion/removal rules still apply to the recorded operations.

The physical representation remains a design gate. A `ContextName` is not a
CONTENT byte position: do not encode its symbol or operation ordinal as a
`GraphNode` offset and alias existing vertices. Reuse a properly typed ordinary
operation/event representation and derived indexing, as appropriate to the
native graph algebra, rather than introducing a second patch-history graph.

A generic patch dependency edge P→Q is not enough: it neither identifies the
foreign name nor proves a file/line/token endpoint. `SolveOrderConflict` is also
not automatically appropriate: the byte graph can already be correctly ordered
while a semantic reference is unresolved.

## State and algebraic obligations

Visible binding claims and their exact reversals are immutable operation facts.
Evaluate the active relation claims under the ordinary selected change filter:

- No valid active claim: the name remains unresolved. Surface a semantic-context
  conflict; never fall back to a coincidentally equal local ID.
- One provider value: the name is bound, while retaining every claim identity.
- Multiple distinct providers: explicit context-binding conflict. No timestamp,
  insertion order, model identity or "latest" claim wins automatically.

Equal concurrent claims remain separately addressable. Reversing one does not
erase the other. Inserting the same change twice remains idempotent.

Required laws, for defined contexts and logical-state equivalence:

1. Composition is associative.
2. Independent context relations commute.
3. A binding followed by its exact inverse restores the prior projection.
4. Reversing that inverse restores the original binding identity.
5. Independent reversals commute and do not erase unrelated claims.
6. Conflicting claims arrive at the same explicit conflict in either order.
7. Selecting no completing operation has no completing effect.
8. Graph/patch names and attachment identities survive every admissible
   commutation and local-ID renaming.

Disjoint name keys alone are **not** proof of independence. Two otherwise valid
context relations can jointly introduce a requirement cycle. Concurrent facts
must be retained and produce an order-independent cycle conflict, not "accept
the first and discard the second". A local planning policy may refuse a cyclic
candidate before recording, but replication must not use arrival order to choose
which signed operation survives. Cycle detection uses only patch prerequisites
and the actual context relations, never a chronological lineage.

## Witness validation and authorization

Before binding is usable, verify:

- Original consumer/provider objects against their original stored encodings.
- The consumer actually declares the specified non-ROOT foreign symbol.
- Every occurrence being bound is covered, including trunk, line, token and
  predecessor references. A partially covered owner symbol is not a repair.
- Canonical inode positions establish the same file; names/paths are display
  hints, not identity authority.
- Provider declarations, graph spans and byte/token relationships establish each
  typed item. Equal text or matching numeric indices do not establish ownership.
- The new operation's context includes all referenced objects and the observed
  effect for an inverse. Validation failures leave the transaction unchanged.
- Current authorization and review policy permit this new operation. A repair
  signature records a present-day decision, never backdates original intent,
  session capture or provenance.

An automated planner must refuse when it cannot establish a unique completion.
If a human supplies an explicit resolution, that is a newly authored patch
decision with its own evidence, not a claim that missing historical evidence was
recovered. Impossible or contradictory typed endpoints still refuse.

## Agent attachment contract

P, Q and their attachment payloads keep their original hashes and bytes. The
relation is attributable to R; it must not reattribute P/Q's code to R. Attachments
to existing graph positions or operation identities remain addressable before,
during and after inverse operations. A name unresolved without R must be reported
as unresolved—not silently attached to another file or agent.

Testing unchanged envelope bytes is necessary but insufficient: native tests must
also exercise attachment lookup and blame through the completed relation. The
standalone model cannot establish this native lookup property.

## Compatibility and native integration gates

No production opcode, wire version, capability name or CLI command is allocated
by this document. A future implementation must:

1. First establish whether existing exact graph transitions or a context-valid
   commutation/reduction can express the needed relation. For example, a truly
   redundant ordering operation may reduce to identity, but that must be proven
   in valid context, not assumed because it is inconvenient to resolve. A passing
   bind/reverse model is not proof that a new opcode is necessary.
2. If a new operation is necessary, version it and activate an explicit required
   capability only after
   verified preparation. Unsupported clients must refuse before mutation or
   interpretation; they must never ignore a completing operation and write under
   the old interpretation. Verify this with actual older binaries.
3. Plan without changing original objects, indexes or working bytes. Use the
   existing operation/effect journal, expected-state leases and recovery, not a
   second journal or raw database edits.
4. Integrate the relation into ordinary semantic graph application, filtering,
   commutation, inversion, conflict display, recording and blame/diff. Merely
   storing a new metadata document is not implementation.
5. Preserve content rendering when semantic context is unresolved, while
   explicitly surfacing the semantic conflict and refusing operations that need
   the missing identity. No unchecked "best effort" semantic writes.
6. Verify the complete real release, not just the first missing relation. Other
   missing context or graph/semantic inconsistencies may remain after this one.
7. Keep the original skill edits intact, test native record and fresh-clone replay,
   and obtain independent review before any real publication or capability flip.

## Executable design checks and their limits

`atomic-repository/tests/semantic_context_algebra.rs` is the reproducible,
version-controlled design model. It models immutable bind facts and exact
reversals over already verified endpoint declarations. Four tests pass with
`cargo test -p atomic-repository --test semantic_context_algebra --locked`. Passing
checks cover independent commutation, inverse identity and inverse-of-inverse,
idempotence, equal concurrent claims, competing-provider conflicts, cycle
conflicts and reversal, missing context, wrong inode, incomplete provider items,
undeclared symbols, ROOT rejection, selected-operation scope and unchanged
attachment payloads. The earlier standalone run is retained as
`atomic-context-completion-model.log` in the approved diagnostic directory;
the repository test run is retained as `atomic-context-completion-design-tests.log`.
Strict Clippy for this test target also passes with
`cargo clippy -p atomic-repository --test semantic_context_algebra --locked -- -D warnings`.

This is an executable algebra model, not a native patch implementation. It does
not prove witness extraction from arbitrary legacy data, static-plus-dynamic
cycle handling in Atomic's actual dependency engine, native view behavior,
signature authorization, wire compatibility, journaling/recovery or correct
attachment lookup. These remain implementation and review requirements.
