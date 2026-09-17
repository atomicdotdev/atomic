---
createdAt: 2026-09-17T17:25:05.324928074+00:00
derivedFrom: ["urn:atomic:task:01M2R5352RGSBC61RH4HTNQAJX-2","urn:atomic:intent:01M2R5352RGSBC61RH4HTNQAJX"]
memoryKind: lesson
status: active
uid: 01m2r6d2nc9gx89rqtqwq8kjth
entry_type: memory
content_hash: MLHWRUBSYTMDQCD4S2ZXHQEYO4JSAQRXROSWQ56EDQIUKNFDO5XA
created_at: 2026-09-17T17:25:05.324941179+00:00
updated_at: 2026-09-17T17:25:05.324941179+00:00
---
Rendering a view hierarchy from parent chains silently dropped views caught in a parent cycle: cycle members never appear in the roots list because their parents 'exist' in the listing, so the depth-first walk from roots never reaches them (surfaced by test_parent_cycle_terminates asserting ordered.len()==2 and getting 0). Takeaway: after the visited-set DFS from roots, sweep any remaining entries in name order from depth 0 so nothing silently vanishes; the same visited set that cuts recursion then makes the leftover walk terminate.
