# PurRDF SHACL integration spike

Isolated test of the **published `purrdf-shapes = 3.0.1`** crate. This is not an
Atomic validator implementation and is deliberately outside the main workspace.
It does not alter Atomic's dependency graph, Rust requirement, or database.

## Run

PurRDF 3.0.1 requires Rust **1.98**; Atomic currently declares **1.90**.
Install the spike toolchain without changing the global default:

```bash
rustup toolchain install 1.98.0 --profile minimal
rustup run 1.98.0 cargo test --manifest-path tools/shacl-purrdf-spike/Cargo.toml --locked
```

If a non-rustup `rustc` on PATH overrides the selected compiler (as with the
Homebrew installation used for this spike), use:

```bash
RUSTC="$(rustup which --toolchain 1.98.0 rustc)" \
RUSTDOC="$(rustup which --toolchain 1.98.0 rustdoc)" \
rustup run 1.98.0 cargo test \
  --manifest-path tools/shacl-purrdf-spike/Cargo.toml --locked -- --nocapture
```

`Cargo.lock` records the complete experiment dependency resolution. No source
patches or `--ignore-rust-version` are used.

## Coverage

- Required plan cardinality: missing, exactly one, and multiple plans.
- Standard report fields, source shape/component, severity, and message.
- Nested `sh:node` acceptance-criterion enum validation.
- SHACL-SPARQL: met criteria require both reviewer and evidence.
- Inverse/sequence property paths, including complex path report serialization.
- Class, datatype, and boolean (`sh:or`) constraints.
- Separate missing properties retain distinct result paths.
- Malformed constraint parameters and SPARQL queries fail shape loading.
- Unsupported SHACL-JS fails loading, even with an empty data graph.
- Valid shapes with no applicable focus nodes conform vacuously (not audit evidence).
- Two project-scoped subjects with identical local IDs remain distinct.

See [`docs/shacl-purrdf-spike-findings.md`](../../docs/shacl-purrdf-spike-findings.md)
for results and integration limits. These tests do not establish complete SHACL
conformance or enterprise compliance.
