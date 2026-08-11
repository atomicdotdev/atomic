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
/// `icebox` is a terminal state (an intent reviewed and set aside — not built).
pub const INTENT_STATUS: &[&str] = &["backlog", "todo", "in_progress", "done", "icebox"];

/// Intent classification (`kind`) value set — the work taxonomy. `feature` is
/// the default (an ordinary unit of work); `review` classifies an intent whose
/// job is to review other work (it carries a `reviews` ref to what it reviews).
/// Default is `feature`, so pre-existing intents omit the key entirely and keep
/// their hashes. Extensible.
pub const INTENT_KIND: &[&str] = &["feature", "review", "bug", "chore", "remediation"];

/// Acceptance-criterion status value set. `unmet`/`met` are the official values
/// (an acceptance criterion is either met or not); the legacy `open` is still
/// accepted by [`is_known_ac_status`] so pre-existing intents keep conforming.
pub const AC_STATUS: &[&str] = &["unmet", "met"];

/// Task status value set (mirrors `TaskShape` `sh:in`). A task is either still
/// `open` or `done`; the renderer checks the box only on `done`. `unmet`/`met`
/// belong to acceptance criteria, not tasks, and are rejected here.
pub const TASK_STATUS: &[&str] = &["open", "done"];

/// Verification-record kind value set (closed). *What* was verified: an
/// automated `unit`/`integration`/`e2e`/`runtime` check, or a `manual` review.
/// A record whose `kind` is outside this set is a gate error, not a new kind the
/// system absorbs (mirrors the triage doc's `verificationKind`).
pub const VERIFICATION_KIND: &[&str] = &["unit", "integration", "manual", "e2e", "runtime"];

/// Verification-record outcome value set (closed). A record is refutable: it
/// either `pass`ed or `fail`ed — there is no ambiguous third state.
pub const OUTCOME: &[&str] = &["pass", "fail"];

/// Verification-record scope value set (closed). `ac` is a fact about a specific
/// acceptance criterion; `view` is a whole-view baseline observation.
pub const VERIFICATION_SCOPE: &[&str] = &["ac", "view"];

/// Triage finding codes (closed set). Every machine-emitted triage finding
/// carries one of these codes so downstream skins (CLI/JSON/HTML) and skills key
/// off a stable, closed taxonomy rather than free-form strings.
pub const FINDING_CODE: &[&str] = &[
    "VIEW_VERIFY_FAIL",
    "GATE_VIOLATION",
    "SCOPE_OUT_BREACH",
    "ORPHAN_CHANGE",
    "MET_AC_NO_EVIDENCE",
    "UNMET_AC_WITH_CANDIDATE",
    "BAGGAGE_DEP",
    "BLAST_UNREVIEWED",
    "STALE_TRIAGE",
    "OPEN_REMEDIATION",
    "UNREVIEWED_CHANGE",
];

/// Directive names recognized by the parser + lift (closed set).
/// Container directives wrap prose; leaf directives carry only edges.
///
/// NOTE: `lift.rs` must only branch on these names. A directive name not in
/// this list is a parse/gate error, never a new type the lift absorbs.
pub const DIRECTIVE_NAMES: &[&str] = &[
    "why", // container: the unconstrained reason (presence enforced, content honest)
    "acceptance-criterion", // container
    "task", // container
    "scope-in", // container
    "scope-out", // container
    "constraint", // container
    "ref", // leaf: a typed dependency edge
    "file-ref", // leaf: a file the task touches
    "memory", // container: carries a memory's body text
    "verification", // leaf: a typed, merkle-pinned verification record on an AC
];

/// Inline directive names (`:name[label]{attrs}` inside running prose).
///
/// Inline recognition is registry-gated the *other* way around from block
/// directives: prose is the unconstrained slot, so a colon-pattern whose name
/// is not registered here stays prose (never an error) — otherwise writing
/// `did:atomic:lee[sic]` in a reason would be a parse failure. Only registered
/// names lift; the closed-vocabulary property still holds for everything that
/// reaches the graph.
pub const INLINE_DIRECTIVE_NAMES: &[&str] = &["ref"];

/// Is this an inline directive name the system lifts from prose?
pub fn is_known_inline_directive(name: &str) -> bool {
    INLINE_DIRECTIVE_NAMES.contains(&name)
}

/// Memory kinds (closed set; mirrors `MemoryShape` `sh:in`). Exactly one.
///
/// `decision` records a durable decision→outcome (an architectural/approach
/// choice and why it was made), distinct from `lesson` (a corrective learning
/// from something that went wrong). Agents emit these at turn end via
/// `atomic memory new --kind decision`.
pub const MEMORY_KIND: &[&str] = &["constraint", "preference", "lesson", "context", "decision"];

/// One-line guidance for each memory kind, in the same order as [`MEMORY_KIND`].
///
/// The single source of truth the CLI (`atomic memory kinds`) and agent skills
/// surface so a model can read a session ledger and classify each durable
/// insight into the right kind — emitting several memories when warranted —
/// instead of being forced into one hardcoded kind.
pub const MEMORY_KIND_GUIDANCE: &[(&str, &str)] = &[
    (
        "constraint",
        "A hard rule or limit the work must respect — an invariant, boundary, or requirement that constrains future changes.",
    ),
    (
        "preference",
        "A soft or stylistic default the team leans toward — a convention, not a hard rule.",
    ),
    (
        "lesson",
        "A corrective learning from something that went wrong or surprised you — the failure and the takeaway.",
    ),
    (
        "context",
        "Durable background or domain knowledge that explains how or why something is — not a rule, not a decision.",
    ),
    (
        "decision",
        "A deliberate choice between real options — what was chosen, why, over which alternatives, and the outcome.",
    ),
];

/// Memory status value set (closed; mirrors `MemoryShape` `sh:in`). Exactly one.
pub const MEMORY_STATUS: &[&str] = &["active", "superseded", "retracted"];

/// Edge (property) names that carry typed references between nodes.
pub const EDGE_NAMES: &[&str] = &[
    "motivatedBy",
    "informedBy",
    "supersedes",
    "previousRevision",
    "about",
    "derivedFrom",
    "satisfies",
    "touchesFile",
    "depends",
    "blockedBy",
    "verifiedBy",
    "evidence",
    "attributedTo",
    "remediates",
    "reviews",
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

/// Non-dependency intent→intent edges a `:::ref` may *also* declare.
///
/// `remediates` is a forward link from a remediation intent to the flawed
/// intent it fixes (a post-insert bug remediated forward — see triage.md). It
/// is an intent→intent reference like `blockedBy`, but semantically it is *not*
/// a dependency/blocker, so it is deliberately kept OUT of [`DEPENDENCY_EDGES`]
/// (the traversable dependency chain) while still being a legitimate `:::ref`.
pub const REMEDIATION_EDGES: &[&str] = &["remediates"];

/// Non-dependency intent→intent edges a `:::ref` may *also* declare, linking a
/// `review`-kind intent to the intent(s) it reviews. Like [`REMEDIATION_EDGES`]
/// these are legitimate intent→intent refs that are NOT dependencies, so they
/// are kept out of [`DEPENDENCY_EDGES`] but folded into [`is_known_ref_edge`].
pub const REVIEW_EDGES: &[&str] = &["reviews"];

/// Is this a typed edge name the system recognizes at all?
pub fn is_known_edge(name: &str) -> bool {
    EDGE_NAMES.contains(&name)
}

/// Is this a valid `:::ref` dependency edge? An out-of-subset (even if otherwise
/// known) edge is not a *dependency* edge — used to keep the dependency chain
/// semantically pure.
pub fn is_known_dependency_edge(name: &str) -> bool {
    DEPENDENCY_EDGES.contains(&name)
}

/// Is this an edge a `:::ref` leaf may declare? The closed set the lift
/// validates `edge=` against: the dependency subset plus the remediation and
/// review intent→intent edges. An edge outside this union is a lift error.
pub fn is_known_ref_edge(name: &str) -> bool {
    is_known_dependency_edge(name)
        || REMEDIATION_EDGES.contains(&name)
        || REVIEW_EDGES.contains(&name)
}

/// Is this a valid intent status value?
pub fn is_known_intent_status(value: &str) -> bool {
    INTENT_STATUS.contains(&value)
}

/// Is this a valid intent classification (`kind`) value? Unknown ⇒ gate error
/// (closed vocabulary).
pub fn is_known_intent_kind(value: &str) -> bool {
    INTENT_KIND.contains(&value)
}

/// Is this a valid acceptance-criterion status value? Accepts the official
/// `unmet`/`met` plus the deprecated legacy `open` (== `unmet`) for back-compat.
pub fn is_known_ac_status(value: &str) -> bool {
    AC_STATUS.contains(&value) || value == "open"
}

/// Is this a valid task status value? Unknown ⇒ gate error (closed vocabulary).
pub fn is_known_task_status(value: &str) -> bool {
    TASK_STATUS.contains(&value)
}

/// Is this a valid memory kind? Unknown ⇒ gate error (closed vocabulary).
pub fn is_known_memory_kind(value: &str) -> bool {
    MEMORY_KIND.contains(&value)
}

/// Is this a valid memory status value? Unknown ⇒ gate error.
pub fn is_known_memory_status(value: &str) -> bool {
    MEMORY_STATUS.contains(&value)
}

/// Is this a valid verification-record kind? Unknown ⇒ gate error.
pub fn is_known_verification_kind(value: &str) -> bool {
    VERIFICATION_KIND.contains(&value)
}

/// Is this a valid verification-record outcome? Unknown ⇒ gate error.
pub fn is_known_outcome(value: &str) -> bool {
    OUTCOME.contains(&value)
}

/// Is this a valid verification-record scope? Unknown ⇒ gate error.
pub fn is_known_verification_scope(value: &str) -> bool {
    VERIFICATION_SCOPE.contains(&value)
}

/// Is this a recognized triage finding code? Unknown ⇒ gate error.
pub fn is_known_finding_code(value: &str) -> bool {
    FINDING_CODE.contains(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_kind_guidance_matches_the_closed_set() {
        // The guidance table is the single source the CLI/skills surface; it
        // must cover exactly the closed set, in the same order — no kind may be
        // addable without a description, and no description may name an unknown
        // kind.
        let guided: Vec<&str> = MEMORY_KIND_GUIDANCE.iter().map(|(k, _)| *k).collect();
        assert_eq!(guided, MEMORY_KIND);
        for (kind, doc) in MEMORY_KIND_GUIDANCE {
            assert!(
                is_known_memory_kind(kind),
                "guidance names unknown kind {kind}"
            );
            assert!(!doc.trim().is_empty(), "kind {kind} has empty guidance");
        }
    }

    #[test]
    fn verification_vocab_accepts_members_and_rejects_unknown() {
        // Every listed member of each closed set is accepted by its predicate,
        // and a value outside the set is rejected — the closed-vocabulary
        // property the gate relies on.
        for k in VERIFICATION_KIND {
            assert!(is_known_verification_kind(k), "kind {k} should be known");
        }
        assert!(!is_known_verification_kind("smoke"));

        for o in OUTCOME {
            assert!(is_known_outcome(o), "outcome {o} should be known");
        }
        assert!(!is_known_outcome("maybe"));

        for s in VERIFICATION_SCOPE {
            assert!(is_known_verification_scope(s), "scope {s} should be known");
        }
        assert!(!is_known_verification_scope("repo"));

        for c in FINDING_CODE {
            assert!(is_known_finding_code(c), "code {c} should be known");
        }
        assert!(!is_known_finding_code("WAT"));
        // The review-coverage gate's code is the newest member (appended last).
        assert_eq!(FINDING_CODE.last(), Some(&"UNREVIEWED_CHANGE"));

        // The new edge and directive names joined their closed registries.
        assert!(is_known_edge("remediates"));
        assert!(is_known_directive("verification"));
    }
}
