//! The closed vocabulary of the versioned Atomic workflow ontology.
//!
//! Domain/range are RDF axioms, not write-time policy checks. SHACL owns
//! conformance. External terms retain their original namespace and meaning.

use serde::{Deserialize, Serialize};

pub const NAMESPACE: &str = "https://atomic.dev/ns/workflow#";
pub const VERSION_IRI: &str = "https://atomic.dev/ns/workflow/1.0.0";
pub const TTL: &str = include_str!("../../ontology/workflow-1.0.0.ttl");
pub const PREFIXES: &[(&str, &str)] = &[
    ("atomic", NAMESPACE),
    ("prov", "http://www.w3.org/ns/prov#"),
    ("dcterms", "http://purl.org/dc/terms/"),
    ("skos", "http://www.w3.org/2004/02/skos/core#"),
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("owl", "http://www.w3.org/2002/07/owl#"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalClass {
    OwlThing,
    SkosConcept,
}

impl ExternalClass {
    pub const fn iri(self) -> &'static str {
        match self {
            Self::OwlThing => "http://www.w3.org/2002/07/owl#Thing",
            Self::SkosConcept => "http://www.w3.org/2004/02/skos/core#Concept",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClassRef {
    Atomic(Class),
    External(ExternalClass),
}

impl ClassRef {
    pub const fn iri(self) -> &'static str {
        match self {
            Self::Atomic(class) => class.iri(),
            Self::External(class) => class.iri(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Datatype {
    String,
    NonNegativeInteger,
    DateTime,
}

impl Datatype {
    pub const fn iri(self) -> &'static str {
        match self {
            Self::String => "http://www.w3.org/2001/XMLSchema#string",
            Self::NonNegativeInteger => "http://www.w3.org/2001/XMLSchema#nonNegativeInteger",
            Self::DateTime => "http://www.w3.org/2001/XMLSchema#dateTime",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Range {
    Class(ClassRef),
    Datatype(Datatype),
}

impl Range {
    pub const fn iri(self) -> &'static str {
        match self {
            Self::Class(class) => class.iri(),
            Self::Datatype(datatype) => datatype.iri(),
        }
    }
}

macro_rules! classes {
    ($($name:ident => $parent:expr),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum Class { $($name),+ }
        impl Class {
            pub const ALL: &'static [Self] = &[$(Self::$name),+];
            pub const fn iri(self) -> &'static str { match self { $(Self::$name => concat!("https://atomic.dev/ns/workflow#", stringify!($name))),+ } }
            pub const fn curie(self) -> &'static str { match self { $(Self::$name => concat!("atomic:", stringify!($name))),+ } }
            pub const fn parents(self) -> &'static [ClassRef] { match self { $(Self::$name => &[$parent]),+ } }
            pub fn from_iri(iri: &str) -> Option<Self> { Self::ALL.iter().copied().find(|class| class.iri() == iri) }
        }
    };
}

use ClassRef::{Atomic as A, External as E};
use ExternalClass::{OwlThing, SkosConcept};
classes! {
    WorkIntent => E(OwlThing), FeatureIntent => A(Class::WorkIntent),
    BugIntent => A(Class::WorkIntent), ReviewIntent => A(Class::WorkIntent),
    ManagedIntent => A(Class::WorkIntent), Goal => E(OwlThing), Memory => E(OwlThing),
    Skill => E(OwlThing), ToolResult => E(OwlThing), Task => E(OwlThing),
    AcceptanceCriterion => E(OwlThing), ScopeItem => E(OwlThing), Constraint => E(OwlThing),
    Todo => E(OwlThing), Session => E(OwlThing), SessionTurn => E(OwlThing),
    ManagedTurn => A(Class::SessionTurn), ProvenanceGraph => E(OwlThing),
    Attestation => E(OwlThing), Finding => E(OwlThing), Change => E(OwlThing),
    View => E(OwlThing), File => E(OwlThing), Identity => E(OwlThing),
    Concept => E(SkosConcept), Project => E(OwlThing), ProjectIdentity => E(OwlThing),
    VerificationStatus => E(OwlThing),
}

macro_rules! predicates {
    ($($name:ident => ($iri:literal, $curie:literal, $domain:expr, $range:expr, $inverse:expr, $super:expr)),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum Predicate { $($name),+ }
        impl Predicate {
            pub const ALL: &'static [Self] = &[$(Self::$name),+];
            pub const fn iri(self) -> &'static str { match self { $(Self::$name => $iri),+ } }
            pub const fn curie(self) -> &'static str { match self { $(Self::$name => $curie),+ } }
            pub fn is_external(self) -> bool { !self.iri().starts_with(NAMESPACE) }
            pub const fn domain(self) -> Option<ClassRef> { match self { $(Self::$name => $domain),+ } }
            pub const fn range(self) -> Option<Range> { match self { $(Self::$name => $range),+ } }
            pub const fn inverse(self) -> Option<Self> { match self { $(Self::$name => $inverse),+ } }
            pub const fn super_predicate(self) -> Option<Self> { match self { $(Self::$name => $super),+ } }
            pub fn from_iri(iri: &str) -> Option<Self> { Self::ALL.iter().copied().find(|predicate| predicate.iri() == iri) }
        }
    };
}

const fn domain(class: Class) -> Option<ClassRef> {
    Some(A(class))
}
const fn range(class: Class) -> Option<Range> {
    Some(Range::Class(A(class)))
}
use Class::*;
use Predicate::*;
predicates! {
    HasTurn => ("https://atomic.dev/ns/workflow#hasTurn", "atomic:hasTurn", domain(Session), range(SessionTurn), Some(TurnOfSession), None),
    HasPlan => ("https://atomic.dev/ns/workflow#hasPlan", "atomic:hasPlan", domain(SessionTurn), range(WorkIntent), Some(GovernsTurn), None),
    GeneratedChange => ("https://atomic.dev/ns/workflow#generatedChange", "atomic:generatedChange", domain(SessionTurn), range(Change), Some(GeneratedBy), Some(ProvGenerated)),
    InformedBy => ("https://atomic.dev/ns/workflow#informedBy", "atomic:informedBy", domain(WorkIntent), None, None, Some(ProvWasDerivedFrom)),
    ExplainedBy => ("https://atomic.dev/ns/workflow#explainedBy", "atomic:explainedBy", None, range(ProvenanceGraph), None, None),
    TurnExplainedBy => ("https://atomic.dev/ns/workflow#turnExplainedBy", "atomic:turnExplainedBy", domain(SessionTurn), range(ProvenanceGraph), None, Some(ExplainedBy)),
    ChangeExplainedBy => ("https://atomic.dev/ns/workflow#changeExplainedBy", "atomic:changeExplainedBy", domain(Change), range(ProvenanceGraph), None, Some(ExplainedBy)),
    HasTodo => ("https://atomic.dev/ns/workflow#hasTodo", "atomic:hasTodo", domain(SessionTurn), range(Todo), None, None),
    Blocks => ("https://atomic.dev/ns/workflow#blocks", "atomic:blocks", domain(WorkIntent), range(WorkIntent), Some(BlockedBy), None),
    AssignedTo => ("https://atomic.dev/ns/workflow#assignedTo", "atomic:assignedTo", domain(WorkIntent), range(Identity), None, None),
    LinkedIntent => ("https://atomic.dev/ns/workflow#linkedIntent", "atomic:linkedIntent", domain(Goal), range(WorkIntent), None, None),
    LinkedGoal => ("https://atomic.dev/ns/workflow#linkedGoal", "atomic:linkedGoal", None, range(Goal), None, None),
    References => ("https://atomic.dev/ns/workflow#references", "atomic:references", Some(E(OwlThing)), Some(Range::Class(E(OwlThing))), None, None),
    HasLabel => ("https://atomic.dev/ns/workflow#hasLabel", "atomic:hasLabel", domain(WorkIntent), Some(Range::Class(E(SkosConcept))), None, None),
    HasTask => ("https://atomic.dev/ns/workflow#hasTask", "atomic:hasTask", domain(WorkIntent), range(Task), None, None),
    HasAcceptanceCriterion => ("https://atomic.dev/ns/workflow#hasAcceptanceCriterion", "atomic:hasAcceptanceCriterion", domain(WorkIntent), range(AcceptanceCriterion), None, None),
    HasScopeIn => ("https://atomic.dev/ns/workflow#hasScopeIn", "atomic:hasScopeIn", domain(WorkIntent), range(ScopeItem), None, Some(HasScopeItem)),
    HasScopeOut => ("https://atomic.dev/ns/workflow#hasScopeOut", "atomic:hasScopeOut", domain(WorkIntent), range(ScopeItem), None, Some(HasScopeItem)),
    HasConstraint => ("https://atomic.dev/ns/workflow#hasConstraint", "atomic:hasConstraint", domain(WorkIntent), range(Constraint), None, None),
    Satisfies => ("https://atomic.dev/ns/workflow#satisfies", "atomic:satisfies", domain(Task), range(AcceptanceCriterion), None, None),
    Touches => ("https://atomic.dev/ns/workflow#touches", "atomic:touches", domain(Task), range(File), None, None),
    ScopeOutFile => ("https://atomic.dev/ns/workflow#scopeOutFile", "atomic:scopeOutFile", domain(WorkIntent), range(File), None, None),
    Remediates => ("https://atomic.dev/ns/workflow#remediates", "atomic:remediates", domain(WorkIntent), range(WorkIntent), None, None),
    Reviews => ("https://atomic.dev/ns/workflow#reviews", "atomic:reviews", domain(ReviewIntent), range(WorkIntent), Some(ReviewedBy), None),
    DirectlyDependsOnChange => ("https://atomic.dev/ns/workflow#directlyDependsOnChange", "atomic:directlyDependsOnChange", domain(Change), range(Change), None, Some(DependsOn)),
    DependsOnIntent => ("https://atomic.dev/ns/workflow#dependsOnIntent", "atomic:dependsOnIntent", domain(WorkIntent), range(WorkIntent), None, Some(DependsOn)),
    ModifiesFile => ("https://atomic.dev/ns/workflow#modifiesFile", "atomic:modifiesFile", domain(Change), range(File), None, Some(Modifies)),
    DirectlyOnView => ("https://atomic.dev/ns/workflow#directlyOnView", "atomic:directlyOnView", domain(Change), range(View), None, None),
    SessionViewContext => ("https://atomic.dev/ns/workflow#sessionViewContext", "atomic:sessionViewContext", domain(Session), range(View), None, None),
    ChildViewOf => ("https://atomic.dev/ns/workflow#childViewOf", "atomic:childViewOf", domain(View), range(View), None, None),
    VisibleFrom => ("https://atomic.dev/ns/workflow#visibleFrom", "atomic:visibleFrom", None, None, None, None),
    AuthoredBy => ("https://atomic.dev/ns/workflow#authoredBy", "atomic:authoredBy", Some(E(OwlThing)), range(Identity), None, Some(ProvWasAttributedTo)),
    CoversChange => ("https://atomic.dev/ns/workflow#coversChange", "atomic:coversChange", domain(Attestation), range(Change), Some(AttestedBy), None),
    Attests => ("https://atomic.dev/ns/workflow#attests", "atomic:attests", domain(Attestation), range(SessionTurn), None, None),
    AttestsSession => ("https://atomic.dev/ns/workflow#attestsSession", "atomic:attestsSession", domain(Attestation), range(Session), None, None),
    SignedBy => ("https://atomic.dev/ns/workflow#signedBy", "atomic:signedBy", domain(Attestation), range(Identity), None, None),
    PreviousAttestation => ("https://atomic.dev/ns/workflow#previousAttestation", "atomic:previousAttestation", domain(Attestation), range(Attestation), None, None),
    Implements => ("https://atomic.dev/ns/workflow#implements", "atomic:implements", domain(Change), range(WorkIntent), None, None),
    AttestedBy => ("https://atomic.dev/ns/workflow#attestedBy", "atomic:attestedBy", domain(Change), range(Attestation), Some(CoversChange), None),
    AboutResource => ("https://atomic.dev/ns/workflow#aboutResource", "atomic:aboutResource", domain(Finding), Some(Range::Class(E(OwlThing))), None, None),
    DependsOn => ("https://atomic.dev/ns/workflow#dependsOn", "atomic:dependsOn", None, None, None, None),
    Modifies => ("https://atomic.dev/ns/workflow#modifies", "atomic:modifies", domain(Change), None, None, None),
    HasScopeItem => ("https://atomic.dev/ns/workflow#hasScopeItem", "atomic:hasScopeItem", domain(WorkIntent), range(ScopeItem), None, None),
    AncestorView => ("https://atomic.dev/ns/workflow#ancestorView", "atomic:ancestorView", domain(View), range(View), None, None),
    TurnOfSession => ("https://atomic.dev/ns/workflow#turnOfSession", "atomic:turnOfSession", domain(SessionTurn), range(Session), Some(HasTurn), None),
    GovernsTurn => ("https://atomic.dev/ns/workflow#governsTurn", "atomic:governsTurn", domain(WorkIntent), range(SessionTurn), Some(HasPlan), None),
    GeneratedBy => ("https://atomic.dev/ns/workflow#generatedBy", "atomic:generatedBy", domain(Change), range(SessionTurn), Some(GeneratedChange), None),
    BlockedBy => ("https://atomic.dev/ns/workflow#blockedBy", "atomic:blockedBy", domain(WorkIntent), range(WorkIntent), Some(Blocks), None),
    ReviewedBy => ("https://atomic.dev/ns/workflow#reviewedBy", "atomic:reviewedBy", domain(WorkIntent), range(ReviewIntent), Some(Reviews), None),
    VerificationResult => ("https://atomic.dev/ns/workflow#verificationResult", "atomic:verificationResult", domain(Attestation), range(VerificationStatus), None, None),
    HasIdentity => ("https://atomic.dev/ns/workflow#hasIdentity", "atomic:hasIdentity", domain(Project), range(ProjectIdentity), None, None),
    Issuer => ("https://atomic.dev/ns/workflow#issuer", "atomic:issuer", domain(ProjectIdentity), range(Identity), None, None),
    Status => ("https://atomic.dev/ns/workflow#status", "atomic:status", None, Some(Range::Datatype(Datatype::String)), None, None),
    Priority => ("https://atomic.dev/ns/workflow#priority", "atomic:priority", None, Some(Range::Datatype(Datatype::String)), None, None),
    FindingCode => ("https://atomic.dev/ns/workflow#findingCode", "atomic:findingCode", domain(Finding), Some(Range::Datatype(Datatype::String)), None, None),
    ProjectId => ("https://atomic.dev/ns/workflow#projectId", "atomic:projectId", domain(ProjectIdentity), Some(Range::Datatype(Datatype::String)), None, None),
    SchemaVersion => ("https://atomic.dev/ns/workflow#schemaVersion", "atomic:schemaVersion", domain(ProjectIdentity), Some(Range::Datatype(Datatype::NonNegativeInteger)), None, None),
    TurnNumber => ("https://atomic.dev/ns/workflow#turnNumber", "atomic:turnNumber", domain(SessionTurn), Some(Range::Datatype(Datatype::NonNegativeInteger)), None, None),
    ProvWasInformedBy => ("http://www.w3.org/ns/prov#wasInformedBy", "prov:wasInformedBy", None, None, None, None),
    ProvWasDerivedFrom => ("http://www.w3.org/ns/prov#wasDerivedFrom", "prov:wasDerivedFrom", None, None, None, None),
    ProvUsed => ("http://www.w3.org/ns/prov#used", "prov:used", None, None, None, None),
    ProvGenerated => ("http://www.w3.org/ns/prov#generated", "prov:generated", None, None, None, None),
    ProvWasAttributedTo => ("http://www.w3.org/ns/prov#wasAttributedTo", "prov:wasAttributedTo", None, None, None, None),
    RdfsLabel => ("http://www.w3.org/2000/01/rdf-schema#label", "rdfs:label", None, None, None, None),
    DctermsDescription => ("http://purl.org/dc/terms/description", "dcterms:description", None, None, None, None),
    DctermsCreated => ("http://purl.org/dc/terms/created", "dcterms:created", None, None, None, None),
    DctermsModified => ("http://purl.org/dc/terms/modified", "dcterms:modified", None, None, None, None),
}

impl Predicate {
    pub const fn as_str(self) -> &'static str {
        self.iri()
    }
    pub const fn property_chain(self) -> &'static [Self] {
        match self {
            Self::Implements => &[Self::GeneratedBy, Self::HasPlan],
            Self::ChangeExplainedBy => &[Self::GeneratedBy, Self::TurnExplainedBy],
            _ => &[],
        }
    }
    pub const fn is_transitive(self) -> bool {
        matches!(self, Self::DependsOn | Self::AncestorView)
    }
}

impl std::ops::Deref for Predicate {
    type Target = str;
    fn deref(&self) -> &str {
        self.iri()
    }
}
impl std::fmt::Display for Predicate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.iri())
    }
}
impl PartialEq<&str> for Predicate {
    fn eq(&self, other: &&str) -> bool {
        self.iri() == *other
    }
}
impl PartialEq<str> for Predicate {
    fn eq(&self, other: &str) -> bool {
        self.iri() == other
    }
}
impl Serialize for Predicate {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.iri())
    }
}
impl<'de> Deserialize<'de> for Predicate {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let iri = String::deserialize(deserializer)?;
        Self::from_iri(&iri)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown canonical predicate: {iri}")))
    }
}

pub fn compact(iri: &str) -> String {
    for (prefix, namespace) in PREFIXES {
        if let Some(local) = iri.strip_prefix(namespace) {
            return format!("{prefix}:{local}");
        }
    }
    iri.to_owned()
}
