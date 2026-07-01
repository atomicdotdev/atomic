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
use crate::node::{AcceptanceCriterion, CanonicalNode, Task, CONTEXT_URL};
use crate::vocab::NodeType;

/// Lift an intent from its frontmatter spine and markdown body.
pub fn lift_intent(frontmatter: &Map<String, Value>, body: &str) -> Result<CanonicalNode> {
    let human_key = require_str(frontmatter, "id")?;
    let uid = opt_str(frontmatter, "uid").unwrap_or_else(|| slug(&human_key));
    let node_id = format!("urn:atomic:intent:{uid}");

    let directives = directive::parse(body)?;

    let mut why = None;
    let mut acs = Vec::new();
    let mut tasks = Vec::new();
    let mut ac_n = 0usize;
    let mut task_n = 0usize;

    for d in &directives {
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
            // Recognized but not lifted in M0.
            "scope-in" | "scope-out" | "constraint" | "ref" | "file-ref" => {}
            other => {
                return Err(CanonicalError::Lift(format!(
                    "directive ':{other}' has no lift rule"
                )))
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
        why,
        content_hash: None,
        attributed_to: opt_str(frontmatter, "attributedTo"),
        created_at: opt_str(frontmatter, "created_at").unwrap_or_default(),
        proof: None,
    })
}

fn lift_ac(d: &Directive, human_key: &str, n: usize) -> Result<AcceptanceCriterion> {
    let local = d
        .id
        .clone()
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
    let local = d
        .id
        .clone()
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
        Some(Value::String(s)) if !s.is_empty() => {
            s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()
        }
        _ => Vec::new(),
    }
}

/// Minimal frontmatter splitter for a full markdown document:
/// `---\n<key: value lines>\n---\n<body>`. Enough for templates and the CLI;
/// values may be quoted strings or `[a, b]` arrays. Not a full YAML parser.
pub fn parse_markdown(doc: &str) -> Result<(Map<String, Value>, String)> {
    let doc = doc.strip_prefix('\u{feff}').unwrap_or(doc);
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
