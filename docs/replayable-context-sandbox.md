# Replayable context-completion sandbox

**Experimental; not for publication, production upgrade, or original-history repair.**
This is a legacy-compatibility/algebra experiment, **not** the product storage
direction. Product integration must preserve the existing compact span graph;
see [the architectural correction](semantic-context-production.md).
The user explicitly authorized storing a new feature-gated operation only in
disposable repositories. No installed binary, source skill checkout, old patch,
original dependency list, trust policy, remote, or original capability was changed.

## Implemented subset

Build `atomic-repository` with `prototype-context-completion`. It also enables
`prototype-inode-transport` and the experimental core codec variant. The persisted
operations are:

- `Bind { consumer: Hash, symbol: u64, provider: Hash }`.
- `Reverse { observed: { change: Hash, ordinal: u32 } }`.
- `DeclareOwnNames { owners: Vec<(u64, Hash)> }` within a newly recorded patch.

External Bind/Reverse carriers contain one context operation at ordinal zero
with no content/semantic writes. A binding explicitly requires both immutable
endpoints; a reversal requires the exact observed operation. Endpoint validation
reconstructs the complete graph-backed witness from verified original encodings.
Line-order occurrences are supported: `Reparent` and the foreign predecessor of
a consumer-owned newly inserted line. Foreign content/token-target updates still
refuse. The executable binding is scoped by consumer hash and encoded symbol,
not a global NodeId alias.

Own-name declarations in this experiment capture its **own current
encoding frame** before hashing/signing, covering every foreign semantic owner
exactly once with an explicit hash prerequisite. This is not reconstructed
old-author evidence. A declaration is a typed operation in its content patch at
its actual ordinal, not a metadata field or sidecar. Its symbols must exactly
match the patch's foreign names and its distinct providers must be declared
prerequisites. Native inode/line/token validation still checks its use. These
intrinsic encoding declarations cannot be independently erased by a completion
reversal. The private writer remains limited to already-marked disposable
experimental repositories; it is not the span-backed product format. The later
schema-4/default non-span detour and full-byte redb copy have been withdrawn.

`prototype_insert_composed` holds the normal operation lock, prepares a journaled
membership/capability operation, and applies the selected object set in one graph
transaction. An execution-only ordering accounts for explicit provider context;
the original stored dependencies and their index are never rewritten. Carrier
effects must match the sealed, validated projection. A carrier cannot be inserted
via ordinary recursive insertion, which refuses before inserting prerequisites.

Capability `prototype-semantic-context-NOT-FOR-PUBLICATION` version 1 lands in the
same graph transaction. Ordinary builds do not advertise support. The installed
ordinary binary refused the experimental sandbox with exit 3; a before/after
database digest confirmed no modification.

Record folds rebuild the context projection from selected immutable operations,
use completed prerequisite order, and invalidate the semantic memo on a changed
projection digest. New records retain active completion carriers for the canonical
inodes they touch and the complete required-consumer context, transitively, rather
than depending on every unrelated completion in a view. A bundled prerequisite's
other operations still need their explicit context for faithful native replay;
that does not mean the new patch modifies those other files.
Reusing already-applied ambient semantic writes under a different binding is
refused; this prototype is intended for fresh sandboxes, not in-place migration.

## Passing bounded tests

```sh
cargo test -p atomic-repository --features prototype-context-completion \
  --test replayable_context_prototype --test semantic_inode_transport_prototype \
  --test native_span_record --test span_insert --locked
cargo test -p atomic-repository --features prototype-context-completion \
  --lib context_ --locked
```

The completion tests cover saved-operation replay into a fresh repository, a real
subsequent record with retained completion requirements, replay of that record,
unchanged original hashes/attachment bytes, idempotence, equal independently
reversible claims, inverse-of-inverse, invalid endpoints/ordinals, and conflicting
valid providers in both input orders. Own-name coverage tampering refuses before
membership, and the new record replays from its hash with shifted receiver IDs.
The witness controls remain executable.

## Actual preserved skill trial

```sh
cargo run -p atomic-repository --features prototype-context-completion \
  --example replay_context_sandbox --locked -- \
  /path/to/preserved-edited-atomic-skills /new/disposable/output
```

The input is read-only and the output must not exist. The example verifies exact
object transfer and original source skill/database bytes, then writes `result.json`.
It never signs or publishes the experiment.

The stored completion for consumer `FWMGY5…`, symbol `15`, provider `VXJVNZ…`
successfully composes that consumer's required scope. This is nine selected
changes including the new carrier, not all fifteen original candidate changes.
That completed scope is also transferred and natively replayed into a second
fresh sandbox without a sidecar.

**Correction after the user's scope review:** the earlier trial reported an
index disagreement in `.github/scripts/test_validate_skill_metadata.py`, but
that file has no occurrence of the requested symbol and was not modified by the
pending edit. The witness validator was over-scoped: it treated unrelated ROOT
`SetMode`/`SetKind` operations as prerequisites for a foreign branch-name proof.
That report was not evidence of a blocker in the triage-review edit.

The validator now checks the requested symbol's actual file/branch occurrences,
while retaining canonical owner constraints and strict index checks at those
occurrences. Two new controls distinguish unrelated attributes from a real
target-reference index mismatch; all fourteen witness tests pass.

Direct audit finds symbol 15 in `HFAP35…`'s historical `atomic-vault` and
`intent-builder` operations, not its Python file or `triage-review` operations.
Scoped reconstruction proves **780 occurrences / 261 distinct branches**, with
`VXJVNZ…` as the sole complete provider. The unchanged source database is verified.

The only non-`Reparent` use in that gap was an inserted line's predecessor, also
line-order context. The sandbox now supports that narrow case. Four completed
names are needed by `HFAP35…`: symbol 12 → `EHTMCB…`, 14 → `4ASHRO…`,
15 → `VXJVNZ…`, and 16 → `FWMGY5…`. Each is stored in a separate validated Bind
patch. Together with the first FWMGY5 completion, all fifteen original objects
replay without changing their bytes or dependency lists.

**Trial 15 successfully records and replays the actual preserved v1.1.1 edit.**
The new experimental skill patch is
`KKGEG2DVJUTQFUFVJJYHQK4R3CJJPSFYJMH7DUASGEL5GBFDAJKQ`.
It replays from **its own hash**, with its declared prerequisite closure, in a
fresh sandbox. No sidecar or caller-supplied binding table is needed. Unrelated
recorded paths retain identical canonical presence/content; source skill and
database bytes remain unchanged. The patch is unsigned and unpublished.

The same example checks concrete commutation: an independently recorded new-file
patch is applied before versus after the real skill patch set. One receiver also
has seven unselected ID-allocation padding objects, and the candidate's local IDs
are verified to differ. The two compositions have identical SetId and canonical
presence/content across the original paths and the independent file. The target
skill and independent file also have identical hash-normalized CRDT line names,
line ownership/state/order, token identities/ownership/state and token ranges.

Comparison uses canonical view entries, not stale directory leftovers:
`materialize_view_to` is additive and can leave bytes for a historically deleted
path. Present entries are additionally checked against materialized bytes.
This is a passing bounded law test, not a proof of every possible commutation.
The full result is retained as `atomic-replayable-context-trial-15/result.json`
in the approved OpenCode temporary directory.

Historical trial 17 passed with the subsequently withdrawn schema-4 detour.
Those results establish only that experiment's correctness checks, **not** that
it preserved span storage or was an approved product architecture. Its artifacts
remain isolated; they are not rewritten or promoted. The product direction is
the existing span record/index/projection flow, with database-growth checks as
well as algebra tests.

## Remaining limits

- Unresolved/conflicting compositions refuse atomically. They are **not** yet
  retained as selected conflict-state operations; reversal of the sole required
  claim therefore refuses unless composed with a restoring/remaining claim.
- General trunk/index/token completion is unsupported. Context proof alone is
  not a proof of all consumer ordering laws or all legacy operation validity.
- Full crash/recovery qualification, ordinary insertion of completed closures,
  cross-view reuse of ambient semantic tables, and production format negotiation
  are not complete. The experimental opcode is not a finalized wire contract.
- Passing these bounded tests is not independent review, production promotion,
  or authorization to push the skill or experimental artifacts.
