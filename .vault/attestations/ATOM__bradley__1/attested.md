---
intentId: ATOM::bradley::1
sourceContentHash: blake3:ab244785fa37722d73bdadfb39957b58d167e03aa9a7e9c4f5a591c80dd2830d
entry_type: attestation
content_hash: PCRRBZEYOBJUBHR4XX63N35GB6Y4F5HCFISCNJCJG3IPHV7XWFTQ
created_at: 2026-10-16T04:53:31.405763+00:00
updated_at: 2026-10-16T04:53:31.405763+00:00
---
{
  "@context": "https://atomic.dev/ns/ctx.jsonld",
  "@id": "urn:atomic:intent:01M50EXZ198HPVF0CQ2Y0AEH4Z",
  "@type": "Intent",
  "attributedTo": "did:atomic:WFYAZJNONDF4MGGDOC5AIAYENDH76UFABKVJFCJBGK5CA374ICEA",
  "contentHash": "blake3:917a6c9db6d589a205e793aa059131829e1a906f563a76b65737b1f1b764db01",
  "createdAt": "",
  "hasAcceptanceCriterion": [
    {
      "@id": "urn:atomic:ac:01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-1",
      "@type": "AcceptanceCriterion",
      "acStatus": "met",
      "evidence": "cargo test -p atomic-core --test kg_ontology --test kg_iri --test kg_vocabulary_lint passed through harness 46",
      "text": "The versioned workflow ontology and typed registry agree, including external domains/ranges, and all retired KG terms have an explicit disposition.",
      "verifiedBy": "kg_ontology and kg_vocabulary_lint"
    },
    {
      "@id": "urn:atomic:ac:01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-2",
      "@type": "AcceptanceCriterion",
      "acStatus": "met",
      "evidence": "project_identity tests, strict IRI roundtrips, signed proof verification, and CLI identity stability passed",
      "text": "Project identity is view-independent, optionally signed, automatically minted on the specified paths, and every supported resource IRI round-trips strictly.",
      "verifiedBy": "project_identity integration and signed identity unit tests"
    },
    {
      "@id": "urn:atomic:ac:01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-3",
      "@type": "AcceptanceCriterion",
      "acStatus": "met",
      "evidence": "Harness 46 passed generation rollback/fencing, direct membership-only updates, sidecar closure, canonical plans, and pre-cutover caller/triage result-set parity",
      "text": "The generation-fenced KG rebuild and all producers/consumers use canonical vocabulary and IRIs, preserving callers/triage result sets and distinguishing common-graph objects from direct view membership.",
      "verifiedBy": "kg_projection, workflow_snapshot, signer_refresh, and golden baseline"
    },
    {
      "@id": "urn:atomic:ac:01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-4",
      "@type": "AcceptanceCriterion",
      "acStatus": "met",
      "evidence": "atomic-shacl tests and isolated 11-test spike passed; cargo +1.90.0 check --workspace --exclude atomic-cli --exclude atomic-shacl --locked passed",
      "text": "PurRDF evaluates builtin and custom shapes through the CLI, preserves standard reports and target coverage, and shared library crates retain Rust 1.90 compatibility.",
      "verifiedBy": "PurRDF profiles, W3C class fixture, real snapshots, and Rust 1.90.0"
    },
    {
      "@id": "urn:atomic:ac:01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-5",
      "@type": "AcceptanceCriterion",
      "acStatus": "met",
      "evidence": "Freshly built CLI passed the full new harness; core/repository library suites, CLI units/session integration, libatomic suite, workspace clippy, formatting, and documentation checks passed",
      "text": "A new full harness under tests/ exercises the implemented workflow against the Phase 1 plan/RFC, including incomplete history, membership-only updates, sidecar ingestion, and validation failures.",
      "verifiedBy": "tests/harness/46_knowledge_graph_closure.sh"
    }
  ],
  "hasConstraint": [
    {
      "@id": "urn:atomic:constraint:01m50exz198hpvf0cq2y0aeh4z-constraint-1",
      "@type": "Constraint",
      "text": "Atomic stores one common causal graph; views are membership filters. Projection follows authoritative objects and actual membership, never download counts, working-copy view context, or Git branch-history assumptions. SHACL evaluation belongs to PurRDF; cryptographic verification remains separate. Shared libraries remain Rust 1.90 compatible."
    }
  ],
  "hasScopeIn": [
    {
      "@id": "urn:atomic:scope:01m50exz198hpvf0cq2y0aeh4z-scope-in-1",
      "@type": "ScopeItem",
      "text": "Phase 1 milestones M1–M4, local projection maintenance at repository ingestion/membership APIs, shared signer-key refresh using existing identity endpoints, CLI/daemon/proto consumers, and full-harness verification."
    }
  ],
  "hasScopeOut": [
    {
      "@id": "urn:atomic:scope:01m50exz198hpvf0cq2y0aeh4z-scope-out-1",
      "@type": "ScopeItem",
      "text": "ProjectIdentity transport/adoption/remote binding, sync wire-contract changes, OWL materialization, unified reconciliation, mandatory publication gates, and persistent code-structure KG edges."
    }
  ],
  "hasTask": [
    {
      "@id": "urn:atomic:task:01M50EXZ198HPVF0CQ2Y0AEH4Z-1",
      "@type": "Task",
      "satisfies": [
        "urn:atomic:ac:01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-1"
      ],
      "taskStatus": "done",
      "text": "Implement M1 ontology, registry, legacy inventory, and consistency checks.",
      "touchesFile": [
        "atomic-core/src/kg/ontology.rs"
      ]
    },
    {
      "@id": "urn:atomic:task:01M50EXZ198HPVF0CQ2Y0AEH4Z-2",
      "@type": "Task",
      "satisfies": [
        "urn:atomic:ac:01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-2"
      ],
      "taskStatus": "done",
      "text": "Implement M2 project identity, storage, signing, strict IRIs, and CLI.",
      "touchesFile": [
        "atomic-core/src/kg/project_id.rs"
      ]
    },
    {
      "@id": "urn:atomic:task:01M50EXZ198HPVF0CQ2Y0AEH4Z-3",
      "@type": "Task",
      "satisfies": [
        "urn:atomic:ac:01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-3"
      ],
      "taskStatus": "done",
      "text": "Capture baselines and implement the atomic M3 producer/consumer cutover, repository ingestion hooks, and external signer-key refresh.",
      "touchesFile": [
        "atomic-repository/src/repository/vault_kg_enrich.rs"
      ]
    },
    {
      "@id": "urn:atomic:task:01M50EXZ198HPVF0CQ2Y0AEH4Z-4",
      "@type": "Task",
      "satisfies": [
        "urn:atomic:ac:01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-4"
      ],
      "taskStatus": "done",
      "text": "Implement M4 CLI-only SHACL adaptation and validation with the scoped compiler requirement.",
      "touchesFile": [
        "atomic-shacl/src/lib.rs"
      ]
    },
    {
      "@id": "urn:atomic:task:01M50EXZ198HPVF0CQ2Y0AEH4Z-5",
      "@type": "Task",
      "satisfies": [
        "urn:atomic:ac:01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-5"
      ],
      "taskStatus": "done",
      "text": "Add and run the full end-to-end harness and affected regression checks.",
      "touchesFile": [
        "tests/harness/46_knowledge_graph_closure.sh"
      ]
    }
  ],
  "humanKey": "ATOM::bradley::1",
  "priority": "medium",
  "proof": {
    "@type": "DataIntegrityProof",
    "cryptosuite": "eddsa-jcs-2022",
    "proofPurpose": "assertionMethod",
    "proofValue": "zS2RYCz9EdBUfnMJGHT5JGPpDhCxFTtJisN53y1vjGGKxqpbtWRYfiHbm1HTFgPrQBdLa7XXJHsuq8GqgwRQD46d",
    "verificationMethod": "did:atomic:WFYAZJNONDF4MGGDOC5AIAYENDH76UFABKVJFCJBGK5CA374ICEA#key-1"
  },
  "status": "in_progress",
  "title": "Implement Phase 1 knowledge graph referential closure",
  "view": "deep-sea-79f7",
  "why": "Make Atomic's derived knowledge graph a canonical, referentially closed workflow read model, with stable project-qualified resources and real SHACL validation, as specified by docs/knowledge-graph-closure-phase1-plan.md and its RFC."
}
