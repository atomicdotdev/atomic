---
author: bradley
created_by: bradley <bradley.hilton@atomic.dev>
goals: []
id: ATOM::bradley::1
priority: medium
project: ATOM
seq: 1
status: needs-review
title: Implement Phase 1 knowledge graph referential closure
uid: 01M50EXZ198HPVF0CQ2Y0AEH4Z
view: deep-sea-79f7
entry_type: intent
content_hash: 3E7U7Y2PSKIW6XDNB7U5PXP4FT4OAI5GKLRYTRFDQXV6YIADLWJQ
created_at: 2026-10-16T04:56:13.716334+00:00
updated_at: 2026-10-16T04:56:13.716334+00:00
---
:::why
Make Atomic's derived knowledge graph a canonical, referentially closed workflow read model, with stable project-qualified resources and real SHACL validation, as specified by docs/knowledge-graph-closure-phase1-plan.md and its RFC.
:::

:::acceptance-criterion{#01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-1 status=met verifiedBy="kg_ontology and kg_vocabulary_lint" evidence="cargo test -p atomic-core --test kg_ontology --test kg_iri --test kg_vocabulary_lint passed through harness 46"}
The versioned workflow ontology and typed registry agree, including external domains/ranges, and all retired KG terms have an explicit disposition.
:::

:::acceptance-criterion{#01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-2 status=met verifiedBy="project_identity integration and signed identity unit tests" evidence="project_identity tests, strict IRI roundtrips, signed proof verification, and CLI identity stability passed"}
Project identity is view-independent, optionally signed, automatically minted on the specified paths, and every supported resource IRI round-trips strictly.
:::

:::acceptance-criterion{#01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-3 status=met verifiedBy="kg_projection, workflow_snapshot, signer_refresh, and golden baseline" evidence="Harness 46 passed generation rollback/fencing, direct membership-only updates, sidecar closure, canonical plans, and pre-cutover caller/triage result-set parity"}
The generation-fenced KG rebuild and all producers/consumers use canonical vocabulary and IRIs, preserving callers/triage result sets and distinguishing common-graph objects from direct view membership.
:::

:::acceptance-criterion{#01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-4 status=met verifiedBy="PurRDF profiles, W3C class fixture, real snapshots, and Rust 1.90.0" evidence="atomic-shacl tests and isolated 11-test spike passed; cargo +1.90.0 check --workspace --exclude atomic-cli --exclude atomic-shacl --locked passed"}
PurRDF evaluates builtin and custom shapes through the CLI, preserves standard reports and target coverage, and shared library crates retain Rust 1.90 compatibility.
:::

:::acceptance-criterion{#01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-5 status=met verifiedBy="tests/harness/46_knowledge_graph_closure.sh" evidence="Freshly built CLI passed the full new harness; core/repository library suites, CLI units/session integration, libatomic suite, workspace clippy, formatting, and documentation checks passed"}
A new full harness under tests/ exercises the implemented workflow against the Phase 1 plan/RFC, including incomplete history, membership-only updates, sidecar ingestion, and validation failures.
:::

:::task{#01M50EXZ198HPVF0CQ2Y0AEH4Z-1 status=done criteria=01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-1}
Implement M1 ontology, registry, legacy inventory, and consistency checks.
::file-ref{path=atomic-core/src/kg/ontology.rs}
:::

:::task{#01M50EXZ198HPVF0CQ2Y0AEH4Z-2 status=done criteria=01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-2}
Implement M2 project identity, storage, signing, strict IRIs, and CLI.
::file-ref{path=atomic-core/src/kg/project_id.rs}
:::

:::task{#01M50EXZ198HPVF0CQ2Y0AEH4Z-3 status=done criteria=01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-3}
Capture baselines and implement the atomic M3 producer/consumer cutover, repository ingestion hooks, and external signer-key refresh.
::file-ref{path=atomic-repository/src/repository/vault_kg_enrich.rs}
:::

:::task{#01M50EXZ198HPVF0CQ2Y0AEH4Z-4 status=done criteria=01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-4}
Implement M4 CLI-only SHACL adaptation and validation with the scoped compiler requirement.
::file-ref{path=atomic-shacl/src/lib.rs}
:::

:::task{#01M50EXZ198HPVF0CQ2Y0AEH4Z-5 status=done criteria=01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-5}
Add and run the full end-to-end harness and affected regression checks.
::file-ref{path=tests/harness/46_knowledge_graph_closure.sh}
:::

:::scope-in
Phase 1 milestones M1–M4, local projection maintenance at repository ingestion/membership APIs, shared signer-key refresh using existing identity endpoints, CLI/daemon/proto consumers, and full-harness verification.
:::

:::scope-out
ProjectIdentity transport/adoption/remote binding, sync wire-contract changes, OWL materialization, unified reconciliation, mandatory publication gates, and persistent code-structure KG edges.
:::

:::constraint
Atomic stores one common causal graph; views are membership filters. Projection follows authoritative objects and actual membership, never download counts, working-copy view context, or Git branch-history assumptions. SHACL evaluation belongs to PurRDF; cryptographic verification remains separate. Shared libraries remain Rust 1.90 compatible.
:::
