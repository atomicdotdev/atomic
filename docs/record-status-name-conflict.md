# Recording a name-conflict resolution

## Symptom

After independently creating the same path on two views and inserting both
changes into one view, materialization reports a name conflict. Replacing the
markers with the content of the identity selected by `TREE` leaves `status`
reporting a modification, but `record` previously returned “Nothing to record”.

The regression is `tests/harness/43_record_status_name_conflict.sh`. It first
reproduced that failure against the unchanged parent branch, before the
implementation was modified.

## Cause

`TREE` selects one inode per path; `REV_TREE` can retain multiple independent
identities claiming that path. Materialization examines all live, view-visible
claimants, but recording previously diffed only the selected inode's content.
Choosing that content exactly produces no content edits, while the unresolved
namespace conflict still requires a patch.

Deferred TREE replay also needs to distinguish an inode's reverse path from
ownership of the forward path. Removing a stale claimant by path alone can
remove another inode's mapping. Changing which claimant TREE selects must
preserve the competing reverse mappings until the graph resolution makes them
inactive for the current view.

## Causal identity invariants

- A descendant draft edits the inherited inode, even when its own change log
  is empty. Parent-chain visibility is sufficient; no ambient fallback is used.
- Sibling drafts may independently create different identities at the same
  path, even with identical bytes. Combining them produces a name conflict.
- Inserting the same creating change preserves its inode and external graph
  identity. Repository-local inode numbers are not a cross-repository identity.

These are asserted directly in
`atomic-repository/tests/causal_file_identity_test.rs`, in addition to the CLI
scenarios in harness 43.

## Atomic representation

The resolution is recorded as graph operations, with dependencies on the
creating changes of the identities involved and the name bindings being removed:

- An empty `SolveNameConflict` edge update identifies the retained inode. TREE
  lifecycle replay and insertion interpret it as selecting that identity for
  the path, including when the patch is already present in the canonical graph.
- A separate `SolveNameConflict` update tombstones the competing **name's
  incoming FOLDER edges**, not its content edges. Its inode, content, semantic
  trunk, branches, and tokens remain intact. A concurrent rename can therefore
  preserve that identity under a different path.
- Any content edits to the retained identity use the existing edit pipeline.

Only identities with live names in the recording view's effective filter are
candidates. Restored bytes matching an existing side prefer that identity.
Ambiguous matches or newly edited content use a stable ordering of external
creating-change hash and inode position. The global TREE occupant and local ID
allocation order do not select the winner. Matching bytes are used only when
recording an explicit resolution; they never deduplicate independent creates.

TREE replay records the removal of a specific path binding, rather than an
unconditional inode deletion, so it does not erase a concurrent rename. Eager
rename handling also checks forward-path ownership before deleting a stale
reverse mapping's path.

The canonical graph and historical change objects remain available. A sibling
view whose effective filter excludes the resolution still sees its original
content. A child inheriting the resolution sees the resolved identity. The
regression checks both perspectives, dependency membership, restoration, and
round-trip materialization for either original side and for newly edited content.
It also inserts only the resolution patch into a separate view, letting Atomic
bring in its dependency closure, and verifies the resulting materialized file.

### Additional test-first findings

The original resolution attempt deleted competing content. A new CLI regression
first demonstrated that resolving the conflict and combining a rename lost the
renamed file. A structural test independently caught non-FOLDER deletion edges
inside `SolveNameConflict`. Both pass after making resolution namespace-only.

The reverse recording order then exposed selection of the wrong inode through
TREE. The new identity-selection policy and ownership checks pass both orders
while preserving both original identities and their distinct contents.

Errors resolving the competing graph identities propagate with the path instead
of becoming a clean no-op. Recording errors also surface when no file could be
recorded.

## Repair workflow

On the affected view, replace the conflict markers with the intended file
content, then record the path normally:

```sh
atomic status
atomic record path/to/file -m "Resolve file name conflict"
atomic status
```

This works even when the intended content exactly matches the currently selected
inode. Removing and re-adding the file is unnecessary.

## Verification

```sh
cargo build -p atomic-cli --release
ATOMIC_BIN="$PWD/target/release/atomic" bash tests/harness/run_all.sh 39 41
cargo test -p atomic-core -p atomic-repository --lib
cargo test -p atomic-repository --test causal_file_identity_test
```

Suite 39 is the unchanged prior view-switch regression. Suite 41 adds the
record/status regression and view-filter checks.
