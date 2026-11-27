# Next-level change compression lab

This work investigates the ceiling beyond the schema-4 production codec. **The
new representations are experiments, not enabled production formats.** No
historical object is rewritten and the existing ATSC-v1 writer is unchanged.

## Measurement contract

Both labs read `.change` files directly and verify the source object hash. They
construct the current unsigned production baseline and compare new representations
at **zstd level 3**, counting complete experimental headers, dictionaries, column
directories and compressed frames. Old signatures are not reused for new hashes.

Every graph candidate must reproduce the baseline graph payload **byte-for-byte**
and deserialize into identical graph operations. Every semantic candidate must
reproduce either the complete production ATSC-v1 payload or the complete expanded
postcard `FileOps` payload and deserialize into identical semantic operations.
This preserves identities, field ordering, optional values, deleted snapshots,
flags, references and opaque bytes, not just final file text.

Whole-object projections retain all unmodified sections and existing file/section
framing. A future implementation needs a new format discriminator/capability,
authenticated hashing, import/export support and signature handling; these
projected bytes are not silently substituted under old object identities.

Both labs report every failed input and exit nonzero if any candidate loses data.
Partial totals must not be presented as full-corpus results. Single-process timing
samples are diagnostic, not rigorous latency guarantees.

## First profile: find the remaining cost

The local corpus now contains 49 objects, one more than the previous benchmark.
The current production baseline totals 49,941,698 unsigned bytes. Compressed
payloads include 19,247,739 graph bytes, 15,521,923 semantic bytes and 14,945,455
authored-content bytes, plus metadata/framing.

Compressing semantic columns independently is useful for attribution but is
**not additive accounting** of the actual combined compressed payload:

| Column | Uncompressed bytes | Standalone zstd-3 bytes |
|---|---:|---:|
| Mixed operation control | 62,199,633 | 7,398,599 |
| Byte-reference deltas | 3,445,800 | 2,749,203 |
| Token kinds | 12,314,667 | 2,114,708 |
| Token-ID columns | 21,906,579 | 1,569,485 |
| Content ranges | 3,251,188 | 1,373,053 |
| Unique literal text | 106,025 | **46,438** |

The main residual is structured metadata, not unique old/deleted text.

## Candidates and rejected shortcuts

### Generic numeric transforms

The semantic lab implements reversible run-length encoding, 128-value byte-plane
transposition, block-minimum bit packing, and even/odd varint transposition.
Applying any one broadly made **all 49 objects larger**. A per-column selection
among those transforms initially saved only 3.66% of semantic bytes (1.14% of
whole-object bytes with a raw fallback). Zstd already handles many regular runs;
blind preprocessing can destroy useful cross-field patterns.

### Grammar-aware control fields

The next candidate splits the ATSC-v1 control stream into twelve columns: file
counts, line counts, old/new/range presence, branch operation, branch predecessor
presence, token counts, leaf operation, leaf predecessor presence, reference type
and token length. A bounded grammar walker reconstructs the exact original mixed
stream. It neither creates nor removes an operation.

### Reference contexts

Another candidate decodes the existing reference deltas, separates authored-content
and literal-pool offsets into independent histories, and reconstructs the original
shared history on decode. It retains every original offset and length.

### Predicting classifications, not identities

A frozen experimental classifier predicts a token's kind from its **existing token
bytes**. A bitmap and explicit exception values restore every authored classification
that differs. Tests deliberately supply classifications that look incorrect and
require them to survive exactly. Kind prediction does not assign boundaries, IDs
or causal operations. This is not replacing semantics with a new tokenization.

The typed `Vec<u8>` bitmap is part of this contract: an inferred integer vector
would use a different postcard integer encoding. Counterexample tests caught that
mistake before any successful corpus result was accepted.

### Predicting lengths with exact boundary exceptions

The strongest semantic candidate also predicts token lengths from the byte
reference's start position. It retains every reference event, leaf count,
identity, predecessor and operation, plus a bitmap and the **actual lengths**
where authored boundaries differ. Tests include splitting `foobar` into `fo` and
`obar`, empty tokens, Unicode and a 5,000-byte token. Exact boundaries survive.

The lab predictor examines ASCII prefixes only, growing a small lookahead up to
4,096 bytes. Non-ASCII starts and longer/different boundaries use exceptions. It
includes a fingerprint of predictor/tokenizer source and rejects mismatched
implementations rather than silently using a different algorithm. That fingerprint
is a lab safeguard, **not a deployable cross-build wire contract**. Production
needs a frozen portable predictor version and retained historical readers.

Length prediction needs authored bytes, original reference values and the literal
pool while restoring control fields. The lab forbids a cyclic combination with
reference-context compression, and restores token kinds only after control and
reference dependencies are ready.

### Field-role graph columns

The graph lab serializes the existing graph payload into a replay stream plus
numeric columns keyed by **field context and primitive type**. An insertion's
start, end, predecessor position and inode position therefore do not continually
reset one shared delta history. Strings share a dictionary. The replay stream
reconstructs the exact original postcard payload before the normal graph parser
checks it.

No edges are coalesced, no linewise operations are converted into spans, and no
conflict/namespace/attribute operations are normalized away. The encoder is driven
by the existing derived serialization rather than a hand-picked list of common
graph variants.

### Field-role semantics with shared authored bytes

The same field-role mechanism can replay complete `FileOps`. An optional encoder
lookup finds exact byte matches in this change's authored content. Token byte
sequences can then use content ranges; missing bytes remain in the frame's literal
dictionary. The decoder needs only the frame and the hash-bound authored bytes.

The encoder may use lexical pieces to **find reuse locations**, but exact byte
equality is checked and misses fall back losslessly. That lookup does not determine
semantic boundaries, classifications or identities. The decoder performs no
tokenization. A typed sequence that is not representable as byte elements falls
back to ordinary field replay; tests cover nested sequence ambiguity.

This generic semantic replay was a **negative result**, with both first-match
content references and sequential-reference elision. It remains reproducible in
the lab, but is not recommended for production.

## Queryability and safety boundaries

- No candidate requires redb, a repository-local ID table, or an external cache.
  Existing dependency/owner declarations and all other original sections remain
  necessary for cross-change interpretation and remain counted in projections.
- Existing source signatures cannot authenticate re-encoded section bytes. The
  experiments verify source hashes, not the trust of arbitrary source signers.
- Frames and reconstructed payloads have explicit size limits; numeric values,
  counts, indexes, arithmetic, padding and stream consumption are checked.
  These are bounded laboratory decoders, not a claim of deployment-ready
  hostile-input hardening.
- The graph replay parser checks vector counts **before** allocating their
  indexes; dictionary/numeric columns must be used and consumed in valid order.
- Some candidates reconstruct an intermediate full postcard/ATSC payload before
  parsing operations. That adds time and memory. Production direct column readers
  could avoid that intermediate, but those improvements are not assumed here.
- These are whole-section experiments, not compressed per-file/range random
  access. Any production integration must preserve a clear selective-read contract.
- No Git blob or Git-pack measurement is implied. Compressed authored bytes are a
  local floor reference, not the same object or historical data model as Git.

## Results

All 49 source hashes and baseline sizes match between labs. Every candidate
passed exact payload and typed-operation checks on all objects, without redb.
Numbers below use zstd 3 and count complete candidate envelopes.

### Best measured projection

| Component | Current production | Graph fields + adaptive semantic candidates | Reduction |
|---|---:|---:|---:|
| Graph payloads | 19,247,739 B | 1,616,681 B | **91.6%** |
| Semantic payloads | 15,521,923 B | 9,036,996 B | **41.8%** |
| Complete unsigned objects | 49,941,698 B | **25,825,713 B** | **48.3% further** |

Graph selection retains the original payload for one small object; the
unconditional graph candidate totals 1,616,786 B. Adaptive semantic candidates
regressed on none of these objects. This is not a universal size guarantee;
production must keep a whole-representation fallback.

The canonical unsigned uncompacted-semantic baseline for these same objects is
106,466,418 B. The best projection is **75.7% smaller** than that baseline and
**1.73×** the 14,945,455 compressed authored-content bytes. This is a measured
next step, not an ultimate entropy limit or a Git-blob comparison.

### Semantic candidate details

| Candidate | Semantic bytes before whole-payload fallback | Objects larger than production |
|---|---:|---:|
| Production ATSC v1 | 15,521,923 | — |
| Broad RLE | 18,504,955 | 49 |
| Broad byte planes | 18,218,007 | 49 |
| Broad bit packing | 19,061,056 | 49 |
| Broad parity split | 16,548,894 | 49 |
| Control fields | 15,065,999 | 7 |
| Reference contexts | 15,514,782 | 31 |
| Kind predictions + exceptions | 13,531,909 | 0 |
| Control/reference/kind combination | 13,075,227 | 2 |
| Length predictions + exceptions | 11,593,174 | 1 |
| Length and kind predictions | **9,616,734** | 0 |
| Adaptive columns, including length prediction | **9,036,996** | 0 |
| Generic semantic fields + content references | 26,282,580 | 49 |
| Generic semantic fields + sequential references | 28,540,003 | 49 |

The graph's generic field strategy did not automatically transfer to semantics.
Predicting redundant lengths/classifications with exact exceptions worked much
better than generic semantic replay or blind integer preprocessing.

### Encode/decode cost

Single warm-process elapsed-time observations across the corpus, including
reconstruction and typed parsing but excluding source-file reads and validation
comparisons. Both baseline and candidate stopwatches stop before payload/operation
equality checks and object destruction. These are `Instant` timings, not hardware
CPU-counter measurements:

| Decode path | Total time |
|---|---:|
| Production graph decompression + parse | ~147 ms |
| Graph field replay + parse | ~221 ms |
| Production semantic decompression + parse | ~469 ms |
| Fixed length/kind semantic candidate + parse | ~1,252 ms |
| Adaptive semantic candidate + parse | ~1,299 ms |

The semantic prototype takes about **2.8×** baseline elapsed decode time and the
graph prototype about **1.5×**. They materialize intermediate payloads and walk
control data multiple times. Smaller storage is not automatically faster cold
querying. An earlier ~3.1× semantic estimate included candidate validation and
cleanup inside one timer; the corrected comparison excludes those on both paths.

The transform-encoding stage excludes construction of its input ATSC-v1 payload
for both fixed and adaptive candidates; it is not a full production-writer timer.
The observed transform stages took ~792 ms fixed and ~4,023 ms adaptive.
Their complete-object projections with graph columns are 26,405,451 B
and 25,825,713 B: adaptive saves another 579,738 B, for roughly five times the
transform-encoding work in the observed pass. These are noisy lab timings, not
production commitments.

## Production recommendation

1. **Graph field columns are the clearest next integration candidate:** large
   savings, unchanged graph algebra, and a smaller measured CPU penalty.
2. Evaluate a direct versioned reader for length/kind exceptions, avoiding
   intermediate ATSC reconstruction. Start with the fixed candidate before
   paying for exhaustive per-column searches.
3. Keep the production codec as fallback and preserve schema-4 identities. New
   formats require explicit version/capability and store/export gates.
4. Do not ship rejected transforms or generic semantic replay merely because
   they look more sophisticated. Their complete measured outputs lose.

## Reproduce

```bash
cargo test -p atomic-core --example semantic_codec_lab --example graph_codec_lab
cargo clippy -p atomic-core --example semantic_codec_lab --example graph_codec_lab --no-deps -- -D warnings
cargo run --release -p atomic-core --example semantic_codec_lab -- .atomic/changes > /external/path/semantic-lab.json
cargo run --release -p atomic-core --example graph_codec_lab -- .atomic/changes > /external/path/graph-lab.json
python3 atomic-core/examples/semantic_codec_lab/report.py --self-test
python3 atomic-core/examples/semantic_codec_lab/report.py /external/path/semantic-lab.json /external/path/graph-lab.json
```

Use identical source hashes when joining results from the two labs; verify their
baseline object sizes agree. Do not combine unrelated corpora or add independent
diagnostic column-frame costs as though they were actual stored totals.
The reporting helper performs those consistency checks, refuses partial/failed
runs, and can additionally accept `semantic_storage` JSON as a third argument to
verify the uncompacted-semantic comparison on the identical corpus.
