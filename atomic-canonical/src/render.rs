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
    let mut s = String::new();

    // Header: human key + title.
    s.push_str(&format!("{}  {}\n", node.human_key, node.title));
    s.push_str(&"─".repeat(48));
    s.push('\n');

    // Spine fields — status is REGENERATED here from the spine, not authored.
    s.push_str(&format!("Status:    {}\n", node.status));
    if let Some(p) = &node.priority {
        s.push_str(&format!("Priority:  {p}\n"));
    }
    if let Some(v) = &node.view {
        s.push_str(&format!("View:      {v}\n"));
    }
    if let Some(m) = &node.motivated_by {
        s.push_str(&format!("Motivated: {m}\n"));
    }
    if !node.informed_by.is_empty() {
        s.push_str(&format!("InformedBy: {}\n", node.informed_by.join(", ")));
    }

    // Why (unconstrained prose).
    if let Some(why) = &node.why {
        s.push_str("\nWhy\n");
        s.push_str(why);
        s.push('\n');
    }

    // Acceptance criteria.
    if !node.has_acceptance_criterion.is_empty() {
        s.push_str("\nAcceptance Criteria\n");
        for ac in &node.has_acceptance_criterion {
            let mark = if ac.ac_status == "met" { "x" } else { " " };
            s.push_str(&format!("  [{mark}] {}\n", ac.text));
            if ac.ac_status == "met" {
                if let (Some(by), Some(ev)) = (&ac.verified_by, &ac.evidence) {
                    s.push_str(&format!("        verified by {by} · {ev}\n"));
                }
            }
        }
    }

    // Tasks.
    if !node.has_task.is_empty() {
        s.push_str("\nTasks\n");
        for t in &node.has_task {
            let mark = if t.task_status == "done" { "x" } else { " " };
            s.push_str(&format!("  [{mark}] {}\n", t.text));
            if !t.satisfies.is_empty() {
                s.push_str(&format!("        satisfies: {}\n", t.satisfies.join(", ")));
            }
            if !t.touches_file.is_empty() {
                s.push_str(&format!("        touches: {}\n", t.touches_file.join(", ")));
            }
        }
    }

    // Scope — a pure projection of the scope sub-nodes; no new authoring site.
    if !node.has_scope_in.is_empty() {
        s.push_str("\nIn Scope\n");
        for item in &node.has_scope_in {
            s.push_str(&format!("  • {}\n", item.text));
        }
    }
    if !node.has_scope_out.is_empty() {
        s.push_str("\nOut of Scope\n");
        for item in &node.has_scope_out {
            s.push_str(&format!("  • {}\n", item.text));
        }
    }

    // Constraints — the checklist the doc describes (numbered).
    if !node.has_constraint.is_empty() {
        s.push_str("\nConstraints\n");
        for (i, c) in node.has_constraint.iter().enumerate() {
            s.push_str(&format!("  {}. {}\n", i + 1, c.text));
        }
    }

    // Dependencies — each ref rendered as 'edge -> to'.
    if !node.depends_on.is_empty() {
        s.push_str("\nDependencies\n");
        for r in &node.depends_on {
            s.push_str(&format!("  {} -> {}\n", r.edge, r.to));
        }
    }

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
