# Native patch algebra prototype

This is a working, non-default prototype for a bounded name-transport case. It
drives Atomic's actual repository, graph, semantic, insertion, materialization
and record paths. It is not the standalone context-binding design model.

It does **not** implement the proposed context-completion opcode, fix arbitrary
legacy history, introduce ancestry, change a stored format or flip a repository
capability. Do not enable it on a production repository or publish its output as
a general compatibility fix.

## Run

Default-build negative control:

```sh
cargo test -p atomic-repository \
  --test semantic_inode_transport_prototype --locked -- --nocapture
```

This runs an explicitly named diagnostic control which expects the existing
default build to render correct bytes but misbind the inserted line. Its green
result does **not** mean default behavior is fixed. Remove or replace that
expectation when a supported fix graduates.

Working native prototype:

```sh
cargo test -p atomic-repository --features prototype-inode-transport \
  --test semantic_inode_transport_prototype --locked -- --nocapture
```

Tests create and clean up their own disposable repositories. They do not use a
remote, install a binary, edit the real skill checkout or grant reviewer trust.
Synthetic session attachment data stays in normal draft fixtures; publication
gates are not disabled or represented as satisfied.

## Native mechanism

The feature adds a localizer at the native FileOps application boundary and at
the record resolver's semantic-fold boundary. It leaves the original `Change`,
its stored bytes, dependency declarations and hash unchanged.

For operations with external names, the localizer:

1. Obtains a single file inode from the operation's explicit graph context,
   correlating graph and semantic entries only inside that same immutable patch.
   It does not choose the file from an ambient path lookup.
2. Resolves the inode's existing trunk and checks the file index/owner binding.
3. Restricts candidate names to the operation's declared patch context and
   canonical graph endpoints. Existing typed branches/tokens must belong to the
   same inode; token references must also name the correct line.
4. Accepts only a unique, consistent, injective owner substitution. It never
   prefers a candidate because its local number equals the encoded number.
5. Transports a copy of the semantic operands through the existing failure-atomic
   `FileOps::remap_referenced_node_ids` primitive. ROOT remains self-relative.

The graph writer then applies ordinary graph operations and the localized
semantic operands in its normal transaction. A refusal aborts view publication.
The reader's fold uses the same transport, rather than inventing a second
interpretation or a persistent historical override map.

## Native experiment

The fixture records actual non-span patches:

- A, B and C create independent files.
- E appends a line to A's file and requires only A.
- R removes E's inserted line and requires E's context.

R is checked to contain graph edge deletions of exactly E's vertices introduced
by E. It does not recreate the baseline file or manufacture new baseline line
identities. All three original A line identities remain alive after R. Thus this
is an actual inverse edit in its tested context, not merely unrecording E.

Source-local unrelated registrations deliberately shift its owner numbers. The
same stored patch bytes are inserted into fresh receivers in all **20** orders
consistent with `A < E < R`; B and C may commute through that sequence.

Every order must satisfy:

- Identical final file contents and order-independent membership SetId.
- E's line belongs to A, never B/C or an accidental numeric alias.
- R tombstones E's line while retaining the original A lines and inode.
- Exact session attachment and immutable stored change bytes remain unchanged.
- Repeated insertion is idempotent, and removing E while R needs it refuses.
- Removing/reinserting R changes projected content as expected.
- Native recording works on **A itself** both while R is absent from the view
  and after R is reinserted, despite ambient rows from other selected states.
- Independent B/C file content remains unchanged through those records.

Negative cases must reject a missing branch, a token from another line,
contradictory branch operands, an undeclared context and an ambiguous owner—even
when one ambiguous candidate numerically matches the encoded symbol. The view's
SetId and graph-derived file bytes must remain unchanged after refusal.

The initial strengthened run passes all four feature-enabled integration tests,
including all 20 legal orders and real recording on the affected file both with
and without the inverse selected. Strict Clippy for this feature/test target
passes. The default-build diagnostic control reproduces the original misbinding.
Logs are retained in the approved diagnostic directory as
`atomic-native-prototype-verified.log`, `atomic-native-prototype-clippy-final.log`
and `atomic-native-prototype-control.log`.

Additional scoped regression runs pass:

- The existing seven native `merge_property_tests` with the prototype enabled,
  including generated disjoint/boundary edits, same-anchor conflicts,
  round-trip content, idempotence and conflict-state honesty.
- Forty existing span insertion/recording tests in both prototype and default
  builds. The existing span localizer remains separate and unchanged.
- The four context-operation design-model tests in the default regression run.
- Strict Clippy for this prototype integration-test target in both modes.

Full workspace/all-feature testing was not performed. These are finite native
experiments, not a general proof of patch-theory laws or a production migration.

Two independent producers also replace the same base line differently. In both
insertion orders the native graph must surface both alternatives as an explicit
conflict, never silently select the last applied value.

## Deliberate limits

### Trial against the actual pending skill edit

A separate prototype-linked native executable replayed the preserved edited
checkout into a new disposable repository, transferring the original verified
change bytes without re-encoding them. Its selected source scope contained the
14 published release changes plus the local intent/attestation receipt. It also
preserved the pending v1.1.1 skill edit containing the feedback loop and grouped
multi-intent review instructions.

The native replay accepted the first **11 of 15** changes, then refused
`FWMGY5TBGUV5RJOMN6F4CXR5UT3DX3YNSRAWS2WBZHECZDFU6Y3A` with
`inode prototype: no typed owner in explicit graph context`. Assembly of the new
skill edit was **not reached**. This is consistent with the separately verified
missing-context case; it is not evidence that the original edit is invalid.

The source skill and `.atomic/atomic.redb` contents were hashed before and after
the trial and were byte-identical. Nothing was signed, published or pushed, and
no capability was changed. Diagnostic artifacts in the approved directory:

- `atomic-skill-edit-prototype-trial.log`
- `atomic-skill-edit-prototype-trial/result.json`
- `atomic-skill-edit-prototype-trial/preserved-edited-skill.md`

The executable was only a diagnostic replay/assembly probe. Even on success, it
was configured not to save or apply an unsigned candidate. It is not a new
supported Atomic CLI command or a substitute for the real publication workflow.

A subsequent read-only [context reconstruction](semantic-context-completion.md#reconstructed-witness-for-this-gap)
found a complete witness for the blocked foreign symbol: 72 occurrences, 24
distinct graph-backed branch declarations, with VXJVNZ as the unique complete
provider in the preserved scope. That witness has not yet been applied as a
native completion operation, so the replay trial's refusal is still the last
native application result for the actual history.

### Unsupported cases

- Context is available only from the existing declared dependencies and graph
  endpoints. Missing context such as the real release's FWMGY5/VXJVNZ gap still
  refuses. This prototype does not implement a completion operation.
- It relies on a healthy canonical inode/trunk index and typed prerequisite
  declarations built by the normal engine. It is not a semantic-index repair
  for repositories whose existing caches are already corrupted.
- Multiple inode anchors, ambiguous provider sets, unsupported external
  creations and unproved context are rejected. Self-only operations retain the
  existing native path; this is not a complete validator for hostile payloads.
- The tests establish the listed finite cases, not general commutation or
  inverse theorems. Arbitrary dependent edits to the support of an inverse,
  all move/rename cases, all cross-view semantic effects and complete
  agent/blame lookup still need broader coverage.
- Candidate enumeration and context construction are prototype code, not a
  measured large-repository optimization.

The normal build remains unchanged. The separate
[context-completion design](semantic-context-completion.md) still requires review
before any new persisted operation or capability is implemented.
