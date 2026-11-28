# ATOM schema 5: authenticated graph/semantic codecs

Schema 5 retains the 64-byte ATOM header, hash table, section framing, content
chunks and trailer. New writers use it only for graph-field v1 and/or predictive
semantic v2 payloads. Schemas 1–4 remain readable. Existing schema-2/3/4 encode
policies are retained on decode; filesystem transport stores original bytes.

## Authenticated interpretation

`CODECS` has section byte **0x05** and is appended to the Rust `SectionType` enum
so prior postcard enum ordinals do not change. Its four decompressed bytes are:

```
[5, graph_codec, semantic_codec, has_span]
```

- Graph: 0 = existing postcard graph, 1 = graph-field v1.
- Semantic: 0 = expanded postcard, 1 = ATSC v1, 2 = predictive ATSC v2.
- Span: 0 or 1. Existing span payload/algebra is unchanged.

At least graph codec 1 or semantic codec 2 must be selected. The writer emits
CODECS after HEADER, DEPS, optional PROVENANCE and SIGNATURE, and before any graph,
span, semantic, content or unhashed body. Exactly one descriptor is required.
It is hashed; signatures and UNHASHED remain excluded as before.

Header flags mirror this descriptor:

| Bit | Meaning in schema 5 |
|---|---|
| 4 | HAS_SPAN |
| 5 | GRAPH_FIELDS |
| 6 | PREDICTIVE_SEMANTICS |
| 7 | COMPACT_SEMANTICS (ATSC v1 or predictive v2) |

These flags **are not authority**: the historical file header is outside the
change hash. The reader requires a matching hashed descriptor before interpreting
body codecs. Changing only a header flag must fail even if the old trailer and
signature are retained. Schema-5 section counting includes CODECS and optional
SPAN. Reserved flags in historical schemas retain their prior ignore-unknown
meaning; a schema-5 descriptor is forbidden there.

`change-format-vnext` required capability version **3** fences repository writers
and readers. File readers also reject unsupported ATOM versions. No server
deployment is implicit in adding these codecs.

## Graph-field v1

The decompressed GRAPH payload starts `ATGF\x01`, followed by postcard fields:

```
path: String
content_start: u64
content_end: u64
control: Vec<u8>
columns: Vec<{ role: u16, count: u32, constant: Option<u64>, data: Vec<u8> }>
dictionary: Vec<String>
aux: Vec<u8>
```

Unsigned integers in numeric data use canonical varints after wrapping-u64
delta/zigzag encoding. Previous value starts at zero for each role. A constant
column instead repeats its declared absolute value `count` times and has empty
data. Counts are nonzero. Roles are unique, ascending and in 0–271. Readers must
consume exactly every declared value and byte. Constant-column elision prevents
large zero-delta buffers from separating correlated position streams in zstd's
uncompressed window.

The control stream starts with operation count, then tags and variable-list
lengths/presence flags in the exact field-call order of `graph_fields/encode.rs`.
Presence flags are 0/1; unknown tags fail. Optional encodings and inode attributes
are consecutive postcard values in `aux`, consumed in operation order.

Graph tags are frozen:

| Tag | Operation | Tag | Operation |
|---|---|---|---|
| 0 | FileAdd | 9 | SolveNameConflict |
| 1 | DirAdd | 10 | UnsolveNameConflict |
| 2 | DirDel | 11 | SolveOrderConflict |
| 3 | DirUndel | 12 | UnsolveOrderConflict |
| 4 | FileDel | 13 | ResurrectZombies |
| 5 | FileUndel | 14 | AddRoot |
| 6 | FileMove | 15 | DelRoot |
| 7 | Edit | 16 | SetAttr |
| 8 | Replacement | | |

Edit atom tags are 0/insertion, 1/edge update. Private prototype operations are
not graph-field v1 tags; automatic encoding keeps the raw graph when necessary.

### Numeric roles

Insertion roles use `12 * context + field`. Contexts are name=0, inode=1,
FileAdd contents=2, Edit insertion=3, Replacement insertion=4, FileMove name=5,
AddRoot name=6 and AddRoot inode=7. Fields:

```
0 flag       1 start       2 end
3 inode.change            4 inode.pos
5 predecessor.change      6 predecessor.pos
7 successor.change        8 successor.pos
```

Predecessor/successor counts are in control. The arrays retain original ordering
and duplicate values; no graph transformation is inferred.

Edge-update roles use `96 + 12 * context + field`. Contexts are delete=0,
delete contents=1, undelete=2, undelete contents=3, move delete=4, edit=5,
replacement=6, solve name=7, unsolve name=8, solve order=9, unsolve order=10,
zombies=11, DelRoot name=12 and DelRoot inode=13. Fields:

```
0 inode.change   1 inode.pos     2 previous flags   3 new flags
4 from.change    5 from.pos      6 to.change        7 to.start
8 to.end         9 introduced_by
```

Edge counts are in control. Remaining global roles:

- 264: path dictionary IDs.
- 265–269: local line numbers for Edit, Replacement, SolveOrder,
  UnsolveOrder and ResurrectZombies respectively.
- 270/271: SetAttr inode change/position.

The UTF-8 dictionary is unique and ordered by first use. IDs may be reused; gaps
or unused entries fail. Root section path and byte range are retained separately.
Mode/name-conflict validation remains the existing graph contract. In particular,
local context is retained even though it does not alter graph traversal.

## Predictive semantic v2

The decompressed SEMANTIC payload starts `ATSC\x02`, followed by postcard fields
in this order (see `semantic/predictive/wire.rs`):

```
predictor: u8 = 1
content_hash: [u8; 32]
metadata: Vec<u8>
controls: [Vec<u8>; 11]
branches, leaves, old_numbers, new_numbers, positions, references, literals: Vec<u8>
length_count: u32
length_mask, length_exceptions: Vec<u8>
kind_count: u32
kind_mask, kind_exceptions: Vec<u8>
```

Metadata and numeric/reference/literal columns retain the ATSC-v1 representations,
with separate branch and leaf IDs. Control roles are file count=0, line count=1,
old/new/range presence=2/3/4, branch op=5, branch predecessor presence=6,
token count=7, leaf op=8, leaf predecessor presence=9 and byte-reference tag=10.
Operation ordinals and all authored fields are unchanged.

A mask contains exactly `ceil(count/8)` bytes, least-significant bit first, with
zero padding. Bit 1 reads the actual value from its exception varint stream; bit 0
uses predictor 1. Length counts include Insert and Replace byte events; kind
counts include Insert only. Every count, column and exception must be consumed.

The direct reader resolves reference start before length, checks source binding
and range arithmetic, then constructs the original token bytes and kind. It does
not restore an ATSC-v1 control stream or serialize a postcard `FileOps` buffer.
Literal-pool frontier checks and fixed expansion costs remain the v1 contract.

### Frozen predictor 1

`predictor_v1.rs` is normative frozen code with golden vectors. It is not the
current tokenizer and has no source-file fingerprint dependency.

Length prediction uses fixed ASCII lexical rules (words, spaces/tabs, CR/LF,
comments, quoted strings/escapes, numbers and listed compound operators). Lookahead
grows from at most 16 to at most 4,096 bytes; it stops before non-ASCII data. A
non-ASCII first byte predicts length one. All different authored lengths—including
zero, unusual word splits and longer tokens—come from exceptions. The reader
does not run the predictor for an explicit exception, bounding prediction work
relative to produced bytes rather than repeatedly scanning long exceptional tails.

Kind prediction uses fixed byte tests, not Unicode classification tables.
Non-ASCII identifiers can therefore require an explicit authored kind. Token-kind
codes 0–8 retain Word, Whitespace, Operator, Punctuation, String, Number, Comment,
Newline, Other. No identities, causal operations or boundaries are invented when
an authored value differs. A new predictor must get a new ID and keep old readers.

## Limits and compatibility

Decompressed schema-5 sections and authored content are capped at 512 MiB. Graph
and semantic typed expansion budgets are 1 GiB with fixed conservative costs,
not ABI-dependent selection. Collections use checked counts/reservations; graph
numeric values are range-checked to their actual field type. Raw graph fallbacks
also use bounded serde access and no untrusted allocation hints. Encoder checks
match reader budgets, with automatic fallback to established representations when
new codec limits are exceeded. Other oversized section errors remain explicit.

New automatic selection compares complete costs, including the compressed CODECS
frame and its section header, and retains old payloads on ties. Forced modes may
be larger. Old graph/semantic encoding policies are retained on decode so official
objects do not silently upgrade when reserialized. Exact-byte filesystem import
remains the transport path for arbitrary historical encodings.

Section-store extensions retain CODECS, HEADER, SPAN and PROVENANCE. Export checks
the reconstructed content hash for every format. Unsupported noncanonical compact
objects are rejected before cache mutation; old lossy cache rows fail closed and
must not be treated as authority instead of the original `.change` files.
