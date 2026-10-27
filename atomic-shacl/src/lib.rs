//! CLI-only SHACL integration. Atomic adapts snapshots and presents reports;
//! PurRDF owns shape parsing, target selection, and constraint execution.

pub const PROJECTION_SHAPES: &str =
    include_str!("../../atomic-core/ontology/shapes/projection-1.0.0.ttl");
pub const PUBLICATION_SHAPES: &str =
    include_str!("../../atomic-core/ontology/shapes/publication-1.0.0.ttl");

mod adapter;
pub use adapter::snapshot_ntriples;

use purrdf_shapes::{
    engine, report::ValidationResult, shapes::Path as ShapePath, term::Term, ShapesImports,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("workflow snapshot adaptation failed: {0}")]
    Snapshot(String),
    #[error("shape loading failed: {0}")]
    Shapes(String),
    #[error("SHACL execution failed: {0}")]
    Execution(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    #[default]
    Projection,
    Publication,
}

#[derive(Clone, Debug)]
pub struct ShapeBundle {
    pub iri: String,
    pub turtle: String,
}

#[derive(Clone, Debug, Default)]
pub struct ValidationRequest {
    pub profile: Profile,
    pub shape: Option<String>,
    pub node: Option<String>,
    pub custom: Vec<ShapeBundle>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct TargetCoverage {
    pub shape: String,
    pub applicable: usize,
    pub evaluated: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ValidationOutput {
    pub engine: &'static str,
    pub engine_version: &'static str,
    pub profile: Profile,
    pub profile_identity: String,
    pub snapshot_identity: String,
    pub conforms: bool,
    pub scope: &'static str,
    pub target_coverage: Vec<TargetCoverage>,
    pub diagnostics: Vec<String>,
    pub results: Vec<Value>,
    /// The complete standard report, including complex paths, literal language/
    /// datatype, nested details and any additional engine report annotations.
    pub report_ntriples: String,
}

/// Only explicitly supplied local bundles can satisfy imports. This function
/// never resolves imports by making network or additional filesystem requests.
pub fn load_custom_shapes(directory: &Path) -> Result<Vec<ShapeBundle>, ValidationError> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut bundles = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|extension| extension == "ttl") && path.is_file() {
            let path = path.canonicalize()?;
            let iri = url::Url::from_file_path(&path)
                .map_err(|_| {
                    ValidationError::Shapes(format!(
                        "cannot form a base IRI for {}",
                        path.display()
                    ))
                })?
                .to_string();
            bundles.push(ShapeBundle {
                iri,
                turtle: std::fs::read_to_string(path)?,
            });
        }
    }
    bundles.sort_by(|a, b| a.iri.cmp(&b.iri));
    Ok(bundles)
}

fn term_value(term: &Term) -> String {
    match term {
        Term::NamedNode(node) => node.as_str().to_owned(),
        _ => term.to_string(),
    }
}

fn path_json(path: &ShapePath) -> Value {
    match path {
        ShapePath::Predicate(iri) => json!({"predicate": iri.as_str()}),
        ShapePath::Inverse(path) => json!({"inverse": path_json(path)}),
        ShapePath::Sequence(paths) => {
            json!({"sequence": paths.iter().map(path_json).collect::<Vec<_>>()})
        }
        ShapePath::Alternative(paths) => {
            json!({"alternative": paths.iter().map(path_json).collect::<Vec<_>>()})
        }
        ShapePath::ZeroOrMore(path) => json!({"zero_or_more": path_json(path)}),
        ShapePath::OneOrMore(path) => json!({"one_or_more": path_json(path)}),
        ShapePath::ZeroOrOne(path) => json!({"zero_or_one": path_json(path)}),
    }
}

fn result_json(result: &ValidationResult) -> Value {
    json!({
        "focus_node": term_value(&result.focus_node),
        "result_path": result.result_path.as_ref().map(term_value),
        "path_structure": result.path_structure.as_ref().map(path_json),
        "source_shape": term_value(&result.source_shape),
        "source_constraint_component": result.source_constraint_component.as_str(),
        "result_severity": result.severity.iri(),
        "result_messages": result.messages.iter().map(|message| json!({"value": message.value(), "rdf_literal": Term::Literal(message.clone()).to_string()})).collect::<Vec<_>>(),
        "value": result.value.as_ref().map(term_value),
        "details": result.details.iter().map(result_json).collect::<Vec<_>>(),
    })
}

/// One parsed snapshot and one parsed shapes/import closure are reused for
/// target coverage and validation. Focus selection is performed by PurRDF;
/// Atomic's callback only records coverage and applies explicit CLI selectors.
pub fn validate_ntriples(
    data: &str,
    snapshot_identity: &str,
    request: &ValidationRequest,
) -> Result<ValidationOutput, ValidationError> {
    let mut bundles = vec![ShapeBundle {
        iri: "https://atomic.dev/ns/shapes/projection/1.0.0".into(),
        turtle: PROJECTION_SHAPES.into(),
    }];
    if request.profile == Profile::Publication {
        bundles.push(ShapeBundle {
            iri: "https://atomic.dev/ns/shapes/publication/1.0.0".into(),
            turtle: PUBLICATION_SHAPES.into(),
        });
    }
    bundles.extend(request.custom.clone());
    let mut imports = ShapesImports::new();
    let mut root = String::new();
    let mut identity = blake3::Hasher::new();
    for bundle in &bundles {
        imports
            .insert_turtle(&bundle.iri, &bundle.turtle)
            .map_err(|e| ValidationError::Shapes(e.to_string()))?;
        root.push_str(&format!(
            "<urn:atomic:shapes:selection> <http://www.w3.org/2002/07/owl#imports> <{}> .\n",
            bundle.iri
        ));
        identity.update(&(bundle.iri.len() as u64).to_be_bytes());
        identity.update(bundle.iri.as_bytes());
        identity.update(&(bundle.turtle.len() as u64).to_be_bytes());
        identity.update(bundle.turtle.as_bytes());
    }
    let shapes = engine::parse_shapes_with_config(
        &root,
        Some("urn:atomic:shapes:selection"),
        None,
        &imports,
    )
    .map_err(|e| ValidationError::Shapes(e.to_string()))?;
    if shapes.rules.entailment
        || !shapes.rules.global_rules.is_empty()
        || !shapes.rules.rule_sets.is_empty()
        || shapes
            .node_shapes
            .iter()
            .any(|shape| !shape.rules.is_empty())
    {
        return Err(ValidationError::Shapes(
            "Phase 1 validates asserted snapshots; inference rules require a later profile".into(),
        ));
    }
    if let Some(shape) = &request.shape {
        if !shapes
            .node_shapes
            .iter()
            .any(|candidate| term_value(&candidate.id) == *shape)
        {
            return Err(ValidationError::Shapes(format!(
                "unknown selected shape: {shape}"
            )));
        }
    }
    let dataset = purrdf_shapes::text_ingest::parse_ntriples_to_dataset(data)
        .map_err(|errors| ValidationError::Snapshot(errors.join("\n")))?;
    let dataset = engine::project_dataset(dataset.as_ref()).map_err(ValidationError::Snapshot)?;
    let mut coverage: BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)> = shapes
        .node_shapes
        .iter()
        .filter(|shape| {
            !shape.deactivated
                && !shape.targets.is_empty()
                && request
                    .shape
                    .as_ref()
                    .is_none_or(|selected| *selected == term_value(&shape.id))
        })
        .map(|shape| (term_value(&shape.id), Default::default()))
        .collect();
    let report =
        engine::validate_projected_dataset_with_focus_filter(dataset, &shapes, |shape, node| {
            let id = term_value(&shape.id);
            if request
                .shape
                .as_ref()
                .is_some_and(|selected| *selected != id)
            {
                return false;
            }
            let node = term_value(node);
            let counts = coverage.entry(id).or_default();
            counts.0.insert(node.clone());
            let evaluate = request
                .node
                .as_ref()
                .is_none_or(|selected| *selected == node);
            if evaluate {
                counts.1.insert(node);
            }
            evaluate
        })
        .map_err(|e| ValidationError::Execution(e.to_string()))?;
    let coverage: Vec<_> = coverage
        .into_iter()
        .map(|(shape, (applicable, evaluated))| TargetCoverage {
            shape,
            applicable: applicable.len(),
            evaluated: evaluated.len(),
        })
        .collect();
    let mut diagnostics = Vec::new();
    if coverage.iter().all(|coverage| coverage.evaluated == 0) {
        diagnostics
            .push("No applicable focus nodes were evaluated; conformance is vacuous.".into());
    }
    diagnostics.push("Phase 1 conformance is not complete publication/audit evidence; inspect coverage and historical-gap findings.".into());
    Ok(ValidationOutput {
        engine: "PurRDF",
        engine_version: purrdf_shapes::VERSION,
        profile: request.profile,
        profile_identity: identity.finalize().to_hex().to_string(),
        snapshot_identity: snapshot_identity.into(),
        conforms: report.conforms,
        scope: if request.node.is_some() || request.shape.is_some() {
            "selected"
        } else {
            "snapshot"
        },
        target_coverage: coverage,
        diagnostics,
        results: report.results.iter().map(result_json).collect(),
        report_ntriples: report.to_ntriples(),
    })
}
