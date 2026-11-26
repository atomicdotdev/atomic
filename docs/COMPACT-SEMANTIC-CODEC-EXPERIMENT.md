# Compact semantic codec experiment

This is a sizing experiment, **not a production change format**. It does not
rewrite `.change` files or modify Atomic's writer, reader, identity, signatures,
database schema, or server.

## Reproduce

```bash
cargo test -p atomic-core --example compact_semantic_codec
cargo run --release -p atomic-core --example compact_semantic_codec -- .atomic/changes 3 > compact-semantic-level3.json
```

The first argument can also name one `.change` file. The second is the zstd
compression level (1–19, default 3). JSON goes to stdout; progress and failures
go to stderr. Put result files outside the repository if they should not be
recorded. A failed object is reported explicitly and makes the command exit
nonzero; totals then cover only successful objects.

The example reads only `.change` files. It never opens redb or a Repository.
For each input it verifies the source object's trailer hash and content-chunk
hashes, decodes the original semantic section, encodes the experimental payload,
and decodes using only that payload and the authored content. Both `FileOps`
equality and byte-for-byte equality with the original expanded postcard semantic
section must hold.

## Encoding under test

The codec preserves the complete authored semantic operations; it does not
retokenize content or infer semantic identities from the resulting text.

- File identity, path, and every trunk-operation variant are retained.
- A control stream retains operation tags, list lengths, and optional fields.
- Separate numeric columns use reversible wrapping-delta/zigzag/varint encoding
  for owners, indices, line numbers, byte ranges, and content-reference offsets.
- Token kinds are retained explicitly in a separate column.
- When a semantic token sequence exactly matches an authored fragment, token
  bytes reference that content. Consecutive references omit their offset.
- Old/deleted text absent from this change, and fragments not matched by the
  experiment's lookup, use a deduplicated literal pool **inside the payload**.
  No information is dropped when a content match is unavailable.
- The columns, literal pool, and a hash binding the supplied content are encoded
  with postcard and compressed together with zstd. Magic/version and the content
  hash are included in the reported size.

Fragment matching tries the recorded content-range hint, the next content
position, and an exact whole-line lookup. It does not search all content for every
token. Lookup limitations affect compression only, not correctness. Even unusual
or reversed recorded content ranges survive unchanged as metadata; they are not
treated as valid lookup hints.

## Reading the numbers

The JSON reports:

- `original_semantic_bytes`: actual compressed semantic payload bytes on disk.
- `baseline_semantic_bytes`: the original expanded semantic payload recompressed
  at the requested level, to distinguish codec savings from compression-level
  changes.
- `compact_semantic_bytes`: the entire experimental compressed semantic payload,
  including its magic/version and content binding.
- `projected_object_bytes`: original object size minus its original semantic
  payload plus the experimental payload. All other original bytes remain counted,
  including graph/content sections, dependency/owner context, framing and signature.
- Encode/decode times are single warm-process measurements, not rigorous latency
  benchmarks. Encoding includes fragment indexing, serialization and compression;
  decoding includes decompression, content hashing and operation reconstruction.

These are projected object sizes, not valid rewritten objects: changing the hashed
semantic section would require new change identities and signatures. Graph-span
compaction is deliberately not included in the projection.

## Server-side losslessness boundary

The decoder needs only the encoded semantic payload and the change's authored
content. Existing dependency and portable-owner declarations remain necessary
for cross-change interpretation; they are not replaced by a database-only lookup
or excluded from projected object sizes. Recovering identical numeric names is not,
by itself, proof that historical objects have complete portable owner declarations.

This experiment proves exact semantic reconstruction, not a complete new format,
remote push/pull integration, historical blame engine, or authentication scheme
for the experimental envelope. Source hash verification is not Ed25519 signature
verification. A production format would need an authenticated section contract,
resource limits hardened for hostile inputs, and a selective-read directory.

The prototype decodes a whole semantic section and hashes the supplied content;
it does **not** yet provide per-file/per-range compressed random access. This
explicitly retains losslessness but defers server query-latency optimization.

Git blobs contain bytes rather than Atomic's semantic/causal information. The
reported compressed-content bytes are a useful local baseline, **not** a measured
Git loose-object or Git-pack comparison.

## Results

Local corpus: 23 objects (schema 1/2, no schema-3 span objects), containing 6,470
file-operation containers, 1,631,730 line operations, and 9,533,568 tokens counted
by the existing `FileOps::token_count` API. All 23 source object hashes and content
chunks verified, and all 23 expanded semantic payloads matched byte-for-byte after
the experimental round-trip. There were no skipped or failed objects.

At zstd level 3 (the current writer default):

| Measurement | Original | Experimental | Reduction |
|---|---:|---:|---:|
| Semantic payload bytes | 54,509,352 | 16,722,130 | **69.3%** |
| Whole-object bytes (projected) | 80,243,191 | 42,455,969 | **47.1%** |

Recompressing the baseline at level 3 exactly reproduced its original compressed
size, so this comparison is not benefiting from a higher compression level.
The largest object projected from 43,277,858 to 22,913,772 bytes, with its semantic
payload reduced from 30,327,896 to 9,963,810 bytes.

The original content chunks total 11,855,733 compressed bytes. The projected
objects are still **3.58×** that content-only baseline: this first codec is a
substantial reduction, but **not yet close to blob-only size**. Unchanged graph
operations remain counted, as do 16.7 MB of compact semantic metadata/literals.

One release-process pass measured approximately 634 ms total encode time and
385 ms total decode time over the corpus, excluding original file reading and
baseline recompression. These are observational timings, not latency promises.

At level 9, the compact semantic payloads total **14,518,607 bytes** and projected
objects total **40,252,446 bytes**: 73.4% less semantic storage and **49.8% less
whole-object storage** than the original files. Recompressing the expanded
baseline at level 9 gives 51,683,824 bytes; relative to that same-level baseline,
the codec saves **71.9%**. The level-9 pass measured approximately 1,102 ms total
encode and 383 ms total decode time. All 23 exact round-trips passed again.

Synthetic checks at level 3:

| Fixture | Compressed authored bytes | Current semantics | Compact semantics |
|---|---:|---:|---:|
| 4,096 generated Rust lines with unique variable names | 6,994 | 189,106 | 16,973 |
| One interior edit retaining an absent old-text snapshot | 19 | 85 | 130 |

The generated-add fixture uses 68,563 semantic bytes with the columnar codec but
without authored-content references, versus 16,973 with references. That isolates
the benefit of sharing authored bytes, rather than attributing everything to
numeric encoding. Synthetic tokenization constructs the fixture only; decoding
never tokenizes.

The tiny edit **regresses by 45 bytes**. A production design should consider a
versioned raw/compact discriminator and select the smaller complete encoding,
including its discriminator overhead. That adaptive choice is not implemented
or silently included in these measurements.

No savings are assumed by the tests: an encoding that gets larger is reported as
such. These measurements motivate further work on run encodings, column layout,
literal pooling, and graph-span integration; they do not justify dropping semantic
information to hit a size target.
