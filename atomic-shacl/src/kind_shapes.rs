//! Generate SHACL shapes from a recorded custom-kind spec.
//!
//! One spec → one `sh:NodeShape`. The shape is the normative validator for the
//! kind; the in-binary `kind_spec` validator remains a write-time pre-check and
//! the test oracle. See `docs/adr/0001-kind-kg-conformance.md`.
//!
//! The projection contract the shape reads is the adapter's:
//!
//! - instances are typed `<project>:kind:<kind>` and `atomic:Concept`
//! - fields are `<project>:kind:<kind>#<field>`
//! - `uid` and `body` are `<project>:kind:<kind>#uid` and `#body`
//! - relations are `<project>:kind:<kind>#<edge>`

use atomic_canonical::kind_spec::{BaseType, FieldSpec, KindSpec};
use std::fmt::Write as _;

use crate::ValidationError;

/// The extension class IRI for a custom kind's instances.
pub fn kind_class(project: &str, kind: &str) -> String {
    format!("{project}:kind:{kind}")
}

/// The RDF class a ref target denotes. Built-in targets keep their workflow
/// ontology class; custom targets use the project-scoped extension class.
fn target_class(project: &str, target: &str) -> String {
    match target {
        "intent" => "https://atomic.dev/ns/workflow#WorkIntent".to_string(),
        "memory" => "https://atomic.dev/ns/workflow#Memory".to_string(),
        custom => kind_class(project, custom),
    }
}

/// Turtle for one kind's shape. `project` is the base IRI
/// `urn:atomic:project:<pid>` (no trailing `:`).
pub fn shape_for(spec: &KindSpec, project: &str) -> Result<String, ValidationError> {
    let class = kind_class(project, &spec.name);
    let mut out = String::new();
    writeln!(out, "@prefix sh: <http://www.w3.org/ns/shacl#> .").unwrap();
    writeln!(out, "@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .").unwrap();
    writeln!(
        out,
        "@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> ."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "<{class}> a sh:NodeShape ;").unwrap();
    writeln!(out, "  sh:targetClass <{class}> ;").unwrap();
    writeln!(out, "  sh:closed true ;").unwrap();
    // The adapter also emits the label, the title summary, the vault
    // timestamps, and the `atomic:references` traversal edge. Those are not spec
    // fields, so the closed shape must ignore them. See ADR 0001 decision 2.
    writeln!(
        out,
        "  sh:ignoredProperties ( <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://www.w3.org/2000/01/rdf-schema#label> <http://purl.org/dc/terms/description> <http://purl.org/dc/terms/created> <http://purl.org/dc/terms/modified> <https://atomic.dev/ns/workflow#references> ) ;"
    )
    .unwrap();

    // uid: required, single-valued, ULID-shaped.
    writeln!(
        out,
        "  sh:property [ sh:path <{class}#uid> ; sh:minCount 1 ; sh:maxCount 1 ; sh:datatype xsd:string ; sh:pattern \"^[0-9A-HJKMNP-TV-Z]{{26}}$\" ] ;"
    )
    .unwrap();

    // body: required when the spec says so.
    if spec.body.required {
        writeln!(
            out,
            "  sh:property [ sh:path <{class}#body> ; sh:minCount 1 ; sh:datatype xsd:string ] ;"
        )
        .unwrap();
    } else {
        writeln!(
            out,
            "  sh:property [ sh:path <{class}#body> ; sh:datatype xsd:string ] ;"
        )
        .unwrap();
    }

    for (name, field) in &spec.fields {
        // A ref's edge label is the predicate the projection emits; without one
        // the field name is the predicate. See ADR 0001 decision 1.
        let predicate = format!("{class}#{}", field.edge.as_deref().unwrap_or(name.as_str()));
        write_property(&mut out, &predicate, field, project)?;
    }

    // The uid triple must equal the instance's concept suffix. SHACL-Core has no
    // string-suffix constraint, so this is a SPARQL constraint.
    writeln!(out, "  sh:sparql [").unwrap();
    writeln!(
        out,
        "    sh:message \"uid must equal the instance's concept suffix\" ;"
    )
    .unwrap();
    writeln!(out, "    sh:select \"\"\"").unwrap();
    writeln!(
        out,
        "      SELECT $this ?uid WHERE {{ $this <{class}#uid> ?uid . FILTER (STRAFTER(STR($this), \".\") != STR(?uid)) }}"
    )
    .unwrap();
    writeln!(out, "    \"\"\"").unwrap();
    writeln!(out, "  ] ;").unwrap();

    // A conflict marker at column 0 is not representable as a SHACL Core string
    // constraint; keep it as a SPARQL constraint over the body.
    writeln!(out, "  sh:sparql [").unwrap();
    writeln!(out, "    sh:message \"body contains a conflict marker\" ;").unwrap();
    writeln!(out, "    sh:select \"\"\"").unwrap();
    writeln!(
        out,
        "      SELECT $this ?body WHERE {{ $this <{class}#body> ?body . FILTER (CONTAINS(STR(?body), \"<<<<<<<\")) }}"
    )
    .unwrap();
    writeln!(out, "    \"\"\"").unwrap();
    writeln!(out, "  ] .").unwrap();

    Ok(out)
}

fn write_property(
    out: &mut String,
    predicate: &str,
    field: &FieldSpec,
    project: &str,
) -> Result<(), ValidationError> {
    let mut constraints = vec![format!("sh:path <{predicate}>")];
    if !field.field_type.list {
        constraints.push("sh:maxCount 1".into());
    }
    if field.required {
        constraints.push("sh:minCount 1".into());
    }
    match field.field_type.base {
        BaseType::Ref => {
            constraints.push("sh:nodeKind sh:IRI".into());
            let targets: Vec<String> = field
                .target
                .iter()
                .map(|target| target_class(project, target))
                .collect();
            match targets.as_slice() {
                [] => {}
                [one] => constraints.push(format!("sh:class <{one}>")),
                many => {
                    let alternatives = many
                        .iter()
                        .map(|target| format!("[ sh:class <{target}> ]"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    constraints.push(format!("sh:or ( {alternatives} )"));
                }
            }
        }
        BaseType::Enum => {
            constraints.push("sh:datatype xsd:string".into());
            let values = field
                .values
                .iter()
                .map(|value| format!("\"{}\"", escape(value)))
                .collect::<Vec<_>>()
                .join(" ");
            constraints.push(format!("sh:in ( {values} )"));
        }
        BaseType::String => constraints.push("sh:datatype xsd:string".into()),
        BaseType::Integer => constraints.push("sh:datatype xsd:integer".into()),
        BaseType::Boolean => constraints.push("sh:datatype xsd:boolean".into()),
    }
    writeln!(out, "  sh:property [ {} ] ;", constraints.join(" ; ")).unwrap();
    Ok(())
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
