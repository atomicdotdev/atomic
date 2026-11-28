# Lossless graph and semantic storage

New changes use a compact semantic payload **only when it is smaller after
compression** than the expanded postcard payload. The comparison uses the same
zstd level (3), includes the compact representation's version and content-binding
overhead, and chooses the raw representation on ties. Historical objects are not
rewritten.

New production writes also compare **graph-field v1** and **predictive semantic
v2** against the previous encodings. Both readers construct typed operations
directly; neither reconstructs an intermediate postcard/ATSC-v1 payload. The
schema-5 manifest's complete cost is included before enabling either new codec.

## Schema 5 production contract

- `ATGF\x01` graph payloads use fixed numeric role IDs for insertion and edge
  fields, separate delta histories and a path dictionary. Globally constant
  numeric columns store an absolute value and count instead of long zero-delta
  buffers, keeping correlated position columns close in the compression window.
  Every graph variant/flag/path/context/attribute is preserved; no edges are
  combined and linewise operations are not converted into spans.
- `ATSC\x02` payloads use **frozen ASCII predictor 1** and explicit length/kind
  exception masks. Existing IDs, counts, predecessors, byte references, deletion
  snapshots and optional metadata remain authoritative. Empty tokens, Unicode,
  nonstandard boundaries and tokens longer than the 4,096-byte prediction window
  retain their actual values through exceptions.
- The predictor lives in `semantic/predictive/predictor_v1.rs`. It calls neither
  the evolving `Tokenizer` nor platform Unicode classification tables. A new
  algorithm needs a new predictor version; historical predictor 1 stays intact.
- Schema **5** adds a **hashed CODECS section** containing
  `[5, graph_codec, semantic_codec, has_span]`. Graph codec is 0/raw or 1/fields;
  semantic codec is 0/raw, 1/ATSC-v1 or 2/predictive-v2. Header flags must match
  this descriptor. This matters because the historical file header is outside
  the change hash: interpretation cannot be authorized by header hints alone.
- New graph/predictive combinations require capability `change-format-vnext`
  version **3** before persistence. Earlier requirements 1 and 2 remain readable.
  Opening an existing repository alone does not upgrade its requirement.
- New writes select only smaller complete representations. If payload savings do
  not pay for the hashed CODECS frame, automatic selection keeps schema 2/3/4.
  Explicit forced policies remain available for testing or intentional conversion.
- `Change::graph_encoding` and `Change::semantic_encoding` retain decoded modes.
  Official historical schema-2/3/4 objects and new signed objects reserialize
  without an automatic encoding upgrade. Filesystem import preserves the exact
  received bytes. No historical files are rewritten by this feature.

The section store retains the authenticated descriptor, span and provenance
context. Reconstructed exports always verify their requested object hash, and
schema-4/5 imports must be reproducible canonically before any database write.
New noncompact schema-2/3 imports also retain provenance; older cache rows that
discarded hashed data fail closed on export rather than fabricating an identity.
Use original filesystem objects for historical schema-1 data and reimport newer
objects whose cached hashed metadata was lost.

The normative wire roles, tags and predictor contract are specified in
[CHANGE-FORMAT-SCHEMA-5.md](CHANGE-FORMAT-SCHEMA-5.md).

Both field and raw graph fallbacks in schema 5 are allocation-bounded; the raw
fallback uses guarded serde access rather than trusting collection size hints.
Graph operations, positions, edges and dictionary expansion use fixed wire costs,
not host-dependent `size_of` decisions. Semantic v1 limits remain unchanged.

### Integrated measurement

At zstd level 3 on the current **50-object** corpus (one more object than the
previous 49-object lab), all sources verified and all new unsigned objects
round-tripped exactly:

| Measurement | Previous production | Integrated production |
|---|---:|---:|
| Graph payloads | 19,275,667 B | **1,603,040 B** |
| Semantic payloads | 15,564,870 B | **9,637,356 B** |
| Complete unsigned objects | 50,044,423 B | **26,445,182 B** |

That is **47.2% less complete-object storage** than schema-4-era production,
without exhaustive numeric-transform search. The uncompacted-semantic baseline
is 106,678,684 B. Content payloads total 14,974,677 B; complete objects are still
about **1.77×** that content-only reference, not a measured Git-blob comparison.

One matched whole-file decode pass took ~1,126 ms new versus ~1,052 ms previous
production across the corpus: about **7% slower**, not the replay lab's ~2.8×
semantic penalty. Both stopwatches include hash verification, content handling,
graph and semantic typed decoding, and exclude equality checks/destruction. These
are single warm-process elapsed observations, not latency guarantees.

An actual 2,048-line native record was 4,445 B with a linewise graph encoded in
fields (228 graph bytes), versus 4,301 B with span storage (66 graph bytes). Both
had 3,550 compressed content bytes and 331 semantic bytes. The regular span
fixture kept its smaller previous semantic encoding, demonstrating the fallback.

`semantic_storage` reports `v1_object_bytes`, `v1_semantic_bytes`,
`v1_graph_bytes` and `v1_decode_ms` alongside the new selection. Both comparisons
are unsigned and retain all metadata/content; old signatures cannot authenticate
new hashes. The older exploratory labs explicitly freeze the old baseline so
their comparisons remain reproducible after the production default changes.

## Schema 4 background and compatibility

- Schemas 1–4 remain readable. Schemas 2/3 decoded by this implementation retain
  their raw encoding policy; reading an existing object does not automatically
  upgrade its semantics when it is serialized again. The pre-existing schema-1
  metadata upgrade behavior is unchanged; use exact-byte import for history.
- Schema **4** explicitly identifies compact semantics. It requires exactly one
  semantic section. Its `HAS_SPAN` flag declares an additional span section;
  span changes without compact semantics continue to use schema 3.
- Schema-4-era native writes raised `change-format-vnext` to capability version **2**
  before object persistence. Earlier readers fail closed on that requirement or
  the unsupported object schema. A version-1 capability remains readable by new
  clients; opening a repository alone does not upgrade it.
- `Change::semantic_encoding` is `Auto` for newly constructed changes and retains
  `Raw`, `Compact`, `CompactMixed`, or `PredictiveV2` on decode. Deliberately changing a loaded object's policy
  creates a different identity and requires a new signature.
- Dependencies, portable semantic owner mappings, namespace operations,
  provenance and other hashed metadata remain present and authenticated.

The native writer fixes compression at level 3, as before. The redb section store
reconstructs objects on export; it retains the schema, compact payload, span and
provenance sections and verifies the reconstructed hash. Before storing a schema-4
object it additionally requires that canonical re-encoding reproduces its hash.
Noncanonical compressed representations are rejected **before any store write**,
rather than accepted and later exported under a wrong identity. Filesystem
`import_change_bytes` retains the original received bytes instead of recompressing
them.

## What is compressed

The `ATSC\x01` payload is an uncompressed postcard envelope inside the usual
compressed, hashed SEMANTIC section. It contains:

1. A layout discriminator and hash of this change's authored content.
2. File metadata retaining trunk IDs, paths and complete trunk operations.
3. A control column retaining counts, operation tags and optional-field presence.
4. Separate line-ID and token-ID columns, each with reversible owner/index deltas.
5. Independent old/new line-number columns, byte-range and reference deltas.
6. Explicit token kinds.
7. A literal byte pool for semantic content not referenced in authored content.

Wrapping-delta/zigzag/varint encoding covers the entire `u64` identity domain,
including descending IDs. `None` and `Some(ROOT)` remain distinct. Old/deleted
token snapshots, replacement identities, moves/reparents, arbitrary bytes,
optional line numbers and original content-range metadata survive exactly.
Decoding does not tokenize, normalize identities or guess predecessor links.

Whole authored fragments are matched using verified range hints, sequential
content, or exact line lookup. A missing match reduces compression only: the
bytes go into the literal pool instead of being dropped. Adjacent content
references omit redundant offsets; repeated literal tokens share pool bytes.

The independent ID streams are an additional optimization over the first
experiment: interleaving a line index with much larger token indices produced
unnecessary jumps. They now have separate delta histories. A mixed-stream layout
is retained in the codec's internal sizing tests to measure that difference;
the production writer emits the separated layout deterministically.

## Server reads without redb

The shared core reader resolves the entire semantic payload from the pushed
`.change` file alone:

```rust,no_run
use atomic_core::change::Change;

let mut file = std::fs::File::open("pushed.change")?;
let (change, verified_hash) = Change::deserialize(&mut file)?;
for file_ops in change.file_ops() {
    // Exact authored paths, line/token operations and identities are available.
    println!("{}: {} line operations", file_ops.path(), file_ops.line_count());
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

Cross-change queries still need the explicitly declared dependency/owner closure,
not the originating repository's local tables. The codec preserves existing
owner declarations; it cannot repair missing historical owner context, and exact
numeric-ID round-trips are not proof that every legacy history is independently
resolvable. Cryptographic signature trust
still requires a public key resolved outside the object; trailer verification
alone is not identity verification. Tests verify signatures using a separate
known public key.

This is a full-file semantic read, not compressed random access. The old
`ChangeReader::semantic_sections()` API fails explicitly for compact objects
because returning content-referencing bytes as an expanded semantic payload
would be misleading. The redb store's expanded semantic-query API reconstructs
non-span compact changes with their content; its existing selective span fence
remains in place. The public low-level `format_v3::semantic::decode` also works
with a decompressed payload and authored content supplied by an object reader.

No server deployment or separate `atomic-storage` repository is changed here.
Servers and clients must use a schema-5-capable reader before exchanging new
objects.

## Resource and integrity bounds

Schema-4 section decompression and total authored content are capped at 512 MiB.
The compact decoder borrows envelope columns, checks counts against available
control bytes, and budgets up to 1 GiB of expanded operation structs, metadata
and token bytes. Fixed conservative struct costs make this budget independent
of pointer width/ABI, so automatic selection does not vary between platforms.
References and arithmetic are checked; allocations use checked
reservations. Unknown versions/tags, invalid flags, invalid ranges, trailing
payload/column bytes, overflows and content-binding mismatches fail closed.
Source byte-range metadata that is not used as a reference is preserved, even
if it is not a usable lookup hint.

The writer applies the same expansion budget and falls back to raw semantics
when semantic operations/content exceed compact-format limits. Other oversized
schema-4 sections are rejected by both reader and writer. Historical raw-format limits
are unchanged. These are format budgets, not a promise that total process RSS
is limited to 1 GiB: source buffers and compression workspace also occupy memory.

## Previous schema-4 measurements

Read-only production benchmark on 48 local objects, zstd 3 throughout:

| Measurement | Canonical raw | Adaptive production | Reduction |
|---|---:|---:|---:|
| Semantic payloads | 71,117,981 B | 15,353,238 B | 78.4% |
| Complete unsigned objects | 105,075,354 B | 49,310,611 B | 53.1% |

All 48 selected the compact encoding. Each source object was verified, and every
new object passed exact operation/content equality plus byte-identical
decode/reserialize checks. No historical file was modified. The benchmark removes
old signatures before constructing new representations; those signatures cannot
authenticate different hashes. Both sides of the comparison are unsigned.

The original 23-object corpus from the earlier experiment remains available:

| Same 23 objects, zstd 3 | Compressed semantic bytes |
|---|---:|
| Original expanded payloads | 54,509,352 |
| Initial experimental codec | 16,722,130 |
| Integrated codec with separated ID/number streams | **11,970,157** |

That is **28.4% less semantic storage than the initial experiment**, without
raising compression level. Complete unsigned objects on this matched subset are
80,239,234 B raw versus 37,700,039 B adaptive. Layout/metadata differences mean
the entire additional reduction should not be attributed to one field alone.
The focused synthetic layout test isolates the ID-stream change: 11,997 B with
mixed IDs versus 248 B with split IDs for 4,096 regular generated source lines.
These synthetic ratios are not general-workload claims.

An actual native record/import test with 2,048 generated source lines further
isolates graph overhead (one observed run, all framing/metadata counted):

| Native recording mode | Complete object | Semantic payload | Graph payload | Content payload |
|---|---:|---:|---:|---:|
| Legacy linewise graph + compact semantics | 20,526 B | 331 B | 16,327 B | 3,550 B |
| Span graph + compact semantics | **4,301 B** | 331 B | 66 B | 3,550 B |

For that regular fixture, the span-backed **complete object is only 21.2% above
its compressed-content payload**. This approaches the requested blob-sized
target for new span-backed additions, but is not a Git-blob benchmark or a claim
about arbitrary edits/history. Small changes can keep raw semantics instead.

The 48-object corpus still needs **3.33×** its compressed-content-only baseline
(14,809,128 B). This is not yet blob-sized: unchanged historical graph operations,
irregular semantic structure and old-text literals remain significant. Further
candidates are measured run encodings for irregular operation streams, compact
span graphs for newly authored objects, and selective file/range indexing. Git
loose blobs and packed history are different baselines; neither was measured here.

## Reproduce

```bash
cargo test -p atomic-core --lib change::format_v3::semantic -- --nocapture
cargo test -p atomic-repository --lib redb_change_store::tests
cargo test -p atomic-repository --test compact_semantics
cargo run --release -p atomic-core --example semantic_storage -- .atomic/changes > /path/outside/repository/semantic-storage.json
```

The benchmark reports every failure and exits nonzero if any object fails.
Totals in that case include successful objects only. It writes JSON/progress to
stdout/stderr, never object files, and does not open redb.
