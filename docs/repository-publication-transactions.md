# Repository publication transactions

This implements the record and provenance-checkpoint slices of the
[unified repository RFC](https://gist.github.com/leefaus/96231b4751ff0cd8ed425f9517d0b50d),
on top of PR #230's `.atomic/atomic.redb` consolidation. Sharing a database
handle alone does not give separate commits all-or-nothing behavior.

## Boundaries

`Repository::record` prepares and verifies V3 object bytes before opening the
writer. `write_recorded` validates that the target view identity and effective
closure still match preparation, then writes the object, native section
projections, identity/dependencies, graph, view membership/state, conflict
resolution and `change.recorded` outbox receipt in one redb transaction. The
existing graph algorithms and closure-based visibility rules are unchanged.
Git import's two writers also put their prepared objects into their existing
graph transaction instead of opening a nested writer.

`publish_bound_provenance_checkpoint` verifies the frozen owner's generation,
hash and turn, and commits the finalized provenance bytes, identity/dependency
and session projections, immutable ledger turn, manifest/head, journal
completion, receipt and `provenance.published` event together. Expensive graph
serialization happens before the writer. Preparation and hash binding remain
durable earlier journal stages so unfinished work can resume.

Unbound checkpoint publication uses the same object/ledger/outbox transaction
without completing an owner attempt. An acknowledgement cannot manufacture a
completed checkpoint: the immutable manifest and corresponding ledger entry
must already exist and the generation must match.

## Authority and retry

`CHANGE_BYTES` preserves the exact verified V3 interchange bytes, including
signatures and unhashed metadata. Native sections/chunks remain query
projections; retaining both costs additional space. `PROVENANCE_OBJECTS` stores
the finalized serialized graph. For newly published objects, redb is
authoritative; `.change` and `.provenance` files are best-effort exports after
commit. Reads and push use the canonical objects even if the export is stale or
missing. Objects from old repositories that have not been imported into these
tables continue to use verified legacy files. The `.change` wire format is
unchanged.

Operation keys and immutable receipts make retries return the original
publication. Publishing A, then B, then retrying A neither emits A twice nor
rewinds the session head. Old checkpoints lacking receipts recover their
original stored manifest; missing or ambiguous evidence is an error. The agent
reconciles a completed checkpoint before retrying Stop, including a crash
after database commit but before saving its JSON session cache.

If record committed before checkpoint preparation, recovery locates the exact
session/turn in the committed change's hashed provenance and completes its
checkpoint before retrying Stop or starting a new prompt. It repairs file-index
entries only after comparing the working bytes with the selected graph closure;
later edits remain dirty. Once a journal-backed turn is idle, another Stop
cannot infer a new turn from file changes or a stale stat cache: a prompt or
tool event must establish the next journal identity.

The outbox is durable repository-local data. This slice does not implement a
Reactor drain/acknowledgement service. Its receipts must be retained even after
delivery; deleting them would remove the current idempotency evidence.

## Schema and recovery

Schema version 2 introduces canonical objects, publication receipts/outbox and
durable allocation marks. Node/view/inode high-water marks are initialized
before the first writable mutation and advanced in the consuming transaction.
Deleting the largest committed ID and reopening cannot reuse it. Gaps caused
by aborted work are harmless. Future or malformed schema versions are rejected
before initialization writes.

Legacy database consolidation verifies source table fingerprints on recovery.
If an old executable committed to a surviving legacy file after the new
database was published, recovery refuses to retire it and preserves both
copies for explicit reconciliation. Renamed legacy files are backups, not a
live rollback target after new writes.

## Failure probes and limits

`publication_atomicity_test` launches child processes with injected errors or
immediate exit at object write, graph/ledger update, last precommit boundary and
after commit. Reopening verifies all-or-nothing tables and recovery without
filesystem exports. Other regressions cover stale generations, exact-byte
exports, optional metadata replacement, old retries and allocator upgrades.

The existing deferred path journal remains a recovery aid filtered by canonical
graph membership; it is not a new authority. File-stat caches, content-search
and record-time KG enrichment remain derived postcommit work. Workspace
materialization is not a redb transaction.

This is not completion of the entire RFC. Remaining work includes transaction
boundaries for other domain mutations, full protobuf contracts and scope
annotations, complete service coverage, restricting direct repository opens,
one `atomicd` owner per user, and Reactor delivery. The RPC/service work can now
invoke these publication operations without splitting their storage commit.
