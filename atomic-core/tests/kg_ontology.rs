use atomic_core::kg::{
    legacy::{Replacement, Target, LEGACY_TERMS},
    ontology::*,
};
use oxrdf::{Subject, Term, Triple};
use oxttl::TurtleParser;
use std::collections::BTreeSet;

fn graph() -> Vec<Triple> {
    TurtleParser::new()
        .for_reader(TTL.as_bytes())
        .collect::<Result<_, _>>()
        .unwrap()
}

fn objects<'a>(graph: &'a [Triple], subject: &str, predicate: &str) -> BTreeSet<&'a str> {
    graph
        .iter()
        .filter_map(|triple| match (&triple.subject, &triple.object) {
            (Subject::NamedNode(s), Term::NamedNode(o))
                if s.as_str() == subject && triple.predicate.as_str() == predicate =>
            {
                Some(o.as_str())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn registry_matches_the_complete_versioned_ontology() {
    let graph = graph();
    let rdfs = "http://www.w3.org/2000/01/rdf-schema#";
    let owl = "http://www.w3.org/2002/07/owl#";
    let rdf_type = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
    for class in Class::ALL {
        assert!(objects(&graph, class.iri(), rdf_type).contains(format!("{owl}Class").as_str()));
        assert_eq!(
            objects(&graph, class.iri(), &format!("{rdfs}subClassOf")),
            class.parents().iter().map(|p| p.iri()).collect()
        );
    }
    for predicate in Predicate::ALL {
        if !predicate.is_external() {
            let property_type = if matches!(predicate.range(), Some(Range::Datatype(_))) {
                "DatatypeProperty"
            } else {
                "ObjectProperty"
            };
            let declarations = objects(&graph, predicate.iri(), rdf_type);
            assert!(
                declarations.contains(format!("{owl}{property_type}").as_str()),
                "missing property declaration: {predicate:?}"
            );
            assert_eq!(
                declarations.contains(format!("{owl}TransitiveProperty").as_str()),
                predicate.is_transitive()
            );
        }
        assert_eq!(
            objects(&graph, predicate.iri(), &format!("{rdfs}domain")),
            predicate.domain().map(ClassRef::iri).into_iter().collect(),
            "domain: {predicate:?}"
        );
        assert_eq!(
            objects(&graph, predicate.iri(), &format!("{rdfs}range")),
            predicate.range().map(Range::iri).into_iter().collect(),
            "range: {predicate:?}"
        );
        assert_eq!(
            objects(&graph, predicate.iri(), &format!("{owl}inverseOf")),
            predicate
                .inverse()
                .map(Predicate::iri)
                .into_iter()
                .collect()
        );
        assert_eq!(
            objects(&graph, predicate.iri(), &format!("{rdfs}subPropertyOf")),
            predicate
                .super_predicate()
                .map(Predicate::iri)
                .into_iter()
                .collect()
        );
        let mut lists = graph.iter().filter(|t| {
            t.subject.to_string() == format!("<{}>", predicate.iri())
                && t.predicate.as_str() == format!("{owl}propertyChainAxiom")
        });
        let mut chain = Vec::new();
        if let Some(triple) = lists.next() {
            let mut cursor = triple.object.clone();
            while cursor.to_string() != "<http://www.w3.org/1999/02/22-rdf-syntax-ns#nil>" {
                let row = |property: &str| {
                    graph
                        .iter()
                        .find(|t| {
                            t.subject.to_string() == cursor.to_string()
                                && t.predicate.as_str() == property
                        })
                        .unwrap()
                        .object
                        .clone()
                };
                chain.push(row("http://www.w3.org/1999/02/22-rdf-syntax-ns#first").to_string());
                cursor = row("http://www.w3.org/1999/02/22-rdf-syntax-ns#rest");
            }
        }
        assert!(lists.next().is_none());
        assert_eq!(
            chain,
            predicate
                .property_chain()
                .iter()
                .map(|p| format!("<{}>", p.iri()))
                .collect::<Vec<_>>()
        );
    }
    for triple in &graph {
        if let Subject::NamedNode(subject) = &triple.subject {
            if subject.as_str().starts_with(NAMESPACE)
                && triple.predicate.as_str().ends_with("#type")
            {
                match triple.object.to_string().as_str() {
                    "<http://www.w3.org/2002/07/owl#Class>" => {
                        assert!(Class::from_iri(subject.as_str()).is_some())
                    }
                    "<http://www.w3.org/2002/07/owl#ObjectProperty>"
                    | "<http://www.w3.org/2002/07/owl#DatatypeProperty>"
                    | "<http://www.w3.org/2002/07/owl#TransitiveProperty>" => {
                        assert!(Predicate::from_iri(subject.as_str()).is_some())
                    }
                    _ => {}
                }
            }
        }
    }
}

#[test]
fn ontology_uses_only_the_declared_rl_constructs() {
    let allowed = [
        "rdf:first",
        "rdf:rest",
        "rdfs:subClassOf",
        "rdfs:subPropertyOf",
        "rdfs:domain",
        "rdfs:range",
        "owl:inverseOf",
        "owl:propertyChainAxiom",
        "rdfs:isDefinedBy",
        "owl:versionIRI",
        "rdfs:label",
        "rdfs:comment",
    ];
    for triple in graph() {
        assert!(
            triple.predicate.as_str() == "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"
                || allowed.contains(&compact(triple.predicate.as_str()).as_str()),
            "unexpected axiom: {triple}"
        );
    }
}

#[test]
fn legacy_inventory_is_unique_and_all_targets_exist() {
    let mut old = BTreeSet::new();
    let target = |target: Target| match target {
        Target::Class(c) => assert!(Class::ALL.contains(&c)),
        Target::Predicate(p) => assert!(Predicate::ALL.contains(&p)),
        Target::Removed { reason } => assert!(!reason.is_empty()),
    };
    for (term, replacement) in LEGACY_TERMS {
        assert!(old.insert(*term), "duplicate legacy term: {term}");
        match replacement {
            Replacement::One(t) => target(*t),
            Replacement::ClassWithRefinement {
                base,
                refinements,
                rule,
            } => {
                target(Target::Class(*base));
                assert!(!rule.is_empty());
                for c in *refinements {
                    target(Target::Class(*c));
                }
            }
            Replacement::SplitByEndpoint(rules) => {
                for rule in *rules {
                    target(rule.target);
                }
            }
        }
    }
    // 81 former constants, plus 20 bare node kinds.
    assert_eq!(old.len(), 101);
}
