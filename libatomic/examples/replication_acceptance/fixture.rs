use super::{cli, Result};
use atomic_core::{
    change::{
        AIVendor, AttestAgent, Attestation, ChangeHeader, Provenance, ProvenanceGraph,
        ProvenanceNode, ProvenanceNodeKind,
    },
    pristine::VaultEntryType,
    types::Hash,
};
use atomic_identity::{Identity, KeyPair};
use atomic_repository::{
    record::RecordOptions, tracking::TrackingOptions, IntentCreateOptions, Repository,
};
use serde_json::{json, Map, Value};
use std::{fs, path::Path};

pub struct Fixture {
    pub intent_urn: String,
    pub change: Hash,
    pub provenance: Hash,
    pub attest: Hash,
    pub intent: String,
    pub intent_path: String,
    pub signed_path: String,
    pub public_key: Vec<u8>,
}
fn write(root: &Path, path: &str, bytes: &[u8]) -> Result<()> {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(path, bytes)?;
    Ok(())
}
fn source_hash(fm: &Map<String, Value>, body: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(serde_json::to_string(fm).unwrap().as_bytes());
    h.update(b"\0");
    h.update(body.as_bytes());
    format!("blake3:{}", h.finalize().to_hex())
}
pub fn create(binary: &Path, root: &Path, config: &Path) -> Result<Fixture> {
    let mut repo = Repository::init(root)?;
    let working_copy = repo.require_working_copy_id()?;
    repo.init_vault()?;
    write(root, "base.txt", b"shared base\n")?;
    repo.add(working_copy, "base.txt", TrackingOptions::default())?;
    repo.record(
        working_copy,
        ChangeHeader::new("base"),
        RecordOptions::new()
            .with_all(false)
            .paths(vec!["base.txt"])
            .sync_vault(false)
            .enrich_kg(false),
    )?;
    repo.create_draft_view("feature", "dev")?;
    drop(repo);
    cli(binary, root, config, &["view", "switch", "feature"])?;
    let repo = Repository::open_existing(root)?;
    let working_copy = repo.require_working_copy_id()?;
    let made = repo.vault_intent_create(IntentCreateOptions {
        title: "Replicated intent".into(),
        priority: None,
        assignee: None,
        labels: vec![],
        session_id: None,
        turn_id: None,
        kind: None,
    })?;
    let entry = repo.vault_intent_show(&made.id)?;
    let fm: Map<String, Value> = serde_json::from_str(&entry.frontmatter_json)?;
    let body = String::from_utf8(entry.content_bytes)?;
    let key = KeyPair::generate();
    let identity = Identity::from_public_key("acceptance", key.public.clone());
    let signed = atomic_canonical::proof::attest(
        atomic_canonical::lift::lift_intent(&fm, &body)?,
        &identity,
        &key,
    );
    let signed_path = format!(
        "attestations/{}/attested.md",
        made.id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            })
            .collect::<String>()
    );
    let hash = source_hash(&fm, &body);
    let signed_json = serde_json::to_string_pretty(&signed)?;
    repo.vault_store(
        &signed_path,
        VaultEntryType::Attestation,
        signed_json.as_bytes().to_vec(),
        json!({"sourceContentHash":hash,"intentId":made.id}).to_string(),
    )?;
    write(root,&format!(".vault/{signed_path}"),format!("---\nentry_type: attestation\nsourceContentHash: '{hash}'\nintentId: '{}'\n---\n{signed_json}",made.id).as_bytes())?;
    let memory="---\nentry_type: memory\nid: replicated-memory\nmemoryKind: lesson\nstatus: active\n---\nReplicated knowledge\n";
    write(root, ".vault/memory/replicated.md", memory.as_bytes())?;
    repo.vault_store(
        "memory/replicated.md",
        VaultEntryType::Memory,
        b"Replicated knowledge\n".to_vec(),
        json!({"id":"replicated-memory","memoryKind":"lesson","status":"active"}).to_string(),
    )?;
    let goal = format!("---\nentry_type: session\nname: acceptance\ndeveloper: fixture\nstatus: active\nintent: '{}'\nstarted_at: '2026-01-01T00:00:00Z'\n---\nGoal linked to recorded intent\n", made.id);
    write(root, ".vault/goals/acceptance/_goal.md", goal.as_bytes())?;
    repo.vault_store("goals/acceptance/_goal.md", VaultEntryType::Session,
        b"Goal linked to recorded intent\n".to_vec(),
        json!({"name":"acceptance","developer":"fixture","status":"active","intent":made.id,"started_at":"2026-01-01T00:00:00Z"}).to_string())?;
    write(root, "code.rs", b"pub fn replicated() -> u32 { 42 }\n")?;
    let paths = vec![
        "code.rs".to_string(),
        format!(".vault/{}", made.intent_file),
        format!(".vault/{signed_path}"),
        ".vault/memory/replicated.md".into(),
        ".vault/goals/acceptance/_goal.md".into(),
    ];
    for path in &paths {
        repo.add(working_copy, path, TrackingOptions::default())?;
    }
    let provenance = Provenance::builder()
        .vendor(AIVendor::OpenAI)
        .model("fixture-model")
        .cost_usd(0.012345)
        .session_id("acceptance-session")
        .build();
    let out = repo.record(
        working_copy,
        ChangeHeader::new("recorded code and vault"),
        RecordOptions::new()
            .with_all(false)
            .paths(paths)
            .provenance(vec![provenance])
            .metadata_bytes(b"session-envelope-sentinel".to_vec())
            .with_unhashed(json!({"display_marker":"replicated unhashed transcript"}))
            .sync_vault(false)
            .enrich_kg(false),
    )?;
    let change = *out.hash();
    let graph = graph(Some(change), None, "acceptance-session", 1, &made.id);
    let provenance = repo.save_provenance_graph(&graph)?;
    let attest = repo.save_attestation(
        &Attestation::builder(
            "acceptance-session",
            AttestAgent::new("fixture", "Fixture", "openai"),
        )
        .add_change(change)
        .cost_usd(0.012345)
        .build(),
    )?;
    // Deliberately no materialized file or recorded change for this durable row.
    repo.vault_store(
        "scratch/db-only.md",
        VaultEntryType::Scratch,
        b"database-only authored work".to_vec(),
        json!({"title":"DB-only"}).to_string(),
    )?;
    {
        use atomic_core::pristine::{KgMutTxnT, MutTxnT};
        let mut txn = repo.pristine().write_txn()?;
        txn.upsert_kg_node(&atomic_core::pristine::vault::KgNode::new(
            "fixture:authored-only",
            "intent",
            "Authored DB-only relationship source",
            "authored",
        ))?;
        txn.commit()?;
    }
    let journal = repo.redb_change_store()?;
    let pending = journal.reserve_provenance_turn("pending-session", 1, 4)?;
    journal.append_provenance_envelope(
        pending.provenance_id,
        pending.generation,
        "pending-event",
        br#"{"fixture":"unsealed journal envelope"}"#,
        4,
    )?;
    Ok(Fixture {
        intent_urn: signed.id.clone(),
        change,
        provenance,
        attest,
        intent: made.id,
        intent_path: made.intent_file,
        signed_path,
        public_key: key.public.as_bytes().to_vec(),
    })
}
fn graph(
    change: Option<Hash>,
    previous: Option<Hash>,
    session: &str,
    time: i64,
    intent: &str,
) -> ProvenanceGraph {
    let mut graph = ProvenanceGraph::builder(session, "acceptance")
        .timestamp(time)
        .plan_id(intent)
        .add_node(ProvenanceNode {
            id: format!("goal-{time}"),
            kind: ProvenanceNodeKind::Goal,
            timestamp: time * 1000,
            summary: "Recorded decision and evidence".into(),
            detail: Some(json!({"fixture":"provenance-detail"}).to_string()),
            change_hash: None,
            tool_name: None,
            tool_call_id: None,
            duration_ms: None,
            classified: false,
            confidence: None,
            consolidated_from: vec![],
        })
        .build();
    graph.changes_explained = change.into_iter().collect();
    graph.previous = previous;
    graph
}
pub fn late(root: &Path, fixture: &Fixture) -> Result<(Hash, Hash)> {
    let repo = Repository::open_existing(root)?;
    let late = repo.save_provenance_graph(&graph(
        Some(fixture.change),
        Some(fixture.provenance),
        "acceptance-session",
        2,
        &fixture.intent,
    ))?;
    let no_code = repo.save_provenance_graph(&graph(
        None,
        Some(late),
        "acceptance-session",
        3,
        &fixture.intent,
    ))?;
    // Update only unhashed metadata under the same change identity.
    let mut change = repo.load_change(&fixture.change)?;
    change.unhashed = Some(json!({"display_marker":"late attachment update"}));
    repo.save_change(&change)?;
    Ok((late, no_code))
}
pub fn orphan(binary: &Path, root: &Path, config: &Path) -> Result<Hash> {
    let mut repo = Repository::open_existing(root)?;
    repo.create_draft_view("orphan", "dev")?;
    drop(repo);
    cli(binary, root, config, &["view", "switch", "orphan"])?;
    let repo = Repository::open_existing(root)?;
    let working_copy = repo.require_working_copy_id()?;
    write(root, "orphan.txt", b"unpromoted code\n")?;
    let memory = "---\nentry_type: memory\nid: orphan-memory\n---\nOnly on orphan\n";
    write(root, ".vault/memory/orphan.md", memory.as_bytes())?;
    repo.vault_store(
        "memory/orphan.md",
        VaultEntryType::Memory,
        b"Only on orphan\n".to_vec(),
        json!({"id":"orphan-memory"}).to_string(),
    )?;
    let paths: Vec<String> = vec!["orphan.txt".into(), ".vault/memory/orphan.md".into()];
    for path in &paths {
        repo.add(working_copy, path, TrackingOptions::default())?;
    }
    let out = repo.record(
        working_copy,
        ChangeHeader::new("unpromoted work"),
        RecordOptions::new()
            .with_all(false)
            .paths(paths)
            .sync_vault(false)
            .enrich_kg(false),
    )?;
    Ok(*out.hash())
}
