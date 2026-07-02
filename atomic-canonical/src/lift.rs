//! The lift — the typed extractor that turns the authored surface (frontmatter
//! spine + directive body) into a canonical node.
//!
//! Frontmatter keys → spine properties. Directive names → node/sub-node types.
//! Directive attributes → typed properties and edges. Directive prose → the
//! unconstrained text slot. Anything without a lift rule stays as narrative and
//! is *not* lifted — in particular the `why` prose is never schematized.
//!
//! M0 lifts: spine, `:::why`, `:::acceptance-criterion`, `:::task` (+ nested
//! `::file-ref`). `scope-in/out`, `constraint`, and `ref` are recognized by the
//! parser (closed vocabulary) but lifted in a later milestone.

use serde_json::{Map, Value};

use crate::directive::{self, Directive};
use crate::error::{CanonicalError, Result};
use crate::node::{
    AcceptanceCriterion, CanonicalNode, Constraint, Ref, ScopeItem, Task, CONTEXT_URL,
};
use crate::vocab::{self, NodeType};

/// The exact set of directive names `lift_intent` branches on. The match and
/// the closed-vocabulary test both read this const so the test cannot rot when
/// the lift gains or loses a branch (bidirectional closure). Every name here
/// must be a member of [`vocab::DIRECTIVE_NAMES`]; nothing in the registry may
/// be a name the lift cannot handle.
pub(crate) const LIFTED_INTENT_DIRECTIVES: &[&str] = &[
    "why",
    "acceptance-criterion",
    "task",
    "scope-in",
    "scope-out",
    "constraint",
    "ref",
    "file-ref",
];

/// Lift an intent from its frontmatter spine and markdown body.
pub fn lift_intent(frontmatter: &Map<String, Value>, body: &str) -> Result<CanonicalNode> {
    let human_key = require_str(frontmatter, "id")?;
    let uid = opt_str(frontmatter, "uid").unwrap_or_else(|| slug(&human_key));
    let node_id = format!("urn:atomic:intent:{uid}");

    let directives = directive::parse(body)?;

    let mut why = None;
    let mut acs = Vec::new();
    let mut tasks = Vec::new();
    let mut scope_in = Vec::new();
    let mut scope_out = Vec::new();
    let mut constraints = Vec::new();
    let mut deps = Vec::new();
    let mut ac_n = 0usize;
    let mut task_n = 0usize;
    let mut scope_in_n = 0usize;
    let mut scope_out_n = 0usize;
    let mut constraint_n = 0usize;

    for d in &directives {
        // Inline (and nested-leaf) `:ref` children of any container sit on the
        // intent's dependency chain, same rules as a top-level `:::ref`. The
        // container prose keeps the inline mention verbatim.
        for child in &d.children {
            if child.name == "ref" {
                deps.push(lift_ref(child)?);
            }
        }
        match d.name.as_str() {
            "why" => {
                // Single authoring site: first `:::why` wins; the reason is
                // unconstrained prose, lifted verbatim, never inspected.
                if why.is_none() {
                    why = Some(d.body.clone());
                }
            }
            "acceptance-criterion" => {
                ac_n += 1;
                acs.push(lift_ac(d, &human_key, ac_n)?);
            }
            "task" => {
                task_n += 1;
                tasks.push(lift_task(d, &human_key, task_n));
            }
            "scope-in" => {
                scope_in_n += 1;
                scope_in.push(lift_scope(d, &human_key, "scope-in", scope_in_n));
            }
            "scope-out" => {
                scope_out_n += 1;
                scope_out.push(lift_scope(d, &human_key, "scope-out", scope_out_n));
            }
            "constraint" => {
                constraint_n += 1;
                constraints.push(lift_constraint(d, &human_key, constraint_n));
            }
            "ref" => {
                deps.push(lift_ref(d)?);
            }
            // `file-ref` is nested-only (consumed inside `task`); at top level
            // it carries no meaning, so it is a recognized no-op.
            "file-ref" => {}
            other => {
                debug_assert!(
                    !LIFTED_INTENT_DIRECTIVES.contains(&other),
                    "'{other}' is in LIFTED_INTENT_DIRECTIVES but has no match arm"
                );
                return Err(CanonicalError::Lift(format!(
                    "directive ':{other}' has no lift rule (lift handles: {})",
                    LIFTED_INTENT_DIRECTIVES.join(", ")
                )));
            }
        }
    }

    Ok(CanonicalNode {
        context: CONTEXT_URL.to_string(),
        type_: NodeType::Intent.as_str().to_string(),
        id: node_id,
        human_key,
        title: opt_str(frontmatter, "title").unwrap_or_default(),
        status: opt_str(frontmatter, "status").unwrap_or_else(|| "backlog".to_string()),
        priority: opt_str(frontmatter, "priority"),
        view: opt_str(frontmatter, "view"),
        motivated_by: opt_str(frontmatter, "motivatedBy"),
        informed_by: str_array(frontmatter, "informedBy"),
        has_acceptance_criterion: acs,
        has_task: tasks,
        has_scope_in: scope_in,
        has_scope_out: scope_out,
        has_constraint: constraints,
        depends_on: deps,
        why,
        content_hash: None,
        attributed_to: opt_str(frontmatter, "attributedTo"),
        created_at: opt_str(frontmatter, "created_at").unwrap_or_default(),
        proof: None,
    })
}

fn lift_ac(d: &Directive, human_key: &str, n: usize) -> Result<AcceptanceCriterion> {
    let local =
        d.id.clone()
            .unwrap_or_else(|| format!("{}-ac-{n}", slug(human_key)));
    Ok(AcceptanceCriterion {
        type_: NodeType::AcceptanceCriterion.as_str().to_string(),
        id: as_urn("ac", &local),
        text: d.body.clone(),
        ac_status: d.attr("status").unwrap_or("open").to_string(),
        verified_by: d.attr("verifiedBy").map(str::to_string),
        evidence: d.attr("evidence").map(str::to_string),
    })
}

fn lift_task(d: &Directive, human_key: &str, n: usize) -> Task {
    let local =
        d.id.clone()
            .unwrap_or_else(|| format!("{}-{n}", slug(human_key)));
    let mut touches: Vec<String> = Vec::new();
    if let Some(f) = d.attr("touchesFile") {
        touches.push(f.to_string());
    }
    for child in &d.children {
        if child.name == "file-ref" {
            if let Some(path) = child.attr("path") {
                touches.push(path.to_string());
            }
        }
    }
    let satisfies = d
        .attr("satisfies")
        .or_else(|| d.attr("criteria"))
        .map(|v| as_urn("ac", v));
    Task {
        type_: NodeType::Task.as_str().to_string(),
        id: as_urn("task", &local),
        text: d.body.clone(),
        task_status: d.attr("status").unwrap_or("open").to_string(),
        touches_file: touches,
        satisfies,
    }
}

/// Lift a `:::scope-in` / `:::scope-out` container into a `ScopeItem`.
/// `kind` is "scope-in" or "scope-out" and drives the generated id slug.
/// Prose body is the unconstrained narrative (never graded).
fn lift_scope(d: &Directive, human_key: &str, kind: &str, n: usize) -> ScopeItem {
    let local =
        d.id.clone()
            .unwrap_or_else(|| format!("{}-{kind}-{n}", slug(human_key)));
    ScopeItem {
        type_: NodeType::ScopeItem.as_str().to_string(),
        id: as_urn("scope", &local),
        text: d.body.clone(),
    }
}

/// Lift a `:::constraint` container into a `Constraint`.
fn lift_constraint(d: &Directive, human_key: &str, n: usize) -> Constraint {
    let local =
        d.id.clone()
            .unwrap_or_else(|| format!("{}-constraint-{n}", slug(human_key)));
    Constraint {
        type_: NodeType::Constraint.as_str().to_string(),
        id: as_urn("constraint", &local),
        text: d.body.clone(),
    }
}

/// Lift a `:::ref{to= edge=}` leaf into a typed dependency edge. Both `to` and
/// `edge` are required (a dependency with no target or no edge type is a lift
/// error), and `edge` must be a known *dependency* edge — a `:::ref` sits on the
/// dependency chain, so edges like `verifiedBy`/`about` are rejected even though
/// they are valid edges elsewhere.
fn lift_ref(d: &Directive) -> Result<Ref> {
    let to = d
        .attr("to")
        .ok_or_else(|| CanonicalError::Lift("':::ref' is missing required 'to'".into()))?
        .to_string();
    let edge = d
        .attr("edge")
        .ok_or_else(|| CanonicalError::Lift("':::ref' is missing required 'edge'".into()))?
        .to_string();
    if !vocab::is_known_dependency_edge(&edge) {
        return Err(CanonicalError::Lift(format!(
            "':::ref' edge '{edge}' is not a dependency edge {:?}",
            vocab::DEPENDENCY_EDGES
        )));
    }
    Ok(Ref {
        type_: d.id.as_ref().map(|_| NodeType::Ref.as_str().to_string()),
        id: d.id.as_ref().map(|id| as_urn("ref", id)),
        to,
        edge,
    })
}

/// Wrap a local id into `urn:atomic:<kind>:<local>` unless already a urn.
fn as_urn(kind: &str, local: &str) -> String {
    if local.starts_with("urn:atomic:") {
        local.to_string()
    } else {
        format!("urn:atomic:{kind}:{local}")
    }
}

fn slug(s: &str) -> String {
    s.to_lowercase()
}

fn require_str(fm: &Map<String, Value>, key: &str) -> Result<String> {
    opt_str(fm, key).ok_or_else(|| CanonicalError::Lift(format!("frontmatter missing '{key}'")))
}

fn opt_str(fm: &Map<String, Value>, key: &str) -> Option<String> {
    match fm.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn str_array(fm: &Map<String, Value>, key: &str) -> Vec<String> {
    match fm.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(s)) if !s.is_empty() => s
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// Minimal frontmatter splitter for a full markdown document:
/// `---\n<key: value lines>\n---\n<body>`. Enough for templates and the CLI;
/// values may be quoted strings or `[a, b]` arrays. Not a full YAML parser.
pub fn parse_markdown(doc: &str) -> Result<(Map<String, Value>, String)> {
    // Normalize CRLF so a Windows-authored file parses identically to LF —
    // otherwise the "---\n" prefix match fails and the whole doc is silently
    // treated as body with empty frontmatter.
    let normalized = doc
        .strip_prefix('\u{feff}')
        .unwrap_or(doc)
        .replace("\r\n", "\n");
    let doc = normalized.as_str();
    let rest = match doc.strip_prefix("---\n") {
        Some(r) => r,
        None => return Ok((Map::new(), doc.to_string())),
    };
    let end = rest
        .find("\n---")
        .ok_or_else(|| CanonicalError::Lift("unterminated frontmatter block".into()))?;
    let fm_src = &rest[..end];
    // body starts after the closing --- line
    let after = &rest[end + 1..]; // at "---..."
    let body = after
        .strip_prefix("---")
        .map(|b| b.trim_start_matches(['\n', '\r']).to_string())
        .unwrap_or_default();

    let mut fm = Map::new();
    for line in fm_src.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(colon) = line.find(':') {
            let key = line[..colon].trim().to_string();
            let raw = line[colon + 1..].trim();
            fm.insert(key, parse_scalar(raw));
        }
    }
    Ok((fm, body))
}

fn parse_scalar(raw: &str) -> Value {
    if raw.starts_with('[') && raw.ends_with(']') {
        let inner = &raw[1..raw.len() - 1];
        let arr: Vec<Value> = inner
            .split(',')
            .map(|s| s.trim().trim_matches(['"', '\'']).to_string())
            .filter(|s| !s.is_empty())
            .map(Value::String)
            .collect();
        return Value::Array(arr);
    }
    let unquoted = raw.trim_matches(['"', '\'']);
    Value::String(unquoted.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fm() -> Map<String, Value> {
        json!({ "id": "WORD-5", "title": "t", "status": "todo" })
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn lifts_scope_in_out_and_constraints() {
        let body = "\
:::scope-in
`src/App.tsx` state and markup.
:::

:::scope-out
Persisting the name across reloads.
:::

:::constraint
Keep it local to the existing app.
:::

:::constraint
Do not touch the global keyboard handler.
:::";
        let node = lift_intent(&fm(), body).unwrap();
        assert_eq!(node.has_scope_in.len(), 1);
        assert_eq!(node.has_scope_out.len(), 1);
        assert_eq!(node.has_constraint.len(), 2);

        // Ids are well-formed urns generated from the slug + ordinal.
        assert_eq!(
            node.has_scope_in[0].id,
            "urn:atomic:scope:word-5-scope-in-1"
        );
        assert_eq!(
            node.has_scope_out[0].id,
            "urn:atomic:scope:word-5-scope-out-1"
        );
        assert_eq!(
            node.has_constraint[0].id,
            "urn:atomic:constraint:word-5-constraint-1"
        );
        assert_eq!(
            node.has_constraint[1].id,
            "urn:atomic:constraint:word-5-constraint-2"
        );
        assert!(node.has_scope_in[0].text.contains("src/App.tsx"));
    }

    #[test]
    fn lifts_ref_dependency() {
        let body = ":::ref{to=urn:atomic:intent:xyz edge=blockedBy}\n:::";
        let node = lift_intent(&fm(), body).unwrap();
        assert_eq!(node.depends_on.len(), 1);
        assert_eq!(node.depends_on[0].to, "urn:atomic:intent:xyz");
        assert_eq!(node.depends_on[0].edge, "blockedBy");
        // No @type/@id emitted when the directive carries no #id.
        assert!(node.depends_on[0].type_.is_none());
        assert!(node.depends_on[0].id.is_none());
    }

    #[test]
    fn lifts_inline_ref_from_why_prose() {
        let body = ":::why\nLocal-only per :ref[the storage constraint]{to=urn:atomic:memory:01J8ZC edge=depends}, not a profile system.\n:::";
        let node = lift_intent(&fm(), body).unwrap();
        // The reason keeps the inline mention verbatim…
        assert!(node
            .why
            .as_deref()
            .unwrap()
            .contains(":ref[the storage constraint]"));
        // …and the edge lands on the dependency chain, typed and validated.
        assert_eq!(node.depends_on.len(), 1);
        assert_eq!(node.depends_on[0].to, "urn:atomic:memory:01J8ZC");
        assert_eq!(node.depends_on[0].edge, "depends");
    }

    #[test]
    fn inline_ref_with_bad_edge_is_error() {
        let body = ":::why\nsee :ref[x]{to=urn:atomic:memory:1 edge=verifiedBy}\n:::";
        assert!(lift_intent(&fm(), body).is_err());
    }

    #[test]
    fn ref_with_unknown_edge_is_error() {
        let body = ":::ref{to=urn:atomic:intent:xyz edge=wat}\n:::";
        assert!(lift_intent(&fm(), body).is_err());
    }

    #[test]
    fn ref_missing_to_is_error() {
        let body = ":::ref{edge=blockedBy}\n:::";
        assert!(lift_intent(&fm(), body).is_err());
    }

    #[test]
    fn ref_missing_edge_is_error() {
        let body = ":::ref{to=urn:atomic:intent:xyz}\n:::";
        assert!(lift_intent(&fm(), body).is_err());
    }

    /// Bidirectional closure over the whole directive registry: every name a
    /// lift branches on is a known directive, and every registry directive is
    /// handled by exactly one lift (the intent lift or the memory lift). This
    /// keeps the closed vocabulary and the lifts from diverging — a new
    /// directive with no lift, or a lift branch outside the registry, fails.
    #[test]
    fn every_lifted_directive_is_known() {
        use crate::memory::LIFTED_MEMORY_DIRECTIVES;

        for name in LIFTED_INTENT_DIRECTIVES {
            assert!(
                vocab::is_known_directive(name),
                "intent lift branches on '{name}' but it is not in the closed registry"
            );
        }
        for name in LIFTED_MEMORY_DIRECTIVES {
            assert!(
                vocab::is_known_directive(name),
                "memory lift branches on '{name}' but it is not in the closed registry"
            );
        }
        // The two lift sets are disjoint (a directive belongs to one node type).
        for name in LIFTED_MEMORY_DIRECTIVES {
            assert!(
                !LIFTED_INTENT_DIRECTIVES.contains(name),
                "'{name}' is claimed by both the intent lift and the memory lift"
            );
        }
        // Every registry directive is handled by exactly one lift.
        for name in vocab::DIRECTIVE_NAMES {
            let intent = LIFTED_INTENT_DIRECTIVES.contains(name);
            let memory = LIFTED_MEMORY_DIRECTIVES.contains(name);
            assert!(
                intent ^ memory,
                "registry lists '{name}' but it is handled by {} lift(s), not exactly one",
                intent as u8 + memory as u8
            );
        }
    }
}
