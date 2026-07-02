//! The JSON-LD `@context` document — the term→IRI mapping that makes the
//! canonical nodes real linked data instead of JSON with LD-looking keys.
//!
//! Nodes reference the context by URL ([`crate::node::CONTEXT_URL`]); this
//! module ships the document itself (embedded at compile time) so local
//! tooling never needs the URL to resolve. [`inline_context`] swaps the URL
//! for the full mapping — that is what the SHACL gate feeds to an RDF
//! processor, which makes every gate run double as a JSON-LD conformance
//! check of our output.
//!
//! Closure discipline: the context is a closed term registry like
//! [`crate::vocab`]. A serialized key with no term here would silently drop
//! out of the RDF projection, so the coverage test walks fully-populated
//! nodes and fails on any unmapped key.

use serde_json::Value;

/// The raw `ctx.jsonld` document (the file `@context` wrapper included).
pub const CONTEXT_JSON: &str = include_str!("../ns/ctx.jsonld");

/// The parsed context document: `{"@context": {...}}`.
pub fn context_document() -> Value {
    serde_json::from_str(CONTEXT_JSON).expect("ctx.jsonld is valid JSON")
}

/// The inner `@context` term map.
pub fn context_terms() -> Value {
    context_document()
        .get("@context")
        .cloned()
        .expect("ctx.jsonld has an @context key")
}

/// Replace a node's `"@context": "<url>"` with the embedded term map, so the
/// document is self-contained for offline RDF processing (rdflib/pyshacl
/// cannot fetch `atomic.dev`). Non-object values pass through unchanged.
pub fn inline_context(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(obj) = value.as_object_mut() {
        obj.insert("@context".to_string(), context_terms());
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::CONTEXT_URL;

    /// Every serialized key of every node type must have a term in the
    /// context (or be a JSON-LD keyword). An unmapped key would silently
    /// vanish from the RDF projection — the opposite of a closed vocabulary.
    #[test]
    fn context_covers_every_serialized_key() {
        let terms = context_terms();
        let terms = terms.as_object().unwrap();

        let mut missing = Vec::new();
        for node in fixtures() {
            collect_missing(&node, terms, &mut missing);
        }
        assert!(
            missing.is_empty(),
            "serialized keys with no @context term: {missing:?}"
        );
    }

    /// Every `@type` string value used by our nodes must also resolve.
    #[test]
    fn context_covers_every_type_value() {
        let terms = context_terms();
        let terms = terms.as_object().unwrap();
        for ty in [
            "Intent",
            "AcceptanceCriterion",
            "Task",
            "ScopeItem",
            "Constraint",
            "Ref",
            "Memory",
            "DataIntegrityProof",
            "Activity",
            "SoftwareAgent",
            "Person",
        ] {
            assert!(terms.contains_key(ty), "@type '{ty}' has no context term");
        }
    }

    #[test]
    fn inline_context_replaces_url_with_terms() {
        let node = serde_json::json!({ "@context": CONTEXT_URL, "@type": "Intent" });
        let inlined = inline_context(&node);
        assert!(inlined["@context"].is_object());
        assert_eq!(inlined["@context"]["atom"], "https://atomic.dev/ns#");
    }

    fn collect_missing(
        value: &Value,
        terms: &serde_json::Map<String, Value>,
        missing: &mut Vec<String>,
    ) {
        match value {
            Value::Object(map) => {
                for (key, v) in map {
                    if !key.starts_with('@') && !terms.contains_key(key) && !missing.contains(key) {
                        missing.push(key.clone());
                    }
                    collect_missing(v, terms, missing);
                }
            }
            Value::Array(items) => {
                for item in items {
                    collect_missing(item, terms, missing);
                }
            }
            _ => {}
        }
    }

    /// Fully-populated fixtures: every optional field set, every collection
    /// non-empty, so every serializable key appears.
    fn fixtures() -> Vec<Value> {
        let intent_md = "\
---
id: WORD-5
uid: 019efe85
title: Add name prompt modal
status: done
priority: medium
view: proud-moon-a08a
motivatedBy: urn:atomic:decision:01J8Z9K2QF
informedBy: [urn:atomic:memory:01J8ZC4R8T]
attributedTo: did:atomic:lee
created_at: 2026-06-25T11:24:25Z
---

:::why
Local-only per :ref[the storage constraint]{to=urn:atomic:memory:01J8ZC edge=depends}.
:::

:::acceptance-criterion{#WORD-5-ac-1 status=met verifiedBy=did:atomic:lee evidence=urn:atomic:change:01J8ZE}
On first load, the app presents a modal.
:::

:::task{#WORD-5-1 status=done satisfies=WORD-5-ac-1}
Add name capture state.
::file-ref{path=src/App.tsx}
:::

:::scope-in
src/App.tsx state and markup.
:::

:::scope-out
Persistence across reloads.
:::

:::constraint
Keep it local.
:::

:::ref{#WORD-5-dep-1 to=urn:atomic:intent:019efe80 edge=blockedBy}
:::
";
        let (fm, body) = crate::lift::parse_markdown(intent_md).unwrap();
        let intent = crate::lift::lift_intent(&fm, &body).unwrap();

        let memory_md = "\
---
id: mem-storage
uid: 01J8ZC4R8T
kind: constraint
status: active
about: [urn:atomic:module:storage]
supersedes: urn:atomic:memory:00OLD
previousRevision: urn:atomic:memory:00OLD
attributedTo: did:atomic:lee
created_at: 2026-05-02T09:14:00Z
---

:::memory
Multi-region writes must converge with no single ordering authority.
:::
";
        let (fm, body) = crate::lift::parse_markdown(memory_md).unwrap();
        let memory = crate::memory::lift_memory(&fm, &body).unwrap();

        let kp = atomic_identity::keypair::KeyPair::generate();
        let id = atomic_identity::identity::Identity::new("t", &kp);
        let intent = crate::proof::attest(intent, &id, &kp);
        let memory = crate::proof::attest_memory(memory, &id, &kp);

        let prov = crate::prov::provenance_graph(&crate::prov::ProvenanceInput {
            session_id: "sess-1".into(),
            agent_name: "claude-code".into(),
            agent_display_name: "Claude Code".into(),
            agent_vendor: "anthropic".into(),
            model: "claude-fable-5".into(),
            started_at: "2026-07-01T22:11:00Z".into(),
            ended_at: Some("2026-07-01T22:14:00Z".into()),
            view: Some("sherpa-run-x".into()),
            change_hashes: vec!["W5GSLAVO".into()],
            used: vec!["urn:atomic:intent:019efe85".into()],
            managed_run: Some(crate::prov::ManagedRunInput {
                run_id: "run-1".into(),
                owner_agent: "sherpa".into(),
                owner_session_id: "sherpa-s1".into(),
                work_item_id: Some("NONA-7".into()),
            }),
            person: Some("did:atomic:lee".into()),
        });

        vec![intent.to_value(), memory.to_value(), prov]
    }
}
