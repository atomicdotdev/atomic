# Ingest-boundary fixtures

Four documents from the conformance suite, copied byte for byte, one per
conformance case. Each carries a fault that a function taking an already-parsed
value cannot report, so each is a test of `jcs::admit_document` rather than of
`jcs::canonicalize`.

Source: the `ai-agent-action` conformance suite, commit `cf3d20e`, at
<https://github.com/probityai/agent-evidence-vectors>. Paths below are the ones
`MANIFEST.json` names, and the layout here mirrors them. Nothing was reformatted:
a repeated member and a fractional number survive a pretty-printer, but the point
of a byte fixture is that it does not have to.

A vector's id is `v` plus sixteen hex digits of SHA-256 over the vector's own
bytes: the statement alone, or the statement and a NUL and the record sidecar
where one ships. Checked against all 53 vectors in the manifest at that commit,
and it holds for all 53. So `statements/v679f56481420e45a.json` hashes to
`679f56481420e45a...` because that vector is its statement, while
`v0f4f2093061d303f` and `v97f5d8777e514257` are each a statement and a record
together and neither half hashes to the id on its own.

For those two the copied file is the record, because the record is the half that
carries the fault: the statements are conformant documents whose declared digest
commits to a record line that is not. So the four files here are four faults, not
four whole vectors, and the id in each row names the vector the fault came from.

| file | vector | manifest path | condition | expected refusal |
| --- | --- | --- | --- | --- |
| `records/v0f4f2093061d303f.jsonl` | `v0f4f2093061d303f` | `records/v0f4f2093061d303f.jsonl` | `aia-c-5` duplicate member | `Error::DuplicateMember { name: "toolName", offset: 130 }` |
| `records/v97f5d8777e514257.jsonl` | `v97f5d8777e514257` | `records/v97f5d8777e514257.jsonl` | `aia-c-9` non-integer in a signed field | `Error::NonIntegerNumber { token: "412.5" }` |
| `statements/v679f56481420e45a.json` | `v679f56481420e45a` | `statements/v679f56481420e45a.json` | `aia-c-14` unsafe integer | `Error::UnsafeInteger { token: "9007199254740993" }` |
| `statements/vd94ac70c9f0d84bf.json` | `vd94ac70c9f0d84bf` | `statements/vd94ac70c9f0d84bf.json` | `aia-c-12` depth exceeded | `Error::TooDeep { limit: 128, offset: 17860 }` |

SHA-256 of each file as committed:

```
9d91bad0d5ed10edb29785fc66f6ca406895890f5828d96e1cec0115c8a97cce  records/v0f4f2093061d303f.jsonl
52251e852eab4dc9eeb26ce0f9ccf13e3852c4ff2bfbb5e1b7380d34c2ef59c0  records/v97f5d8777e514257.jsonl
679f56481420e45a6196f2be61f29d51cc76b011e04bd8df8d6af6064c53511b  statements/v679f56481420e45a.json
d94ac70c9f0d84bf1fc287376d1e2416785ce51df443a97a2d13765f5867883e  statements/vd94ac70c9f0d84bf.json
```

Two of the four are refused by RFC 8785 alone; the other two need the RFC 7493
profile `jcs::admit_document` applies. `tests/ingest_boundary.rs` says which is
which, and asserts the variant rather than a message.

## The depth cap from both sides, generated here

Three documents that did not come from the conformance suite. They were
generated for this crate as the bytes `[` repeated *d* times, `null`, `]`
repeated *d* times, with no trailing newline, so each file is exactly 2*d* + 4
bytes and `tests/depth_boundary.rs` asserts that length before it reads
anything else. `aia-c-12` above is one container past the cap on a real
statement; these sit on the cap itself, one either side, which is where a
second, undeclared depth bound shows up as a disagreement between the
admission and the parse that follows it.

| file | depth | bytes | condition | expected |
| --- | --- | --- | --- | --- |
| `depth/127.json` | 127 | 258 | one under `jcs::MAX_DEPTH` | admitted |
| `depth/128.json` | 128 | 260 | at `jcs::MAX_DEPTH` | admitted; canonicalizes to the same bytes; identity through `encode_for_transport` and `decode_from_transport` |
| `depth/129.json` | 129 | 262 | one past `jcs::MAX_DEPTH` | `Error::TooDeep { limit: 128, offset: 128 }` |

SHA-256 of each file as committed:

```
89fed8bdcec19b53a9cfeb6365642fcd7f84a12c0a8435224c1821f0183fa908  depth/127.json
9c2ec3e5c558bde9c95f21cd30b81503b5a76ab11f0a1bcbc9e55f45fa38bcf4  depth/128.json
aea6374322f648efe766b46b158da515b832005c4c2b2ba76de0f3f770bed699  depth/129.json
```
