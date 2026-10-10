# Portable provenance exports

Build with `cargo build -p atomic-cli --features dsse-export` to enable an
optional DSSE export for existing W3C PROV projections:

```sh
atomic provenance show <CHANGE> --identity reviewer --dsse > export.dsse.json
atomic provenance trace <CHANGE> --identity reviewer --json --dsse > export.dsse.json
```

Ordinary `show` and `trace --json` output stays unsigned. `--sign` still adds
Atomic's native Data Integrity proof; combine `--sign --dsse` to retain that
proof inside the portable export. No repository objects or sidecars are written.

The DSSE payload type is
`application/vnd.atomic.provenance-export.v1+json`. Its JSON payload contains
`schema: "atomic-provenance-export/v1"`,
`coverage: "exporter-signed-projections-only"`, and a nonempty `projections`
array. Each element is the existing projected JSON-LD value, including any
native proof. DSSE signs the payload type and exactly those serialized bytes;
it does not change native hashes, V3 signatures, or Data Integrity proofs.

The selected identity signs as **exporter**. The Person in the existing
projection is resolved from that identity at export time; it does not recover
the original author. A valid export signature authenticates the exporter's
statement. It does not establish that graph claims are true or complete, that
the exporter was authorized to act, or that referenced changes were verified.
Referenced change bytes, input resources and test evidence are not included.
Unknown links remain omitted. The exported graph is a projection, not the full
captured decision graph or a complete session history.

Pin the exporter's public key independently, for example through your existing
reviewer-key policy. An envelope's `keyid` is an untrusted hint. Verification
requires that separately selected base32 Ed25519 public key:

```sh
atomic provenance verify-dsse export.dsse.json --public-key <PINNED_BASE32_KEY> > verified.json
# Also accepts stdin; this consumer needs no repository or local identity store.
cat export.dsse.json | atomic provenance verify-dsse --public-key <PINNED_BASE32_KEY> > verified.json
```

The consumer rejects wrong payload types, invalid signatures, duplicate JSON
members, unknown export fields, unsupported schema/coverage declarations,
non-I-JSON integers or fractional numbers, depth above 128, and input above
8 MiB. It emits the exact authenticated payload bytes only after verification
and profile checks. It does not separately verify a native proof or fetch
referenced objects. A reviewer who needs those checks must perform them too.

An independent Python reader is included for interoperability checks:

```sh
python -m pip install 'cryptography==50.0.0'
python tools/provenance-export/verify_dsse.py export.dsse.json --public-key <PINNED_BASE32_KEY> > verified.json
```

Both readers consume the bytes used to verify the signature, without decoding
the original envelope a second time. The dedicated CI workflow runs the real
CLI against a persisted provenance graph, compares both readers' output bytes,
tests key/type/payload refusal, and checks that export changes no repository
files. These are project-operated checks, not an independently operated result
or a claim that an outside reviewer has adopted this format.

This feature reuses Atomic's existing identity keys via DSSE's signer/verifier
traits and disables DSSE's default cryptographic backend. Existing key storage
limitations still apply: the current CLI resolves keys from its local identity
store and prints the existing unencrypted-development-key caveat when signing.
This does not change the separate external-signing/owner protocol.
