---
memoryId: 01m51gg55garte6ykkvxkqcb38
sourceContentHash: blake3:aadc44d29b6fc23becbc8061990dab5b8e0d70fc0ea9bef5528eac6a8c9192b3
entry_type: attestation
content_hash: AZZZGAPMDMJHM5ZI2TCXAGGITXT3EZ7IWVSNHZCWZBQ6XZUXG5AQ
created_at: 2026-10-16T04:46:14.313426+00:00
updated_at: 2026-10-16T04:46:14.313426+00:00
---
{
  "@context": "https://atomic.dev/ns/ctx.jsonld",
  "@id": "urn:atomic:memory:01m51gg55garte6ykkvxkqcb38",
  "@type": "Memory",
  "attributedTo": "did:atomic:WFYAZJNONDF4MGGDOC5AIAYENDH76UFABKVJFCJBGK5CA374ICEA",
  "contentHash": "blake3:e8ea136a6ceba3458e07d22a6da9ec3c73930bb208f9d7fb1cffa92d841c3558",
  "createdAt": "2026-10-16T04:46:05Z",
  "derivedFrom": [
    "urn:atomic:ac:01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-4",
    "urn:atomic:intent:01M50EXZ198HPVF0CQ2Y0AEH4Z"
  ],
  "memoryKind": "lesson",
  "proof": {
    "@type": "DataIntegrityProof",
    "cryptosuite": "eddsa-jcs-2022",
    "proofPurpose": "assertionMethod",
    "proofValue": "z219Q8P733Q1Qf9v3eVx87D9Kix1rRi7LFF6Jo1zxenzWnksLR2gtB5dCeNU9ZCEwAm6CooqXAc73NGvYTX3eqCuY",
    "verificationMethod": "did:atomic:WFYAZJNONDF4MGGDOC5AIAYENDH76UFABKVJFCJBGK5CA374ICEA#key-1"
  },
  "status": "active",
  "text": "PurRDF 3.0.1 follows owl:imports only on a shapes-document anchor (its loaded IRI, an owl:Ontology, or a sh:ShapesGraph). An unanchored imports triple is ordinary RDF data, so it is not a valid negative fixture for blocked remote imports. Test controlled imports with an anchored subject; Atomic supplies only explicit local shape bundles and lets PurRDF resolve or reject the import closure."
}
