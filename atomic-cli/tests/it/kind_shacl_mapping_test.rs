//! What the kind spec language means in SHACL, run instead of read.
//!
//! One ADR spec goes through `validate_instance`; the same rules, written by hand
//! as a shape, go through `atomic_shacl::validate_ntriples`. Each row edits one
//! good instance, derives its triples, and pins both outcomes exactly. `witness`
//! is the mapping's projection rule, and the shape is the spec's translation: no
//! production code does either, and the spec language does not compile to SHACL.
//!
//! The rows split three ways:
//! - [`AGREE`]: both reject, value for value, and a code pairs with one component.
//! - [`RDF_CANNOT_SEE`]: the validator rejects and the triples conform, because
//!   what it objects to has no triple.
//! - [`NAMES_DIFFER`] and the `sh:defaultValue` and system-key tests: both have an
//!   opinion, and it is not the same one.
//!
//! So the spec language is not 1:1 with SHACL-Core. To see the table:
//! `cargo test -p atomic-cli --test it kind_shacl_mapping_test -- --nocapture --test-threads=1`

use atomic_canonical::kind_spec::{validate_instance, KindSpec, RefResolver};
use atomic_shacl::{validate_ntriples, ShapeBundle, ValidationError, ValidationRequest};
use serde_json::{json, Map, Value};

const INSTANCE: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TR";
const TARGET: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TS";
const FOREIGN: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TT";
const MISSING: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TV";

const SHAPE_ID: &str = "urn:kind:AdrShape";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

const ADR_SHAPE: &str = r#"
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
@prefix k: <urn:kind:> .
@prefix f: <urn:kind:field:> .

k:AdrShape a sh:NodeShape ;
  sh:targetClass k:adr ;
  sh:closed true ;
  sh:ignoredProperties ( rdf:type ) ;
  sh:property
    [ sh:path f:uid ; sh:minCount 1 ; sh:maxCount 1 ; sh:datatype xsd:string ] ,
    [ sh:path f:title ; sh:maxCount 1 ; sh:datatype xsd:string ] ,
    [ sh:path f:status ; sh:minCount 1 ; sh:maxCount 1 ;
      sh:in ( "proposed" "accepted" "superseded" ) ] ,
    [ sh:path f:priority ; sh:maxCount 1 ; sh:datatype xsd:integer ] ,
    [ sh:path f:archived ; sh:maxCount 1 ; sh:datatype xsd:boolean ] ,
    [ sh:path f:tags ; sh:datatype xsd:string ] ,
    [ sh:path f:reviewers ; sh:minCount 1 ; sh:datatype xsd:string ] ,
    [ sh:path f:phases ; sh:in ( "design" "build" ) ] ,
    [ sh:path f:supersedes ; sh:class k:adr ] ,
    [ sh:path f:decidedBy ; sh:maxCount 1 ; sh:class k:adr ] ,
    [ sh:path f:about ; sh:maxCount 1 ;
      sh:or ( [ sh:class k:adr ] [ sh:class k:rfc ] ) ] ,
    [ sh:path f:body ; sh:minCount 1 ] .
"#;

const STATUS_PROPERTY: &str = "sh:path f:status ;";

fn adr_spec(status_default: Option<&str>) -> KindSpec {
    let mut status = json!({
        "type": "enum",
        "values": ["proposed", "accepted", "superseded"],
        "required": true,
    });
    if let Some(default) = status_default {
        status["default"] = json!(default);
    }
    let spec = json!({
        "name": "adr",
        "specVersion": 1,
        "fields": {
            "status": status,
            "priority": {"type": "integer"},
            "archived": {"type": "boolean"},
            "tags": {"type": "string[]"},
            "reviewers": {"type": "string[]", "required": true},
            "phases": {"type": "enum[]", "values": ["design", "build"]},
            "supersedes": {"type": "ref[]", "target": ["adr"], "edge": "supersedes"},
            "decidedBy": {"type": "ref", "target": ["adr"], "edge": "decidedBy"},
            "about": {"type": "ref", "target": ["adr", "rfc"], "edge": "about"},
        },
        "body": {"required": true},
    });
    KindSpec::from_frontmatter("adr", spec.as_object().unwrap()).unwrap()
}

/// `adr:<TARGET>` and `rfc:<FOREIGN>` exist. Nothing else does.
struct TwoTargets;

impl RefResolver for TwoTargets {
    fn resolve(&self, target_kind: &str, raw: &str) -> Option<String> {
        matches!((target_kind, raw), ("adr", TARGET) | ("rfc", FOREIGN)).then(|| instance_iri(raw))
    }
}

fn instance_iri(uid: &str) -> String {
    format!("urn:kind:instance:{uid}")
}

fn good_frontmatter() -> Map<String, Value> {
    json!({
        "uid": INSTANCE,
        "title": "Use redb",
        "status": "accepted",
        "priority": 1,
        "archived": false,
        "tags": ["storage", "storage-engine"],
        "reviewers": ["alice"],
        "phases": ["design"],
        "supersedes": [TARGET],
        "decidedBy": TARGET,
    })
    .as_object()
    .unwrap()
    .clone()
}

/// How a ref value that resolves to nothing appears as RDF. Either form is
/// plausible, and they meet at the same constraint.
#[derive(Clone, Copy)]
enum Unresolved {
    Literal,
    IriWithNoType,
}

fn literal(value: &Value) -> String {
    match value {
        Value::String(text) => serde_json::to_string(text).unwrap(),
        Value::Bool(flag) => format!("\"{flag}\"^^<{XSD}boolean>"),
        Value::Number(number) if number.is_f64() => format!("\"{number}\"^^<{XSD}double>"),
        Value::Number(number) => format!("\"{number}\"^^<{XSD}integer>"),
        other => format!(
            "{}^^<urn:atomic:kinds:json>",
            serde_json::to_string(&other.to_string()).unwrap()
        ),
    }
}

/// The instance as triples.
///
/// A list is its values, so an empty list and `null` are no triples. A ref that
/// resolves is the target's IRI. A body that is not blank is one triple. A
/// conforming `adr:<TARGET>` and an `rfc:<FOREIGN>` are always in the graph.
fn witness(frontmatter: &Map<String, Value>, body: &str, unresolved: Unresolved) -> String {
    let mut lines = vec![
        format!("<{}> <{RDF_TYPE}> <urn:kind:adr> .", instance_iri(TARGET)),
        format!(
            "<{}> <urn:kind:field:uid> \"{TARGET}\" .",
            instance_iri(TARGET)
        ),
        format!(
            "<{}> <urn:kind:field:status> \"accepted\" .",
            instance_iri(TARGET)
        ),
        format!(
            "<{}> <urn:kind:field:reviewers> \"alice\" .",
            instance_iri(TARGET)
        ),
        format!("<{}> <urn:kind:field:body> \"b\" .", instance_iri(TARGET)),
        format!("<{}> <{RDF_TYPE}> <urn:kind:rfc> .", instance_iri(FOREIGN)),
    ];
    let subject = instance_iri(INSTANCE);
    lines.push(format!("<{subject}> <{RDF_TYPE}> <urn:kind:adr> ."));
    for (key, value) in frontmatter {
        let values: Vec<&Value> = match value {
            Value::Array(items) => items.iter().collect(),
            Value::Null => Vec::new(),
            other => vec![other],
        };
        for value in values {
            let object = if ["supersedes", "decidedBy", "about"].contains(&key.as_str()) {
                ref_object(value, unresolved)
            } else {
                literal(value)
            };
            lines.push(format!("<{subject}> <urn:kind:field:{key}> {object} ."));
        }
    }
    if !body.trim().is_empty() {
        lines.push(format!(
            "<{subject}> <urn:kind:field:body> {} .",
            literal(&json!(body))
        ));
    }
    lines.join("\n") + "\n"
}

fn ref_object(value: &Value, unresolved: Unresolved) -> String {
    let Some(raw) = value.as_str() else {
        return literal(value);
    };
    let bare = raw.split_once(':').map_or(raw, |(_, id)| id);
    if bare == TARGET || bare == FOREIGN {
        return format!("<{}>", instance_iri(bare));
    }
    match unresolved {
        Unresolved::Literal => literal(value),
        Unresolved::IriWithNoType => format!("<{}>", instance_iri(bare)),
    }
}

/// `sh:minCount` from `http://www.w3.org/ns/shacl#MinCountConstraintComponent`.
fn component_name(iri: &str) -> &str {
    iri.strip_prefix("http://www.w3.org/ns/shacl#")
        .and_then(|name| name.strip_suffix("ConstraintComponent"))
        .unwrap_or(iri)
}

/// The sorted component of every result, with the shape having really run on the
/// instance and on nothing else that failed.
fn shacl_components(triples: &str, shape: &str) -> Result<Vec<String>, ValidationError> {
    let report = validate_ntriples(
        triples,
        "kind-shacl-mapping",
        &ValidationRequest {
            shape: Some(SHAPE_ID.into()),
            custom: vec![ShapeBundle {
                iri: "urn:kind:adr-shape".into(),
                turtle: shape.into(),
            }],
            ..Default::default()
        },
    )?;
    let coverage = report
        .target_coverage
        .iter()
        .find(|coverage| coverage.shape == SHAPE_ID)
        .unwrap_or_else(|| panic!("the shape did not run: {report:?}"));
    assert_eq!(coverage.evaluated, 2, "{report:?}");
    let mut components = Vec::new();
    for result in &report.results {
        assert_eq!(
            result["focus_node"],
            instance_iri(INSTANCE),
            "{}",
            report.report_ntriples
        );
        components.push(
            component_name(result["source_constraint_component"].as_str().unwrap()).to_string(),
        );
    }
    components.sort_unstable();
    assert_eq!(report.conforms, components.is_empty());
    Ok(components)
}

fn validator_codes(
    spec: &KindSpec,
    frontmatter: &Map<String, Value>,
    body: &str,
    file_uid: &str,
) -> Vec<String> {
    let report = validate_instance(spec, file_uid, frontmatter, body, &TwoTargets);
    assert_eq!(report.conforms, report.violations.is_empty());
    let mut codes: Vec<String> = report
        .violations
        .iter()
        .map(|violation| violation.code.as_str().to_string())
        .collect();
    codes.sort_unstable();
    codes
}

const GOOD_BODY: &str = "We chose redb.\n";

struct Row {
    construct: &'static str,
    edit: fn(&mut Map<String, Value>),
    body: &'static str,
    file_uid: &'static str,
    unresolved: Unresolved,
    codes: &'static [&'static str],
    components: &'static [&'static str],
}

const fn row(
    construct: &'static str,
    edit: fn(&mut Map<String, Value>),
    codes: &'static [&'static str],
    components: &'static [&'static str],
) -> Row {
    Row {
        construct,
        edit,
        body: GOOD_BODY,
        file_uid: INSTANCE,
        unresolved: Unresolved::Literal,
        codes,
        components,
    }
}

impl Row {
    const fn body(mut self, body: &'static str) -> Self {
        self.body = body;
        self
    }

    const fn file_uid(mut self, file_uid: &'static str) -> Self {
        self.file_uid = file_uid;
        self
    }

    const fn iri_with_no_type(mut self) -> Self {
        self.unresolved = Unresolved::IriWithNoType;
        self
    }
}

fn set(frontmatter: &mut Map<String, Value>, key: &str, value: Value) {
    frontmatter.insert(key.to_string(), value);
}

fn no_edit(_: &mut Map<String, Value>) {}

const AGREE: &[Row] = &[
    row(
        "required field missing",
        |fm| {
            fm.remove("status");
        },
        &["MISSING_REQUIRED"],
        &["MinCount"],
    ),
    row(
        "required field null",
        |fm| set(fm, "status", Value::Null),
        &["MISSING_REQUIRED"],
        &["MinCount"],
    ),
    row(
        "required list empty",
        |fm| set(fm, "reviewers", json!([])),
        &["MISSING_REQUIRED"],
        &["MinCount"],
    ),
    row(
        "uid missing",
        |fm| {
            fm.remove("uid");
        },
        &["MISSING_UID"],
        &["MinCount"],
    ),
    row("body blank", no_edit, &["BODY_REQUIRED"], &["MinCount"]).body("  \n"),
    row(
        "enum value outside values",
        |fm| set(fm, "status", json!("maybe")),
        &["NOT_IN_ENUM"],
        &["In"],
    ),
    row(
        "enum list, every bad value counted",
        |fm| set(fm, "phases", json!(["design", "ship", "stop"])),
        &["NOT_IN_ENUM", "NOT_IN_ENUM"],
        &["In", "In"],
    ),
    row(
        "integer given a string",
        |fm| set(fm, "priority", json!("high")),
        &["WRONG_TYPE"],
        &["Datatype"],
    ),
    row(
        "integer given a float",
        |fm| set(fm, "priority", json!(1.5)),
        &["WRONG_TYPE"],
        &["Datatype"],
    ),
    row(
        "integer given a mapping",
        |fm| set(fm, "priority", json!({"a": 1})),
        &["WRONG_TYPE"],
        &["Datatype"],
    ),
    row(
        "boolean given a string",
        |fm| set(fm, "archived", json!("no")),
        &["WRONG_TYPE"],
        &["Datatype"],
    ),
    row(
        "string list, every bad value counted",
        |fm| set(fm, "tags", json!(["ok", 1, true])),
        &["WRONG_TYPE", "WRONG_TYPE"],
        &["Datatype", "Datatype"],
    ),
    row(
        "title not a string",
        |fm| set(fm, "title", json!(3)),
        &["WRONG_TYPE"],
        &["Datatype"],
    ),
    row(
        "uid not a string",
        |fm| set(fm, "uid", json!(3)),
        &["WRONG_TYPE"],
        &["Datatype"],
    ),
    row(
        "undeclared field",
        |fm| set(fm, "owner", json!("me")),
        &["UNDECLARED_FIELD"],
        &["Closed"],
    ),
    row(
        "scalar given two values",
        |fm| set(fm, "status", json!(["proposed", "accepted"])),
        &["CARDINALITY"],
        &["MaxCount"],
    ),
    row(
        "ref unresolved, value a literal",
        |fm| set(fm, "supersedes", json!([MISSING])),
        &["UNRESOLVED_REF"],
        &["Class"],
    ),
    row(
        "ref with two targets, unresolved",
        |fm| set(fm, "about", json!(MISSING)),
        &["UNRESOLVED_REF"],
        &["Or"],
    ),
    row(
        "ref unresolved, value an IRI with no type",
        |fm| set(fm, "supersedes", json!([MISSING])),
        &["UNRESOLVED_REF"],
        &["Class"],
    )
    .iri_with_no_type(),
];

const RDF_CANNOT_SEE: &[Row] = &[
    row(
        "scalar given a one-item list",
        |fm| set(fm, "status", json!(["accepted"])),
        &["CARDINALITY"],
        &[],
    ),
    row(
        "list given a scalar",
        |fm| set(fm, "tags", json!("storage")),
        &["CARDINALITY"],
        &[],
    ),
    row(
        "uid differs from the filename",
        |fm| set(fm, "uid", json!(TARGET)),
        &["UID_MISMATCH"],
        &[],
    ),
    row(
        "filename not a ULID",
        |fm| set(fm, "uid", json!("not-a-ulid")),
        &["INVALID_ULID"],
        &[],
    )
    .file_uid("not-a-ulid"),
    row(
        "conflict marker in the body",
        no_edit,
        &["CONFLICT_MARKER"],
        &[],
    )
    .body("<<<<<<< ours\nredb\n=======\nsqlite\n>>>>>>> theirs\n"),
];

const NAMES_DIFFER: &[Row] = &[
    row(
        "enum given a non-string",
        |fm| set(fm, "status", json!(3)),
        &["WRONG_TYPE"],
        &["In"],
    ),
    row(
        "ref given a non-string",
        |fm| set(fm, "supersedes", json!([3])),
        &["WRONG_TYPE"],
        &["Class"],
    ),
    row(
        "ref naming a kind outside target",
        |fm| set(fm, "supersedes", json!([format!("rfc:{FOREIGN}")])),
        &["REF_KIND_MISMATCH"],
        &["Class"],
    ),
];

fn print_row(table: &str, construct: &str, codes: &str, components: &str) {
    println!("{table:<14} | {construct:<42} | {codes:<28} | {components}");
}

fn assert_rows(table: &str, rows: &[Row]) {
    let spec = adr_spec(None);
    for row in rows {
        let mut frontmatter = good_frontmatter();
        (row.edit)(&mut frontmatter);
        let codes = validator_codes(&spec, &frontmatter, row.body, row.file_uid);
        let triples = witness(&frontmatter, row.body, row.unresolved);
        let components = shacl_components(&triples, ADR_SHAPE)
            .unwrap_or_else(|error| panic!("{}: {error}\n{triples}", row.construct));
        print_row(
            table,
            row.construct,
            &codes.join(", "),
            &components.join(", "),
        );
        assert_eq!(codes, row.codes, "validator: {}\n{triples}", row.construct);
        assert_eq!(
            components, row.components,
            "shacl: {}\n{triples}",
            row.construct
        );
    }
}

#[test]
fn a_good_instance_conforms_under_both() {
    let spec = adr_spec(None);
    let good = good_frontmatter();
    assert!(validator_codes(&spec, &good, GOOD_BODY, INSTANCE).is_empty());
    let triples = witness(&good, GOOD_BODY, Unresolved::Literal);
    assert!(shacl_components(&triples, ADR_SHAPE).unwrap().is_empty());

    // Optional values may go, in either form, and a qualified ref is the same ref.
    let mut sparse = good_frontmatter();
    for key in ["title", "priority", "archived", "phases", "decidedBy"] {
        sparse.remove(key);
    }
    set(&mut sparse, "tags", Value::Null);
    set(&mut sparse, "supersedes", json!([format!("adr:{TARGET}")]));
    set(&mut sparse, "about", json!(FOREIGN));
    assert!(validator_codes(&spec, &sparse, GOOD_BODY, INSTANCE).is_empty());
    let triples = witness(&sparse, GOOD_BODY, Unresolved::Literal);
    assert!(shacl_components(&triples, ADR_SHAPE).unwrap().is_empty());
}

#[test]
fn constructs_both_reject_pair_a_code_with_a_component() {
    assert_rows("agree", AGREE);
}

#[test]
fn constructs_with_no_triple_are_invisible_to_shacl() {
    assert_rows("rdf-cannot-see", RDF_CANNOT_SEE);
}

#[test]
fn some_constructs_both_reject_under_different_names() {
    assert_rows("names-differ", NAMES_DIFFER);
}

/// The reason a default is never written into a shape: the validator reports a
/// missing required field whether or not it has a default, and SHACL 1.2 uses the
/// default as the value, so `sh:minCount` passes. `default` is a write-time rule
/// in the spec language and a validation rule in SHACL.
#[test]
fn sh_default_value_masks_sh_min_count() {
    let defaulted_shape = ADR_SHAPE.replace(
        STATUS_PROPERTY,
        &format!("{STATUS_PROPERTY} sh:defaultValue \"proposed\" ;"),
    );
    let mut frontmatter = good_frontmatter();
    frontmatter.remove("status");
    let triples = witness(&frontmatter, GOOD_BODY, Unresolved::Literal);

    // The control, identical but for `sh:defaultValue`.
    assert_eq!(shacl_components(&triples, ADR_SHAPE).unwrap(), ["MinCount"]);
    assert!(
        shacl_components(&triples, &defaulted_shape)
            .unwrap()
            .is_empty(),
        "sh:defaultValue no longer masks sh:minCount; the mapping table changes"
    );
    assert_eq!(
        validator_codes(
            &adr_spec(Some("proposed")),
            &frontmatter,
            GOOD_BODY,
            INSTANCE
        ),
        ["MISSING_REQUIRED"]
    );
    print_row(
        "default",
        "required field with a default, missing",
        "MISSING_REQUIRED",
        "",
    );

    // A present value is still checked, so only absence is masked.
    set(&mut frontmatter, "status", json!("maybe"));
    let triples = witness(&frontmatter, GOOD_BODY, Unresolved::Literal);
    assert_eq!(
        shacl_components(&triples, &defaulted_shape).unwrap(),
        ["In"]
    );
    print_row(
        "default",
        "required field with a default, bad value",
        "NOT_IN_ENUM",
        "In",
    );
}

/// A default is a value node, so the other constraints run on it. The spec
/// language rejects such a default when the definition loads instead.
#[test]
fn sh_default_value_is_checked_like_a_value() {
    let shape = ADR_SHAPE.replace(
        STATUS_PROPERTY,
        &format!("{STATUS_PROPERTY} sh:defaultValue \"bogus\" ;"),
    );
    let mut frontmatter = good_frontmatter();
    frontmatter.remove("status");
    let triples = witness(&frontmatter, GOOD_BODY, Unresolved::Literal);
    assert_eq!(shacl_components(&triples, &shape).unwrap(), ["In"]);

    let mut spec = adr_spec(None);
    let status = spec.fields.get_mut("status").unwrap();
    status.default = Some(json!("bogus"));
    let raw = serde_json::to_value(&spec).unwrap();
    assert!(
        KindSpec::from_frontmatter("adr", raw.as_object().unwrap()).is_err(),
        "the spec language refuses an invalid default"
    );
}

/// A list default has no SHACL form: at most one `sh:defaultValue`, and more than
/// one fails the whole shape load, not one field.
#[test]
fn a_list_default_cannot_be_written_as_sh_default_value() {
    let shape = ADR_SHAPE.replace(
        "sh:path f:tags ;",
        "sh:path f:tags ; sh:defaultValue \"a\" ; sh:defaultValue \"b\" ;",
    );
    let triples = witness(&good_frontmatter(), GOOD_BODY, Unresolved::Literal);
    let error = shacl_components(&triples, &shape).unwrap_err();
    assert!(matches!(error, ValidationError::Shapes(_)), "{error}");
    assert!(error.to_string().contains("sh:defaultValue"), "{error}");

    let mut spec = adr_spec(None);
    spec.fields.get_mut("tags").unwrap().default = Some(json!(["a", "b"]));
    let raw = serde_json::to_value(&spec).unwrap();
    assert!(KindSpec::from_frontmatter("adr", raw.as_object().unwrap()).is_ok());
}

/// `sh:closed` does not permit `rdf:type` on its own, so the shape has to name it.
#[test]
fn sh_closed_rejects_rdf_type_unless_ignored() {
    let shape = ADR_SHAPE.replace("sh:ignoredProperties ( rdf:type ) ;", "");
    assert_ne!(shape, ADR_SHAPE);
    let triples = witness(&good_frontmatter(), GOOD_BODY, Unresolved::Literal);
    let report = validate_ntriples(
        &triples,
        "kind-shacl-mapping",
        &ValidationRequest {
            shape: Some(SHAPE_ID.into()),
            custom: vec![ShapeBundle {
                iri: "urn:kind:adr-shape".into(),
                turtle: shape,
            }],
            ..Default::default()
        },
    )
    .unwrap();
    let closed: Vec<&Value> = report
        .results
        .iter()
        .filter(|result| {
            component_name(result["source_constraint_component"].as_str().unwrap()) == "Closed"
        })
        .collect();
    // The target and the instance, once each.
    assert_eq!(closed.len(), 2, "{}", report.report_ntriples);
    for result in closed {
        assert_eq!(result["result_path"], RDF_TYPE);
    }
}

/// The index stamps these keys and the validator skips them. `sh:closed` needs
/// its own, longer ignore list for the same result, or the projection must drop
/// them: the two closed-key lists are not the same list.
#[test]
fn system_keys_pass_the_validator_and_fail_a_closed_shape() {
    let mut frontmatter = good_frontmatter();
    set(
        &mut frontmatter,
        "created_at",
        json!("2026-11-05T12:00:00Z"),
    );
    assert!(validator_codes(&adr_spec(None), &frontmatter, GOOD_BODY, INSTANCE).is_empty());
    let triples = witness(&frontmatter, GOOD_BODY, Unresolved::Literal);
    assert_eq!(shacl_components(&triples, ADR_SHAPE).unwrap(), ["Closed"]);

    let shape = ADR_SHAPE.replace(
        "sh:ignoredProperties ( rdf:type ) ;",
        "sh:ignoredProperties ( rdf:type f:created_at ) ;",
    );
    assert!(shacl_components(&triples, &shape).unwrap().is_empty());
    print_row("system-key", "created_at present", "", "Closed");
}
