---
intentId: ATOM::bradley::2
sourceContentHash: blake3:9cf2379bca5d91441901911ca0bdefcfbfeaf095d032be085580f3f54dde081d
entry_type: attestation
content_hash: Z2WXSXC5YGU34KA66ZBR4ZH5HWJ4DFZX7ELXFZ3MERKQYQFOO5OA
created_at: 2026-10-16T15:56:19.421002+00:00
updated_at: 2026-10-16T15:56:19.421002+00:00
---
{
  "@context": "https://atomic.dev/ns/ctx.jsonld",
  "@id": "urn:atomic:intent:01M52MAXBMBSEFD9N5GB8YNXHV",
  "@type": "Intent",
  "attributedTo": "did:atomic:2FMHT6HKUEBZDK6OVFZHPS2JNCRVTFXJSSNRQB75O6O44CXCMEUA",
  "contentHash": "blake3:9a2f1f3a216a6bc45debdbe13614fd7b24c91defdefa475f0bf2efbb260db5f1",
  "createdAt": "",
  "dependsOn": [
    {
      "edge": "reviews",
      "to": "urn:atomic:intent:01M50EXZ198HPVF0CQ2Y0AEH4Z"
    }
  ],
  "hasAcceptanceCriterion": [
    {
      "@id": "urn:atomic:ac:01M52MAXBMBSEFD9N5GB8YNXHV-ac-1",
      "@type": "AcceptanceCriterion",
      "acStatus": "met",
      "evidence": "Harness 46 ontology/IRI/identity tests and the independently rerun signed identity verification passed; see review.md M1/M2 and checks 2-5.",
      "requiredKinds": [
        "integration"
      ],
      "text": "The inspected M1 ontology/typed registry and M2 local identity minting, storage, strict mint/parse and signature invariants satisfy author AC1/AC2 under independent checks.",
      "verifications": [
        {
          "@type": "VerificationRecord",
          "kind": "integration",
          "observation": "Harness 46 core kg_ontology/kg_iri/kg_vocabulary_lint and repository project_identity passed; signed identity unit test passed. Pin source is view show deep-sea-79f7, not an unavailable triage report.",
          "observedAtMerkle": "IXTPRO5JWHQVJ3KFGS3G4TFISJFYWEW6VE2BYFF5QMD2PV5G3IUA",
          "outcome": "pass",
          "scope": "ac"
        }
      ],
      "verifiedBy": "did:atomic:2FMHT6HKUEBZDK6OVFZHPS2JNCRVTFXJSSNRQB75O6O44CXCMEUA"
    },
    {
      "@id": "urn:atomic:ac:01M52MAXBMBSEFD9N5GB8YNXHV-ac-2",
      "@type": "AcceptanceCriterion",
      "acStatus": "unmet",
      "requiredKinds": [
        "integration"
      ],
      "text": "Author AC3 genuinely holds: canonical projection preserves owner evidence, repairs late references on ingestion, and resolves distinct canonical identities correctly.",
      "verifications": [
        {
          "@type": "VerificationRecord",
          "kind": "integration",
          "observation": "Blocked by reproducible R1 child evidence overwrite, R2 late change reference not repaired, and R3 exact canonical IRI case collision. repro.rs exits 0 because its assertions confirm the bugs; this correctness judgment is fail.",
          "observedAtMerkle": "IXTPRO5JWHQVJ3KFGS3G4TFISJFYWEW6VE2BYFF5QMD2PV5G3IUA",
          "outcome": "fail",
          "scope": "ac"
        }
      ]
    },
    {
      "@id": "urn:atomic:ac:01M52MAXBMBSEFD9N5GB8YNXHV-ac-3",
      "@type": "AcceptanceCriterion",
      "acStatus": "met",
      "evidence": "Harness signer_refresh and real snapshot tests passed; independent poisoned-cache, DID/key mismatch, valid signature, and payload tampering checks passed in repro.rs.",
      "requiredKinds": [
        "integration"
      ],
      "text": "The reviewed signer discovery and offline cryptographic boundary checks key fingerprints separately from SHACL and rejects mismatched keys or tampered signed payloads in the executed cases.",
      "verifications": [
        {
          "@type": "VerificationRecord",
          "kind": "integration",
          "observation": "libatomic signer_refresh 2/2 and real snapshots 4/4 passed; repro.rs confirms poisoned cache remains Unverifiable, valid key becomes Verified, and tampering becomes Failed.",
          "observedAtMerkle": "IXTPRO5JWHQVJ3KFGS3G4TFISJFYWEW6VE2BYFF5QMD2PV5G3IUA",
          "outcome": "pass",
          "scope": "ac"
        }
      ],
      "verifiedBy": "did:atomic:2FMHT6HKUEBZDK6OVFZHPS2JNCRVTFXJSSNRQB75O6O44CXCMEUA"
    },
    {
      "@id": "urn:atomic:ac:01M52MAXBMBSEFD9N5GB8YNXHV-ac-4",
      "@type": "AcceptanceCriterion",
      "acStatus": "met",
      "evidence": "PurRDF profiles 6/6, W3C 1/1, real snapshots 4/4 and CLI custom shapes/error exits passed in harness 46; cargo +1.90.0 check --workspace --exclude atomic-cli --exclude atomic-shacl --locked --quiet passed.",
      "requiredKinds": [
        "integration"
      ],
      "text": "The reviewed M4 runtime uses pinned PurRDF over asserted snapshots, preserves reports and coverage, rejects validation errors, and shared libraries build on Rust 1.90. Candidate packaging is assessed separately below.",
      "verifications": [
        {
          "@type": "VerificationRecord",
          "kind": "integration",
          "observation": "Harness 46 profiles, custom shapes, controlled imports, report/path fidelity, vacuous diagnostics and exits passed; shared-library Rust 1.90 check exited 0; extra publication Failed-status fixture rejects.",
          "observedAtMerkle": "IXTPRO5JWHQVJ3KFGS3G4TFISJFYWEW6VE2BYFF5QMD2PV5G3IUA",
          "outcome": "pass",
          "scope": "ac"
        }
      ],
      "verifiedBy": "did:atomic:2FMHT6HKUEBZDK6OVFZHPS2JNCRVTFXJSSNRQB75O6O44CXCMEUA"
    },
    {
      "@id": "urn:atomic:ac:01M52MAXBMBSEFD9N5GB8YNXHV-ac-5",
      "@type": "AcceptanceCriterion",
      "acStatus": "met",
      "evidence": "Fresh workspace CLI passed tests/harness/46_knowledge_graph_closure.sh, including caller/triage golden result sets. review.md records exact executed tests and limits; repro.rs preserves additional adversarial cases.",
      "requiredKinds": [
        "e2e"
      ],
      "text": "Meaningful existing end-to-end verification and adversarial reproductions were independently executed and reported accurately; passing happy-path tests are not treated as proof of all author criteria.",
      "verifications": [
        {
          "@type": "VerificationRecord",
          "kind": "e2e",
          "observation": "Harness 46 exited 0 within the 150s subprocess bound. Standalone repro final run exited 0, confirming three bugs and two negative-security checks. No claim of full workspace/clippy/rustdoc reruns.",
          "observedAtMerkle": "IXTPRO5JWHQVJ3KFGS3G4TFISJFYWEW6VE2BYFF5QMD2PV5G3IUA",
          "outcome": "pass",
          "scope": "ac"
        }
      ],
      "verifiedBy": "did:atomic:2FMHT6HKUEBZDK6OVFZHPS2JNCRVTFXJSSNRQB75O6O44CXCMEUA"
    },
    {
      "@id": "urn:atomic:ac:01M52MAXBMBSEFD9N5GB8YNXHV-ac-6",
      "@type": "AcceptanceCriterion",
      "acStatus": "unmet",
      "requiredKinds": [
        "manual"
      ],
      "text": "The recorded candidate includes required CI/release updates and the author handoff has a fresh, verified attestation; author AC4/AC5 delivery is complete.",
      "verifications": [
        {
          "@type": "VerificationRecord",
          "kind": "manual",
          "observation": "R4: ci.yml and release.yml remain untracked and absent from candidate diff-stat; author intent verify exits 2 with stale attestation. Existing harness omits R1-R3 cases, so all five author ACs are not established.",
          "observedAtMerkle": "IXTPRO5JWHQVJ3KFGS3G4TFISJFYWEW6VE2BYFF5QMD2PV5G3IUA",
          "outcome": "fail",
          "scope": "ac"
        }
      ]
    },
    {
      "@id": "urn:atomic:ac:01M52MAXBMBSEFD9N5GB8YNXHV-ac-7",
      "@type": "AcceptanceCriterion",
      "acStatus": "unmet",
      "requiredKinds": [
        "manual"
      ],
      "text": "A complete reproducible triage report, walkthrough and content-addressed reference pin the promotion candidate and its linkage/blast-radius findings.",
      "verifications": [
        {
          "@type": "VerificationRecord",
          "kind": "manual",
          "observation": "Full triage unavailable after 120s/600s timeouts and externally terminated background retry. Only bounded candidates/view show obtained. This field is a source-view pin, not a fabricated triage reference.",
          "observedAtMerkle": "IXTPRO5JWHQVJ3KFGS3G4TFISJFYWEW6VE2BYFF5QMD2PV5G3IUA",
          "outcome": "fail",
          "scope": "ac"
        }
      ]
    }
  ],
  "hasConstraint": [
    {
      "@id": "urn:atomic:constraint:01m52maxbmbsefd9n5gb8ynxhv-constraint-1",
      "@type": "Constraint",
      "text": "Use the delegated reviewer identity for all signing. Preserve unrelated working-copy content. Leave failed review criteria unmet; do not manufacture approval."
    },
    {
      "@id": "urn:atomic:constraint:01m52maxbmbsefd9n5gb8ynxhv-constraint-2",
      "@type": "Constraint",
      "text": "This blocked review binds the exact report and reproduction source: review.md SHA-256 803c74b5599b2037bb8b6542274717554e28fafe75605a911a40ad879f013b09; repro.rs SHA-256 30ebcdcd5030e1307307aee11ba9b3152dc24aebfa4750b8c0b396b8604daca0. Observed Merkle IXTPRO5JWHQVJ3KFGS3G4TFISJFYWEW6VE2BYFF5QMD2PV5G3IUA comes from source-view show, not a full triage report. Verdict: blocked, do not promote on this review."
    }
  ],
  "hasScopeIn": [
    {
      "@id": "urn:atomic:scope:01m52maxbmbsefd9n5gb8ynxhv-scope-in-1",
      "@type": "ScopeItem",
      "text": "Review evidence, narrative report, acceptance judgments, and reviewer attestation for ATOM::bradley::1."
    }
  ],
  "hasScopeOut": [
    {
      "@id": "urn:atomic:scope:01m52maxbmbsefd9n5gb8ynxhv-scope-out-1",
      "@type": "ScopeItem",
      "text": "Application fixes, author intent edits, default identity changes, recording, view creation/switching or promotion."
    }
  ],
  "hasTask": [
    {
      "@id": "urn:atomic:task:01M52MAXBMBSEFD9N5GB8YNXHV-1",
      "@type": "Task",
      "satisfies": [
        "urn:atomic:ac:01M52MAXBMBSEFD9N5GB8YNXHV-ac-1"
      ],
      "taskStatus": "done",
      "text": "Inspect the plan, work intent, signed author lesson, candidate metadata and actual source; author the evidence-backed narrative and acceptance judgments.",
      "touchesFile": [
        ".vault/intents/01M52MAXBMBSEFD9N5GB8YNXHV/review.md"
      ]
    },
    {
      "@id": "urn:atomic:task:01M52MAXBMBSEFD9N5GB8YNXHV-2",
      "@type": "Task",
      "satisfies": [
        "urn:atomic:ac:01M52MAXBMBSEFD9N5GB8YNXHV-ac-2"
      ],
      "taskStatus": "done",
      "text": "Run adversarial scratch reproductions for projection correctness and cryptographic rejection, preserving their source as review evidence.",
      "touchesFile": [
        ".vault/intents/01M52MAXBMBSEFD9N5GB8YNXHV/repro.rs"
      ]
    },
    {
      "@id": "urn:atomic:task:01M52MAXBMBSEFD9N5GB8YNXHV-3",
      "@type": "Task",
      "satisfies": [
        "urn:atomic:ac:01M52MAXBMBSEFD9N5GB8YNXHV-ac-5"
      ],
      "taskStatus": "done",
      "text": "Independently rerun the Phase 1 harness and targeted identity, KG and shared-library MSRV checks, recording actual results and limits.",
      "touchesFile": [
        ".vault/intents/01M52MAXBMBSEFD9N5GB8YNXHV/review.md"
      ]
    }
  ],
  "humanKey": "ATOM::bradley::2",
  "kind": "review",
  "priority": "medium",
  "proof": {
    "@type": "DataIntegrityProof",
    "cryptosuite": "eddsa-jcs-2022",
    "proofPurpose": "assertionMethod",
    "proofValue": "z2Yec5SLdztrCscXhpXZmZnq6JgCNERLVFyUnHtcEArHU6V7pnKwz1nQwGjm3Ch6m7uE3z4dyhcBHazBF5sCAtxFt",
    "verificationMethod": "did:atomic:2FMHT6HKUEBZDK6OVFZHPS2JNCRVTFXJSSNRQB75O6O44CXCMEUA#key-1"
  },
  "status": "in_progress",
  "title": "Review Phase 1 KG referential closure",
  "view": "golden-snowflake-b8a8",
  "why": "Independently establish whether the Phase 1 KG closure implementation actually satisfies its plan and author criteria before promotion. This is a fresh reviewer context with a separately delegated key, not a claim of a different model."
}
