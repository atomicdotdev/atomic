use atomic_core::{
    kg::ontology::{Class, Datatype, Predicate},
    KgEdge, KgNode,
};
use oxrdf::{Literal, NamedNode};
use std::collections::{BTreeMap, BTreeSet};

use crate::ValidationError;

fn named(iri: &str) -> Result<String, ValidationError> {
    NamedNode::new(iri)
        .map(|node| node.to_string())
        .map_err(|e| ValidationError::Snapshot(e.to_string()))
}

/// Export asserted workflow facts only. Unknown JSON metadata stays application
/// data. No domain/range, inverse, or property-chain inference is performed.
pub fn snapshot_ntriples(nodes: &[KgNode], edges: &[KgEdge]) -> Result<String, ValidationError> {
    let mut triples = BTreeSet::new();
    const TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
    const SUBCLASS: &str = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
    for class in Class::ALL {
        for parent in class.parents() {
            triples.insert(format!(
                "{} <{SUBCLASS}> {} .\n",
                named(class.iri())?,
                named(parent.iri())?
            ));
        }
    }
    for status in ["Verified", "Failed", "Unverifiable"] {
        triples.insert(format!(
            "<https://atomic.dev/ns/workflow#{status}> <{TYPE}> <{}> .\n",
            Class::VerificationStatus.iri()
        ));
    }
    for node in nodes {
        let subject = named(&node.id)?;
        for class in &node.types {
            if Class::from_iri(class).is_none() && !is_extension_class(class) {
                return Err(ValidationError::Snapshot(format!(
                    "unknown asserted class: {class}"
                )));
            }
            triples.insert(format!("{subject} <{TYPE}> {} .\n", named(class)?));
        }
        let mut literal = |predicate: Predicate,
                           value: &str,
                           datatype: Option<Datatype>|
         -> Result<(), ValidationError> {
            let literal = match datatype {
                Some(datatype) => {
                    Literal::new_typed_literal(value, NamedNode::new_unchecked(datatype.iri()))
                }
                None => Literal::new_simple_literal(value),
            };
            triples.insert(format!("{subject} <{}> {literal} .\n", predicate.iri()));
            Ok(())
        };
        literal(Predicate::RdfsLabel, &node.label, None)?;
        if let Some(summary) = &node.summary {
            literal(Predicate::DctermsDescription, summary, None)?;
        }
        if let Some(metadata) = &node.metadata {
            if let Some(kinds) = metadata.get("required_kinds") {
                let kinds = kinds.as_array().ok_or_else(|| {
                    ValidationError::Snapshot("required_kinds must be an array".into())
                })?;
                for kind in kinds {
                    literal(
                        Predicate::RequiredKind,
                        kind.as_str().ok_or_else(|| {
                            ValidationError::Snapshot("verification kind must be a string".into())
                        })?,
                        Some(Datatype::String),
                    )?;
                }
            }
            for (key, predicate, datatype) in [
                ("status", Predicate::Status, Datatype::String),
                ("source_node", Predicate::SourceNode, Datatype::String),
                (
                    "observation_role",
                    Predicate::ObservationRole,
                    Datatype::String,
                ),
                (
                    "observation_kind",
                    Predicate::ObservationKind,
                    Datatype::String,
                ),
                (
                    "source_reference",
                    Predicate::SourceReference,
                    Datatype::String,
                ),
                ("priority", Predicate::Priority, Datatype::String),
                ("required_kind", Predicate::RequiredKind, Datatype::String),
                (
                    "verification_kind",
                    Predicate::VerificationKind,
                    Datatype::String,
                ),
                ("observed_state", Predicate::ObservedState, Datatype::String),
                ("reviewed_state", Predicate::ReviewedState, Datatype::String),
                (
                    "reviewed_context",
                    Predicate::ReviewedContext,
                    Datatype::String,
                ),
                (
                    "verification_reference",
                    Predicate::VerificationReference,
                    Datatype::String,
                ),
                ("evidence", Predicate::EvidenceText, Datatype::String),
                ("verified_by", Predicate::VerifiedByText, Datatype::String),
                ("finding_code", Predicate::FindingCode, Datatype::String),
                (
                    "object_availability",
                    Predicate::ObjectAvailability,
                    Datatype::String,
                ),
                (
                    "projection_completeness",
                    Predicate::ProjectionCompleteness,
                    Datatype::String,
                ),
                ("created_at", Predicate::DctermsCreated, Datatype::DateTime),
                ("updated_at", Predicate::DctermsModified, Datatype::DateTime),
                ("project_id", Predicate::ProjectId, Datatype::String),
            ] {
                if let Some(value) = metadata.get(key).filter(|value| !value.is_null()) {
                    let value = value.as_str().ok_or_else(|| {
                        ValidationError::Snapshot(format!("{key} must be a string on {}", node.id))
                    })?;
                    literal(predicate, value, Some(datatype))?;
                }
            }
            for (key, predicate) in [
                ("turn_number", Predicate::TurnNumber),
                ("schema_version", Predicate::SchemaVersion),
            ] {
                if let Some(value) = metadata.get(key).filter(|value| !value.is_null()) {
                    let value = value.as_u64().ok_or_else(|| {
                        ValidationError::Snapshot(format!(
                            "{key} must be a non-negative integer on {}",
                            node.id
                        ))
                    })?;
                    literal(
                        predicate,
                        &value.to_string(),
                        Some(Datatype::NonNegativeInteger),
                    )?;
                }
            }
            // Custom kind fields and relations project into the project-scoped
            // extension namespace. See docs/adr/0001-kind-kg-conformance.md.
            if let Some(kind_class) = metadata.get("kind_class").and_then(|v| v.as_str()) {
                // The projection also carries the extension class in
                // `KgNode.types`; `triples` is a set, so this assertion is
                // idempotent and keeps metadata-only fixtures valid.
                triples.insert(format!("{subject} <{TYPE}> {} .\n", named(kind_class)?));
                for (key, value) in metadata.as_object().into_iter().flatten() {
                    if let Some(field) = key.strip_prefix("field.") {
                        emit_kind_values(
                            &mut triples,
                            &subject,
                            &format!("{kind_class}#{field}"),
                            value,
                        )?;
                    }
                }
                for key in ["kind_uid", "kind_body"] {
                    if let Some(value) = metadata.get(key).filter(|v| !v.is_null()) {
                        emit_kind_values(
                            &mut triples,
                            &subject,
                            &format!("{kind_class}#{}", key.trim_start_matches("kind_")),
                            value,
                        )?;
                    }
                }
            }
        }
    }
    // Map a kind instance's IRI to its extension class so its relation edges can
    // be emitted with the kind's own predicate as well as atomic:references.
    let kind_classes: BTreeMap<&str, &str> = nodes
        .iter()
        .filter_map(|node| {
            node.metadata
                .as_ref()
                .and_then(|metadata| metadata.get("kind_class"))
                .and_then(|value| value.as_str())
                .map(|class| (node.id.as_str(), class))
        })
        .collect();
    for edge in edges {
        triples.insert(format!(
            "{} <{}> {} .\n",
            named(&edge.from_id)?,
            edge.kind.iri(),
            named(&edge.to_id)?
        ));
        if let Some(kind_edge) = edge
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("kind_edge"))
            .and_then(|value| value.as_str())
        {
            if let Some(kind_class) = kind_classes.get(edge.from_id.as_str()) {
                triples.insert(format!(
                    "{} <{}#{kind_edge}> {} .\n",
                    named(&edge.from_id)?,
                    kind_class,
                    named(&edge.to_id)?
                ));
            }
        }
    }
    Ok(triples.into_iter().collect())
}

/// A project-scoped extension class: `urn:atomic:project:<pid>:kind:<kind>`.
fn is_extension_class(iri: &str) -> bool {
    atomic_core::kg::iri::is_kind_class(iri)
}

/// Emit one triple per scalar value. Arrays fan out; objects and nulls are not
/// projectable kind values and are skipped.
fn emit_kind_values(
    triples: &mut BTreeSet<String>,
    subject: &str,
    predicate: &str,
    value: &serde_json::Value,
) -> Result<(), ValidationError> {
    match value {
        serde_json::Value::Null => Ok(()),
        serde_json::Value::Array(items) => {
            for item in items {
                emit_kind_values(triples, subject, predicate, item)?;
            }
            Ok(())
        }
        serde_json::Value::Object(_) => Ok(()),
        serde_json::Value::String(text) => {
            triples.insert(format!(
                "{subject} <{predicate}> {} .\n",
                Literal::new_simple_literal(text)
            ));
            Ok(())
        }
        serde_json::Value::Bool(flag) => {
            triples.insert(format!(
                "{subject} <{predicate}> {} .\n",
                Literal::new_typed_literal(
                    flag.to_string(),
                    NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#boolean")
                )
            ));
            Ok(())
        }
        serde_json::Value::Number(number) => {
            let datatype = if number.is_i64() || number.is_u64() {
                "http://www.w3.org/2001/XMLSchema#integer"
            } else {
                "http://www.w3.org/2001/XMLSchema#double"
            };
            triples.insert(format!(
                "{subject} <{predicate}> {} .\n",
                Literal::new_typed_literal(number.to_string(), NamedNode::new_unchecked(datatype))
            ));
            Ok(())
        }
    }
}
