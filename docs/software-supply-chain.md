# Software Supply Chain Checks

Atomic hashes and signs canonical JSON, so the bytes it admits and the bytes it writes have to agree with every other implementation of the same standards. This page lists the CI jobs that hold those properties, the version each one is pinned to, and how to run each one locally. For what each record means to a user, and how Atomic's records map to RFC 8785, W3C Data Integrity, DIDs, W3C PROV, DSSE and in-toto, see the user guide page [Software Supply Chain](https://docs.atomic.dev/concepts/software-supply-chain).

## The standards in the code

| Piece | Standard | Code |
|---|---|---|
| Canonical bytes before hashing or signing | RFC 8785 (JCS) | `atomic-canonical/src/jcs.rs`, `canonicalize` |
| Admission of JSON that arrives as bytes | RFC 7493 (I-JSON), integers only, depth 128 | `atomic-canonical/src/jcs.rs`, `admit_document`, through the `jcs-admit` crate |
| Content hash of a canonical node | `blake3:` of the JCS bytes | `atomic-canonical/src/hash.rs` |
| Proofs on intents, memories, grants, triage reports and PROV projections | W3C Data Integrity, `eddsa-jcs-2022` | `atomic-canonical/src/proof.rs` |
| Signer identifiers | `did:atomic`, `did:key` | `atomic-canonical/src/did.rs` |
| Agent grants and revocations | Signed `AgentDelegation`, `AgentDelegationRequest`, `DelegationRevocation` | `atomic-canonical/src/delegation.rs` |
| Change signatures | Ed25519 over the content hash, domain separated, key supplied by the verifier | `atomic-core/src/change/signing.rs` |
| Release downloads | SHA-256 | `.github/workflows/release.yml`, `checksums-sha256.txt` |

The content hash and the Data Integrity proof both go through `jcs::canonicalize`, and every path that receives a grant certificate as bytes and later hashes, signs or verifies it goes through `jcs::admit_document` first. Keep it that way: a second canonicalization path is a second answer to what was signed.

## The CI jobs

| Job | What fails it | Pinned to |
|---|---|---|
| `Test` (`ci.yml`) | `atomic-canonical/tests/jcs_vectors.rs`, `depth_boundary.rs` and `ingest_boundary.rs` on Linux, macOS and Windows | `jcs-admit` in the workspace `Cargo.toml` |
| `Conformance vectors` (`ci.yml`) | A vector in the `ai-agent-action` suite of [agent-evidence-vectors](https://github.com/probityai/agent-evidence-vectors) whose verdict `admit_document` and `canonicalize` disagree with: a byte fault that is admitted, a document that should be admitted and is refused, or an accepting record line that is not already canonical | The suite at the commit of its `v0.25.0` tag; `jcs-admit` 0.1.1 |

The conformance suite is not vendored. The test in `atomic-canonical/tests/conformance_corpus.rs` is ignored by default and reads the suite from `AEV_SUITE`:

```bash
git clone --depth 1 --branch v0.25.0 https://github.com/probityai/agent-evidence-vectors /tmp/aev
AEV_SUITE=/tmp/aev/vectors-ai-agent-action \
  cargo test -p atomic-canonical --test conformance_corpus -- --ignored
```

`NOT_WHOLE_DOCUMENT` in that test names the one accepting vector it skips, `v43175cd2ad003639`, and why: the suite counts that vector's depth limit inside its `extensions` member, while `admit_document` counts from the document root. The rejecting side of the same limit, `vd94ac70c9f0d84bf`, is still checked.

## Raising a pin

To take a newer suite release, change both the `ref` commit and its tag comment in the `Conformance vectors` job, run the command above against the new tag, and fix or explain every vector that disagrees in the same pull request. Dependabot proposes each new `jcs-admit` release as a pull request against `dev` (`.github/dependabot.yml`), and that pull request runs the `Test` job's canonical tests and the conformance replay before it can land. A vector that starts failing after a raise is a disagreement about bytes that would change a hash or a signature, so it blocks the raise until it is understood.
