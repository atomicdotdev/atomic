# Vectors this entry point cannot decide

`jcs::canonicalize` takes an already-parsed `serde_json::Value`. Three conformance
vectors ask for a refusal that no function of that shape can give, because the
fault either vanished during the parse or is not a fault under RFC 8785 at all.
They are listed here rather than dropped, so the follow-up has its brief in-tree.

| vector | condition | what it asks for | why not here |
| --- | --- | --- | --- |
| `v0f4f2093061d303f` | duplicate member | reject `{"a":1,"a":2}` | `serde_json` keeps one of the two members while parsing, so the repeat is gone before `canonicalize` is called. Measured: the document canonicalizes to `{"a":2}`, and `{"a":2,"a":1}` to `{"a":1}` -- two wire documents, two canonical forms, and a signature over either verifies. |
| `v679f56481420e45a` | unsafe integer | reject `9007199254740993` | RFC 8785 defers number formatting to ECMAScript, which has one numeric type, so the specification *admits* the token and writes the double it rounds to. Refusing it is the RFC 7493 I-JSON profile, which is a stricter profile rather than RFC 8785 itself. Pinned as an accept in `number-rounded-to-its-double.json`. |
| `v97f5d8777e514257` | non-integer in a signed field | reject `0.7` | Same shape: RFC 8785 admits a fractional number. Refusing one is a field-level profile decision, not a canonicalization rule. |

A fourth case is half-covered. `vd94ac70c9f0d84bf` asks a canonicalizer to refuse
a document nested one container past a stated cap with a catchable error. The
accept half is pinned in `depth-at-the-cap-is-canonicalized.json`; the refusal
half needs a fallible boundary, and an infallible `canonicalize(&Value) -> String`
has nowhere to put it.

## What closes all four

A strict decoder on the raw bytes, ahead of this function: it refuses a repeated
member, caps nesting at 128 with an error rather than a stack walk, refuses a
string that is not a sequence of Unicode scalar values, and optionally applies the
I-JSON safe-integer profile. `jcs-admit` on crates.io does exactly that and then
hands canonical output to the same `serde_json_canonicalizer` this file already
uses, so adopting it adds refusals without changing a single byte of output.

The place it belongs is wherever a document arrives as bytes rather than as a
value built in-process.
