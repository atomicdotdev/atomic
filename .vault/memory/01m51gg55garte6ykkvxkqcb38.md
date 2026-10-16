---
createdAt: 2026-10-16T04:46:05Z
derivedFrom: ["urn:atomic:ac:01M50EXZ198HPVF0CQ2Y0AEH4Z-ac-4","urn:atomic:intent:01M50EXZ198HPVF0CQ2Y0AEH4Z"]
memoryKind: lesson
status: active
uid: 01m51gg55garte6ykkvxkqcb38
entry_type: memory
content_hash: XNS46AU2K6VSPCKK5MG36Q4YJBY6INVI3AQWYRHS6ETW76D7WFZQ
created_at: 2026-10-16T04:46:05.491853+00:00
updated_at: 2026-10-16T04:46:05.491853+00:00
---
PurRDF 3.0.1 follows owl:imports only on a shapes-document anchor (its loaded IRI, an owl:Ontology, or a sh:ShapesGraph). An unanchored imports triple is ordinary RDF data, so it is not a valid negative fixture for blocked remote imports. Test controlled imports with an anchored subject; Atomic supplies only explicit local shape bundles and lets PurRDF resolve or reject the import closure.
