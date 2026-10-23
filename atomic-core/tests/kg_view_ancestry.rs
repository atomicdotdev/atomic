//! Ancestry axioms describe entailments, not automatic KG query expansion.

use atomic_core::{
    kg::{
        ontology::{Class, Predicate, TTL},
        Direction,
    },
    pristine::{KgMutTxnT, KgTxnT, MutTxnT, Pristine},
    KgEdge, KgNode,
};
use oxrdf::{Subject, Term};
use oxttl::TurtleParser;
use std::collections::BTreeSet;

type Fact = (&'static str, Predicate, &'static str);

fn asserted() -> BTreeSet<Fact> {
    BTreeSet::from([
        ("urn:view:child", Predicate::ChildViewOf, "urn:view:parent"),
        ("urn:view:parent", Predicate::ChildViewOf, "urn:view:root"),
        ("urn:change:one", Predicate::DirectlyOnView, "urn:view:root"),
    ])
}

// Test-only application of subproperty and transitivity rules. Production KG
// queries do not run an OWL reasoner; keep that boundary explicit below.
fn declared_closure(mut facts: BTreeSet<Fact>) -> BTreeSet<Fact> {
    loop {
        let previous = facts.clone();
        for &(subject, predicate, object) in &previous {
            if let Some(parent) = predicate.super_predicate() {
                facts.insert((subject, parent, object));
            }
            if predicate.is_transitive() {
                for &(next_subject, next_predicate, next_object) in &previous {
                    if object == next_subject && predicate == next_predicate {
                        facts.insert((subject, predicate, next_object));
                    }
                }
            }
        }
        if facts == previous {
            return facts;
        }
    }
}

#[test]
fn turtle_and_registry_link_child_to_ancestor_not_the_reverse() {
    let graph = TurtleParser::new()
        .for_reader(TTL.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let objects = |subject: &str, predicate: &str| -> BTreeSet<String> {
        graph
            .iter()
            .filter_map(|triple| match (&triple.subject, &triple.object) {
                (Subject::NamedNode(s), Term::NamedNode(o))
                    if s.as_str() == subject && triple.predicate.as_str() == predicate =>
                {
                    Some(o.as_str().to_owned())
                }
                _ => None,
            })
            .collect()
    };
    let subproperty = "http://www.w3.org/2000/01/rdf-schema#subPropertyOf";
    assert_eq!(
        objects(Predicate::ChildViewOf.iri(), subproperty),
        BTreeSet::from([Predicate::AncestorView.iri().to_owned()])
    );
    assert!(objects(Predicate::AncestorView.iri(), subproperty).is_empty());
    assert!(objects(Predicate::DirectlyOnView.iri(), subproperty).is_empty());
    assert!(objects(
        Predicate::ChildViewOf.iri(),
        "http://www.w3.org/2002/07/owl#inverseOf"
    )
    .is_empty());
    assert!(objects(
        Predicate::AncestorView.iri(),
        "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"
    )
    .contains("http://www.w3.org/2002/07/owl#TransitiveProperty"));
    assert_eq!(
        Predicate::ChildViewOf.super_predicate(),
        Some(Predicate::AncestorView)
    );
    assert_eq!(Predicate::ChildViewOf.inverse(), None);
    assert!(!Predicate::ChildViewOf.is_transitive());
    assert!(Predicate::AncestorView.is_transitive());
    assert_eq!(Predicate::AncestorView.super_predicate(), None);
    assert_eq!(Predicate::DirectlyOnView.super_predicate(), None);
}

#[test]
fn three_level_ancestry_closure_preserves_direction_and_direct_membership() {
    let facts = asserted();
    let closure = declared_closure(facts.clone());
    let mut expected = facts;
    expected.extend([
        ("urn:view:child", Predicate::AncestorView, "urn:view:parent"),
        ("urn:view:parent", Predicate::AncestorView, "urn:view:root"),
        ("urn:view:child", Predicate::AncestorView, "urn:view:root"),
    ]);
    // Exact equality excludes reverse/self ancestry, child->root direct
    // parenthood, and inherited directlyOnView membership for the change.
    assert_eq!(closure, expected);
    assert!(!closure.iter().any(|(s, _, o)| s == o));
    assert_eq!(
        closure
            .iter()
            .filter(|(_, p, _)| *p == Predicate::DirectlyOnView)
            .count(),
        1
    );
}

#[test]
fn ordinary_kg_queries_return_asserted_edges_not_ancestry_entailments() {
    let dir = tempfile::tempdir().unwrap();
    let pristine = Pristine::open(dir.path().join("db")).unwrap();
    let mut txn = pristine.write_txn().unwrap();
    txn.init_kg().unwrap();
    for id in ["urn:view:child", "urn:view:parent", "urn:view:root"] {
        txn.upsert_kg_node(&KgNode::new(id, Class::View, id, "test"))
            .unwrap();
    }
    txn.upsert_kg_node(&KgNode::new(
        "urn:change:one",
        Class::Change,
        "change",
        "test",
    ))
    .unwrap();
    for (subject, predicate, object) in asserted() {
        txn.upsert_kg_edge(&KgEdge::new(subject, object, predicate))
            .unwrap();
    }
    txn.commit().unwrap();

    let txn = pristine.read_txn().unwrap();
    let outgoing = txn.get_kg_edges_from("urn:view:child").unwrap();
    assert_eq!(outgoing.len(), 1);
    assert_eq!(outgoing[0].kind, Predicate::ChildViewOf);
    assert_eq!(outgoing[0].to_id, "urn:view:parent");
    let ancestors = txn
        .kg_neighbors_filtered(
            "urn:view:child",
            2,
            Some(Predicate::AncestorView),
            Direction::Outgoing,
        )
        .unwrap();
    assert!(ancestors.edges.is_empty());
    let parents = txn
        .kg_neighbors_filtered(
            "urn:view:child",
            2,
            Some(Predicate::ChildViewOf),
            Direction::Outgoing,
        )
        .unwrap();
    assert_eq!(parents.edges.len(), 2);
    assert_eq!(txn.count_kg_edges().unwrap(), asserted().len());
}
