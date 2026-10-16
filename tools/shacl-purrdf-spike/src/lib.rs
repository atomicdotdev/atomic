//! Isolated integration probe of the published PurRDF SHACL engine.
//! No Atomic database, policy evaluator, or syntax extraction is involved.

#[cfg(test)]
mod tests {
    use purrdf_shapes::engine::{parse_shapes, validate_graphs};
    use purrdf_shapes::report::ValidationReport;

    const PREFIXES: &str = r#"
        @prefix atomic: <https://atomic.dev/ns/workflow#> .
        @prefix sh: <http://www.w3.org/ns/shacl#> .
        @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
        @prefix ex: <urn:atomic:project:TEST:> .
    "#;
    const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
    const NS: &str = "https://atomic.dev/ns/workflow#";

    fn iri(local: &str) -> String {
        format!("<urn:atomic:project:TEST:{local}>")
    }

    fn class(subject: &str, name: &str) -> String {
        format!("{} <{RDF_TYPE}> <{NS}{name}> .\n", iri(subject))
    }

    fn edge(subject: &str, predicate: &str, object: &str) -> String {
        format!("{} <{NS}{predicate}> {} .\n", iri(subject), iri(object))
    }

    fn literal(subject: &str, predicate: &str, value: &str) -> String {
        format!("{} <{NS}{predicate}> \"{value}\" .\n", iri(subject))
    }

    fn validate(data: &str, shapes: &str) -> ValidationReport {
        validate_graphs(data, &format!("{PREFIXES}\n{shapes}"), None)
            .expect("shape loading and validation must succeed, not silently skip rules")
    }

    #[test]
    fn required_plan_cardinality_and_report_fields() {
        let shapes = r#"
            ex:TurnShape a sh:NodeShape; sh:targetClass atomic:ManagedTurn;
              sh:property ex:PlanProperty .
            ex:PlanProperty sh:path atomic:hasPlan; sh:minCount 1; sh:maxCount 1;
              sh:message "A managed turn needs exactly one plan." .
        "#;
        let missing = class("turn:1", "ManagedTurn");
        let report = validate(&missing, shapes);
        assert!(!report.conforms);
        assert_eq!(report.results.len(), 1);
        let result = &report.results[0];
        assert_eq!(result.focus_value(), "urn:atomic:project:TEST:turn:1");
        assert!(
            result
                .result_path
                .as_ref()
                .unwrap()
                .to_string()
                .contains("hasPlan")
        );
        assert!(result.source_shape.to_string().contains("PlanProperty"));
        assert!(
            result
                .source_constraint_component
                .to_string()
                .contains("MinCount")
        );
        assert!(result.severity.iri().ends_with("Violation"));
        assert_eq!(result.messages.len(), 1);
        let nt = report.to_ntriples();
        for field in ["focusNode", "resultPath", "sourceShape", "resultMessage"] {
            assert!(nt.contains(field), "missing standard report field: {field}");
        }
        let good = format!("{missing}{}", edge("turn:1", "hasPlan", "intent:A"));
        assert!(validate(&good, shapes).conforms);
        let two = format!("{good}{}", edge("turn:1", "hasPlan", "intent:B"));
        assert!(!validate(&two, shapes).conforms);
    }

    #[test]
    fn nested_acceptance_criterion_shape_is_enforced() {
        let shapes = r#"
            ex:IntentShape a sh:NodeShape; sh:targetClass atomic:WorkIntent;
              sh:property [ sh:path atomic:hasAcceptanceCriterion; sh:node ex:ACShape ] .
            ex:ACShape a sh:NodeShape;
              sh:property [ sh:path atomic:status; sh:minCount 1; sh:in ("open" "met") ] .
        "#;
        let base = format!(
            "{}{}",
            class("intent:A", "WorkIntent"),
            edge("intent:A", "hasAcceptanceCriterion", "ac:1")
        );
        assert!(!validate(&base, shapes).conforms);
        assert!(
            !validate(
                &format!("{base}{}", literal("ac:1", "status", "bogus")),
                shapes
            )
            .conforms
        );
        assert!(
            validate(
                &format!("{base}{}", literal("ac:1", "status", "met")),
                shapes
            )
            .conforms
        );
    }

    #[test]
    fn sparql_conditional_met_requires_verifier_and_evidence() {
        let shapes = r#"
            ex:ACShape a sh:NodeShape; sh:targetClass atomic:AcceptanceCriterion;
              sh:sparql [ a sh:SPARQLConstraint;
                sh:message "Met criteria require a verifier and evidence.";
                sh:select """
                    PREFIX atomic: <https://atomic.dev/ns/workflow#>
                    SELECT $this WHERE {
                      $this atomic:status "met" .
                      FILTER (
                        NOT EXISTS { $this atomic:verifiedBy ?verifier } ||
                        NOT EXISTS { $this atomic:evidence ?evidence }
                      )
                    }
                """ ] .
        "#;
        let base = class("ac:1", "AcceptanceCriterion");
        assert!(
            validate(
                &format!("{base}{}", literal("ac:1", "status", "open")),
                shapes
            )
            .conforms
        );
        let met = format!("{base}{}", literal("ac:1", "status", "met"));
        assert!(!validate(&met, shapes).conforms);
        let verified = format!("{met}{}", edge("ac:1", "verifiedBy", "identity:reviewer"));
        assert!(!validate(&verified, shapes).conforms);
        let complete = format!("{verified}{}", literal("ac:1", "evidence", "test-report"));
        assert!(validate(&complete, shapes).conforms);
    }

    #[test]
    fn inverse_sequence_path_follows_change_to_turn_to_intent() {
        let shapes = r#"
            ex:ChangeShape a sh:NodeShape; sh:targetClass atomic:Change;
              sh:property [
                sh:path ( [ sh:inversePath atomic:generatedChange ] atomic:hasPlan );
                sh:minCount 1
              ] .
        "#;
        let base = format!(
            "{}{}",
            class("change:1", "Change"),
            edge("turn:1", "generatedChange", "change:1")
        );
        let report = validate(&base, shapes);
        assert!(!report.conforms);
        assert!(report.results[0].result_path.is_some());
        assert!(report.results[0].path_structure.is_some());
        assert!(report.to_ntriples().contains("inversePath"));
        assert!(
            validate(
                &format!("{base}{}", edge("turn:1", "hasPlan", "intent:A")),
                shapes
            )
            .conforms
        );
    }

    #[test]
    fn class_datatype_and_boolean_constraints() {
        let shapes = r#"
            ex:IntentShape a sh:NodeShape; sh:targetClass atomic:WorkIntent;
              sh:property [ sh:path atomic:assignedTo; sh:minCount 1; sh:class atomic:Identity ];
              sh:property [ sh:path atomic:priority; sh:datatype xsd:integer ];
              sh:or (
                [ sh:property [ sh:path atomic:status; sh:hasValue "backlog" ] ]
                [ sh:property [ sh:path atomic:hasTask; sh:minCount 1 ] ]
              ) .
        "#;
        let base = format!(
            "{}{}{}",
            class("intent:A", "WorkIntent"),
            edge("intent:A", "assignedTo", "identity:reviewer"),
            literal("intent:A", "status", "backlog")
        );
        assert!(!validate(&base, shapes).conforms);
        let typed = format!("{base}{}", class("identity:reviewer", "Identity"));
        assert!(validate(&typed, shapes).conforms);
        let wrong_datatype = format!("{typed}{}", literal("intent:A", "priority", "high"));
        assert!(!validate(&wrong_datatype, shapes).conforms);
        let correct_datatype = format!(
            "{typed}{} <{NS}priority> \"1\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n",
            iri("intent:A")
        );
        assert!(validate(&correct_datatype, shapes).conforms);
        let active = correct_datatype.replace("\"backlog\"", "\"active\"");
        assert!(!validate(&active, shapes).conforms);
        assert!(
            validate(
                &format!("{active}{}", edge("intent:A", "hasTask", "task:1")),
                shapes
            )
            .conforms
        );
    }

    #[test]
    fn two_missing_properties_produce_distinct_paths() {
        let shapes = r#"
            ex:ChangeShape a sh:NodeShape; sh:targetClass atomic:Change;
              sh:property [ sh:path atomic:implements; sh:minCount 1 ];
              sh:property [ sh:path atomic:attestedBy; sh:minCount 1 ] .
        "#;
        let report = validate(&class("change:1", "Change"), shapes);
        assert!(!report.conforms);
        assert_eq!(report.results.len(), 2);
        let paths: std::collections::BTreeSet<_> = report
            .results
            .iter()
            .map(|r| r.result_path.as_ref().unwrap().to_string())
            .collect();
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn malformed_shape_parameter_is_rejected() {
        let shapes = format!(
            r#"{PREFIXES}
            ex:BadShape a sh:NodeShape; sh:targetClass atomic:Change;
              sh:property [ sh:path atomic:implements; sh:minCount "one" ] .
        "#
        );
        assert!(parse_shapes(&shapes, None).is_err());
    }

    #[test]
    fn unsupported_shacl_js_is_rejected_even_with_no_targets() {
        let shapes = format!(
            r#"{PREFIXES}
            ex:Unsupported a sh:NodeShape; sh:targetClass atomic:Change;
              sh:js [ a sh:JSConstraint; sh:jsFunctionName "validate" ] .
        "#
        );
        assert!(parse_shapes(&shapes, None).is_err());
        assert!(validate_graphs("", &shapes, None).is_err());
    }

    #[test]
    fn malformed_sparql_query_is_rejected() {
        let shapes = format!(
            r#"{PREFIXES}
            ex:BadQuery a sh:NodeShape; sh:targetClass atomic:Change;
              sh:sparql [ a sh:SPARQLConstraint; sh:select "SELECT $this WHERE {{" ] .
        "#
        );
        assert!(parse_shapes(&shapes, None).is_err());
        assert!(validate_graphs("", &shapes, None).is_err());
    }

    #[test]
    fn valid_shapes_with_no_focus_nodes_conform_vacuously() {
        let shapes = r#"
            ex:ChangeShape a sh:NodeShape; sh:targetClass atomic:Change;
              sh:property [ sh:path atomic:attestedBy; sh:minCount 1 ] .
        "#;
        let report = validate("", shapes);
        assert!(report.conforms);
        assert!(report.results.is_empty());
        // Atomic must report target coverage separately: this is not audit evidence.
        assert!(!validate(&class("change:1", "Change"), shapes).conforms);
    }

    #[test]
    fn project_scoped_subjects_do_not_merge_in_a_combined_graph() {
        let shapes = r#"
            ex:TurnShape a sh:NodeShape; sh:targetClass atomic:ManagedTurn;
              sh:property [ sh:path atomic:hasPlan; sh:minCount 1 ] .
        "#;
        let data = format!(
            "{}{}{}",
            class("turn:1", "ManagedTurn"),
            class("turn:1", "ManagedTurn").replace(":TEST:", ":OTHER:"),
            edge("turn:1", "hasPlan", "intent:A")
        );
        let report = validate(&data, shapes);
        assert!(!report.conforms);
        assert_eq!(report.results.len(), 1);
        assert_eq!(
            report.results[0].focus_value(),
            "urn:atomic:project:OTHER:turn:1"
        );
    }
}
