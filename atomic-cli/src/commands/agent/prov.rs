//! `atomic agent prov` — export a session's provenance as a W3C PROV graph.
//!
//! Reads the recorded session file (`.atomic/sessions/<id>.json`) and projects
//! it into PROV-DM serialized with PROV-O vocabulary in JSON-LD: the session
//! as a `prov:Activity` (used/generated/associated agent), the executor as a
//! `prov:SoftwareAgent`, and — when the session carries a managed-run stamp —
//! the `prov:actedOnBehalfOf` delegation chain executor → orchestrator →
//! person. This is the "hand the auditor one addressable subgraph" command
//! from Recording the Why, fed by real recorded data.
//!
//! `--shacl` additionally runs the tier-2 formal gate (pyshacl) over the
//! emitted graph and fails on non-conformance.

use clap::Args;
use serde_json::Value;

use atomic_canonical::prov::{provenance_graph, ManagedRunInput, ProvenanceInput};
use atomic_canonical::shacl;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Export a session's provenance as a PROV-O JSON-LD graph.
#[derive(Debug, Args)]
pub struct Prov {
    /// The session id (a file under `.atomic/sessions/`).
    session_id: String,

    /// The person (DID) the work was ultimately performed for. Omitted edges
    /// are omitted from the graph, never invented.
    #[arg(long)]
    person: Option<String>,

    /// Inputs the session used (intent/memory URNs); repeatable.
    #[arg(long = "used")]
    used: Vec<String>,

    /// Validate the emitted graph with the tier-2 SHACL gate (pyshacl).
    #[arg(long)]
    shacl: bool,
}

impl Command for Prov {
    fn run(&self) -> CliResult<()> {
        let repo_root = find_repository_root()?;
        let path = repo_root
            .join(".atomic")
            .join("sessions")
            .join(format!("{}.json", self.session_id));
        let raw = std::fs::read_to_string(&path).map_err(|e| CliError::InvalidArgument {
            message: format!("no session '{}' ({}: {e})", self.session_id, path.display()),
        })?;
        let session: Value = serde_json::from_str(&raw).map_err(|e| CliError::InvalidArgument {
            message: format!("session file {} is not valid JSON: {e}", path.display()),
        })?;

        let input = provenance_input(&session, &self.session_id, &self.used, &self.person)?;
        let graph = provenance_graph(&input);

        if self.shacl {
            if !shacl::is_available() {
                return Err(CliError::InvalidArgument {
                    message: "no SHACL engine found — install pyshacl or set ATOMIC_PYSHACL"
                        .to_string(),
                });
            }
            let report = shacl::validate_value(&graph)
                .map_err(|e| CliError::Internal(anyhow::anyhow!(e.to_string())))?;
            if !report.conforms {
                eprintln!("{}", report.report.trim());
                return Err(CliError::InvalidArgument {
                    message: "provenance graph does not conform to the SHACL shapes".to_string(),
                });
            }
            eprintln!("SHACL: conforms");
        }

        println!(
            "{}",
            serde_json::to_string_pretty(&graph)
                .expect("provenance graph serialization is infallible")
        );
        Ok(())
    }
}

/// Map a raw session file into the projection input. Reads the JSON
/// generically (not the typed `AgentSession`) so sessions written by builds
/// with or without the managed-run stamp both export — absent fields are
/// omitted edges, never errors.
fn provenance_input(
    session: &Value,
    session_id: &str,
    used: &[String],
    person: &Option<String>,
) -> CliResult<ProvenanceInput> {
    let str_of = |key: &str| -> String {
        session
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };

    let started_at = str_of("started_at");
    if started_at.is_empty() {
        return Err(CliError::InvalidArgument {
            message: format!("session '{session_id}' carries no started_at"),
        });
    }

    let managed_run = session.get("managed_run").and_then(|m| {
        Some(ManagedRunInput {
            run_id: m.get("run_id")?.as_str()?.to_string(),
            owner_agent: m.get("owner_agent")?.as_str()?.to_string(),
            owner_session_id: m
                .get("owner_session_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            work_item_id: m
                .get("work_item_id")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    });

    Ok(ProvenanceInput {
        session_id: session_id.to_string(),
        agent_name: str_of("agent_name"),
        agent_display_name: str_of("agent_display_name"),
        agent_vendor: str_of("agent_vendor"),
        model: str_of("model"),
        started_at,
        ended_at: session
            .get("ended_at")
            .and_then(Value::as_str)
            .map(str::to_string),
        view: session
            .get("view_name")
            .and_then(Value::as_str)
            .map(str::to_string),
        change_hashes: session
            .get("recorded_change_hashes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        used: used.to_vec(),
        managed_run,
        person: person.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session_json() -> Value {
        json!({
            "session_id": "prov-demo",
            "view_name": "quiet-ridge-cb69",
            "agent_name": "claude-code",
            "agent_display_name": "Claude Code",
            "agent_vendor": "anthropic",
            "model": "",
            "started_at": "2026-07-02T12:53:52.160912Z",
            "ended_at": null,
            "recorded_change_hashes": ["RCPDVOCN"],
            "managed_run": {
                "run_id": "run-9",
                "owner_agent": "sherpa",
                "owner_session_id": "sherpa-s1",
                "work_item_id": "NONA-7"
            }
        })
    }

    #[test]
    fn maps_real_session_shape_including_stamp() {
        let input = provenance_input(
            &session_json(),
            "prov-demo",
            &["urn:atomic:intent:x".to_string()],
            &Some("did:atomic:lee".to_string()),
        )
        .unwrap();
        assert_eq!(input.agent_name, "claude-code");
        assert_eq!(input.view.as_deref(), Some("quiet-ridge-cb69"));
        assert_eq!(input.change_hashes, vec!["RCPDVOCN"]);
        let run = input.managed_run.unwrap();
        assert_eq!(run.run_id, "run-9");
        assert_eq!(run.owner_agent, "sherpa");
        assert_eq!(run.work_item_id.as_deref(), Some("NONA-7"));
    }

    #[test]
    fn session_without_stamp_maps_without_run() {
        let mut s = session_json();
        s.as_object_mut().unwrap().remove("managed_run");
        let input = provenance_input(&s, "prov-demo", &[], &None).unwrap();
        assert!(input.managed_run.is_none());
        assert!(input.person.is_none());
    }

    #[test]
    fn missing_started_at_is_an_error() {
        let mut s = session_json();
        s.as_object_mut().unwrap().remove("started_at");
        assert!(provenance_input(&s, "x", &[], &None).is_err());
    }
}
