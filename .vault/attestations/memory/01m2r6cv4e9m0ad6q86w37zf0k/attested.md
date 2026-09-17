---
memoryId: 01m2r6cv4e9m0ad6q86w37zf0k
sourceContentHash: blake3:2c818e059cbf6dd40b0296db80683dccc4fac53f91b630ec2b0a8b2f24afe004
entry_type: attestation
content_hash: GCTHCNOQ6RCJODJWYZ6X5XM2DUXT3IY4LCW3KHG6CX7YYIF6V2HA
created_at: 2026-09-17T17:24:57.626991996+00:00
updated_at: 2026-09-17T17:24:57.626991996+00:00
---
{
  "@context": "https://atomic.dev/ns/ctx.jsonld",
  "@id": "urn:atomic:memory:01m2r6cv4e9m0ad6q86w37zf0k",
  "@type": "Memory",
  "attributedTo": "did:atomic:YKOT5JBCBRJ6RRUXTH6HYS4LO3DOGLJ76LBUGM4RL74D2GHI2XAQ",
  "contentHash": "blake3:aba911c4d17945932d195587209208c3cd08b02b889ec3fb9ca8c7bd5f55672e",
  "createdAt": "2026-09-17T17:24:57.614092910+00:00",
  "derivedFrom": [
    "urn:atomic:ac:01M2R5352RGSBC61RH4HTNQAJX-ac-2",
    "urn:atomic:intent:01M2R5352RGSBC61RH4HTNQAJX"
  ],
  "memoryKind": "decision",
  "proof": {
    "@type": "DataIntegrityProof",
    "cryptosuite": "eddsa-jcs-2022",
    "proofPurpose": "assertionMethod",
    "proofValue": "z65XjM2mzSMo9AR1y9yB1brRkkDS6iM9eHiRodPNvdve3AsCM3mwvxnUWQCFcBt898MfoN7LPFEnpa6Ap6J8pswGr",
    "verificationMethod": "did:atomic:YKOT5JBCBRJ6RRUXTH6HYS4LO3DOGLJ76LBUGM4RL74D2GHI2XAQ#key-1"
  },
  "status": "active",
  "text": "For 'atomic view list' empty-view filtering, 'pending changes' was deliberately defined as ViewInfo::own_change_count (changes recorded into the view that are not visible through the parent chain) — not unrecorded working-copy status, which would require per-view workspace inspection. A view is hidden iff own_change_count == 0 AND no shown descendant exists AND it is not the current view; ancestors of shown views are pulled in so the hierarchy stays connected. Rejected alternative: hiding views whose parent has changes (would make hiding useless, since drafts always parent onto shared views). Outcome: pure helpers (compute_visibility, tree_order, render_line, summary_line) in the CLI module plus an -a/--all override."
}
