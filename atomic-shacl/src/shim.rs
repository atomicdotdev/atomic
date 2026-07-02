//! The deterministic JSON-LD → RDF projection (the "shim").
//!
//! SHACL validates RDF, so a canonical node's JSON-LD value must first become
//! triples. The single most important correctness surface here is faithfully
//! reproducing the hand-coded gate's presence semantics — including its
//! deliberate **trim asymmetry**:
//!
//!   * `why` / `text` are rejected when WHITESPACE-ONLY (the gate uses `.trim()`),
//!     so the shim emits their triple only when the trimmed value is non-empty.
//!   * `attributedTo` / `verifiedBy` / `evidence` are accepted when whitespace-only
//!     (the gate uses `.unwrap_or("").is_empty()` — no trim), so the shim emits
//!     their triple whenever the raw value is non-empty (present-empty `""` and an
//!     omitted key both project to nothing — matching the gate).
//!
//! Everything else (closed-set values like `status`/`memoryKind`/`acStatus`) is
//! projected verbatim so `sh:in` can match. Nested typed sub-nodes
//! (`hasAcceptanceCriterion`, `hasScopeIn/Out`, …) become edges to child subjects
//! keyed on the child's `@id`, so `verifiedBy`/`evidence` land on the AC child —
//! not the parent — exactly where the AcceptanceCriterionShape looks for them.

use oxirs_core::model::{GraphName, Literal, NamedNode, Quad};
use oxirs_core::ConcreteStore;
use serde_json::{Map, Value};

use crate::engine::ShaclError;

/// The Atomic vocabulary namespace. Every JSON-LD short key `k` projects to the
/// IRI `NS + k`, and every `@type` value `T` projects to the class IRI `NS + T`.
/// This is the term→IRI table the shapes target; keeping it a single constant is
/// what lets shim and shapes agree.
pub const NS: &str = "https://atomic.dev/ns#";

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// Build an RDF store from a canonical node's JSON-LD value.
pub fn node_to_store(node: &Value) -> Result<ConcreteStore, ShaclError> {
    let store = ConcreteStore::new().map_err(|e| ShaclError::Store(e.to_string()))?;
    for quad in node_to_quads(node)? {
        store
            .insert_quad(quad)
            .map_err(|e| ShaclError::Store(e.to_string()))?;
    }
    Ok(store)
}

/// Project a canonical node's JSON-LD value into a deterministic set of quads.
/// Pure (no store), so the projection can be unit-tested quad-for-quad.
pub fn node_to_quads(node: &Value) -> Result<Vec<Quad>, ShaclError> {
    let obj = node
        .as_object()
        .ok_or_else(|| ShaclError::Store("node is not a JSON object".into()))?;
    let mut quads = Vec::new();
    add_node(obj, &mut quads)?;
    Ok(quads)
}

/// Whether a string field is projected, given the gate's per-field semantics.
/// Exposed so the differential corpus can assert the asymmetry directly.
pub fn should_emit(key: &str, value: &str) -> bool {
    match key {
        // Trimmed presence (whitespace-only is absent).
        "why" | "text" => !value.trim().is_empty(),
        // Untrimmed presence (whitespace-only counts as present).
        "attributedTo" | "verifiedBy" | "evidence" => !value.is_empty(),
        // Closed-set / other scalars: project verbatim (empty is harmless — no
        // rule references these except via sh:in, which an empty value fails).
        _ => true,
    }
}

fn iri(s: &str) -> Result<NamedNode, ShaclError> {
    NamedNode::new(s).map_err(|e| ShaclError::Store(format!("invalid IRI {s:?}: {e}")))
}

fn subject_of(obj: &Map<String, Value>) -> Result<NamedNode, ShaclError> {
    let id = obj
        .get("@id")
        .and_then(Value::as_str)
        .ok_or_else(|| ShaclError::Store("node/sub-node missing @id".into()))?;
    iri(id)
}

/// Emit all triples for one node (or sub-node), recursing into typed children.
fn add_node(obj: &Map<String, Value>, quads: &mut Vec<Quad>) -> Result<(), ShaclError> {
    let subject = subject_of(obj)?;

    // @type -> rdf:type -> the class IRI (so sh:targetClass / sh:node match).
    if let Some(t) = obj.get("@type").and_then(Value::as_str) {
        quads.push(Quad::new(
            subject.clone(),
            iri(RDF_TYPE)?,
            iri(&format!("{NS}{t}"))?,
            GraphName::DefaultGraph,
        ));
    }

    for (key, val) in obj {
        if key == "@context" || key == "@id" || key == "@type" {
            continue;
        }
        let pred = iri(&format!("{NS}{key}"))?;

        match val {
            // A present proof (object) projects to a single presence triple; an
            // absent proof projects to nothing, so sh:minCount 1 fires.
            Value::Object(_) if key == "proof" => {
                quads.push(Quad::new(
                    subject.clone(),
                    pred,
                    Literal::new_simple_literal("present"),
                    GraphName::DefaultGraph,
                ));
            }
            Value::String(s) => {
                if should_emit(key, s) {
                    quads.push(Quad::new(
                        subject.clone(),
                        pred,
                        Literal::new_simple_literal(s.as_str()),
                        GraphName::DefaultGraph,
                    ));
                }
            }
            Value::Array(items) => {
                for item in items {
                    match item {
                        Value::String(s) => {
                            quads.push(Quad::new(
                                subject.clone(),
                                pred.clone(),
                                Literal::new_simple_literal(s.as_str()),
                                GraphName::DefaultGraph,
                            ));
                        }
                        // A typed sub-node: edge to the child @id, then recurse so
                        // the child's own fields (acStatus/verifiedBy/evidence, …)
                        // land on the CHILD subject.
                        Value::Object(child) if child.contains_key("@id") => {
                            quads.push(Quad::new(
                                subject.clone(),
                                pred.clone(),
                                subject_of(child)?,
                                GraphName::DefaultGraph,
                            ));
                            add_node(child, quads)?;
                        }
                        // Objects without an @id (e.g. a bare dependsOn Ref) are
                        // ungated; skip them rather than mint a synthetic subject.
                        _ => {}
                    }
                }
            }
            // Numbers/bools/null: none of our gated fields use them. Skip.
            _ => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Render quads to sorted "S P O" strings for order-independent assertions.
    fn triples(node: &Value) -> Vec<String> {
        let mut out: Vec<String> = node_to_quads(node)
            .expect("project")
            .iter()
            .map(|q| {
                format!(
                    "{} {} {}",
                    q.subject(),
                    q.predicate(),
                    q.object()
                )
            })
            .collect();
        out.sort();
        out
    }

    fn has(node: &Value, needle: &str) -> bool {
        triples(node).iter().any(|t| t.contains(needle))
    }

    #[test]
    fn trim_asymmetry_why_whitespace_only_is_absent() {
        // why is whitespace-only -> NOT projected (gate rejects it).
        let n = json!({"@id":"urn:i:1","@type":"Intent","why":"   ","status":"todo"});
        assert!(!has(&n, "#why"), "whitespace-only why must not be projected");
        // a real why IS projected.
        let n2 = json!({"@id":"urn:i:1","@type":"Intent","why":"because","status":"todo"});
        assert!(has(&n2, "#why"), "a real why must be projected");
    }

    #[test]
    fn trim_asymmetry_attributed_to_whitespace_only_is_present() {
        // attributedTo whitespace-only -> IS projected (gate accepts it, no trim).
        let n = json!({"@id":"urn:i:1","@type":"Intent","attributedTo":"   "});
        assert!(
            has(&n, "#attributedTo"),
            "whitespace-only attributedTo must be projected (untrimmed)"
        );
        // present-empty "" -> NOT projected (matches .is_empty()).
        let n2 = json!({"@id":"urn:i:1","@type":"Intent","attributedTo":""});
        assert!(
            !has(&n2, "#attributedTo"),
            "empty attributedTo must project to nothing"
        );
    }

    #[test]
    fn proof_presence_projects_one_triple_when_present_none_when_absent() {
        let present = json!({"@id":"urn:i:1","@type":"Intent","proof":{"proofValue":"z1"}});
        assert!(has(&present, "#proof"), "present proof must project a triple");
        let absent = json!({"@id":"urn:i:1","@type":"Intent"});
        assert!(!has(&absent, "#proof"), "absent proof projects nothing");
    }

    #[test]
    fn ac_child_fields_land_on_the_child_subject_not_the_parent() {
        let n = json!({
            "@id":"urn:i:1","@type":"Intent",
            "hasAcceptanceCriterion":[{
                "@id":"urn:ac:1","@type":"AcceptanceCriterion",
                "text":"t","acStatus":"met","verifiedBy":"lee","evidence":"pr#1"
            }]
        });
        let ts = triples(&n);
        // parent -> child edge
        assert!(ts.iter().any(|t| t.contains("urn:i:1")
            && t.contains("#hasAcceptanceCriterion")
            && t.contains("urn:ac:1")));
        // verifiedBy/evidence/acStatus are on the CHILD subject
        assert!(ts.iter().any(|t| t.starts_with("<urn:ac:1>") && t.contains("#verifiedBy")));
        assert!(ts.iter().any(|t| t.starts_with("<urn:ac:1>") && t.contains("#evidence")));
        assert!(ts.iter().any(|t| t.starts_with("<urn:ac:1>") && t.contains("#acStatus")));
        // and NOT on the parent
        assert!(!ts.iter().any(|t| t.starts_with("<urn:i:1>") && t.contains("#verifiedBy")));
    }

    #[test]
    fn omitted_empty_collections_project_no_edges() {
        let n = json!({"@id":"urn:i:1","@type":"Intent","status":"todo"});
        assert!(!has(&n, "#hasScopeIn"));
        assert!(!has(&n, "#hasAcceptanceCriterion"));
    }
}
