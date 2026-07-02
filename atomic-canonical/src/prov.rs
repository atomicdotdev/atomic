//! W3C PROV projection — the provenance graph as PROV-DM, serialized with
//! PROV-O vocabulary in JSON-LD.
//!
//! Per "Recording the Why": every agent action is a `prov:Activity`,
//! associated with a `prov:SoftwareAgent` that `actedOnBehalfOf` an
//! orchestrator and ultimately a `prov:Person`. The graph for a session is a
//! *named graph* — one addressable unit you can hand to an auditor whole.
//!
//! PROV-DM mapping:
//! - a recorded agent **session** is a `prov:Activity` (started/ended times,
//!   `prov:used` inputs, `prov:generated` change entities,
//!   `prov:wasAssociatedWith` its agent);
//! - the **executor** is a `prov:SoftwareAgent`;
//! - a **managed run** (Sherpa's `lifecycle begin`, carried on the session as
//!   the `managed_run` stamp) is itself a `prov:Activity` associated with the
//!   orchestrator agent, and the delegation chain is expressed with
//!   `prov:actedOnBehalfOf` (Agent → Agent): executor → orchestrator → person;
//! - the **person** is a `prov:Person` identified by DID.
//!
//! `atom:partOfRun` links the session activity to the run activity — PROV-DM
//! has no standard sub-activity relation, so this stays a namespaced term
//! rather than overloading `prov:wasInformedBy` (which means communication,
//! not containment).
//!
//! This module is a projection (a compile target): it takes plain input data
//! — in production, an `AgentSession` file — and emits the JSON-LD value.
//! Nothing here is hand-authored.

use serde_json::{json, Map, Value};

use crate::node::CONTEXT_URL;

/// The managed-run stamp carried by a session recorded under an orchestrator
/// (mirrors `atomic-agent`'s `ManagedRunStamp`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedRunInput {
    pub run_id: String,
    pub owner_agent: String,
    pub owner_session_id: String,
    pub work_item_id: Option<String>,
}

/// Everything the projection needs about one recorded session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenanceInput {
    pub session_id: String,
    pub agent_name: String,
    pub agent_display_name: String,
    pub agent_vendor: String,
    pub model: String,
    /// RFC 3339 timestamps.
    pub started_at: String,
    pub ended_at: Option<String>,
    /// The view the session recorded onto.
    pub view: Option<String>,
    /// Base32 change hashes recorded by the session (become
    /// `urn:atomic:change:<hash>` entities).
    pub change_hashes: Vec<String>,
    /// Inputs the activity `prov:used` (intent/memory URNs), when known.
    pub used: Vec<String>,
    pub managed_run: Option<ManagedRunInput>,
    /// The person the work was ultimately performed for, as a DID.
    pub person: Option<String>,
}

/// IRI builders — one place so the graph and its consumers cannot drift.
pub fn session_activity_iri(session_id: &str) -> String {
    format!("urn:atomic:activity:session:{session_id}")
}
pub fn agent_iri(agent_name: &str) -> String {
    format!("urn:atomic:agent:{agent_name}")
}
pub fn run_activity_iri(run_id: &str) -> String {
    format!("urn:atomic:run:{run_id}")
}
pub fn change_iri(hash: &str) -> String {
    format!("urn:atomic:change:{hash}")
}
pub fn graph_iri(session_id: &str) -> String {
    format!("urn:atomic:provgraph:session:{session_id}")
}

/// Project a session into its PROV-O named graph.
pub fn provenance_graph(input: &ProvenanceInput) -> Value {
    let mut graph: Vec<Value> = Vec::new();

    // The session activity.
    let mut activity = Map::new();
    activity.insert("@type".into(), json!("Activity"));
    activity.insert("@id".into(), json!(session_activity_iri(&input.session_id)));
    activity.insert("sessionId".into(), json!(input.session_id));
    activity.insert("startedAtTime".into(), json!(input.started_at));
    if let Some(ended) = &input.ended_at {
        activity.insert("endedAtTime".into(), json!(ended));
    }
    activity.insert(
        "wasAssociatedWith".into(),
        json!(agent_iri(&input.agent_name)),
    );
    if !input.used.is_empty() {
        activity.insert("used".into(), json!(input.used));
    }
    if !input.change_hashes.is_empty() {
        let changes: Vec<String> = input.change_hashes.iter().map(|h| change_iri(h)).collect();
        activity.insert("generated".into(), json!(changes));
    }
    if let Some(view) = &input.view {
        activity.insert("view".into(), json!(view));
    }
    if let Some(run) = &input.managed_run {
        activity.insert("partOfRun".into(), json!(run_activity_iri(&run.run_id)));
    }
    graph.push(Value::Object(activity));

    // The executor agent, with its delegation edge.
    let mut executor = Map::new();
    executor.insert("@type".into(), json!("SoftwareAgent"));
    executor.insert("@id".into(), json!(agent_iri(&input.agent_name)));
    executor.insert("agentName".into(), json!(input.agent_name));
    if !input.agent_display_name.is_empty() {
        executor.insert("agentDisplayName".into(), json!(input.agent_display_name));
    }
    if !input.agent_vendor.is_empty() {
        executor.insert("vendor".into(), json!(input.agent_vendor));
    }
    if !input.model.is_empty() {
        executor.insert("model".into(), json!(input.model));
    }
    match (&input.managed_run, &input.person) {
        // Managed: executor acted on behalf of the orchestrator.
        (Some(run), _) => {
            executor.insert("actedOnBehalfOf".into(), json!(agent_iri(&run.owner_agent)));
        }
        // Direct with a known person: executor acted on behalf of them.
        (None, Some(person)) => {
            executor.insert("actedOnBehalfOf".into(), json!(person));
        }
        (None, None) => {}
    }
    graph.push(Value::Object(executor));

    // The managed run: an orchestration activity + the orchestrator agent.
    if let Some(run) = &input.managed_run {
        let mut run_activity = Map::new();
        run_activity.insert("@type".into(), json!("Activity"));
        run_activity.insert("@id".into(), json!(run_activity_iri(&run.run_id)));
        run_activity.insert("runId".into(), json!(run.run_id));
        run_activity.insert("ownerSessionId".into(), json!(run.owner_session_id));
        run_activity.insert(
            "wasAssociatedWith".into(),
            json!(agent_iri(&run.owner_agent)),
        );
        if let Some(work_item) = &run.work_item_id {
            run_activity.insert("workItem".into(), json!(work_item));
        }
        graph.push(Value::Object(run_activity));

        let mut owner = Map::new();
        owner.insert("@type".into(), json!("SoftwareAgent"));
        owner.insert("@id".into(), json!(agent_iri(&run.owner_agent)));
        owner.insert("agentName".into(), json!(run.owner_agent));
        if let Some(person) = &input.person {
            owner.insert("actedOnBehalfOf".into(), json!(person));
        }
        graph.push(Value::Object(owner));
    }

    // The person.
    if let Some(person) = &input.person {
        graph.push(json!({ "@type": "Person", "@id": person }));
    }

    json!({
        "@context": CONTEXT_URL,
        "@id": graph_iri(&input.session_id),
        "@graph": graph,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn managed_input() -> ProvenanceInput {
        ProvenanceInput {
            session_id: "inner-1".into(),
            agent_name: "claude-code".into(),
            agent_display_name: "Claude Code".into(),
            agent_vendor: "anthropic".into(),
            model: "claude-fable-5".into(),
            started_at: "2026-07-01T22:11:00Z".into(),
            ended_at: Some("2026-07-01T22:14:00Z".into()),
            view: Some("sherpa-run-x".into()),
            change_hashes: vec!["W5GSLAVO".into()],
            used: vec!["urn:atomic:intent:019efe85".into()],
            managed_run: Some(ManagedRunInput {
                run_id: "run-1".into(),
                owner_agent: "sherpa".into(),
                owner_session_id: "sherpa-s1".into(),
                work_item_id: Some("NONA-7".into()),
            }),
            person: Some("did:atomic:lee".into()),
        }
    }

    fn find<'a>(graph: &'a Value, id: &str) -> &'a Value {
        graph["@graph"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["@id"] == id)
            .unwrap_or_else(|| panic!("no node {id}"))
    }

    #[test]
    fn managed_session_projects_full_delegation_chain() {
        let g = provenance_graph(&managed_input());

        assert_eq!(g["@id"], "urn:atomic:provgraph:session:inner-1");

        let activity = find(&g, "urn:atomic:activity:session:inner-1");
        assert_eq!(activity["@type"], "Activity");
        assert_eq!(
            activity["wasAssociatedWith"],
            "urn:atomic:agent:claude-code"
        );
        assert_eq!(activity["generated"][0], "urn:atomic:change:W5GSLAVO");
        assert_eq!(activity["used"][0], "urn:atomic:intent:019efe85");
        assert_eq!(activity["partOfRun"], "urn:atomic:run:run-1");
        assert_eq!(activity["startedAtTime"], "2026-07-01T22:11:00Z");

        // The PROV delegation chain: executor → orchestrator → person.
        let executor = find(&g, "urn:atomic:agent:claude-code");
        assert_eq!(executor["@type"], "SoftwareAgent");
        assert_eq!(executor["actedOnBehalfOf"], "urn:atomic:agent:sherpa");

        let orchestrator = find(&g, "urn:atomic:agent:sherpa");
        assert_eq!(orchestrator["actedOnBehalfOf"], "did:atomic:lee");

        let run = find(&g, "urn:atomic:run:run-1");
        assert_eq!(run["@type"], "Activity");
        assert_eq!(run["wasAssociatedWith"], "urn:atomic:agent:sherpa");
        assert_eq!(run["workItem"], "NONA-7");

        let person = find(&g, "did:atomic:lee");
        assert_eq!(person["@type"], "Person");
    }

    #[test]
    fn direct_session_delegates_straight_to_person() {
        let mut input = managed_input();
        input.managed_run = None;
        let g = provenance_graph(&input);

        let executor = find(&g, "urn:atomic:agent:claude-code");
        assert_eq!(executor["actedOnBehalfOf"], "did:atomic:lee");
        assert!(g["@graph"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["@id"] != "urn:atomic:agent:sherpa"));
    }

    #[test]
    fn unknown_person_omits_delegation_not_fakes_it() {
        let mut input = managed_input();
        input.person = None;
        let g = provenance_graph(&input);

        // Executor still delegates to the orchestrator (that edge is known)…
        let executor = find(&g, "urn:atomic:agent:claude-code");
        assert_eq!(executor["actedOnBehalfOf"], "urn:atomic:agent:sherpa");
        // …but the orchestrator's person edge is absent, not invented.
        let orchestrator = find(&g, "urn:atomic:agent:sherpa");
        assert!(orchestrator.get("actedOnBehalfOf").is_none());
        assert!(g["@graph"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["@type"] != "Person"));
    }

    #[test]
    fn empty_collections_are_omitted() {
        let mut input = managed_input();
        input.change_hashes.clear();
        input.used.clear();
        input.ended_at = None;
        let g = provenance_graph(&input);
        let activity = find(&g, "urn:atomic:activity:session:inner-1");
        assert!(activity.get("generated").is_none());
        assert!(activity.get("used").is_none());
        assert!(activity.get("endedAtTime").is_none());
    }
}
