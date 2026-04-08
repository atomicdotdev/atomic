use atomic_core::change::provenance_graph as pg;

use crate::provenance::types::{EdgeKind, NodeKind};

// =============================================================================
// Kind conversion helpers
// =============================================================================

/// Convert from agent-side `NodeKind` to core `ProvenanceNodeKind`.
pub(crate) fn convert_node_kind(kind: NodeKind) -> pg::ProvenanceNodeKind {
    match kind {
        NodeKind::Goal => pg::ProvenanceNodeKind::Goal,
        NodeKind::Exploration => pg::ProvenanceNodeKind::Exploration,
        NodeKind::Decision => pg::ProvenanceNodeKind::Decision,
        NodeKind::Commitment => pg::ProvenanceNodeKind::Commitment,
        NodeKind::Verification => pg::ProvenanceNodeKind::Verification,
        NodeKind::Execution => pg::ProvenanceNodeKind::Execution,
        NodeKind::HumanGate => pg::ProvenanceNodeKind::HumanGate,
        NodeKind::PatchProposal => pg::ProvenanceNodeKind::PatchProposal,
        NodeKind::Error => pg::ProvenanceNodeKind::Error,
        NodeKind::Todo => pg::ProvenanceNodeKind::Todo,
        NodeKind::TodoStatusChange => pg::ProvenanceNodeKind::TodoStatusChange,
        NodeKind::PhaseTransition => pg::ProvenanceNodeKind::PhaseTransition,
        NodeKind::Lesson => pg::ProvenanceNodeKind::Lesson,
        NodeKind::LlmResponse => pg::ProvenanceNodeKind::LlmResponse,
        NodeKind::HumanGateResolution => pg::ProvenanceNodeKind::HumanGateResolution,
    }
}

/// Convert from agent-side `EdgeKind` to core `ProvenanceEdgeKind`.
pub(crate) fn convert_edge_kind(kind: EdgeKind) -> pg::ProvenanceEdgeKind {
    match kind {
        EdgeKind::LedTo => pg::ProvenanceEdgeKind::LedTo,
        EdgeKind::ExploredVia => pg::ProvenanceEdgeKind::ExploredVia,
        EdgeKind::CommittedVia => pg::ProvenanceEdgeKind::CommittedVia,
        EdgeKind::VerifiedBy => pg::ProvenanceEdgeKind::VerifiedBy,
        EdgeKind::BlockedBy => pg::ProvenanceEdgeKind::BlockedBy,
        EdgeKind::ResumedAfter => pg::ProvenanceEdgeKind::ResumedAfter,
        EdgeKind::FailedWith => pg::ProvenanceEdgeKind::FailedWith,
    }
}

// =============================================================================
// Free functions
// =============================================================================

/// Create a short prefix from a session ID for use in node IDs.
///
/// Takes up to the first 8 characters. If the session ID is a UUID,
/// this is the first segment before the first hyphen (or the first 8
/// chars, whichever is shorter).
pub(crate) fn make_session_prefix(session_id: &str) -> String {
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        return "s".to_string();
    }
    // Take first segment before hyphen, capped at 8 chars
    let segment = trimmed.split('-').next().unwrap_or(trimmed);
    let capped: String = segment.chars().take(8).collect();
    if capped.is_empty() {
        "s".to_string()
    } else {
        capped
    }
}

/// Truncate a prompt string for display, preserving word boundaries where possible.
pub(crate) fn truncate_prompt(prompt: &str, max_len: usize) -> String {
    let trimmed = prompt.trim();
    if trimmed.len() <= max_len {
        return trimmed.to_string();
    }

    // Try to break at a word boundary
    let truncated = &trimmed[..max_len.saturating_sub(3)];
    if let Some(last_space) = truncated.rfind(' ') {
        if last_space > max_len / 2 {
            return format!("{}...", &truncated[..last_space]);
        }
    }

    format!("{}...", truncated)
}

/// Shorten a hash for display (first 8 characters).
pub(crate) fn short_hash(hash: &str) -> &str {
    if hash.len() > 8 {
        &hash[..8]
    } else {
        hash
    }
}

/// Build the `detail` JSON for a tool-derived node.
pub(crate) fn build_tool_detail(
    kind: NodeKind,
    tool_name: &str,
    tool_input: Option<&serde_json::Value>,
    tool_output: Option<&str>,
) -> Option<serde_json::Value> {
    match kind {
        NodeKind::Exploration => {
            let path = tool_input
                .and_then(|v| {
                    v.get("path")
                        .or_else(|| v.get("file"))
                        .or_else(|| v.get("glob"))
                        .or_else(|| v.get("regex"))
                })
                .and_then(|v| v.as_str());
            path.map(|p| serde_json::json!({"tool": tool_name, "target": p}))
        }

        NodeKind::Commitment => {
            // Extract file path from tool_input — OpenCode uses "filePath" (camelCase)
            // while other agents may use "path", "file", or "file_path" (snake_case).
            // Also check the top-level "file_path" field added by the enriched plugin.
            let path = tool_input
                .and_then(|v| {
                    v.get("filePath")
                        .or_else(|| v.get("path"))
                        .or_else(|| v.get("file"))
                        .or_else(|| v.get("file_path"))
                })
                .and_then(|v| v.as_str());

            let mut detail = serde_json::json!({"tool": tool_name});

            if let Some(p) = path {
                // Store both the full path and a shortened display path
                detail["file_path"] = serde_json::Value::String(p.to_string());
                // Shorten: take last 2-3 path components for display
                let short = shorten_path(p);
                detail["file"] = serde_json::Value::String(short);
            }

            // Determine operation: create vs edit
            // "write" tool = create (new file), "edit"/"multiedit"/"patch" = edit
            let operation = match tool_name.to_lowercase().as_str() {
                "write" | "write_file" | "create" | "create_file" => "create",
                "delete_file" | "remove_file" => "delete",
                _ => "edit",
            };
            detail["operation"] = serde_json::Value::String(operation.to_string());

            // Pull in filediff from the enriched after-tool payload if present.
            // The plugin sends: { filediff: { file, before, after, additions, deletions } }
            if let Some(filediff) = tool_input.and_then(|v| v.get("filediff")).cloned() {
                detail["filediff"] = filediff;
            }

            // Pull in unified diff string
            if let Some(diff) = tool_input
                .and_then(|v| v.get("diff"))
                .and_then(|v| v.as_str())
            {
                detail["diff"] = serde_json::Value::String(diff.to_string());
            }

            // Pull in diagnostics from the enriched after-tool payload
            if let Some(diag) = tool_input.and_then(|v| v.get("diagnostics")).cloned() {
                detail["diagnostics"] = diag;
            }

            // Pull in title (human-readable description)
            if let Some(title) = tool_input
                .and_then(|v| v.get("title"))
                .and_then(|v| v.as_str())
            {
                detail["title"] = serde_json::Value::String(title.to_string());
            }

            // Check if the file existed before (write to new file vs overwrite)
            if let Some(exists) = tool_input
                .and_then(|v| v.get("exists"))
                .and_then(|v| v.as_bool())
            {
                detail["exists"] = serde_json::Value::Bool(exists);
                if !exists {
                    detail["operation"] = serde_json::Value::String("create".to_string());
                }
            }

            Some(detail)
        }

        NodeKind::Verification => {
            let cmd = tool_input
                .and_then(|v| v.get("command").or_else(|| v.get("cmd")))
                .and_then(|v| v.as_str());
            let description = tool_input
                .and_then(|v| v.get("description"))
                .and_then(|v| v.as_str());
            let exit_code = tool_input
                .and_then(|v| v.get("exit_code"))
                .and_then(|v| v.as_i64());
            let mut detail = serde_json::json!({});
            if let Some(c) = cmd {
                detail["command"] = serde_json::Value::String(c.to_string());
            }
            if let Some(d) = description {
                detail["description"] = serde_json::Value::String(d.to_string());
            }
            if let Some(code) = exit_code {
                detail["exit_code"] = serde_json::Value::Number(code.into());
            }
            // Try to determine pass/fail from exit code first, then output heuristics
            if let Some(code) = exit_code {
                detail["passed"] = serde_json::Value::Bool(code == 0);
            } else if let Some(output) = tool_output {
                let lower = output.to_lowercase();
                if lower.contains("fail") || lower.contains("error") || lower.contains("failed") {
                    detail["passed"] = serde_json::Value::Bool(false);
                } else if lower.contains("pass")
                    || lower.contains("ok")
                    || lower.contains("success")
                {
                    detail["passed"] = serde_json::Value::Bool(true);
                }
            }
            // Pull in diagnostics if present
            if let Some(diag) = tool_input.and_then(|v| v.get("diagnostics")).cloned() {
                detail["diagnostics"] = diag;
            }
            // Truncated output summary
            if let Some(output) = tool_output {
                let truncated: String = output.chars().take(300).collect();
                detail["output_summary"] = serde_json::Value::String(truncated);
            }
            Some(detail)
        }

        NodeKind::Execution => {
            let cmd = tool_input
                .and_then(|v| v.get("command").or_else(|| v.get("cmd")))
                .and_then(|v| v.as_str());
            let description = tool_input
                .and_then(|v| v.get("description"))
                .and_then(|v| v.as_str());
            let exit_code = tool_input
                .and_then(|v| v.get("exit_code"))
                .and_then(|v| v.as_i64());
            let mut detail = serde_json::json!({});
            if let Some(c) = cmd {
                detail["command"] = serde_json::Value::String(c.to_string());
            }
            if let Some(d) = description {
                detail["description"] = serde_json::Value::String(d.to_string());
            }
            if let Some(code) = exit_code {
                detail["exit_code"] = serde_json::Value::Number(code.into());
            }
            // Truncated output summary
            if let Some(output) = tool_output {
                let truncated: String = output.chars().take(300).collect();
                detail["output_summary"] = serde_json::Value::String(truncated);
            }
            if detail.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                None
            } else {
                Some(detail)
            }
        }

        NodeKind::Error => {
            let mut detail = serde_json::json!({"tool": tool_name});
            if let Some(output) = tool_output {
                // Truncate error output to keep detail manageable
                let truncated: String = output.chars().take(500).collect();
                detail["error"] = serde_json::Value::String(truncated);
            }
            Some(detail)
        }

        _ => None,
    }
}

/// Shorten a file path to the last 2-3 components for display.
///
/// `/Users/leefaus/Projects/hello-world/src/index.ts` → `src/index.ts`
/// `/Users/leefaus/Projects/hello-world/tsconfig.json` → `tsconfig.json`
fn shorten_path(full_path: &str) -> String {
    let parts: Vec<&str> = full_path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 2 {
        return parts.join("/");
    }

    // Look for common project markers to find the project-relative path
    let markers = ["src", "lib", "app", "test", "tests", "dist", "pkg", "cmd"];
    for (i, part) in parts.iter().enumerate() {
        if markers.contains(&part.to_lowercase().as_str()) {
            return parts[i..].join("/");
        }
    }

    // Fall back to the last 2 segments
    parts[parts.len() - 2..].join("/")
}
