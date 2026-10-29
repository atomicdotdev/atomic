//! Frozen pre-cutover KG vocabulary inventory. These are dispositions, not aliases.
//! Projectors classify authoritative data; legacy strings are never accepted by
//! the canonical write API.

use super::ontology::{Class, Predicate};

#[derive(Clone, Copy, Debug)]
pub enum Target {
    Class(Class),
    Predicate(Predicate),
    Removed { reason: &'static str },
}

#[derive(Clone, Copy, Debug)]
pub struct EndpointRule {
    pub from_kind: Option<&'static str>,
    pub to_kind: Option<&'static str>,
    pub target: Target,
}

#[derive(Clone, Copy, Debug)]
pub enum Replacement {
    One(Target),
    ClassWithRefinement {
        base: Class,
        refinements: &'static [Class],
        rule: &'static str,
    },
    SplitByEndpoint(&'static [EndpointRule]),
}

use Class as C;
use Predicate as P;
use Replacement::{ClassWithRefinement, One, SplitByEndpoint};
const fn class(class: Class) -> Replacement {
    One(Target::Class(class))
}
const fn predicate(predicate: Predicate) -> Replacement {
    One(Target::Predicate(predicate))
}
const fn removed(reason: &'static str) -> Replacement {
    One(Target::Removed { reason })
}
const fn endpoint(from: &'static str, to: &'static str, predicate: Predicate) -> EndpointRule {
    EndpointRule {
        from_kind: Some(from),
        to_kind: Some(to),
        target: Target::Predicate(predicate),
    }
}
const INTENT: Replacement = ClassWithRefinement {
    base: C::WorkIntent,
    refinements: &[
        C::FeatureIntent,
        C::BugIntent,
        C::ReviewIntent,
        C::ManagedIntent,
    ],
    rule: "Canonical kind chooses feature/bug/review; active lifecycle chooses ManagedIntent",
};
const TURN: Replacement = ClassWithRefinement {
    base: C::SessionTurn,
    refinements: &[C::ManagedTurn],
    rule: "ManagedTurn iff plan_id is present",
};

pub static LEGACY_TERMS: &[(&str, Replacement)] = &[
    (
        "atom",
        removed("Prefix replaced by the absolute PREFIXES registry"),
    ),
    (
        "prov",
        removed("Prefix replaced by the absolute PREFIXES registry"),
    ),
    (
        "skos",
        removed("Prefix replaced by the absolute PREFIXES registry"),
    ),
    ("dc", removed("Use dcterms for source metadata")),
    ("vault", removed("Workflow terms use the atomic namespace")),
    (
        "rdf",
        removed("Prefix replaced by the absolute PREFIXES registry"),
    ),
    ("prov:wasAssociatedWith", predicate(P::LinkedGoal)),
    ("prov:used", predicate(P::ProvUsed)),
    ("prov:wasGeneratedBy", removed("Never emitted")),
    ("prov:wasInformedBy", predicate(P::ProvWasInformedBy)),
    ("prov:wasDerivedFrom", predicate(P::ProvWasDerivedFrom)),
    ("atom:informedBy", predicate(P::InformedBy)),
    (
        "prov:hadPlan",
        SplitByEndpoint(&[
            endpoint("session_turn", "intent", P::HasPlan),
            EndpointRule {
                from_kind: Some("session"),
                to_kind: Some("intent"),
                target: Target::Removed {
                    reason: "Use session hasTurn / turn hasPlan",
                },
            },
        ]),
    ),
    ("prov:hadMember", predicate(P::HasTurn)),
    ("prov:generated", predicate(P::GeneratedChange)),
    ("skos:broader", removed("Never emitted")),
    ("skos:narrower", removed("Never emitted")),
    ("skos:subject", removed("Never emitted")),
    ("skos:related", removed("Never emitted")),
    ("dc:type", removed("Never emitted")),
    (
        "dc:creator",
        removed("Derive authoredBy from typed source fields"),
    ),
    (
        "dc:date",
        removed("Derive dcterms timestamps from typed source fields"),
    ),
    (
        "dc:description",
        removed("Derive dcterms description from typed source fields"),
    ),
    (
        "rdf:type",
        removed("Class assertions use kg_types, not neighborhood edges"),
    ),
    (
        "vault:blocks",
        removed("Unused duplicate of uppercase edge"),
    ),
    (
        "vault:blockedBy",
        removed("Unused duplicate of uppercase edge"),
    ),
    ("vault:status", predicate(P::Status)),
    (
        "vault:assignedTo",
        removed("Unused duplicate of uppercase edge"),
    ),
    (
        "vault:linkedIntent",
        removed("Unused duplicate of uppercase edge"),
    ),
    (
        "vault:linkedGoal",
        removed("Unused duplicate of uppercase edge"),
    ),
    (
        "vault:references",
        removed("Unused duplicate of uppercase edge"),
    ),
    ("vault:appliesTo", removed("Never emitted")),
    (
        "vault:hasLabel",
        removed("Unused duplicate of uppercase edge"),
    ),
    ("vault:priority", predicate(P::Priority)),
    ("vault:explainedBy", predicate(P::TurnExplainedBy)),
    ("vault:hasTodo", predicate(P::HasTodo)),
    ("vault:Goal", class(C::Goal)),
    ("vault:Intent", INTENT),
    ("vault:Memory", class(C::Memory)),
    ("vault:Skill", class(C::Skill)),
    ("vault:ToolResult", class(C::ToolResult)),
    ("vault:File", class(C::File)),
    ("vault:Identity", class(C::Identity)),
    ("vault:Concept", class(C::Concept)),
    ("vault:Session", class(C::Session)),
    ("vault:SessionTurn", TURN),
    ("vault:Todo", class(C::Todo)),
    ("vault:ProvenanceGraph", class(C::ProvenanceGraph)),
    ("vault:Task", class(C::Task)),
    ("vault:AcceptanceCriterion", class(C::AcceptanceCriterion)),
    ("vault:ScopeItem", class(C::ScopeItem)),
    ("vault:Constraint", class(C::Constraint)),
    (
        "MODIFIES",
        SplitByEndpoint(&[
            endpoint("change", "file", P::ModifiesFile),
            EndpointRule {
                from_kind: Some("change"),
                to_kind: Some("entity"),
                target: Target::Removed {
                    reason: "Source entities stay in the syntax index",
                },
            },
        ]),
    ),
    ("DEFINES", removed("Code structure is parsed live")),
    (
        "DEPENDS_ON",
        SplitByEndpoint(&[
            endpoint("change", "change", P::DirectlyDependsOnChange),
            endpoint("intent", "intent", P::DependsOnIntent),
        ]),
    ),
    (
        "ON_VIEW",
        SplitByEndpoint(&[
            endpoint("change", "view", P::DirectlyOnView),
            endpoint("session", "view", P::SessionViewContext),
        ]),
    ),
    ("CHILD_OF", predicate(P::ChildViewOf)),
    ("VISIBLE_FROM", predicate(P::VisibleFrom)),
    ("CALLS", removed("Code structure is parsed live")),
    ("IMPORTS", removed("Code structure is parsed live")),
    ("EXPORTS", removed("Code structure is parsed live")),
    ("REFERENCES", predicate(P::References)),
    ("LINKED_INTENT", predicate(P::LinkedIntent)),
    ("LINKED_GOAL", predicate(P::LinkedGoal)),
    ("ASSIGNED_TO", predicate(P::AssignedTo)),
    ("BLOCKS", predicate(P::Blocks)),
    ("BLOCKED_BY", predicate(P::BlockedBy)),
    ("AUTHORED_BY", predicate(P::AuthoredBy)),
    ("HAS_LABEL", predicate(P::HasLabel)),
    ("PART_OF", removed("Module hierarchy is derived from paths")),
    ("INCLUDES", removed("Code structure is parsed live")),
    ("HAS_TASK", predicate(P::HasTask)),
    (
        "HAS_ACCEPTANCE_CRITERION",
        predicate(P::HasAcceptanceCriterion),
    ),
    ("HAS_SCOPE_IN", predicate(P::HasScopeIn)),
    ("HAS_SCOPE_OUT", predicate(P::HasScopeOut)),
    ("HAS_CONSTRAINT", predicate(P::HasConstraint)),
    ("SATISFIES", predicate(P::Satisfies)),
    ("TOUCHES", predicate(P::Touches)),
    ("REMEDIATES", predicate(P::Remediates)),
    ("REVIEWS", predicate(P::Reviews)),
    ("SCOPE_OUT_FILE", predicate(P::ScopeOutFile)),
    ("goal", class(C::Goal)),
    ("intent", INTENT),
    ("memory", class(C::Memory)),
    ("skill", class(C::Skill)),
    ("tool_result", class(C::ToolResult)),
    ("file", class(C::File)),
    ("identity", class(C::Identity)),
    ("concept", class(C::Concept)),
    ("session", class(C::Session)),
    ("session_turn", TURN),
    ("todo", class(C::Todo)),
    ("provenance", class(C::ProvenanceGraph)),
    ("task", class(C::Task)),
    ("acceptance_criterion", class(C::AcceptanceCriterion)),
    ("scope", class(C::ScopeItem)),
    ("constraint", class(C::Constraint)),
    ("change", class(C::Change)),
    ("view", class(C::View)),
    ("module", removed("Module hierarchy is derived from paths")),
    (
        "entity",
        removed("Source entities stay in the syntax index"),
    ),
];
