//! The render projection — a canonical node → a human-readable view.
//!
//! A read is a pure function of the node. Crucially, projected fields like the
//! status line are **regenerated from the spine**, never read from a body the
//! author wrote. This is the single-authoring-site principle made real: there
//! is no second place status can live, so it can never contradict the spine.
//!
//! M0 implements the CLI text target. The editor-panel and web-HTML targets and
//! reference-at-render resolution arrive in a later milestone; the `Target`
//! enum leaves room for them.

use crate::memory::MemoryNode;
use crate::node::CanonicalNode;

/// Render output target. Only `Cli` is implemented in M0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Cli,
}

/// Project a canonical Intent node to a rendered string for the given target.
/// The CLI view is one complete tree, including classification and review
/// targets, so callers do not need to append their own metadata sections.
pub fn render(node: &CanonicalNode, target: Target) -> String {
    match target {
        Target::Cli => render_cli(node),
    }
}

/// Project a canonical Memory node to a rendered string. Kept as a separate
/// entry (rather than a `Target::Memory` variant) so each `render*` stays typed
/// to the node it can actually hold — `render` cannot receive a Memory it has
/// no fields for, and this cannot receive an Intent. Same pure-projection
/// contract: no second authoring site, and the status line is regenerated from
/// the spine.
pub fn render_memory(node: &MemoryNode, target: Target) -> String {
    match target {
        Target::Cli => render_memory_cli(node),
    }
}

fn render_cli(node: &CanonicalNode) -> String {
    let mut root = Tree::new(format!("Intent {} — {}", node.human_key, node.title));
    // Status is regenerated from the canonical spine, not authored prose.
    let mut metadata = Tree::new("Metadata");
    metadata.push(format!("Status: {}", node.status));
    metadata.push(format!("Kind: {}", node.kind));
    if let Some(priority) = &node.priority {
        metadata.push(format!("Priority: {priority}"));
    }
    if let Some(view) = &node.view {
        metadata.push(format!("View: {view}"));
    }
    if let Some(motivation) = &node.motivated_by {
        metadata.push(format!("Motivated by: {motivation}"));
    }
    metadata.group(
        "Informed by",
        node.informed_by.iter().map(Tree::new).collect(),
    );
    root.children.push(metadata);

    if let Some(why) = &node.why {
        root.group(
            "Why",
            why.lines()
                .filter(|line| !line.trim().is_empty())
                .map(Tree::new)
                .collect(),
        );
    }
    root.group(
        "Acceptance Criteria",
        node.has_acceptance_criterion
            .iter()
            .map(|ac| {
                let mark = if ac.ac_status == "met" { "x" } else { " " };
                let mut criterion = Tree::new(format!("[{mark}] {}", ac.text));
                if let Some(by) = &ac.verified_by {
                    criterion.push(format!("Verified by: {by}"));
                }
                if let Some(evidence) = &ac.evidence {
                    criterion.push(format!("Evidence: {evidence}"));
                }
                criterion.group(
                    "Required checks",
                    ac.required_kinds.iter().map(Tree::new).collect(),
                );
                criterion.group(
                    "Verifications",
                    ac.verifications
                        .iter()
                        .map(|record| {
                            let mut verification = Tree::new(format!(
                                "{}: {} ({})",
                                record.kind, record.outcome, record.scope
                            ));
                            verification
                                .push(format!("Observed at Merkle: {}", record.observed_at_merkle));
                            if let Some(reference) = &record.reference {
                                verification.push(format!("Reference: {reference}"));
                            }
                            if let Some(observation) = &record.observation {
                                verification.push(format!("Observation: {observation}"));
                            }
                            verification
                        })
                        .collect(),
                );
                criterion
            })
            .collect(),
    );
    root.group(
        "Tasks",
        node.has_task
            .iter()
            .map(|task| {
                let mark = if task.task_status == "done" { "x" } else { " " };
                let mut item = Tree::new(format!("[{mark}] {}", task.text));
                // Preserve the legacy scalar/list representation; rendering is read-only.
                item.group(
                    "Satisfies",
                    task.satisfies.as_slice().iter().map(Tree::new).collect(),
                );
                item.group("Files", task.touches_file.iter().map(Tree::new).collect());
                item
            })
            .collect(),
    );
    for (label, scope) in [
        ("In Scope", &node.has_scope_in),
        ("Out of Scope", &node.has_scope_out),
    ] {
        root.group(
            label,
            scope
                .iter()
                .map(|scope| {
                    let mut item = Tree::new(&scope.text);
                    item.group("Files", scope.files.iter().map(Tree::new).collect());
                    item
                })
                .collect(),
        );
    }
    root.group(
        "Constraints",
        node.has_constraint
            .iter()
            .enumerate()
            .map(|(i, constraint)| Tree::new(format!("{}. {}", i + 1, constraint.text)))
            .collect(),
    );

    // These refs may be review, remediation or dependency relations, not all
    // dependencies. Dedicated groups avoid duplicating review targets.
    root.group(
        "Relationships",
        node.depends_on
            .iter()
            .filter(|reference| {
                !matches!(
                    reference.edge.as_str(),
                    "declaresImplementation" | "reviews"
                )
            })
            .map(|reference| Tree::new(format!("{} -> {}", reference.edge, reference.to)))
            .collect(),
    );
    let reviews: Vec<_> = node
        .depends_on
        .iter()
        .filter(|reference| reference.edge == "reviews")
        .map(|reference| Tree::new(&reference.to))
        .collect();
    if node.kind == "review" && reviews.is_empty() {
        root.push("Reviews: none declared");
    } else {
        root.group("Reviews", reviews);
    }
    root.group(
        "Declared implementation (not captured turn provenance)",
        node.depends_on
            .iter()
            .filter(|reference| reference.edge == "declaresImplementation")
            .map(|reference| {
                let mut declaration = Tree::new(&reference.to);
                declaration.push(format!(
                    "Declaration evidence: {}",
                    reference
                        .declaration_evidence
                        .as_deref()
                        .unwrap_or("missing evidence")
                ));
                declaration
            })
            .collect(),
    );

    let mut provenance = Tree::new("Provenance");
    provenance.push(format!("ID: {}", node.id));
    if let Some(author) = &node.attributed_to {
        provenance.push(format!("Author: {author}"));
    }
    if let Some(hash) = &node.content_hash {
        provenance.push(format!("Hash: {hash}"));
    }
    provenance.push(format!(
        "Signed: {}",
        if node.proof.is_some() { "yes" } else { "no" }
    ));
    root.children.push(provenance);
    root.render()
}

/// A presentation tree, never a new source of canonical relationships.
struct Tree {
    label: String,
    children: Vec<Tree>,
}

impl Tree {
    fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            children: Vec::new(),
        }
    }

    fn push(&mut self, label: impl Into<String>) {
        self.children.push(Self::new(label));
    }

    fn group(&mut self, label: &str, children: Vec<Self>) {
        if !children.is_empty() {
            self.children.push(Self {
                label: label.into(),
                children,
            });
        }
    }

    fn render(&self) -> String {
        fn clean(label: &str) -> String {
            label
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect()
        }
        let mut out = format!("{}\n", clean(&self.label));
        let mut pending: Vec<_> = self
            .children
            .iter()
            .enumerate()
            .rev()
            .map(|(i, child)| (child, String::new(), i + 1 == self.children.len()))
            .collect();
        while let Some((node, prefix, last)) = pending.pop() {
            let branch = if last { "└──" } else { "├──" };
            out.push_str(&format!("{prefix}{branch} {}\n", clean(&node.label)));
            let prefix = format!("{prefix}{}", if last { "    " } else { "│   " });
            for (i, child) in node.children.iter().enumerate().rev() {
                pending.push((child, prefix.clone(), i + 1 == node.children.len()));
            }
        }
        out
    }
}

fn render_memory_cli(node: &MemoryNode) -> String {
    let mut s = String::new();

    // Header: memory kind + @id.
    s.push_str(&format!("Memory ({})  {}\n", node.memory_kind, node.id));
    s.push_str(&"─".repeat(48));
    s.push('\n');

    // Status is REGENERATED here from the spine, never read from prose.
    s.push_str(&format!("Status:    {}\n", node.status));

    // About — the INPUT edges (modules/domains this memory is relevant to).
    if !node.about.is_empty() {
        s.push_str("\nAbout\n");
        for a in &node.about {
            s.push_str(&format!("  • {a}\n"));
        }
    }

    if !node.derived_from.is_empty() {
        s.push_str("\nDerived From\n");
        for source in &node.derived_from {
            s.push_str(&format!("  • {source}\n"));
        }
    }

    // Revision chain (only if present).
    if node.supersedes.is_some() || node.previous_revision.is_some() {
        s.push_str("\nRevision\n");
        if let Some(sup) = &node.supersedes {
            s.push_str(&format!("  supersedes:       {sup}\n"));
        }
        if let Some(prev) = &node.previous_revision {
            s.push_str(&format!("  previousRevision: {prev}\n"));
        }
    }

    // The open text body (never graded).
    s.push('\n');
    s.push_str(&node.text);
    s.push('\n');

    // Provenance footer.
    s.push('\n');
    s.push_str(&format!("id:        {}\n", node.id));
    if let Some(a) = &node.attributed_to {
        s.push_str(&format!("author:    {a}\n"));
    }
    if let Some(h) = &node.content_hash {
        s.push_str(&format!("hash:      {h}\n"));
    }
    s.push_str(&format!(
        "signed:    {}\n",
        if node.proof.is_some() { "yes" } else { "no" }
    ));

    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{default_kind, Constraint, Ref, ScopeItem, CONTEXT_URL};
    use crate::vocab::NodeType;

    fn tree_fixture() -> CanonicalNode {
        serde_json::from_value(serde_json::json!({
            "@context": CONTEXT_URL, "@type": "Intent", "@id": "urn:atomic:intent:tree",
            "humanKey": "TEST-1", "title": "A complete tree", "status": "in_progress",
            "kind": "review", "priority": "high", "view": "dev", "createdAt": "2026-10-25T00:00:00Z",
            "why": "Readable output\nKeep the evidence",
            "hasAcceptanceCriterion": [{
                "@type": "AcceptanceCriterion", "@id": "urn:atomic:ac:tree-1", "text": "Tree renders",
                "acStatus": "met", "verifiedBy": "test-tree", "evidence": "cargo test passed"
            }],
            "hasTask": [{
                "@type": "Task", "@id": "urn:atomic:task:tree-1", "text": "Render the tree",
                "taskStatus": "done", "satisfies": "urn:atomic:ac:tree-1", "touchesFile": ["src/main.rs", "test/tree.rs"]
            }],
            "hasScopeIn": [{"@type": "ScopeItem", "@id": "urn:atomic:scope:tree-in", "text": "Terminal rendering", "files": ["src/main.rs"]}],
            "hasScopeOut": [{"@type": "ScopeItem", "@id": "urn:atomic:scope:tree-out", "text": "JSON schema"}],
            "hasConstraint": [{"@type": "Constraint", "@id": "urn:atomic:constraint:tree-1", "text": "Keep signatures intact"}],
            "dependsOn": [
                {"edge": "reviews", "to": "urn:atomic:intent:work"},
                {"edge": "blockedBy", "to": "urn:atomic:intent:blocker"},
                {"edge": "declaresImplementation", "to": "urn:atomic:change:example", "declarationEvidence": "Explicit author claim"}
            ]
        })).unwrap()
    }

    #[test]
    fn intent_is_one_complete_tree_including_metadata_reviews_and_provenance() {
        let text = render(&tree_fixture(), Target::Cli);
        assert_eq!(
            text,
            concat!(
                "Intent TEST-1 — A complete tree\n",
                "├── Metadata\n",
                "│   ├── Status: in_progress\n",
                "│   ├── Kind: review\n",
                "│   ├── Priority: high\n",
                "│   └── View: dev\n",
                "├── Why\n",
                "│   ├── Readable output\n",
                "│   └── Keep the evidence\n",
                "├── Acceptance Criteria\n",
                "│   └── [x] Tree renders\n",
                "│       ├── Verified by: test-tree\n",
                "│       └── Evidence: cargo test passed\n",
                "├── Tasks\n",
                "│   └── [x] Render the tree\n",
                "│       ├── Satisfies\n",
                "│       │   └── urn:atomic:ac:tree-1\n",
                "│       └── Files\n",
                "│           ├── src/main.rs\n",
                "│           └── test/tree.rs\n",
                "├── In Scope\n",
                "│   └── Terminal rendering\n",
                "│       └── Files\n",
                "│           └── src/main.rs\n",
                "├── Out of Scope\n",
                "│   └── JSON schema\n",
                "├── Constraints\n",
                "│   └── 1. Keep signatures intact\n",
                "├── Relationships\n",
                "│   └── blockedBy -> urn:atomic:intent:blocker\n",
                "├── Reviews\n",
                "│   └── urn:atomic:intent:work\n",
                "├── Declared implementation (not captured turn provenance)\n",
                "│   └── urn:atomic:change:example\n",
                "│       └── Declaration evidence: Explicit author claim\n",
                "└── Provenance\n",
                "    ├── ID: urn:atomic:intent:tree\n",
                "    └── Signed: no\n",
            )
        );
        assert_eq!(text.matches("urn:atomic:intent:work").count(), 1);
    }

    #[test]
    fn tree_preserves_legacy_satisfies_shape_and_canonical_bytes() {
        let scalar = tree_fixture();
        let before = scalar.signing_bytes();
        let text = render(&scalar, Target::Cli);
        assert_eq!(scalar.signing_bytes(), before);
        let mut list = scalar.to_value();
        list["hasTask"][0]["satisfies"] = serde_json::json!(["urn:atomic:ac:tree-1"]);
        let list: CanonicalNode = serde_json::from_value(list).unwrap();
        assert_eq!(render(&list, Target::Cli), text);
        assert!(scalar.to_value()["hasTask"][0]["satisfies"].is_string());
        assert!(list.to_value()["hasTask"][0]["satisfies"].is_array());
    }

    #[test]
    fn empty_optional_sections_are_omitted_but_missing_review_targets_remain_explicit() {
        let mut node = tree_fixture();
        node.why = Some(" \n ".into());
        node.has_task.clear();
        node.has_acceptance_criterion.clear();
        node.has_scope_in.clear();
        node.has_scope_out.clear();
        node.has_constraint.clear();
        node.depends_on.clear();
        let text = render(&node, Target::Cli);
        for label in [
            "Why",
            "Tasks",
            "Acceptance Criteria",
            "In Scope",
            "Out of Scope",
            "Constraints",
            "Relationships",
            "Declared implementation",
        ] {
            assert!(!text.contains(label));
        }
        assert!(text.contains("├── Reviews: none declared\n└── Provenance\n"));
        node.kind = "feature".into();
        assert!(!render(&node, Target::Cli).contains("Reviews"));
    }

    #[test]
    fn verification_records_and_proof_state_are_preserved_without_tree_injection() {
        let mut node = tree_fixture();
        let criterion = &mut node.has_acceptance_criterion[0];
        criterion.required_kinds.push("unit".into());
        criterion
            .verifications
            .push(crate::node::VerificationRecord {
                type_: "VerificationRecord".into(),
                kind: "unit".into(),
                outcome: "pass".into(),
                scope: "task".into(),
                observed_at_merkle: "state-hash".into(),
                reference: Some("tree-test".into()),
                observation: Some("Checked\n\u{1b}[31m".into()),
            });
        node.attributed_to = Some("did:atomic:tester".into());
        node.content_hash = Some("content-hash".into());
        node.proof = Some(crate::node::Proof {
            type_: "DataIntegrityProof".into(),
            cryptosuite: "eddsa-jcs-2022".into(),
            verification_method: "did:atomic:tester#key".into(),
            proof_purpose: "assertionMethod".into(),
            proof_value: "test-proof".into(),
        });
        let text = render(&node, Target::Cli);
        assert!(text.contains("Required checks\n"));
        assert!(text.contains("unit: pass (task)"));
        assert!(text.contains("Observed at Merkle: state-hash"));
        assert!(text.contains("Reference: tree-test"));
        assert!(text.contains("Observation: Checked  [31m"));
        assert!(!text.contains('\u{1b}'));
        assert!(text.contains("Author: did:atomic:tester"));
        assert!(text.ends_with("    └── Signed: yes\n"));
    }

    fn scope(id: &str, text: &str) -> ScopeItem {
        ScopeItem {
            type_: NodeType::ScopeItem.as_str().to_string(),
            id: id.to_string(),
            text: text.to_string(),
            files: Vec::new(),
        }
    }

    #[test]
    fn render_includes_scope_and_constraints() {
        let node = CanonicalNode {
            context: CONTEXT_URL.to_string(),
            type_: NodeType::Intent.as_str().to_string(),
            id: "urn:atomic:intent:render-1".to_string(),
            human_key: "R-1".to_string(),
            title: "Render".to_string(),
            status: "todo".to_string(),
            kind: default_kind(),
            priority: None,
            view: None,
            motivated_by: None,
            informed_by: Vec::new(),
            has_acceptance_criterion: Vec::new(),
            has_task: Vec::new(),
            has_scope_in: vec![scope("urn:atomic:scope:r-1-scope-in-1", "the modal markup")],
            has_scope_out: vec![scope(
                "urn:atomic:scope:r-1-scope-out-1",
                "persisting across reloads",
            )],
            has_constraint: vec![
                Constraint {
                    type_: NodeType::Constraint.as_str().to_string(),
                    id: "urn:atomic:constraint:r-1-constraint-1".to_string(),
                    text: "keep it local".to_string(),
                },
                Constraint {
                    type_: NodeType::Constraint.as_str().to_string(),
                    id: "urn:atomic:constraint:r-1-constraint-2".to_string(),
                    text: "do not touch the keyboard handler".to_string(),
                },
            ],
            depends_on: vec![Ref {
                declaration_evidence: None,
                type_: None,
                id: None,
                to: "urn:atomic:intent:xyz".to_string(),
                edge: "blockedBy".to_string(),
            }],
            why: Some("a reason".to_string()),
            content_hash: None,
            attributed_to: None,
            created_at: "2026-06-25T00:00:00Z".to_string(),
            proof: None,
        };

        let text = render(&node, Target::Cli);
        assert!(text.contains("Out of Scope"), "render:\n{text}");
        assert!(text.contains("persisting across reloads"));
        assert!(text.contains("keep it local"));
        assert!(text.contains("do not touch the keyboard handler"));
        // Dependency rendered as 'edge -> to'.
        assert!(text.contains("blockedBy"));
        assert!(text.contains("blockedBy -> urn:atomic:intent:xyz"));
    }

    #[test]
    fn render_memory_regenerates_status_from_spine() {
        let node = MemoryNode {
            context: CONTEXT_URL.to_string(),
            type_: NodeType::Memory.as_str().to_string(),
            id: "urn:atomic:memory:render-1".to_string(),
            memory_kind: "constraint".to_string(),
            text: "no single ordering authority for multi-region writes".to_string(),
            about: vec![
                "urn:atomic:module:storage".to_string(),
                "urn:atomic:module:replication".to_string(),
            ],
            derived_from: vec![
                "urn:atomic:intent:render-intent".to_string(),
                "urn:atomic:change:render-change".to_string(),
            ],
            status: "active".to_string(),
            supersedes: None,
            previous_revision: None,
            content_hash: None,
            attributed_to: None,
            created_at: "2026-05-02T09:14:00Z".to_string(),
            proof: None,
        };

        let text = render_memory(&node, Target::Cli);
        // Status regenerated from the spine.
        assert!(text.contains("Status:    active"), "render:\n{text}");
        // Kind + about entries + the text body all project.
        assert!(text.contains("constraint"));
        assert!(text.contains("urn:atomic:module:storage"));
        assert!(text.contains("urn:atomic:module:replication"));
        assert!(text.contains("Derived From"));
        assert!(text.contains("urn:atomic:intent:render-intent"));
        assert!(text.contains("no single ordering authority for multi-region writes"));
    }
}
