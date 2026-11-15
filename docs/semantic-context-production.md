# Semantic context integration into the compact span graph

**Corrected direction:** use the existing span-backed graph and semantic
projection. The purpose of the span work is to keep persistent graph/database
growth small. Identity correctness must be integrated into that design, not used
to create a parallel default linewise/non-span product path.

## Architectural correction

The preceding schema-4 proposal and `change_original_bytes` redb table were an
assistant detour, not an approved extension of the span design. Passing their
record/replay tests did not establish that they preserved compact storage.

That detour has been removed from source: no schema-4 discriminator, default
writer, standalone context section/capability, HashedChange field, or complete
encoded-object redb table remains. No live database, original change or capability
was rewritten to retract it. Disposable schema-4 trial artifacts are historical
only; do not publish them or reinterpret their hashes as span changes.

The useful shared `SemanticNameFrame` validator remains in memory and is used by
the existing span owner-map validator/localizer. It changes neither the existing
span encoding nor the graph storage model. Experimental legacy completion facts
and their algebra tests remain separately gated; they are not a replacement for
span-native recording or evidence that product integration is complete.

## Existing architecture to preserve

- `atomic-core/src/pristine/span_patch_storage.rs` explicitly keeps content in
  change objects. Its tables index file identities, authored span ranges and
  range edits—not another complete copy of each change.
- `SpanPatch.semantic_owners` already carries portable owner hashes where CRDT
  compatibility operations require them. Reuse that representation and its
  capability fence rather than adding a competing default encoding.
- Span semantic projections retain exact original authored fragments and their
  hash-derived identities. Do not manufacture a single historical CRDT author
  for a line composed from several original changes.
- Preserve original hashes, dependency facts, signatures and agent attachments.
  Any completion of missing legacy context must be an explicit validated patch
  operation, not ancestry, path-based rebinding or an unchecked sidecar.

The existing storage work and its release gates are documented in
`docs/git-span-import-handoff.md` and
`.vault/goals/span-backed-storage-integration/work-tree.md`. Historical benchmark
numbers there are not new measurements or permission to bypass unfinished gates.

## Storage regression guard

`atomic-repository/tests/span_storage_guard.rs` uses a disposable, span-enabled
repository and the ordinary native record path. It records a 2,048-line root,
then eight small edits, compacts only the closed disposable database and reopens.
The guard requires:

1. Each edit stays a typed span range edit, with less than 256 inserted bytes.
2. No content `Edit`/`Replacement` graph hunks are emitted as a fallback.
3. GRAPH/INODE_GRAPH row counts remain constant through the edits and reopen.
4. Ordinary recording does not duplicate content into the optional database
   content cache.
5. Compacted database growth remains bounded, and reconstructed bytes are exact.

Observed on the corrected source: compacted `atomic.redb` grows from
**2,015,232 to 2,138,112 bytes**; GRAPH/INODE_GRAPH retain **4 / 2 rows**. This is
a bounded native regression fixture, not a real-project size claim or a proof of
general scalability. The initial native semantic baseline is included; rows were
not deleted to manufacture a saving. Change objects, journals and other required
metadata remain intact.

## Legacy graph ranges in the ordinary span path

The default-built implementation now supports forward span edits over verified
legacy byte graphs. It does **not** retrofit the old numeric CRDT names or add
the sandbox's `Bind`/`DeclareOwnNames` operations to the product format.

- A file is identified by its original creator hash and `GraphOp::FileAdd`
  ordinal. Its canonical graph position resolves the receiver-local inode.
  Paths and sender CRDT indices are not identity authorities.
- The read-only adapter checks an unambiguous, non-zombie legacy graph under
  the explicit dependency closure. Physical vertex splits are normalized to
  their immutable original insertion ranges. Every visible byte must have one
  original declaration for that file.
- Span payload **version 3** extends the existing `SpanOp::Edit` algebra to those
  verified legacy sources; existing version-2 bytes remain unchanged. A
  `ProjectedSpanEdit` binds the exact semantic baseline. No original change,
  dependency, signature, or attachment is rewritten, and historical content is
  not copied into redb or re-attributed to a new author.
- `span-legacy-context` version 1 and the existing projected-semantics fence
  prevent older clients from silently ignoring the interpretation. These reader
  fences remain while immutable objects remain stored, including after undo.
- Ordinary record, import and recursive insertion use the existing transactional
  span index and operation journal. The only extra legacy binding is the small
  derived portable-file-to-inode index. Missing typed span indexes cannot be
  relabeled as legacy sources.
- Rendering checks stored edit rows against hashed operations and requires
  every selected edit to be reachable. Bounded legacy output validates the
  fragment layout and edits without concatenating the original file first.
  Unavailable, ambiguous or conflicting ranges refuse rather than inventing
  semantic ownership.
- Canonical repository content APIs are already view-aware. The low-level
  ambient CRDT output API now refuses with `SpanContextRequired` when authored
  span projection supersedes its retained cache; it never guesses a latest view.

### Real candidate trial

`cargo run -p atomic-repository --example span_skill_replay --locked --
<preserved-input> <new-output>` uses only ordinary APIs and default features.
The first successful run retained fifteen original objects, recorded the actual
skill edit as `HTRBETCOS5P7KWQJSK5DQC6HFW3OBEVZZIT65RXQTVBGWWCIOL7Q`
(8,419 inserted bytes, 159 span operations), replayed from the candidate's own
hash and passed independent-file commutation under shifted receiver allocations.
The projected line/token identities and unrelated entries matched; original
encodings/attachments were transferred unchanged and the source database and
skill bytes were preserved. This trial is not a signature, publication, or an
independent review verdict.

Fresh trial 3, after capability-read hardening, records candidate
`OQRSJCHWXVSXWTQILIUABBB4JCVPGC3NHJPNXCCZFRCVM6S5IE5Q` with the same
8,419 inserted bytes and 159 span operations. GRAPH/INODE_GRAPH stay at
**14,218 / 14,206** through the real skill edit. Both independent-file insertion
orders yield **14,222 / 14,208** rows and equal authored line/token identities.
Reopen after disposable receiver compaction preserves state; the compacted
receiver is **4,435,968 bytes** (not a matched growth benchmark). The installed
older client refuses the sender with exit 3 naming `span-legacy-context`; its
database SHA-256 is unchanged after refusal. Results are retained in
`atomic-span-product-completion-trial-3/result.json` in the approved temporary
directory.

Six focused `legacy_span_context` tests pass: empty/repeated edits and fresh
replay; malformed ordinal/provider/version/declarations/baseline refusal;
retained fences after record/reopen/undo; disjoint cross-view composition with
an unaffected parent; atomic refusal of overlapping edits; and refusal rather
than stale bytes when an edit index or required capability is missing. These
are bounded examples, not exhaustive conflict/recovery qualification.

Broad release acceptance remains open for remote/server compatibility, all
producer/consumer audits, cross-view/conflict combinations, and fault-injected
interruption/recovery qualification. Do not infer deployment approval from the
passing disposable trial.

```sh
cargo test -p atomic-repository --test span_storage_guard --locked -- --nocapture
cargo test -p atomic-repository --test native_span_record --test span_insert --locked
cargo test -p atomic-core --lib change::semantic_context --locked
cargo test -p atomic-repository --test legacy_span_context --locked
```

The extended storage guard also covers a legacy 2,048-line root followed by eight
ordinary span edits: GRAPH/INODE_GRAPH remain **4,100 / 4,098**, each edit inserts
less than 256 bytes, and compacted database size grows from **2,539,520 to
2,678,784 bytes** in the final measured run. Its span index contains one file
binding, sixteen authored nodes and sixteen edits; retained CRDT cache rows stay
at **1 trunk / 2,048 lines / 16,384 tokens**, without new fabricated history.
The existing native root guard remains **4 / 2** graph rows, with compacted DB
**2,015,232 to 2,150,400 bytes**. Historical sizes above differ slightly with journal metadata;
neither fixture drops required metadata or rewrites old objects to make a saving.
Exact-byte import and hash-root replay are now part of both storage fixtures.

Final validation: the complete core/repository run passes **6,198 tests** including
doctests (with the existing ignored cases); the subsequently extended focused
legacy suite passes all **6** tests. The feature-gated completion/inode
compatibility suites pass **9** tests. Workspace/all-target compilation, strict
workspace library Clippy and strict targeted test/example Clippy pass. Strict
workspace/all-target Clippy remains blocked by the pre-existing `useless_vec`
test lint at `atomic-core/src/pristine/git_closure.rs:276`; that unrelated file
was not changed. Logs: `atomic-span-full-tests-final.log`,
`atomic-span-legacy-context-tests-qualified.log`, `atomic-span-storage-qualified.log`,
`atomic-span-compat-qualified.log` and `atomic-span-final-*.log` in the approved
temporary directory.

## Remaining integration

The ordinary compact-span record/index/projection implementation and the actual
skill reproduction are now present. Continue qualification at those boundaries,
not by restoring the discarded schema-4 path. Keep normal publication, lock,
lease and recovery policy. Remote/server and all-producer compatibility, detailed
crash-injection cases and independent promotion review are still open. The
experimental composed executor delegates newly recorded span candidates to the
same product span writer; its private completion operations stay feature-gated.

Intent `ATOM::continuouslee::253` remains in progress. The former schema-4 results
are withdrawn as product acceptance, not relabeled as successful span integration.
No binary was installed and no experiment was pushed.
