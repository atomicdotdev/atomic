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
}

impl NodeType {
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeType::Intent => "Intent",
            NodeType::AcceptanceCriterion => "AcceptanceCriterion",
            NodeType::Task => "Task",
        }
    }
}

/// Intent status value set (mirrors the doc's `IntentShape` `sh:in`).
pub const INTENT_STATUS: &[&str] = &["backlog", "todo", "in_progress", "done"];

/// Acceptance-criterion status value set.
pub const AC_STATUS: &[&str] = &["open", "met"];

/// Directive names recognized by the parser + lift (closed set).
/// Container directives wrap prose; leaf directives carry only edges.
pub const DIRECTIVE_NAMES: &[&str] = &[
    "why",                 // container: the unconstrained reason (presence enforced, content honest)
    "acceptance-criterion", // container
    "task",                // container
    "scope-in",            // container
    "scope-out",           // container
    "constraint",          // container
    "ref",                 // leaf: a typed dependency edge
    "file-ref",            // leaf: a file the task touches
];

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

/// Is this a valid intent status value?
pub fn is_known_intent_status(value: &str) -> bool {
    INTENT_STATUS.contains(&value)
}

/// Is this a valid acceptance-criterion status value?
pub fn is_known_ac_status(value: &str) -> bool {
    AC_STATUS.contains(&value)
}
