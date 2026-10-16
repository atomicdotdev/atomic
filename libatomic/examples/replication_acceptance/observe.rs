use super::{fixture::Fixture, Result};
use atomic_core::{
    pristine::{tables, KgTxnT, ViewTxnT},
    types::Base32,
};
use atomic_repository::Repository;
use libatomic::{proto, LocalService};
use prost::Message;
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, path::Path};

pub fn view_id(root: &Path, name: &str) -> Result<u64> {
    let repo = Repository::open_readonly(root)?;
    let txn = repo.pristine().read_txn()?;
    Ok(txn.get_view(name)?.ok_or("view missing")?.id)
}
pub fn storage(root: &Path, id: Option<u64>) -> Result<Value> {
    // Diagnostic only: inspect physical rows without a write handle.
    let db = redb::Builder::new().open_read_only(root.join(".atomic/atomic.redb"))?;
    let txn = db.begin_read()?;
    let prefix = id.map(u64::to_be_bytes);
    macro_rules! rows {
        ($definition:expr) => {{
            let table = txn.open_table($definition)?;
            let mut count = 0;
            for row in table.iter()? {
                let (key, _) = row?;
                if prefix.is_none_or(|p| key.value().starts_with(&p)) {
                    count += 1;
                }
            }
            count
        }};
    }
    let changes = rows!(tables::VIEW_CHANGES);
    let reverse = rows!(tables::REV_VIEW_CHANGES);
    let states = rows!(tables::STATES);
    let merkle = rows!(tables::MERKLE_CHAIN);
    let pending = match txn.open_table(tables::PROVENANCE_TURNS) {
        Ok(table) => Some(table.len()?),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(error) => return Err(error.into()),
    };
    let journal = match txn.open_table(tables::PROVENANCE_JOURNAL_EVENTS) {
        Ok(table) => Some(table.len()?),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(error) => return Err(error.into()),
    };
    Ok(
        json!({"view_id":id,"view_change_rows":changes,"reverse_view_change_rows":reverse,"state_rows":states,"merkle_rows":merkle,
        "graph_edges":txn.open_multimap_table(tables::GRAPH)?.len()?,
        "pending_turns":pending,"pending_journal_events":journal}),
    )
}
pub fn repository(root: &Path, fixture: &Fixture) -> Result<Value> {
    let repo = Repository::open_readonly(root)?;
    let views = repo.list_views()?;
    let mut graph_files = BTreeMap::new();
    let mut tags = BTreeMap::new();
    let mut filters = BTreeMap::new();
    let paths = vec![
        "code.rs".to_string(),
        "orphan.txt".into(),
        ".vault/memory/replicated.md".into(),
        ".vault/memory/orphan.md".into(),
        ".vault/goals/acceptance/_goal.md".into(),
        format!(".vault/{}", fixture.intent_path),
        format!(".vault/{}", fixture.signed_path),
    ];
    for view in &views {
        let txn = repo.pristine().read_txn()?;
        let state = txn.get_view(view)?.ok_or("listed view disappeared")?;
        let ids =
            atomic_repository::repository::collect_visible_change_ids_with_deps(&txn, &state)?;
        filters.insert(view, json!({"id":state.id,"kind":format!("{:?}",state.kind),"parent":state.parent,"own_change_count":state.change_count,
            "effective_change_ids":ids.iter_dependency_first().map(|id|id.get()).collect::<Vec<_>>()}));
        drop(txn);
        let mut files = BTreeMap::new();
        for path in &paths {
            let value = match repo.get_file_content_on_view(path, view) {
                Ok(Some(bytes)) => json!({"visible":true,"text":String::from_utf8_lossy(&bytes)}),
                Ok(None) => json!({"visible":false}),
                Err(e) => json!({"error":e.to_string()}),
            };
            files.insert(path.clone(), value);
        }
        graph_files.insert(view, files);
        tags.insert(
            view,
            repo.list_tags_for_view(view)?
                .into_iter()
                .map(|tag| tag.name)
                .collect::<Vec<_>>(),
        );
    }
    let mut vault = BTreeMap::new();
    for path in [
        "memory/replicated.md",
        "memory/orphan.md",
        "scratch/db-only.md",
        "goals/acceptance/_goal.md",
        fixture.intent_path.as_str(),
        fixture.signed_path.as_str(),
    ] {
        let value = match repo.vault_retrieve(path) {
            Ok(Some(entry)) => {
                json!({"available":true,"frontmatter":entry.frontmatter_json,"body":String::from_utf8_lossy(&entry.content_bytes)})
            }
            Ok(None) => json!({"available":false}),
            Err(e) => json!({"error":e.to_string()}),
        };
        vault.insert(path.to_string(), value);
    }
    let change = repo.load_change(&fixture.change);
    let inline = match change {
        Ok(change) => {
            json!({"available":true,"metadata_bytes":change.hashed.metadata.len(),"provenance_entries":change.hashed.provenance.len(),"cost_micro_usd":change.hashed.provenance.iter().map(|p|p.cost.micro_usd).sum::<u64>(),"file_ops":change.hashed.file_ops.len(),"unhashed":change.unhashed})
        }
        Err(e) => json!({"available":false,"error":e.to_string()}),
    };
    let graph = repo.load_provenance_graph(&fixture.provenance);
    let provenance = match graph {
        Ok(graph) => {
            json!({"available":true,"nodes":graph.nodes.len(),"plan_id":graph.plan_id,"changes":graph.changes_explained.iter().map(|h|h.to_base32()).collect::<Vec<_>>()})
        }
        Err(e) => json!({"available":false,"error":e.to_string()}),
    };
    let txn = repo.pristine().read_txn()?;
    let search = repo.vault_kg_search("Replicated", 20, None)?;
    let mut search_edges = BTreeMap::new();
    for node in &search {
        search_edges.insert(node.id.clone(), txn.get_kg_edges_from(&node.id)?);
    }
    let knowledge = json!({"canonical_id_lookup": txn.get_kg_node(&fixture.intent_urn)?,
        "search_results": search,"search_edges":search_edges,
        "authored_db_only_node": txn.get_kg_node("fixture:authored-only")?});
    drop(txn);
    let ledger = match repo.get_session_ledger("acceptance-session") {
        Ok(ledger) => serde_json::to_value(ledger)?,
        Err(e) => json!({"error":e.to_string()}),
    };
    Ok(
        json!({"views":views,"current_view":repo.current_view(),"graph_files":graph_files,"tags":tags,"view_filters":filters,"vault_global":vault,"change":inline,"provenance":provenance,"attestation_available":repo.has_attestation(&fixture.attest),"session_ledger":ledger,"knowledge":knowledge,
        "recorded_change":fixture.change.to_base32(),"materialized_code":fs::read_to_string(root.join("code.rs")).ok()}),
    )
}
pub fn backend(root: &Path, fixture: &Fixture) -> Result<Value> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let service=LocalService::new();
        let resolved=service.call("/atomic.DaemonService/ResolveRepository",&proto::ResolveRepositoryRequest{path:root.display().to_string()}.encode_to_vec()).await?;
        let reference=proto::ResolveRepositoryResponse::decode(resolved.as_slice())?.repository;
        let request=proto::GetVaultEntryRequest{repository:reference.clone(),id:fixture.intent.clone(),kind:proto::VaultEntityKind::Intent as i32,include_bundle:true,..Default::default()};
        let entry=match service.call("/atomic.VaultService/GetVaultEntry",&request.encode_to_vec()).await {Ok(bytes)=>{let result=proto::GetVaultEntryResponse::decode(bytes.as_slice())?;json!({"available":result.entry.is_some(),"title":result.entry.map(|e|e.title),"snapshot_available":result.snapshot.is_some()})},Err(e)=>json!({"available":false,"error":e.to_string()})};
        let request=proto::VerifyAttestationRequest{repository:reference,target:Some(proto::verify_attestation_request::Target::Subject(proto::AttestationTarget{kind:Some(proto::attestation_target::Kind::IntentId(fixture.intent.clone()))})),public_key:Some(fixture.public_key.clone()),..Default::default()};
        let verify=match service.call("/atomic.AttestationService/VerifyAttestation",&request.encode_to_vec()).await {Ok(bytes)=>{let result=proto::VerifyAttestationResponse::decode(bytes.as_slice())?;json!({"signature_valid":result.signature_valid,"source_fresh":result.source_fresh,"signed_document_available":result.signed_document.is_some(),"reason":result.reason})},Err(e)=>json!({"error":e.to_string()})};
        Ok(json!({"intent_query":entry,"signed_intent_query":verify}))
    })
}
