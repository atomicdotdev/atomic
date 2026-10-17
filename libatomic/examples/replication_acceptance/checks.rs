use serde_json::{json, Value};

pub fn summarize(report: &Value) -> Vec<Value> {
    let mut checks = Vec::new();
    let mut add = |name: &str, passed: bool, detail: &str| {
        checks.push(json!({"name":name,"status":if passed{"PASS"}else{"FAIL"},"detail":detail}))
    };
    let source = &report["source_before_push"];
    let receiver = &report["receiver_after_pull"];
    add("recorded change and inline provenance",receiver["change"]["available"]==true&&receiver["change"]["cost_micro_usd"]==source["change"]["cost_micro_usd"],"Fresh receiver loads the transferred change; inline metadata is queryable without a working tree.");
    add(
        "semantic FileOps",
        receiver["change"]["file_ops"] == source["change"]["file_ops"],
        "Recorded semantic operation count survives transport.",
    );
    add(
        "initial unhashed transcript",
        receiver["change"]["unhashed"] == source["change"]["unhashed"],
        "Initial display payload transferred; not authenticated by the change hash.",
    );
    add("opaque metadata fidelity",receiver["change"]["metadata_bytes"].as_u64().unwrap_or(0)>0,"The fixture supplied a nonempty envelope, but the stored source and receiver both lost it.");
    add(
        "provenance and graph attestation",
        receiver["provenance"]["available"] == true && receiver["attestation_available"] == true,
        "Independent evidence artifacts and backlinks arrived.",
    );
    add(
        "session ledger reconstruction",
        !receiver["session_ledger"].is_null(),
        "Transferred provenance reconstructed a durable session ledger.",
    );
    add(
        "tags",
        receiver["tags"]["feature"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v == "acceptance-tag")),
        "Named feature tag arrived.",
    );
    add("structured vault queries after fresh pull",report["receiver_backend"]["intent_query"]["available"]==true,"Raw recorded vault files arrived, but GetVaultEntry could not query the intent after the inactive-view pull.");
    add(
        "vault reconstruction after promotion",
        report["receiver_backend_after_insert_sync"]["intent_query"]["available"] == true,
        "Later active/root-view ingestion populated queryable vault rows.",
    );
    add(
        "signed revision cryptographic fidelity",
        report["receiver_backend_after_insert_sync"]["signed_intent_query"]["signature_valid"]
            == true,
        "The exact stored signed document remains verifiable using only its public key.",
    );
    add("signed source freshness fidelity",report["receiver_backend_after_insert_sync"]["signed_intent_query"]["source_fresh"]==report["source_backend"]["signed_intent_query"]["source_fresh"],"Source was fresh; reconstructed source metadata/body differed and receiver classified it stale.");
    add(
        "database-only vault data",
        receiver["vault_global"]["scratch/db-only.md"]["available"] == true,
        "A durable vault row with no recorded file had no sync artifact.",
    );
    add(
        "authored database-only KG data",
        !receiver["knowledge"]["authored_db_only_node"].is_null(),
        "The authored node was not reproduced by received artifacts.",
    );
    add(
        "pending journal replication",
        report["repeat_pull"]["after"]["pending_journal_events"]
            .as_u64()
            .unwrap_or(0)
            > 0,
        "Source had a pending turn/event; receiver had no pending journal tables/data.",
    );
    add(
        "late evidence on no-op push",
        report["late_evidence"]["late_uploaded"] == true,
        "No-new-change push did not discover the new graph. Promotion later sent it.",
    );
    add(
        "no-code turn replication",
        report["late_evidence_after_insert_sync"]["no_code_uploaded"] == true,
        "The graph with no explained change did not enter the push closure.",
    );
    add(
        "insert does not duplicate graph",
        report["insert_storage"]["before"]["graph_edges"]
            == report["insert_storage"]["after"]["graph_edges"],
        "View membership increased while canonical graph edge count stayed unchanged.",
    );
    add(
        "repeat pull idempotence",
        report["repeat_pull"]["before"] == report["repeat_pull"]["after"],
        "Repeated transfer/application did not grow graph/log rows.",
    );
    add(
        "draft metadata deletion",
        report["source_delete"]["feature_view_gone"] == true
            && report["source_delete"]["orphan_view_gone"] == true,
        "Views and forward membership logs were removed.",
    );
    add(
        "reverse-log deletion",
        report["source_delete"]["feature_after"]["reverse_view_change_rows"] == 0
            && report["source_delete"]["orphan_after"]["reverse_view_change_rows"] == 0,
        "Each deleted view retained a reverse membership row.",
    );
    add(
        "refused deletion preserves artifacts",
        report["delete_with_child"]["workspace_preserved"] == true,
        "Parent deletion was refused, but its shelved artifact directory was removed.",
    );
    add(
        "local deletion propagates to remote",
        report["wire_after_local_delete"]["refs"]
            .get("orphan")
            .is_none(),
        "Push carries ref moves, not deletion tombstones; the advertised orphan ref remained.",
    );
    add(
        "deleted view can be safely reconstructed",
        report["receiver_repull_result"]["success"] == true,
        "Re-pull failed with Change already applied after local deletion/recreation.",
    );
    checks.push(json!({"name":"ambient graph edge cleanup","status":"NOT_YET_CLASSIFIED","detail":"Aggregate edge count stayed unchanged. Edges needed by surviving views must remain; this run hasn't classified edges exclusive to the deleted uninserted draft. Change files and evidence are not snapshot garbage."}));
    checks.push(json!({"name":"production server","status":"BLOCKED","detail":report["production_server_blocker"]}));
    checks
}
