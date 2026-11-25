//! Conformance tests for the generated custom-kind SHACL shapes.
//!
//! `kind_shapes::shape_for` turns a recorded `KindSpec` into a closed
//! `sh:NodeShape`, and `snapshot_ntriples` projects the knowledge graph into
//! the N-Triples the shape runs over. These tests pin the contract between the
//! two: a valid instance conforms, and each spec construct (required, datatype,
//! enum, closed, ref class) fails exactly when it should. See
//! `docs/adr/0001-kind-kg-conformance.md`.
//!
//! The instance IRIs are minted with the same helpers the vault projection uses
//! (`iri::mint_concept` / `iri::mint_intent`), so the `sh:pattern` and SPARQL
//! suffix constraints see the production shape.

use atomic_canonical::kind_spec::KindSpec;
use atomic_core::{
    kg::{
        iri,
        ontology::{Class as C, Predicate as P},
        project_id::ProjectId,
    },
    KgEdge, KgNode,
};
use atomic_shacl::{
    kind_shapes::{kind_class, shape_for},
    snapshot_ntriples, validate_ntriples, Profile, ShapeBundle, ValidationError, ValidationOutput,
    ValidationRequest,
};
use serde_json::{json, Value};

const UID: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TR";
const TARGET_UID: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TS";
const INTENT_UID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

const SHACL: &str = "http://www.w3.org/ns/shacl#";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
const DCTERMS_CREATED: &str = "http://purl.org/dc/terms/created";
const DCTERMS_MODIFIED: &str = "http://purl.org/dc/terms/modified";
const DCTERMS_DESCRIPTION: &str = "http://purl.org/dc/terms/description";
const ATOMIC_REFERENCES: &str = "https://atomic.dev/ns/workflow#references";
const WORK_INTENT: &str = "https://atomic.dev/ns/workflow#WorkIntent";

/// The kind spec under test: a required string, an optional integer, a boolean,
/// an enum, a required edge-less ref to a built-in intent, and a required ref to
/// a custom kind. `body` is required so the generated shape exercises the body
/// `sh:minCount` as well.
fn adr_spec() -> KindSpec {
    let frontmatter = json!({
        "name": "adr",
        "specVersion": 1,
        "title": "Architecture Decision Record",
        "fields": {
            "owner": { "type": "string", "required": true },
            "priority": { "type": "integer" },
            "archived": { "type": "boolean" },
            "status": {
                "type": "enum",
                "values": ["proposed", "accepted"],
                "required": true
            },
            "intent": { "type": "ref", "target": ["intent"], "required": true },
            "supersedes": { "type": "ref", "target": ["rfc"], "edge": "supersedes", "required": true }
        },
        "body": { "required": true }
    });
    KindSpec::from_frontmatter("adr", frontmatter.as_object().unwrap()).expect("ADR spec parses")
}

/// A generated project, its kind class IRIs, and the IRIs of the valid fixture
/// nodes. The project is random per test so the extension IRIs cannot collide
/// with a previous run's triples.
struct Fixture {
    project: ProjectId,
    base: String,
    class: String,
    rfc_class: String,
    instance: String,
    rfc: String,
    intent: String,
    spec: KindSpec,
}

impl Fixture {
    fn new() -> Self {
        let project = ProjectId::generate();
        let base = project.to_urn();
        let class = kind_class(&base, "adr");
        let rfc_class = kind_class(&base, "rfc");
        let instance = iri::mint_concept(&project, &format!("adr.{UID}"));
        let rfc = iri::mint_concept(&project, &format!("rfc.{TARGET_UID}"));
        let intent = iri::mint_intent(&project, INTENT_UID).expect("intent IRI");
        Self {
            project,
            base,
            class,
            rfc_class,
            instance,
            rfc,
            intent,
            spec: adr_spec(),
        }
    }

    fn shape(&self) -> String {
        shape_for(&self.spec, &self.base).expect("shape generates")
    }

    /// The valid projection: the instance, its hub, its custom-kind target, its
    /// built-in intent target, and the `atomic:references` edges the projection
    /// emits. The metadata carries the same bookkeeping keys the vault
    /// projection writes, so the closed shape is tested against real triples.
    fn valid(&self) -> (Vec<KgNode>, Vec<KgEdge>) {
        let instance = KgNode::new(&self.instance, C::Concept, "ADR instance", "test")
            .with_metadata(json!({
                "kind_name": "adr",
                "kind_class": self.class,
                "kind_uid": UID,
                "kind_body": "We chose redb.\n",
                "uid": UID,
                "vault_path": "kinds/adr/01JD8ZK3M6W2Q4V7X9B5N1C3TR.md",
                "derived_from_vault_path": "kinds/adr/01JD8ZK3M6W2Q4V7X9B5N1C3TR.md",
                "created_at": "2026-11-24T00:00:00Z",
                "updated_at": "2026-11-24T00:00:00Z",
                "field.owner": "alice",
                "field.priority": 42,
                "field.archived": false,
                "field.status": "accepted"
            }));
        // The projection carries the extension class in `types`; this fixture
        // exercises the adapter's metadata path only, so the class is asserted
        // from `metadata.kind_class`.

        let rfc = KgNode::new(&self.rfc, C::Concept, "RFC 1", "test").with_metadata(json!({
            "kind_class": self.rfc_class
        }));

        let intent = KgNode::new(&self.intent, C::WorkIntent, "Intent 1", "test");
        let hub = KgNode::new(
            iri::mint_concept(&self.project, "kind.adr"),
            C::Concept,
            "ADR",
            "test",
        );

        let edges = vec![
            KgEdge::new(&self.instance, &hub.id, P::References)
                .with_metadata(json!({"edges": ["@kind"]})),
            KgEdge::new(&self.instance, &self.rfc, P::References)
                .with_metadata(json!({"kind_edge": "supersedes"})),
            KgEdge::new(&self.instance, &self.intent, P::References)
                .with_metadata(json!({"kind_edge": "intent"})),
        ];
        (vec![instance, rfc, intent, hub], edges)
    }

    fn run(&self, nodes: &[KgNode], edges: &[KgEdge]) -> ValidationOutput {
        run(&self.shape(), nodes, edges)
    }

    fn coverage<'a>(&self, report: &'a ValidationOutput) -> &'a atomic_shacl::TargetCoverage {
        report
            .target_coverage
            .iter()
            .find(|coverage| coverage.shape == self.class)
            .unwrap_or_else(|| panic!("the kind shape did not run: {report:?}"))
    }
}

/// Run a shape over the adapter's projection. The request mirrors the
/// production `kind validate` gate: `Profile::Kind` plus the generated Turtle
/// as a custom bundle.
fn run(shape: &str, nodes: &[KgNode], edges: &[KgEdge]) -> ValidationOutput {
    let data = snapshot_ntriples(nodes, edges).expect("snapshot projects");
    validate_ntriples(
        &data,
        "kind-shapes-test",
        &ValidationRequest {
            profile: Profile::Kind,
            custom: vec![ShapeBundle {
                iri: "urn:atomic:kind-shapes".into(),
                turtle: shape.into(),
            }],
            ..Default::default()
        },
    )
    .expect("shape parses")
}

/// The SHACL constraint component without its namespace and suffix, e.g.
/// `MinCountConstraintComponent` becomes `MinCount`.
fn component_name(iri: &str) -> &str {
    let short = iri.strip_prefix(SHACL).unwrap_or(iri);
    short.strip_suffix("ConstraintComponent").unwrap_or(short)
}

/// `(component, result_path)` pairs for every violation, sorted so a test can
/// compare the whole set exactly.
fn violations(report: &ValidationOutput) -> Vec<(String, String)> {
    let mut violations: Vec<(String, String)> = report
        .results
        .iter()
        .map(|result| {
            (
                component_name(result["source_constraint_component"].as_str().unwrap()).to_string(),
                result["result_path"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    violations.sort();
    violations
}

fn predicate(class: &str, field: &str) -> String {
    format!("{class}#{field}")
}

fn metadata_of(node: &mut KgNode) -> &mut serde_json::Map<String, Value> {
    node.metadata.as_mut().unwrap().as_object_mut().unwrap()
}

#[test]
fn valid_instance_projected_through_the_adapter_conforms() {
    let fixture = Fixture::new();
    let (nodes, edges) = fixture.valid();
    let report = fixture.run(&nodes, &edges);

    assert!(report.conforms, "{}", report.report_ntriples);
    assert!(report.results.is_empty(), "{:?}", report.results);
    let coverage = fixture.coverage(&report);
    assert_eq!(coverage.applicable, 1, "{report:?}");
    assert_eq!(coverage.evaluated, 1, "{report:?}");
}

#[test]
fn missing_required_field_is_a_min_count_violation() {
    let fixture = Fixture::new();
    let (mut nodes, edges) = fixture.valid();
    metadata_of(&mut nodes[0]).remove("field.owner");

    let report = fixture.run(&nodes, &edges);
    assert!(!report.conforms, "{}", report.report_ntriples);
    assert_eq!(
        violations(&report),
        [("MinCount".into(), predicate(&fixture.class, "owner"))]
    );
}

#[test]
fn wrong_datatype_is_a_datatype_violation() {
    let fixture = Fixture::new();
    let (mut nodes, edges) = fixture.valid();
    metadata_of(&mut nodes[0]).insert("field.priority".into(), json!("high"));

    let report = fixture.run(&nodes, &edges);
    assert!(!report.conforms, "{}", report.report_ntriples);
    assert_eq!(
        violations(&report),
        [("Datatype".into(), predicate(&fixture.class, "priority"))]
    );
}

#[test]
fn enum_value_outside_the_allowed_set_is_an_in_violation() {
    let fixture = Fixture::new();
    let (mut nodes, edges) = fixture.valid();
    metadata_of(&mut nodes[0]).insert("field.status".into(), json!("maybe"));

    let report = fixture.run(&nodes, &edges);
    assert!(!report.conforms, "{}", report.report_ntriples);
    assert_eq!(
        violations(&report),
        [("In".into(), predicate(&fixture.class, "status"))]
    );
}

#[test]
fn closed_shape_rejects_an_undeclared_extension_field() {
    let fixture = Fixture::new();
    let (mut nodes, edges) = fixture.valid();
    metadata_of(&mut nodes[0]).insert("field.extra".into(), json!("not in the spec"));

    let report = fixture.run(&nodes, &edges);
    assert!(!report.conforms, "{}", report.report_ntriples);
    assert_eq!(
        violations(&report),
        [("Closed".into(), predicate(&fixture.class, "extra"))]
    );
}

#[test]
fn declared_extension_fields_are_accepted_by_the_closed_shape() {
    let fixture = Fixture::new();
    let (nodes, edges) = fixture.valid();
    let report = fixture.run(&nodes, &edges);

    assert!(report.conforms, "{}", report.report_ntriples);
    assert!(
        violations(&report)
            .iter()
            .all(|(component, _)| component != "Closed"),
        "{:?}",
        report.results
    );
}

#[test]
fn kind_edge_projects_the_custom_relation_predicate() {
    let fixture = Fixture::new();
    let (nodes, edges) = fixture.valid();
    let data = snapshot_ntriples(&nodes, &edges).unwrap();

    let supersedes = format!(
        "<{}> <{}> <{}>",
        fixture.instance,
        predicate(&fixture.class, "supersedes"),
        fixture.rfc
    );
    assert!(data.contains(&supersedes), "missing {supersedes}\n{data}");

    let report = fixture.run(&nodes, &edges);
    assert!(report.conforms, "{}", report.report_ntriples);
}

#[test]
fn edge_less_ref_projects_under_the_field_name() {
    let fixture = Fixture::new();
    let (nodes, edges) = fixture.valid();
    let data = snapshot_ntriples(&nodes, &edges).unwrap();

    let intent = format!(
        "<{}> <{}> <{}>",
        fixture.instance,
        predicate(&fixture.class, "intent"),
        fixture.intent
    );
    assert!(data.contains(&intent), "missing {intent}\n{data}");

    let report = fixture.run(&nodes, &edges);
    assert!(report.conforms, "{}", report.report_ntriples);
}

#[test]
fn required_ref_without_a_matching_edge_is_a_min_count_violation() {
    let fixture = Fixture::new();
    let (nodes, mut edges) = fixture.valid();
    edges.retain(|edge| {
        edge.metadata
            .as_ref()
            .and_then(|m| m.get("kind_edge"))
            .and_then(Value::as_str)
            != Some("intent")
    });

    let report = fixture.run(&nodes, &edges);
    assert!(!report.conforms, "{}", report.report_ntriples);
    assert_eq!(
        violations(&report),
        [("MinCount".into(), predicate(&fixture.class, "intent"))]
    );
}

#[test]
fn generated_turtle_uses_the_custom_kind_and_builtin_intent_classes() {
    let fixture = Fixture::new();
    let shape = fixture.shape();

    let custom = format!("sh:class <{}>", fixture.rfc_class);
    let builtin = format!("sh:class <{WORK_INTENT}>");
    assert!(shape.contains(&custom), "missing {custom}\n{shape}");
    assert!(shape.contains(&builtin), "missing {builtin}\n{shape}");

    // The ref predicates are the field's edge when it has one and the field
    // name otherwise. See ADR 0001 decision 1.
    assert!(
        shape.contains(&format!(
            "sh:path <{}>",
            predicate(&fixture.class, "supersedes")
        )),
        "{shape}"
    );
    assert!(
        shape.contains(&format!(
            "sh:path <{}>",
            predicate(&fixture.class, "intent")
        )),
        "{shape}"
    );
}

#[test]
fn custom_ref_target_must_carry_the_custom_extension_class() {
    let fixture = Fixture::new();
    let (mut nodes, edges) = fixture.valid();
    // The rfc target loses its extension class but stays a valid Concept node.
    // The extension class is carried in metadata and asserted by the adapter, so
    // dropping it means clearing the metadata key, not just `types`.
    nodes[1].types = vec![C::Concept.iri().to_owned()];
    nodes[1].metadata = None;

    let report = fixture.run(&nodes, &edges);
    assert!(!report.conforms, "{}", report.report_ntriples);
    assert_eq!(
        violations(&report),
        [("Class".into(), predicate(&fixture.class, "supersedes"))]
    );
}

#[test]
fn builtin_intent_ref_target_must_carry_the_work_intent_class() {
    let fixture = Fixture::new();
    let (mut nodes, edges) = fixture.valid();
    // The intent target loses its WorkIntent type but stays a valid Concept node.
    nodes[2].types = vec![C::Concept.iri().to_owned()];

    let report = fixture.run(&nodes, &edges);
    assert!(!report.conforms, "{}", report.report_ntriples);
    assert_eq!(
        violations(&report),
        [("Class".into(), predicate(&fixture.class, "intent"))]
    );
}

#[test]
fn generated_turtle_parses_and_targets_the_valid_instance() {
    let fixture = Fixture::new();
    let (nodes, edges) = fixture.valid();
    let report = fixture.run(&nodes, &edges);

    assert!(report.conforms, "{}", report.report_ntriples);
    let coverage = fixture.coverage(&report);
    assert_eq!(coverage.applicable, 1, "{report:?}");
    assert_eq!(coverage.evaluated, 1, "{report:?}");
    assert!(
        report
            .diagnostics
            .iter()
            .all(|message| !message.contains("vacuous")),
        "{:?}",
        report.diagnostics
    );
}

#[test]
fn malformed_shape_is_an_error_not_a_vacuous_pass() {
    let fixture = Fixture::new();
    let (nodes, edges) = fixture.valid();
    let data = snapshot_ntriples(&nodes, &edges).unwrap();
    let malformed = format!(
        "@prefix sh: <{SHACL}> .\n<{}> a sh:NodeShape ; sh:targetClass <{}> ; sh:property [ sh:path <{}> ; sh:minCount \"one\" ] .",
        fixture.class,
        fixture.class,
        predicate(&fixture.class, "owner"),
    );

    let error = validate_ntriples(
        &data,
        "kind-shapes-test",
        &ValidationRequest {
            profile: Profile::Kind,
            custom: vec![ShapeBundle {
                iri: "urn:atomic:kind-shapes".into(),
                turtle: malformed,
            }],
            ..Default::default()
        },
    )
    .expect_err("a malformed shape must fail to parse");
    assert!(matches!(error, ValidationError::Shapes(_)), "{error}");
}

/// The ignored set is exactly the predicates the adapter emits on a kind node
/// besides the spec's own fields: its types, label, title summary, vault
/// timestamps, and the `atomic:references` traversal edges. See ADR 0001
/// decision 2.
#[test]
fn ignored_properties_cover_the_adapter_bookkeeping() {
    let fixture = Fixture::new();
    let shape = fixture.shape();
    let ignored = ignored_properties(&shape);

    assert_eq!(
        ignored,
        vec![
            RDF_TYPE.to_string(),
            RDFS_LABEL.to_string(),
            DCTERMS_DESCRIPTION.to_string(),
            DCTERMS_CREATED.to_string(),
            DCTERMS_MODIFIED.to_string(),
            ATOMIC_REFERENCES.to_string(),
        ]
    );

    // A valid instance carrying those bookkeeping triples must not trip the
    // closed shape.
    let (nodes, edges) = fixture.valid();
    let report = fixture.run(&nodes, &edges);
    assert!(report.conforms, "{}", report.report_ntriples);
}

/// Extract the `sh:ignoredProperties` list from the generated Turtle. The
/// generator writes one parenthesised list on one line, so a line scan is
/// enough and keeps the test independent of the SHACL engine's parser.
fn ignored_properties(shape: &str) -> Vec<String> {
    let start = shape
        .find("sh:ignoredProperties (")
        .expect("ignoredProperties emitted")
        + "sh:ignoredProperties (".len();
    let rest = &shape[start..];
    let end = rest.find(')').expect("ignoredProperties closed");
    rest[..end]
        .split_whitespace()
        .map(|term| {
            term.trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        })
        .collect()
}

/// The vault projection sets a kind instance's `title` as `KgNode::summary`,
/// which the adapter emits as `dcterms:description`. The generated closed shape
/// ignores that predicate, so a title-bearing instance conforms.
#[test]
fn title_summary_is_ignored_by_the_closed_shape() {
    let fixture = Fixture::new();
    let (mut nodes, edges) = fixture.valid();
    nodes[0].summary = Some("Use redb".into());

    let report = fixture.run(&nodes, &edges);
    assert!(report.conforms, "{}", report.report_ntriples);
}

/// The generated shape closes the five constructs SHACL-Core cannot see. The
/// uid suffix check is SHACL-SPARQL because the instance's concept IRI carries
/// the uid after a dot and Core has no suffix constraint.
#[test]
fn uid_must_match_the_instance_concept_suffix() {
    let fixture = Fixture::new();
    let (mut nodes, edges) = fixture.valid();
    metadata_of(&mut nodes[0]).insert("kind_uid".into(), json!(TARGET_UID));

    let report = fixture.run(&nodes, &edges);
    assert!(!report.conforms, "{}", report.report_ntriples);
    assert!(
        violations(&report).contains(&("SPARQL".into(), String::new())),
        "{:?}",
        violations(&report)
    );
}

/// A conflict marker in the body is a SHACL-SPARQL constraint, because Core has
/// no substring constraint.
#[test]
fn conflict_marker_in_the_body_is_rejected() {
    let fixture = Fixture::new();
    let (mut nodes, edges) = fixture.valid();
    metadata_of(&mut nodes[0]).insert(
        "kind_body".into(),
        json!("<<<<<<< ours\nredb\n=======\nsqlite\n>>>>>>> theirs\n"),
    );

    let report = fixture.run(&nodes, &edges);
    assert!(!report.conforms, "{}", report.report_ntriples);
    assert!(
        violations(&report).contains(&("SPARQL".into(), String::new())),
        "{:?}",
        violations(&report)
    );
}

/// `uid` must be a 26-character Crockford base32 ULID. The pattern is
/// SHACL-Core, so the violation carries the property path.
#[test]
fn uid_must_be_a_ulid() {
    let fixture = Fixture::new();
    let (mut nodes, edges) = fixture.valid();
    metadata_of(&mut nodes[0]).insert("kind_uid".into(), json!("not-a-ulid"));

    let report = fixture.run(&nodes, &edges);
    assert!(!report.conforms, "{}", report.report_ntriples);
    assert!(
        violations(&report).contains(&("Pattern".into(), predicate(&fixture.class, "uid"))),
        "{:?}",
        violations(&report)
    );
}
