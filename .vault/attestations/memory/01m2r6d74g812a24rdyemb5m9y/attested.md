---
memoryId: 01m2r6d74g812a24rdyemb5m9y
sourceContentHash: blake3:fb0769cc11d9635ae940e0b981f703698f7acebc5e8ad9959ae9211f3b884d6a
entry_type: attestation
content_hash: RP7TGQFOWATMA46SIDPX6VTK66BOEOI3YMTKDTFBY6O3LLZV3KDA
created_at: 2026-09-17T17:25:09.920010241+00:00
updated_at: 2026-09-17T17:25:09.920010241+00:00
---
{
  "@context": "https://atomic.dev/ns/ctx.jsonld",
  "@id": "urn:atomic:memory:01m2r6d74g812a24rdyemb5m9y",
  "@type": "Memory",
  "attributedTo": "did:atomic:YKOT5JBCBRJ6RRUXTH6HYS4LO3DOGLJ76LBUGM4RL74D2GHI2XAQ",
  "contentHash": "blake3:f3a8dc34cea44264ccf07b774c9a6b7225001f3a49182facbfa5ad497fe7d3a8",
  "createdAt": "2026-09-17T17:25:09.904173651+00:00",
  "derivedFrom": [
    "urn:atomic:intent:01M2R5352RGSBC61RH4HTNQAJX"
  ],
  "memoryKind": "constraint",
  "proof": {
    "@type": "DataIntegrityProof",
    "cryptosuite": "eddsa-jcs-2022",
    "proofPurpose": "assertionMethod",
    "proofValue": "z355TG7XEZv92LuKRaXR3owfw5nJ9nMLTthaMQ8LMyBta1HzCVnTjeRvF7MPiccDBGaBgwi3yh5AYX2ddwqpPocmq",
    "verificationMethod": "did:atomic:YKOT5JBCBRJ6RRUXTH6HYS4LO3DOGLJ76LBUGM4RL74D2GHI2XAQ#key-1"
  },
  "status": "active",
  "text": "On this machine /tmp is a 15G tmpfs with a hard quota; cargo builds that exceed it fail mid-compile with 'Disk quota exceeded' while writing cc temp files (e.g. libsqlite3-sys), even when df still shows free space. Long-running builds in /tmp/atomic must use CARGO_TARGET_DIR under /home (e.g. ~/.cache/atomic-target, 262G free on the root fs) and prune old /tmp work dirs."
}
