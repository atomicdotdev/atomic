---
createdAt: 2026-10-12T15:35:36Z
derivedFrom: ["urn:atomic:task:01M4R8PZCD07ADTNQW8R5GBCM3-1","urn:atomic:intent:01M4R8PZCD07ADTNQW8R5GBCM3"]
memoryKind: decision
status: active
uid: 01m4rc2hnc1t3ap8nv9rgzfrzj
entry_type: memory
content_hash: ZGHFYBOKPMBJ2NILUZ7T3DXYCXX7BROVVORO5OBZ5BJGSXRDTEVA
created_at: 2026-10-12T15:35:36.074340+00:00
updated_at: 2026-10-12T15:35:36.074340+00:00
---
Hook operational diagnostics use immutable session-scoped events in the existing atomic.redb and local/reactor service layer. They cannot reuse running-turn provenance envelopes: parse failures have no reserved turn, and post-completion failures must remain recordable without changing replay semantics. Transactional append and event-ID deduplication replace shared health snapshots; per-event outage files preserve observations until the service can replay them.
