//! The gate — a SHACL-style validator that decides whether a node may enter
//! the graph. M0 hand-codes the shapes in Rust; a real SHACL evaluator over
//! Turtle shapes arrives in a later milestone. The *semantics* are what matter
//! here and they match the doc:
//!
//!   - **Presence is enforced, content is left honest.** The gate requires the
//!     reason *exists* (a `why`), authorship is present, and the proof is
//!     present. It never grades the reason's prose.
//!   - **`status: done` is granted, not written.** A caller advancing an intent
//!     to `done` must pass the gate first.
//!   - **A met acceptance criterion must carry evidence.** `acStatus = met`
//!     without `verifiedBy` + `evidence` is rejected (a checked box with no
//!     proof fails).
//!   - **Closed world.** Unknown status values are rejected.
//!
//! The gate never auto-fixes a load-bearing fact — it only reports.

use crate::memory::MemoryNode;
use crate::node::CanonicalNode;
use crate::vocab;

/// A single shape violation.
#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    /// The `@id` of the focus node (or sub-node) that failed.
    pub focus_node: String,
    /// The shape that was violated.
    pub shape: String,
    /// The property path at fault (if any).
    pub path: Option<String>,
    /// Human-readable message.
    pub message: String,
}

/// The result of validating a node.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationReport {
    pub conforms: bool,
    pub results: Vec<Violation>,
}

impl ValidationReport {
    fn from(results: Vec<Violation>) -> Self {
        ValidationReport {
            conforms: results.is_empty(),
            results,
        }
    }
}

impl std::fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.conforms {
            return write!(f, "conforms: yes");
        }
        writeln!(f, "conforms: no ({} violation(s))", self.results.len())?;
        for v in &self.results {
            let path = v.path.as_deref().unwrap_or("-");
            writeln!(
                f,
                "  ✗ [{}] {} ({}): {}",
                v.shape, v.focus_node, path, v.message
            )?;
        }
        Ok(())
    }
}

/// Validate a canonical Intent node against `IntentShape` (+ sub-shapes).
pub fn validate_intent(node: &CanonicalNode) -> ValidationReport {
    let mut out = Vec::new();
    let focus = &node.id;

    // status ∈ closed set, exactly one (it is a single field, so cardinality holds).
    if !vocab::is_known_intent_status(&node.status) {
        out.push(Violation {
            focus_node: focus.clone(),
            shape: "IntentShape".into(),
            path: Some("status".into()),
            message: format!(
                "status '{}' is not one of {:?}",
                node.status,
                vocab::INTENT_STATUS
            ),
        });
    }

    // attributedTo must be present (sh:class prov:Agent deferred — see M0 notes).
    if node.attributed_to.as_deref().unwrap_or("").is_empty() {
        out.push(Violation {
            focus_node: focus.clone(),
            shape: "IntentShape".into(),
            path: Some("attributedTo".into()),
            message: "author (attributedTo) must be present as a DID".into(),
        });
    }

    // proof must be present.
    if node.proof.is_none() {
        out.push(Violation {
            focus_node: focus.clone(),
            shape: "IntentShape".into(),
            path: Some("proof".into()),
            message: "intent must carry a Data Integrity proof".into(),
        });
    }

    // presence-enforced: a reason must exist (content left honest).
    match &node.why {
        Some(w) if !w.trim().is_empty() => {}
        _ => out.push(Violation {
            focus_node: focus.clone(),
            shape: "IntentShape".into(),
            path: Some("why".into()),
            message: "a reason (why) must be present — its content is not graded".into(),
        }),
    }

    // Scope-out presence: if scope is being declared (scope-in present), the
    // intent must also state what is out of scope — the boundaries the agent
    // must respect. This is presence-enforced (we never read the prose). An
    // intent with no scope section at all is not forced to declare scope-out.
    if !node.has_scope_in.is_empty() && node.has_scope_out.is_empty() {
        out.push(Violation {
            focus_node: focus.clone(),
            shape: "IntentShape".into(),
            path: Some("hasScopeOut".into()),
            message:
                "a scope declaration must state what is out of scope (the boundaries the agent must respect)"
                    .into(),
        });
    }

    // Each acceptance criterion satisfies AcceptanceCriterionShape.
    for ac in &node.has_acceptance_criterion {
        if !vocab::is_known_ac_status(&ac.ac_status) {
            out.push(Violation {
                focus_node: ac.id.clone(),
                shape: "AcceptanceCriterionShape".into(),
                path: Some("acStatus".into()),
                message: format!(
                    "acStatus '{}' is not one of {:?}",
                    ac.ac_status,
                    vocab::AC_STATUS
                ),
            });
        }
        if ac.ac_status == "met" {
            let has_verifier = !ac.verified_by.as_deref().unwrap_or("").is_empty();
            let has_evidence = !ac.evidence.as_deref().unwrap_or("").is_empty();
            if !has_verifier || !has_evidence {
                out.push(Violation {
                    focus_node: ac.id.clone(),
                    shape: "AcceptanceCriterionShape".into(),
                    path: Some("acStatus".into()),
                    message: "a met acceptance criterion must carry verifiedBy and evidence".into(),
                });
            }
        }
    }

    // Referential integrity: every `satisfies` edge on a task must point at an
    // acceptance criterion that this intent actually declares. A dangling edge
    // means the task claims to fulfill a criterion that does not exist (a typo
    // or a stale reference), which would silently break the task→criterion link.
    let ac_ids: std::collections::HashSet<&str> = node
        .has_acceptance_criterion
        .iter()
        .map(|ac| ac.id.as_str())
        .collect();
    for task in &node.has_task {
        // taskStatus ∈ closed set. A stray value like `unmet` (acceptance-criterion
        // vocabulary) would otherwise pass silently and never check the box.
        if !vocab::is_known_task_status(&task.task_status) {
            out.push(Violation {
                focus_node: task.id.clone(),
                shape: "TaskShape".into(),
                path: Some("taskStatus".into()),
                message: format!(
                    "taskStatus '{}' is not one of {:?}",
                    task.task_status,
                    vocab::TASK_STATUS
                ),
            });
        }
        // `as_slice` so the gate holds a legacy scalar `satisfies` to the same
        // rule as a list — a dangling criterion reference is a violation either way.
        for target in task.satisfies.as_slice() {
            if !ac_ids.contains(target.as_str()) {
                out.push(Violation {
                    focus_node: task.id.clone(),
                    shape: "TaskShape".into(),
                    path: Some("satisfies".into()),
                    message: format!(
                        "task satisfies '{target}' which is not an acceptance criterion on this intent"
                    ),
                });
            }
        }
    }

    // Rollup: a `done` intent asserts the work is complete, so its headline
    // status must be honest against its own checklist — every task `done` and
    // every acceptance criterion `met`. A done intent with an open task or an
    // unmet criterion is internally contradictory. Only the terminal `done`
    // status triggers this; in-flight states (todo/in_progress) may carry open
    // work by definition.
    if node.status == "done" {
        for task in &node.has_task {
            if task.task_status != "done" {
                out.push(Violation {
                    focus_node: focus.clone(),
                    shape: "IntentShape".into(),
                    path: Some("status".into()),
                    message: format!(
                        "intent status is 'done' but task '{}' is '{}' (every task must be done)",
                        task.id, task.task_status
                    ),
                });
            }
        }
        for ac in &node.has_acceptance_criterion {
            if ac.ac_status != "met" {
                out.push(Violation {
                    focus_node: focus.clone(),
                    shape: "IntentShape".into(),
                    path: Some("status".into()),
                    message: format!(
                        "intent status is 'done' but acceptance criterion '{}' is '{}' (every criterion must be met)",
                        ac.id, ac.ac_status
                    ),
                });
            }
        }
    }

    ValidationReport::from(out)
}

/// Validate a canonical Memory node against `MemoryShape`. Mirrors the doc's
/// shape: `memoryKind` and `status` are closed value sets (exactly one each),
/// `attributedTo` and `proof` must be present, and the `text` must be present
/// and non-empty. As everywhere, presence is enforced and content left honest —
/// no rule reads the memory prose.
pub fn validate_memory(node: &MemoryNode) -> ValidationReport {
    let mut out = Vec::new();
    let focus = &node.id;

    // memoryKind ∈ closed set, exactly one (single field ⇒ cardinality holds).
    if !vocab::is_known_memory_kind(&node.memory_kind) {
        out.push(Violation {
            focus_node: focus.clone(),
            shape: "MemoryShape".into(),
            path: Some("memoryKind".into()),
            message: format!(
                "memoryKind '{}' is not one of {:?}",
                node.memory_kind,
                vocab::MEMORY_KIND
            ),
        });
    }

    // status ∈ closed set, exactly one.
    if !vocab::is_known_memory_status(&node.status) {
        out.push(Violation {
            focus_node: focus.clone(),
            shape: "MemoryShape".into(),
            path: Some("status".into()),
            message: format!(
                "status '{}' is not one of {:?}",
                node.status,
                vocab::MEMORY_STATUS
            ),
        });
    }

    // attributedTo must be present (sh:class prov:Agent deferred — see M0 notes).
    if node.attributed_to.as_deref().unwrap_or("").is_empty() {
        out.push(Violation {
            focus_node: focus.clone(),
            shape: "MemoryShape".into(),
            path: Some("attributedTo".into()),
            message: "author (attributedTo) must be present as a DID".into(),
        });
    }

    // proof must be present.
    if node.proof.is_none() {
        out.push(Violation {
            focus_node: focus.clone(),
            shape: "MemoryShape".into(),
            path: Some("proof".into()),
            message: "memory must carry a Data Integrity proof".into(),
        });
    }

    // presence-enforced: the memory text must exist (content left honest).
    if node.text.trim().is_empty() {
        out.push(Violation {
            focus_node: focus.clone(),
            shape: "MemoryShape".into(),
            path: Some("text".into()),
            message: "a memory must carry text — its content is not graded".into(),
        });
    }

    // Directionality (load-bearing): `supersedes` / `previousRevision` are
    // BACKWARD edges — a NEWER memory names what it replaced. A retired
    // (superseded) memory therefore gains NO new edge; "what replaced me" is
    // found by traversing INBOUND supersedes edges from the newer active
    // memory, never by a forward pointer on the old node. So there is
    // deliberately no `superseded ⇒ supersedes present` rule here: enforcing
    // one would force the retired node to point forward at its successor,
    // which is exactly the "references its future uses" anti-pattern the doc
    // forbids (and re-introduces the symmetric-edge contradiction risk).

    ValidationReport::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{AcceptanceCriterion, ScopeItem, Task, CONTEXT_URL};
    use crate::proof;
    use crate::vocab::NodeType;
    use atomic_identity::identity::Identity;
    use atomic_identity::keypair::KeyPair;

    fn scope_item(id: &str, text: &str) -> ScopeItem {
        ScopeItem {
            type_: NodeType::ScopeItem.as_str().to_string(),
            id: id.to_string(),
            text: text.to_string(),
        }
    }

    fn ac(id: &str) -> AcceptanceCriterion {
        AcceptanceCriterion {
            type_: NodeType::AcceptanceCriterion.as_str().to_string(),
            id: id.to_string(),
            text: "a checkable outcome".to_string(),
            ac_status: "unmet".to_string(),
            verified_by: None,
            evidence: None,
        }
    }

    fn task(id: &str, satisfies: &[&str]) -> Task {
        Task {
            type_: NodeType::Task.as_str().to_string(),
            id: id.to_string(),
            text: "a work item".to_string(),
            task_status: "open".to_string(),
            touches_file: Vec::new(),
            satisfies: satisfies
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .into(),
        }
    }

    /// A minimal, fully attested Intent that conforms — the base for mutation.
    fn attested_base() -> CanonicalNode {
        let kp = KeyPair::generate();
        let id = Identity::new("lee", &kp);
        let node = CanonicalNode {
            context: CONTEXT_URL.to_string(),
            type_: NodeType::Intent.as_str().to_string(),
            id: "urn:atomic:intent:gate-1".to_string(),
            human_key: "GATE-1".to_string(),
            title: "Gate base".to_string(),
            status: "todo".to_string(),
            priority: None,
            view: None,
            motivated_by: None,
            informed_by: Vec::new(),
            has_acceptance_criterion: Vec::new(),
            has_task: Vec::new(),
            has_scope_in: Vec::new(),
            has_scope_out: Vec::new(),
            has_constraint: Vec::new(),
            depends_on: Vec::new(),
            why: Some("a reason".to_string()),
            content_hash: None,
            attributed_to: None,
            created_at: "2026-06-25T00:00:00Z".to_string(),
            proof: None,
        };
        proof::attest(node, &id, &kp)
    }

    #[test]
    fn gate_requires_scope_out_when_scope_in_present() {
        let mut node = attested_base();
        node.has_scope_in = vec![scope_item("urn:atomic:scope:gate-1-scope-in-1", "in")];

        // Scope-in declared but no scope-out → non-conforming on hasScopeOut.
        let report = validate_intent(&node);
        assert!(!report.conforms);
        assert!(report
            .results
            .iter()
            .any(|v| v.path.as_deref() == Some("hasScopeOut")));

        // Adding scope-out satisfies the rule.
        node.has_scope_out = vec![scope_item("urn:atomic:scope:gate-1-scope-out-1", "out")];
        assert!(validate_intent(&node).conforms);
    }

    #[test]
    fn gate_does_not_force_scope_out_without_scope_in() {
        // No scope section at all: a minimal intent is not forced to declare it.
        let node = attested_base();
        assert!(validate_intent(&node).conforms);
    }

    #[test]
    fn gate_accepts_task_satisfying_declared_criteria() {
        let mut node = attested_base();
        node.has_acceptance_criterion = vec![
            ac("urn:atomic:ac:gate-1-ac-1"),
            ac("urn:atomic:ac:gate-1-ac-2"),
        ];
        node.has_task = vec![task(
            "urn:atomic:task:gate-1-1",
            &["urn:atomic:ac:gate-1-ac-1", "urn:atomic:ac:gate-1-ac-2"],
        )];
        assert!(
            validate_intent(&node).conforms,
            "{}",
            validate_intent(&node)
        );
    }

    #[test]
    fn gate_rejects_task_satisfying_unknown_criterion() {
        let mut node = attested_base();
        node.has_acceptance_criterion = vec![ac("urn:atomic:ac:gate-1-ac-1")];
        // The second target does not exist on this intent.
        node.has_task = vec![task(
            "urn:atomic:task:gate-1-1",
            &["urn:atomic:ac:gate-1-ac-1", "urn:atomic:ac:gate-1-ac-9"],
        )];
        let report = validate_intent(&node);
        assert!(!report.conforms);
        assert!(report.results.iter().any(|v| v.shape == "TaskShape"
            && v.path.as_deref() == Some("satisfies")
            && v.message.contains("gate-1-ac-9")));
    }

    #[test]
    fn gate_accepts_known_task_status() {
        for status in ["open", "done"] {
            let mut node = attested_base();
            let mut t = task("urn:atomic:task:gate-1-1", &[]);
            t.task_status = status.to_string();
            node.has_task = vec![t];
            assert!(
                validate_intent(&node).conforms,
                "taskStatus '{status}' should conform"
            );
        }
    }

    #[test]
    fn gate_rejects_unknown_task_status() {
        let mut node = attested_base();
        // `unmet` is acceptance-criterion vocabulary, not a valid task status.
        let mut t = task("urn:atomic:task:gate-1-1", &[]);
        t.task_status = "unmet".to_string();
        node.has_task = vec![t];

        let report = validate_intent(&node);
        assert!(!report.conforms);
        assert!(report.results.iter().any(|v| v.shape == "TaskShape"
            && v.path.as_deref() == Some("taskStatus")
            && v.message.contains("unmet")));
    }

    #[test]
    fn gate_rejects_done_intent_with_open_task() {
        let mut node = attested_base();
        node.status = "done".to_string();
        node.has_acceptance_criterion = vec![{
            let mut a = ac("urn:atomic:ac:gate-1-ac-1");
            a.ac_status = "met".to_string();
            a.verified_by = Some("did:atomic:lee".to_string());
            a.evidence = Some("urn:atomic:change:01J8".to_string());
            a
        }];
        // Task is still open while the intent claims to be done.
        node.has_task = vec![task(
            "urn:atomic:task:gate-1-1",
            &["urn:atomic:ac:gate-1-ac-1"],
        )];

        let report = validate_intent(&node);
        assert!(!report.conforms);
        assert!(report.results.iter().any(|v| v.shape == "IntentShape"
            && v.path.as_deref() == Some("status")
            && v.message.contains("every task must be done")));
    }

    #[test]
    fn gate_rejects_done_intent_with_unmet_criterion() {
        let mut node = attested_base();
        node.status = "done".to_string();
        // Criterion is unmet while the intent claims to be done.
        node.has_acceptance_criterion = vec![ac("urn:atomic:ac:gate-1-ac-1")];

        let report = validate_intent(&node);
        assert!(!report.conforms);
        assert!(report.results.iter().any(|v| v.shape == "IntentShape"
            && v.path.as_deref() == Some("status")
            && v.message.contains("every criterion must be met")));
    }

    #[test]
    fn gate_accepts_done_intent_with_all_work_complete() {
        let mut node = attested_base();
        node.status = "done".to_string();
        node.has_acceptance_criterion = vec![{
            let mut a = ac("urn:atomic:ac:gate-1-ac-1");
            a.ac_status = "met".to_string();
            a.verified_by = Some("did:atomic:lee".to_string());
            a.evidence = Some("urn:atomic:change:01J8".to_string());
            a
        }];
        node.has_task = vec![{
            let mut t = task("urn:atomic:task:gate-1-1", &["urn:atomic:ac:gate-1-ac-1"]);
            t.task_status = "done".to_string();
            t
        }];

        assert!(
            validate_intent(&node).conforms,
            "{}",
            validate_intent(&node)
        );
    }

    /// A `met` acceptance criterion carrying the required verifier + evidence.
    fn met_ac(id: &str) -> AcceptanceCriterion {
        let mut a = ac(id);
        a.ac_status = "met".to_string();
        a.verified_by = Some("did:atomic:lee".to_string());
        a.evidence = Some("urn:atomic:change:01J8".to_string());
        a
    }

    /// A `done` task satisfying the given criteria.
    fn done_task(id: &str, satisfies: &[&str]) -> Task {
        let mut t = task(id, satisfies);
        t.task_status = "done".to_string();
        t
    }

    #[test]
    fn gate_accepts_done_intent_with_many_tasks_and_criteria_all_complete() {
        let mut node = attested_base();
        node.status = "done".to_string();
        node.has_acceptance_criterion = vec![
            met_ac("urn:atomic:ac:gate-1-ac-1"),
            met_ac("urn:atomic:ac:gate-1-ac-2"),
            met_ac("urn:atomic:ac:gate-1-ac-3"),
        ];
        node.has_task = vec![
            done_task("urn:atomic:task:gate-1-1", &["urn:atomic:ac:gate-1-ac-1"]),
            done_task(
                "urn:atomic:task:gate-1-2",
                &["urn:atomic:ac:gate-1-ac-2", "urn:atomic:ac:gate-1-ac-3"],
            ),
        ];

        assert!(
            validate_intent(&node).conforms,
            "{}",
            validate_intent(&node)
        );
    }

    #[test]
    fn gate_rejects_done_intent_when_any_of_many_children_incomplete() {
        let mut node = attested_base();
        node.status = "done".to_string();
        // Two criteria met, one still unmet.
        node.has_acceptance_criterion = vec![
            met_ac("urn:atomic:ac:gate-1-ac-1"),
            ac("urn:atomic:ac:gate-1-ac-2"), // unmet
            met_ac("urn:atomic:ac:gate-1-ac-3"),
        ];
        // Two tasks done, one still open.
        node.has_task = vec![
            done_task("urn:atomic:task:gate-1-1", &["urn:atomic:ac:gate-1-ac-1"]),
            task("urn:atomic:task:gate-1-2", &["urn:atomic:ac:gate-1-ac-2"]), // open
            done_task("urn:atomic:task:gate-1-3", &["urn:atomic:ac:gate-1-ac-3"]),
        ];

        let report = validate_intent(&node);
        assert!(!report.conforms);

        // Exactly one rollup violation per incomplete child (the open task and
        // the unmet criterion), and none for the complete ones.
        let task_violations: Vec<_> = report
            .results
            .iter()
            .filter(|v| v.message.contains("every task must be done"))
            .collect();
        assert_eq!(task_violations.len(), 1, "{report}");
        assert!(task_violations[0].message.contains("gate-1-2"));

        let ac_violations: Vec<_> = report
            .results
            .iter()
            .filter(|v| v.message.contains("every criterion must be met"))
            .collect();
        assert_eq!(ac_violations.len(), 1, "{report}");
        assert!(ac_violations[0].message.contains("gate-1-ac-2"));
    }

    /// A minimal, fully attested Memory that conforms — the base for mutation.
    fn attested_memory() -> MemoryNode {
        let kp = KeyPair::generate();
        let id = Identity::new("lee", &kp);
        let node = MemoryNode {
            context: CONTEXT_URL.to_string(),
            type_: NodeType::Memory.as_str().to_string(),
            id: "urn:atomic:memory:gate-1".to_string(),
            memory_kind: "constraint".to_string(),
            text: "a durable constraint".to_string(),
            about: vec!["urn:atomic:module:storage".to_string()],
            derived_from: Vec::new(),
            status: "active".to_string(),
            supersedes: None,
            previous_revision: None,
            content_hash: None,
            attributed_to: None,
            created_at: "2026-05-02T09:14:00Z".to_string(),
            proof: None,
        };
        proof::attest_memory(node, &id, &kp)
    }

    #[test]
    fn memory_gate_rejects_unknown_kind() {
        let mut node = attested_memory();
        node.memory_kind = "architecture".to_string();
        let report = validate_memory(&node);
        assert!(!report.conforms);
        assert!(report
            .results
            .iter()
            .any(|v| v.shape == "MemoryShape" && v.path.as_deref() == Some("memoryKind")));
    }

    #[test]
    fn memory_gate_rejects_unknown_status() {
        let mut node = attested_memory();
        node.status = "archived".to_string();
        let report = validate_memory(&node);
        assert!(!report.conforms);
        assert!(report
            .results
            .iter()
            .any(|v| v.shape == "MemoryShape" && v.path.as_deref() == Some("status")));
    }

    #[test]
    fn memory_gate_requires_proof_and_attributed_to_and_text() {
        // An un-attested, empty-text memory violates all three presence rules.
        let node = MemoryNode {
            context: CONTEXT_URL.to_string(),
            type_: NodeType::Memory.as_str().to_string(),
            id: "urn:atomic:memory:bare".to_string(),
            memory_kind: "lesson".to_string(),
            text: "   ".to_string(),
            about: Vec::new(),
            derived_from: Vec::new(),
            status: "active".to_string(),
            supersedes: None,
            previous_revision: None,
            content_hash: None,
            attributed_to: None,
            created_at: "2026-05-02T09:14:00Z".to_string(),
            proof: None,
        };
        let report = validate_memory(&node);
        assert!(!report.conforms);
        for path in ["proof", "attributedTo", "text"] {
            assert!(
                report
                    .results
                    .iter()
                    .any(|v| v.path.as_deref() == Some(path)),
                "expected a violation on {path}"
            );
        }

        // A fully attested memory with text conforms.
        assert!(validate_memory(&attested_memory()).conforms);
    }

    #[test]
    fn superseded_memory_gains_no_forward_edge() {
        // Directionality: flipping status to `superseded` must NOT require (or
        // add) a forward pointer to the successor. A first-revision memory
        // (supersedes: none) that is later retired still conforms — the retired
        // node references nothing new; the successor points back at it instead.
        let mut node = attested_memory();
        node.status = "superseded".to_string();
        node.supersedes = None;
        node.previous_revision = None;
        assert!(
            validate_memory(&node).conforms,
            "a retired memory must not be forced to name its successor"
        );
    }

    #[test]
    fn memory_serialization_has_no_intent_backedge() {
        // Directionality: a memory names its inputs (`about`) and revision chain,
        // never the intents that consume it. Assert no consuming-edge key leaks.
        let node = attested_memory();
        let value = node.to_value();
        let obj = value.as_object().unwrap();
        for key in obj.keys() {
            let k = key.to_ascii_lowercase();
            assert!(
                !k.contains("intent") && !k.contains("usedby") && !k.contains("consumedby"),
                "memory carries a consuming back-edge key: {key}"
            );
        }
        // Only the allowed memory edges appear (about here; revision chain omitted).
        assert!(obj.contains_key("about"));
        assert!(!obj.contains_key("supersedes"));
        assert!(!obj.contains_key("previousRevision"));
    }
}
