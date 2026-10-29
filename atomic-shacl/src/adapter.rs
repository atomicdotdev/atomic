use atomic_core::{
    kg::ontology::{Class, Datatype, Predicate},
    KgEdge, KgNode,
};
use oxrdf::{Literal, NamedNode};
use std::collections::BTreeSet;

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
            if Class::from_iri(class).is_none() {
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
            for (key, predicate, datatype) in [
                ("status", Predicate::Status, Datatype::String),
                ("priority", Predicate::Priority, Datatype::String),
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
        }
    }
    for edge in edges {
        triples.insert(format!(
            "{} <{}> {} .\n",
            named(&edge.from_id)?,
            edge.kind.iri(),
            named(&edge.to_id)?
        ));
    }
    Ok(triples.into_iter().collect())
}
