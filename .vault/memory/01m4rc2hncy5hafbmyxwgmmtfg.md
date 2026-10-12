---
createdAt: 2026-10-12T15:35:35Z
derivedFrom: ["urn:atomic:task:01M4R8PZCD07ADTNQW8R5GBCM3-1","urn:atomic:intent:01M4R8PZCD07ADTNQW8R5GBCM3"]
memoryKind: lesson
status: active
uid: 01m4rc2hncy5hafbmyxwgmmtfg
entry_type: memory
content_hash: AK7VZOLN4TUU7GJSXVDN3KTOFN4MD6JPR7Y3ICFO5UWV625VNH3A
created_at: 2026-10-12T15:35:35.856542+00:00
updated_at: 2026-10-12T15:35:35.856542+00:00
---
Resolve hook session correlation in the caller and carry the parent ID explicitly on typed service requests. Relying on daemon environment variables misattributes concurrent children, and deserializing TurnEvent drops undeclared top-level parent fields. Keep caller payload context through parsing, project it to the RPC body, and derive session-local outcomes so one child success cannot clear another session's failure.
