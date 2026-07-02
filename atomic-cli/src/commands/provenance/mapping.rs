//! `atomic_core::change::ProvenanceGraph` -> `ProvActivityInput` mapping.
//!
//! This is the ONLY site that touches `atomic-core` types on the provenance
//! path: `atomic-canonical` is `atomic-core` independent, so all `Hash`/base32
//! handling and the graph->input conversion live here at the CLI boundary.
//!
//! Read-only, compute-on-demand: mapping never writes anything.

use atomic_canonical::prov::{
    activity_urn, change_urn, normalize_agent_slug, ProvActivityInput,
};
use atomic_core::change::ProvenanceGraph;
use atomic_core::types::{Base32, Hash};
use atomic_repository::Repository;

/// Build a unique activity id for a turn's graph.
///
/// All turns of one session share `session_id`, so keying the activity purely on
/// `session_id` would collide across turns. We disambiguate with the explained
/// change's base32 so each turn-activity is uniquely addressable (and
/// `turnParent` resolution stays consistent with this scheme).
///
/// The id is keyed on the GRAPH itself — `<session_id>#<first-explained-change>`
/// (or just `<session_id>` if it explains none) — NOT on the user-traced change.
/// This is what makes turnParent join: a graph has ONE activity @id regardless
/// of which of its explained changes was traced, so a child turn's `turnParent`
/// reference to it always matches.
pub fn activity_id_for(graph: &ProvenanceGraph) -> String {
    match graph.changes_explained.first() {
        Some(h) => format!("{}#{}", graph.session_id, h.to_base32()),
        None => graph.session_id.clone(),
    }
}

/// Map a loaded `ProvenanceGraph` into the plain projector input.
///
/// `change_hash` is the change the user asked to trace (it names the subgraph and
/// the projected activity). `person_did` is the real signer's `did:atomic`,
/// resolved from the signing identity by the caller — it is NOT in the graph.
///
/// `graph.previous` is a hash of the PREVIOUS PROVENANCE GRAPH (not an activity
/// id): we load that graph and use ITS activity id for `turnParent`. If it can't
/// be loaded, `turnParent` is omitted (best-effort) rather than fabricated.
pub fn map_graph_to_input(
    repo: &Repository,
    graph: &ProvenanceGraph,
    change_hash: &Hash,
    person_did: &str,
) -> ProvActivityInput {
    let change_id_base32 = change_hash.to_base32();
    let activity_id = activity_id_for(graph);

    let generated = graph
        .changes_explained
        .iter()
        .map(|h| change_urn(&h.to_base32()))
        .collect();

    // `previous` is a hash of the previous PROVENANCE GRAPH — load it and use
    // ITS own activity id (session_id # first-explained-change). Best-effort.
    let turn_parent = graph.previous.and_then(|prev_hash| {
        repo.load_provenance_graph(&prev_hash)
            .ok()
            // The parent activity is keyed the same graph-stable way as this one
            // (activity_id_for), so trace walks a consistent, joinable chain.
            .map(|prev| activity_urn(&activity_id_for(&prev)))
    });

    let agent_vendor = (!graph.agent_vendor.is_empty()).then(|| graph.agent_vendor.clone());

    ProvActivityInput {
        change_id_base32,
        activity_id,
        // Timestamp is Unix SECONDS on the graph but MILLISECONDS on nodes;
        // to avoid mixing units the thin slice leaves times as None (the shape
        // simply omits absent times).
        started_at: None,
        ended_at: None,
        agent_slug: normalize_agent_slug(&graph.agent_name),
        agent_display_name: graph.agent_display_name.clone(),
        agent_vendor,
        person_did: person_did.to_string(),
        generated,
        turn_parent,
    }
}
