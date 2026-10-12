---
intentId: ATOM::bradley::1
sourceContentHash: blake3:917226069097a311cb4a128e7696baac20edec31b113e7c0b9df0d75f774268e
entry_type: attestation
content_hash: 4Z276LCJT24CGJWNZRPLVJTDKQZVUTF4C62KCO7JU3KJXIZG5LCA
created_at: 2026-10-12T15:36:35.583622+00:00
updated_at: 2026-10-12T15:36:35.583622+00:00
---
{
  "@context": "https://atomic.dev/ns/ctx.jsonld",
  "@id": "urn:atomic:intent:01M4R8PZCD07ADTNQW8R5GBCM3",
  "@type": "Intent",
  "attributedTo": "did:atomic:WFYAZJNONDF4MGGDOC5AIAYENDH76UFABKVJFCJBGK5CA374ICEA",
  "contentHash": "blake3:5b7dfab9b8bd2493ec1987e7cf6508746b16233d6fe7fdfe5c2afd95623b651f",
  "createdAt": "",
  "hasAcceptanceCriterion": [
    {
      "@id": "urn:atomic:ac:01M4R8PZCD07ADTNQW8R5GBCM3-ac-1",
      "@type": "AcceptanceCriterion",
      "acStatus": "met",
      "evidence": "Hook tracing/health tests, transactional concurrent session-log tests, local and TCP gRPC dispatch tests, and CLI multi-process/outage regressions pass. Workspace Clippy, formatting, documentation, and protobuf contract checks pass. Workspace tests outside atomic-cli and CLI tests pass except the existing provenance_rpc_sink fixture requiring atomicd; the sibling Reactor now builds atomic-reactor instead.",
      "text": "Native hooks, typed receive, and shared service dispatch persist tracing events through the existing database/service infrastructure. Concurrent parent/child events survive, malformed input remains diagnosable, and status reports session-scoped health without rewriting hook-health.json. Relevant tests, formatting, protocol checks, and workspace Clippy pass.",
      "verifiedBy": "OpenCode (openrouter/openai/gpt-6.1-sol-pro)"
    }
  ],
  "hasConstraint": [
    {
      "@id": "urn:atomic:constraint:01m4r8pzcd07adtnqw8r5gbcm3-constraint-1",
      "@type": "Constraint",
      "text": "Use the existing database ownership and local/reactor service model. Operational diagnostics must preserve the original hook result and remain independent of turn-constrained provenance replay. Preserve unrelated user edits."
    }
  ],
  "hasScopeIn": [
    {
      "@id": "urn:atomic:scope:01m4r8pzcd07adtnqw8r5gbcm3-scope-in-1",
      "@type": "ScopeItem",
      "text": "Hook diagnostics, session operational logs, service access, and related tests."
    }
  ],
  "hasScopeOut": [
    {
      "@id": "urn:atomic:scope:01m4r8pzcd07adtnqw8r5gbcm3-scope-out-1",
      "@type": "ScopeItem",
      "text": "Release/version automation and unrelated vault edits."
    }
  ],
  "hasTask": [
    {
      "@id": "urn:atomic:task:01M4R8PZCD07ADTNQW8R5GBCM3-1",
      "@type": "Task",
      "satisfies": [
        "urn:atomic:ac:01M4R8PZCD07ADTNQW8R5GBCM3-ac-1"
      ],
      "taskStatus": "done",
      "text": "Implement transactional event storage, service append/query methods, tracing emission, session correlation, health projection, and concurrency regressions.",
      "touchesFile": [
        "atomic-agent/src/hook_health.rs",
        "atomic-repository/src/redb_change_store",
        "libatomic/src/daemon/services_agent.rs",
        "atomic-cli/src/commands/agent"
      ]
    }
  ],
  "humanKey": "ATOM::bradley::1",
  "priority": "medium",
  "proof": {
    "@type": "DataIntegrityProof",
    "cryptosuite": "eddsa-jcs-2022",
    "proofPurpose": "assertionMethod",
    "proofValue": "zEjZUA8BagyqfJZhPHfEMpRv1LziDDac3xyHVby538TZskdStsUXbWP2F8FRdfdkbMzAy9sxmzvNEBj7fWSEiV5X",
    "verificationMethod": "did:atomic:WFYAZJNONDF4MGGDOC5AIAYENDH76UFABKVJFCJBGK5CA374ICEA#key-1"
  },
  "status": "needs-review",
  "title": "Replace hook health snapshots with session-scoped operational logs",
  "view": "dev",
  "why": "Concurrent parent and child agents can lose hook-health updates in the shared JSON snapshot. Persist session-scoped operational events and derive health from those events so failures remain attributable and concurrent dispatches cannot overwrite one another."
}
