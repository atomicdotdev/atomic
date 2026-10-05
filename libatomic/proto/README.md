# libatomic — the canonical Atomic contract

`libatomic` is the one crate that owns the Atomic service contract:

- **`proto/atomic/`** — the canonical protobuf sources (package `atomic`):
  shared primitives, the stable error model, and one folder per service
  (`<svc>_service.proto` for the RPCs, `<svc>_messages.proto` for that
  service's request/response messages). This tree is the contract's single
  source of truth; there is no second copy.
- **codegen** — `build.rs` compiles the tree with
  [`protox`](https://crates.io/crates/protox) (a pure-Rust `protoc`
  replacement — no system protoc install) and `tonic-build`, generating
  both clients and servers into the `atomic` module:

  ```rust
  use libatomic::atomic::{StatusRequest, repository_query_service_client::RepositoryQueryServiceClient};
  // transport-neutral alias:
  use libatomic::proto as pb;
  ```

- **`proto/check_contract.py`** — the descriptor contract gate. It compiles
  the tree and enforces the invariants the contract promises (details
  below); run it from any throwaway environment, it never writes into the
  source tree.

## Layout

```
libatomic/
  build.rs                  protox + tonic-build codegen (no system protoc)
  src/lib.rs                the generated `atomic` module (+ `proto` alias)
  proto/
    README.md               <- this file
    check_contract.py      <- protobuf compilation + descriptor contract gate
    atomic/
      common/               <- shared primitives
        options.proto       scope, capabilities, callers, effects (extensions 51001-51004)
        common.proto        repository/view/snapshot IDs, request/response metadata, hashes
        change.proto        shared change-domain message shapes
        error.proto         the stable error model
        pagination.proto     PageCursor {seq, offset}
        event.proto         the agent event envelope
      services/<service>/   <- one folder per service
        <svc>_service.proto     the service definition — RPCs only
        <svc>_messages.proto     that service's messages
```

## Status: DRAFT for alignment

Numbers are correctable until review and the first tagged release —
**add-only thereafter**. Field and extension numbers are never reused or
renumbered; removed field numbers are reserved. Older draft clients must
fail version negotiation rather than send an ignored field to a writer.

## The contract gate

`check_contract.py` compiles the tree and verifies the declared
invariants: every method declares `execution_scope`, `allowed_caller`,
`effect`, and `required_capability`; reads stay read-only (no write/admin
authority, no mutation replay echo); mutations carry request metadata and
echo it in the response; retired fields stay reserved; workspace effects
stay LOCAL; sandbox callers never receive broad authority; and
generation/snapshot fencing is in place.

```bash
# TEMP_DIR is an existing temporary directory outside the repository
python3 -m venv "$TEMP_DIR/proto-venv"
"$TEMP_DIR/proto-venv/bin/pip" install grpcio-tools
"$TEMP_DIR/proto-venv/bin/python" proto/check_contract.py --temp-dir "$TEMP_DIR"
```

The import root is `proto/`; imports read `atomic/common/common.proto`.
The checker verifies declarations — it does not claim to test any
implementation of them.
