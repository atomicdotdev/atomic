//! The closed vocabulary registry — the single source of truth.
//!
//! Per "Recording the Why": node types, edge types, status value sets,
//! directive names, and memory kinds come from a fixed registry. An
//! unrecognized member is a gate error, not a type the system quietly
//! absorbs. Every other layer (the lift, the directive parser, the gate
//! shapes, the templates) derives its accepted names from here so they
//! cannot diverge.
//!
//! This is the M0 stub of the registry — enough to prove the Intent round
//! trip. It grows (Memory kinds, PROV edges, full directive set) in later
//! milestones, but the closure property holds from day one.

/// Canonical node types (`@type` values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeType {
    Intent,
    AcceptanceCriterion,
    Task,
    ScopeItem,
    Constraint,
    Ref,
    Memory,
}

impl NodeType {
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeType::Intent => "Intent",
            NodeType::AcceptanceCriterion => "AcceptanceCriterion",
            NodeType::Task => "Task",
            NodeType::ScopeItem => "ScopeItem",
            NodeType::Constraint => "Constraint",
            NodeType::Ref => "Ref",
            NodeType::Memory => "Memory",
        }
    }
}

/// Intent status value set (mirrors the doc's `IntentShape` `sh:in`).
pub const INTENT_STATUS: &[&str] = &["backlog", "todo", "in_progress", "done"];

/// Acceptance-criterion status value set.
pub const AC_STATUS: &[&str] = &["open", "met"];

/// Directive names recognized by the parser + lift (closed set).
/// Container directives wrap prose; leaf directives carry only edges.
///
/// NOTE: `lift.rs` must only branch on these names. A directive name not in
/// this list is a parse/gate error, never a new type the lift absorbs.
pub const DIRECTIVE_NAMES: &[&str] = &[
    "why",                 // container: the unconstrained reason (presence enforced, content honest)
    "acceptance-criterion", // container
    "task",                // container
    "scope-in",            // container
    "scope-out",           // container
    "constraint",          // container
    "ref",                 // leaf: a typed dependency edge
    "file-ref",            // leaf: a file the task touches
    "memory",              // container: carries a memory's body text
];

/// Memory kinds (closed set; mirrors `MemoryShape` `sh:in`). Exactly one.
pub const MEMORY_KIND: &[&str] = &["constraint", "preference", "lesson", "context"];

/// Memory status value set (closed; mirrors `MemoryShape` `sh:in`). Exactly one.
pub const MEMORY_STATUS: &[&str] = &["active", "superseded", "retracted"];

/// Edge (property) names that carry typed references between nodes.
pub const EDGE_NAMES: &[&str] = &[
    "motivatedBy",
    "informedBy",
    "supersedes",
    "previousRevision",
    "about",
    "satisfies",
    "touchesFile",
    "depends",
    "blockedBy",
    "verifiedBy",
    "evidence",
    "attributedTo",
];

/// Is this a directive name the system recognizes? Unknown ⇒ gate error.
pub fn is_known_directive(name: &str) -> bool {
    DIRECTIVE_NAMES.contains(&name)
}

/// The subset of edges a `:::ref` dependency may declare. A `:::ref` names a
/// *dependency* on the traversable dependency chain, so its `edge=` is
/// restricted to these — not the full [`EDGE_NAMES`] set (a `:::ref` claiming
/// `edge=verifiedBy` or `edge=about` would put a semantically bogus edge on the
/// dependency chain the gate keeps traversable).
pub const DEPENDENCY_EDGES: &[&str] = &["depends", "blockedBy"];

/// Is this a typed edge name the system recognizes at all?
pub fn is_known_edge(name: &str) -> bool {
    EDGE_NAMES.contains(&name)
}

/// Is this a valid `:::ref` dependency edge? The lift validates `edge=` against
/// this subset — an out-of-subset (even if otherwise known) edge is a lift error.
pub fn is_known_dependency_edge(name: &str) -> bool {
    DEPENDENCY_EDGES.contains(&name)
}

/// Is this a valid intent status value?
pub fn is_known_intent_status(value: &str) -> bool {
    INTENT_STATUS.contains(&value)
}

/// Is this a valid acceptance-criterion status value?
pub fn is_known_ac_status(value: &str) -> bool {
    AC_STATUS.contains(&value)
}

/// Is this a valid memory kind? Unknown ⇒ gate error (closed vocabulary).
pub fn is_known_memory_kind(value: &str) -> bool {
    MEMORY_KIND.contains(&value)
}

/// Is this a valid memory status value? Unknown ⇒ gate error.
pub fn is_known_memory_status(value: &str) -> bool {
    MEMORY_STATUS.contains(&value)
}
