use atomic_core::{
    kg::{iri, ontology::Class},
    types::Hash,
    KgNode,
};
use atomic_shacl::{snapshot_ntriples, validate_ntriples, ValidationRequest};

#[test]
fn change_availability_is_exported_and_contradictory_projection_is_rejected() {
    let id = iri::mint_change(&Hash::of(b"availability"));
    for (availability, completeness, conforms) in [
        ("unknown", "reference-only", true),
        ("absent", "reference-only", true),
        ("available", "reference-only", true),
        ("available", "projected", true),
        ("absent", "projected", false),
        ("unknown", "projected", false),
        ("guessed", "reference-only", false),
    ] {
        let node = KgNode::new(&id, Class::Change, "change", "test").with_metadata(
            serde_json::json!({"object_availability": availability, "projection_completeness": completeness}));
        let rdf = snapshot_ntriples(&[node], &[]).unwrap();
        assert!(rdf.contains("workflow#objectAvailability>"));
        assert!(rdf.contains("workflow#projectionCompleteness>"));
        let result =
            validate_ntriples(&rdf, "availability", &ValidationRequest::default()).unwrap();
        assert_eq!(
            result.conforms, conforms,
            "{availability}/{completeness}: {:?}",
            result.results
        );
    }
}
