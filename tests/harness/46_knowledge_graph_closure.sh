#!/usr/bin/env bash
# Phase 1: ontology/identity, real producer closure, RDF/SHACL, CLI, and
# pre-cutover caller/triage result-set parity. All repositories are temporary.
set -euo pipefail
HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"
python3 "$HARNESS_DIR/kg_closure_e2e.py" --binary "$ATOMIC_BIN"
python3 "$HARNESS_DIR/kg_closure_baseline.py" --binary "$ATOMIC_BIN" --check
# CI already runs these suites in its workspace test step, and invoking them
# here with a different package/feature set rebuilds the crate graph.
if [[ "${ATOMIC_HARNESS_SKIP_CARGO_TESTS:-0}" == 1 ]]; then
    exit 0
fi
cargo test --manifest-path "$HARNESS_DIR/../../Cargo.toml" -p atomic-core --test kg_ontology --test kg_iri --test kg_vocabulary_lint --quiet
cargo test --manifest-path "$HARNESS_DIR/../../Cargo.toml" -p atomic-repository --test it --quiet -- project_identity:: kg_projection::
cargo test --manifest-path "$HARNESS_DIR/../../Cargo.toml" -p atomic-shacl --quiet
cargo test --manifest-path "$HARNESS_DIR/../../Cargo.toml" -p libatomic --test signer_refresh --quiet
cargo test --manifest-path "$HARNESS_DIR/../../Cargo.toml" -p atomic-cli --test it --quiet -- kg_service_integration_test::
