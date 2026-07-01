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

use crate::node::CanonicalNode;

/// Render output target. Only `Cli` is implemented in M0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Cli,
}

/// Project a canonical node to a rendered string for the given target.
pub fn render(node: &CanonicalNode, target: Target) -> String {
    match target {
        Target::Cli => render_cli(node),
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
            if !t.touches_file.is_empty() {
                s.push_str(&format!("        touches: {}\n", t.touches_file.join(", ")));
            }
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
