use atomic_shacl::{validate_ntriples, ShapeBundle, ValidationRequest};
use oxttl::TurtleParser;

#[test]
fn w3c_class_001_preserves_subclass_matching_and_exact_standard_results() {
    let turtle = include_str!("fixtures/w3c-class-001.ttl");
    let triples = TurtleParser::new()
        .for_reader(turtle.as_bytes())
        .map(|triple| format!("{} .\n", triple.unwrap()))
        .collect::<String>();
    let report = validate_ntriples(
        &triples,
        "w3c:core/node/class-001",
        &ValidationRequest {
            custom: vec![ShapeBundle {
                iri: "urn:w3c:class-001".into(),
                turtle: turtle.into(),
            }],
            shape: Some("http://datashapes.org/sh/tests/core/node/class-001.test#TestShape".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!report.conforms);
    assert_eq!(report.results.len(), 2);
    let mut focus: Vec<_> = report
        .results
        .iter()
        .map(|result| result["focus_node"].as_str().unwrap())
        .collect();
    focus.sort();
    assert_eq!(
        focus,
        [
            "http://datashapes.org/sh/tests/core/node/class-001.test#Quokki",
            "http://datashapes.org/sh/tests/core/node/class-001.test#Typeless"
        ]
    );
    for result in &report.results {
        assert_eq!(
            result["source_constraint_component"],
            "http://www.w3.org/ns/shacl#ClassConstraintComponent"
        );
        assert_eq!(
            result["result_severity"],
            "http://www.w3.org/ns/shacl#Violation"
        );
        assert_eq!(result["focus_node"], result["value"]);
    }
    assert_eq!(report.target_coverage[0].evaluated, 4);
}
