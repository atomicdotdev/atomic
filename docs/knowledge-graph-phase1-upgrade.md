# Knowledge graph Phase 1 upgrade

This release changes the workflow KG API. Consumers must use absolute resource,
class, and predicate IRIs. There is no legacy-vocabulary compatibility window.

- `KgNode.types` replaces `kind` (protobuf: repeated `types`). Each asserted
  class is an IRI under `https://atomic.dev/ns/workflow#`.
- Edge `kind` / protobuf `relation` is an absolute predicate IRI. `QueryGraph`
  responses carry `api_version: 1`; clients reject unsupported versions.
- Changes, provenance, and session attestations use full Blake3 hashes. Local
  workflow resources are qualified by the repository's stable ProjectId.
- Query-plan JSON requires `schema_version: 1`. Filter with `field: "type"`
  and a class IRI. Neighborhood steps accept a predicate IRI and `direction`
  (`incoming`, `outgoing`, or `both`).
- `query callers` accepts a bare symbol or `path#name` and parses source live.
  Code entities/calls/imports/modules are no longer persisted in the KG.

The first full writable open mints an unsigned upgrade identity if needed and
rebuilds the derived graph once. Lightweight/repair/read-only opens do not run
that migration. KG reads on an old generation report `KgNeedsRebuild`; ordinary
repository reads remain available. Force reconstruction with:

```sh
atomic repo identity show --json
atomic query reindex --full
```

Validation runs locally in the CLI, using PurRDF 3.0.1:

```sh
atomic query validate --format json
atomic query validate --profile publication --format json
atomic query validate --shape <shape-iri> --node <resource-iri>
```

Identity inspection and validation fetch their data through the shared RPC
handlers in both local and reactor modes, using the central repository connection
opener. They are read-only: missing identity/generation state requires an explicit
`atomic query reindex --full`. Reindex also performs signer discovery handler-side;
the CLI resolves credentials without opening a repository database. PurRDF still
runs client-side on the returned immutable snapshot.

Projection validation accepts a missing workflow shortcut only with its matching
per-change finding. Publication validation additionally checks managed plans,
provenance, session coverage, and explicit successful DID signature verification.
Historical gaps remain visible. Inspect target coverage: conformance with no
applicable targets is vacuous, not evidence of workflow completeness.

Local enterprise `.atomic/shapes/*.ttl` policies use the same engine. Relative
IRIs resolve against each file's canonical file URL. Imports resolve only among
explicitly loaded bundles; unavailable imports are errors, without network fetches.
Exit codes are **0** for conformance, **1** for violations, and **3** for shape or
engine errors. JSON preserves the structured results and complete RDF report.

Signer discovery after pull/reindex uses existing authenticated identity APIs.
Keys are cached only after matching their fingerprint DID. Servers unable to
resolve a fingerprint DID leave the attestation `Unverifiable`. Signature
verification remains separate from SHACL. No OWL inference or mandatory
publication hook is enabled in this phase.

The CLI and `atomic-shacl` require Rust 1.98. Shared libraries retain Rust 1.90.
ProjectIdentity transport, adoption, and remote binding are deferred; Phase 1
clones have local upgrade identities until that transport is implemented.

Run the Phase 1 acceptance harness with a freshly built CLI:

```sh
cargo build -p atomic-cli --bin atomic
ATOMIC_BIN="$PWD/target/debug/atomic" bash tests/harness/46_knowledge_graph_closure.sh
```

## Agent timestamp precision (no KG upgrade)

New agent activity timestamps use Unix epoch **milliseconds**: provenance nodes
and graph headers, session turns and lifecycle times, AI request metadata, and
turn-envelope timings. RPC timestamps retain that precision in their standard
seconds-plus-nanoseconds representation. Causal links remain authoritative for
ordering; clocks and milliseconds alone do not establish causality.

Readers and chronological comparisons also accept historical second epochs.
Untagged values below 100 billion in absolute magnitude retain their historical
seconds interpretation; larger values are interpreted as milliseconds. Ambiguous
values near 1970 cannot be distinguished without a unit tag. New graph writers
use the explicit `timestamp_millis` setter; raw historical values remain intact.

Existing incorrect cached session-turn dates are corrected in query results,
including read-only queries. This correction requires no generation change,
migration, or reindex and does not rewrite provenance bytes or historical hashes.
