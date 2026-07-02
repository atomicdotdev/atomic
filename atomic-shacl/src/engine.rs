//! The synchronous SHACL bridge over the pre-1.0 oxirs engine.
//!
//! oxirs's validation path is fully synchronous — `ShapeLoaderBuilder::build`
//! (not just `build_async`), `ValidationEngine::new`, and `validate_store` all
//! run on the calling thread — so no async runtime is needed. This module owns
//! the three primitives the higher layers (shim, shapes, differential corpus)
//! build on:
//!
//!   1. [`load_shapes`] — parse Turtle shape text into oxirs `Shape`s.
//!   2. [`validate`] — run the shapes over an oxirs [`ConcreteStore`].
//!
//! It is deliberately thin: the JSON-LD→RDF projection (the shim) and the
//! violation→`Violation` mapping live in their own modules so this file stays a
//! faithful, testable wrapper around the raw oxirs API.

use std::collections::BTreeSet;

use atomic_canonical::memory::MemoryNode;
use atomic_canonical::node::CanonicalNode;
use indexmap::IndexMap;
use oxirs_core::model::Term;
use oxirs_core::ConcreteStore;
use oxirs_shacl::{Shape, ShapeId, ValidationConfig, ValidationEngine, ValidationReport};

use crate::shapes::{INTENT_SHAPES_TTL, MEMORY_SHAPES_TTL};
use crate::shim::node_to_store;

/// Errors from the oxirs bridge. Kept small; the differential corpus treats any
/// engine error as "shadow unavailable" and never lets it affect authority.
#[derive(Debug, thiserror::Error)]
pub enum ShaclError {
    /// A Turtle shape graph failed to parse / load.
    #[error("failed to load SHACL shapes: {0}")]
    ShapeLoad(String),
    /// The validation run itself failed.
    #[error("SHACL validation failed: {0}")]
    Validate(String),
    /// The RDF store could not be created or populated.
    #[error("RDF store error: {0}")]
    Store(String),
}

/// Parse a Turtle shapes graph into oxirs `Shape`s, keyed by `ShapeId` as
/// `ValidationEngine` requires.
///
/// `strict` maps to oxirs's `strict_mode`: when set, a shape graph the loader
/// cannot fully understand is an error rather than a silently-dropped shape —
/// the fail-closed posture a security gate needs.
pub fn load_shapes(turtle: &str, strict: bool) -> Result<IndexMap<ShapeId, Shape>, ShaclError> {
    let shapes = oxirs_shacl::ShapeLoaderBuilder::new()
        .from_rdf_data(turtle.to_string(), "turtle".to_string(), None)
        .strict_mode(strict)
        .build()
        .map_err(|e| ShaclError::ShapeLoad(e.to_string()))?;

    Ok(shapes.into_iter().map(|s| (s.id.clone(), s)).collect())
}

/// Run `shapes` over `store` and return the raw oxirs report.
pub fn validate(
    shapes: &IndexMap<ShapeId, Shape>,
    store: &ConcreteStore,
) -> Result<ValidationReport, ShaclError> {
    let mut engine = ValidationEngine::new(shapes, ValidationConfig::default());
    engine
        .validate_store(store)
        .map_err(|e| ShaclError::Validate(e.to_string()))
}

/// A finding key for differential comparison against the hand-coded gate:
/// `(focus_node_iri, path_short_name)`. The gate produces at most one violation
/// per such pair, so a set of these is the right comparison granularity.
pub type FindingKey = (String, Option<String>);

/// A SHACL report normalized so it can be diffed against the gate's
/// `ValidationReport`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedReport {
    pub conforms: bool,
    /// Order-independent set of `(focus, path)` findings.
    pub findings: BTreeSet<FindingKey>,
    /// `(focus, path, message)` triples, retained for diagnostics/divergence reports.
    pub raw: Vec<(String, Option<String>, String)>,
}

/// Validate a canonical Intent node through the real SHACL engine.
pub fn validate_intent_shacl(node: &CanonicalNode) -> Result<NormalizedReport, ShaclError> {
    let shapes = load_shapes(INTENT_SHAPES_TTL, true)?;
    let store = node_to_store(&node.to_value())?;
    Ok(normalize(&validate(&shapes, &store)?))
}

/// Validate a canonical Memory node through the real SHACL engine.
pub fn validate_memory_shacl(node: &MemoryNode) -> Result<NormalizedReport, ShaclError> {
    let shapes = load_shapes(MEMORY_SHAPES_TTL, true)?;
    let store = node_to_store(&node.to_value())?;
    Ok(normalize(&validate(&shapes, &store)?))
}

/// Normalize a raw oxirs report into `(focus, path)` findings.
///
/// `result_path` is present for Core property constraints (sh:minCount, sh:in);
/// for SHACL-SPARQL constraints it is typically absent, so we recover the gate's
/// path from the (shape-authored) message — the only place the two conditionals'
/// paths (`hasScopeOut`, `acStatus`) are pinned.
pub fn normalize(report: &ValidationReport) -> NormalizedReport {
    let mut findings = BTreeSet::new();
    let mut raw = Vec::new();
    for v in report.violations() {
        let focus = term_iri(&v.focus_node);
        let message = v.result_message.clone().unwrap_or_default();
        let path = match &v.result_path {
            Some(p) => Some(short_name(&p.to_string())),
            None => path_from_message(&message),
        };
        findings.insert((focus.clone(), path.clone()));
        raw.push((focus, path, message));
    }
    NormalizedReport {
        conforms: report.conforms(),
        findings,
        raw,
    }
}

/// The IRI string of a focus node (no angle brackets), matching the gate's
/// `focus_node` (`node.id` / `ac.id`).
fn term_iri(t: &Term) -> String {
    match t {
        Term::NamedNode(n) => n.as_str().to_string(),
        other => other.to_string().trim_matches(|c| c == '<' || c == '>').to_string(),
    }
}

/// The local name of an IRI (everything after the last `#` or `/`).
fn short_name(iri: &str) -> String {
    iri.rsplit(['#', '/']).next().unwrap_or(iri).to_string()
}

/// Recover the gate's path for a SHACL-SPARQL constraint from its message.
fn path_from_message(message: &str) -> Option<String> {
    if message.contains("out of scope") {
        Some("hasScopeOut".to_string())
    } else if message.contains("verifiedBy and evidence") {
        Some("acStatus".to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod smoke {
    //! Physical proof (M1) that the real oxirs 0.3.1 API works end-to-end in our
    //! toolchain: parse a Turtle NodeShape, build a store, validate, and read the
    //! report both ways (violating and conforming).

    use super::*;
    use oxirs_core::model::{Literal, NamedNode, Quad};

    const SHAPE_TTL: &str = r#"
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.org/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

ex:ThingShape a sh:NodeShape ;
    sh:targetClass ex:Thing ;
    sh:property [
        sh:path ex:name ;
        sh:minCount 1 ;
        sh:datatype xsd:string ;
    ] .
"#;

    fn thing_typed_quad() -> Quad {
        Quad::new(
            NamedNode::new("http://example.org/t1").unwrap(),
            NamedNode::new("http://www.w3.org/1999/02/22-rdf-syntax-ns#type").unwrap(),
            NamedNode::new("http://example.org/Thing").unwrap(),
            oxirs_core::model::GraphName::DefaultGraph,
        )
    }

    #[test]
    fn oxirs_flags_missing_required_property() {
        let shapes = load_shapes(SHAPE_TTL, true).expect("shapes load");
        let store = ConcreteStore::new().expect("store");
        // An ex:Thing with NO ex:name violates sh:minCount 1.
        store.insert_quad(thing_typed_quad()).expect("insert");

        let report = validate(&shapes, &store).expect("validate");
        assert!(
            !report.conforms(),
            "a Thing missing its required ex:name must not conform"
        );
        assert!(
            !report.violations().is_empty(),
            "the missing-property violation must be reported"
        );
    }

    #[test]
    fn real_intent_and_memory_shapes_load_under_strict_mode() {
        // M2/M4 gate: our real shapes exercise sh:in, sh:node, and sh:sparql.
        // If oxirs's strict loader rejects any of these, load fails here and the
        // whole approach is blocked — so this is the earliest hard checkpoint.
        let intent = load_shapes(crate::shapes::INTENT_SHAPES_TTL, true)
            .expect("intent shapes must load under strict mode");
        assert!(
            !intent.is_empty(),
            "intent shape graph produced no shapes (sh:in/sh:node/sh:sparql may not parse)"
        );
        let memory = load_shapes(crate::shapes::MEMORY_SHAPES_TTL, true)
            .expect("memory shapes must load under strict mode");
        assert!(!memory.is_empty(), "memory shape graph produced no shapes");
    }

    #[test]
    fn oxirs_passes_when_required_property_present() {
        let shapes = load_shapes(SHAPE_TTL, true).expect("shapes load");
        let store = ConcreteStore::new().expect("store");
        store.insert_quad(thing_typed_quad()).expect("insert type");
        store
            .insert_quad(Quad::new(
                NamedNode::new("http://example.org/t1").unwrap(),
                NamedNode::new("http://example.org/name").unwrap(),
                Literal::new_simple_literal("hello"),
                oxirs_core::model::GraphName::DefaultGraph,
            ))
            .expect("insert name");

        let report = validate(&shapes, &store).expect("validate");
        assert!(
            report.conforms(),
            "a Thing with its required ex:name present must conform (got {:?})",
            report.violations()
        );
    }
}
