use atomic_core::change::{
    Change, ChangeHeader, ProvenanceEdge, ProvenanceEdgeKind, ProvenanceGraph, ProvenanceNode,
    ProvenanceNodeKind,
};
use atomic_core::types::Hash;
use atomic_repository::{IntentCreateOptions, IntentCreateResult, Repository};
use libatomic::{proto, LocalService};
use prost::Message;

fn intent(repo: &Repository, title: &str, session: Option<&str>) -> IntentCreateResult {
    repo.vault_intent_create(IntentCreateOptions {
        title: title.into(),
        priority: None,
        assignee: None,
        labels: vec![],
        session_id: session.map(str::to_string),
        turn_id: session.map(|_| 1),
        kind: None,
    })
    .unwrap()
}
fn fixture() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.init_vault().unwrap();
    (dir, repo)
}
fn save(repo: &Repository, message: &str) -> Hash {
    repo.save_change(&Change::empty(ChangeHeader::new(message)))
        .unwrap()
}
fn node(
    id: &str,
    kind: ProvenanceNodeKind,
    detail: Option<String>,
    hash: Option<Hash>,
) -> ProvenanceNode {
    ProvenanceNode {
        id: id.into(),
        kind,
        timestamp: 1,
        summary: id.into(),
        detail,
        change_hash: hash,
        tool_name: None,
        tool_call_id: None,
        duration_ms: None,
        classified: false,
        confidence: None,
        consolidated_from: vec![],
    }
}
async fn get(root: &std::path::Path, hash: Hash, metadata: bool) -> proto::GetChangeResponse {
    let service = LocalService::new();
    let resolved = service
        .call(
            "/atomic.DaemonService/ResolveRepository",
            &proto::ResolveRepositoryRequest {
                path: root.display().to_string(),
            }
            .encode_to_vec(),
        )
        .await
        .unwrap();
    let reference = proto::ResolveRepositoryResponse::decode(resolved.as_slice())
        .unwrap()
        .repository;
    let bytes = service
        .call(
            "/atomic.RepositoryQueryService/GetChange",
            &proto::GetChangeRequest {
                repository: reference,
                r#ref: Some(proto::ChangeRef {
                    kind: Some(proto::change_ref::Kind::Hash(proto::Hash {
                        value: hash.0.to_vec(),
                        ..Default::default()
                    })),
                }),
                includes: Some(proto::ChangeIncludes {
                    metadata,
                    ..Default::default()
                }),
                ..Default::default()
            }
            .encode_to_vec(),
        )
        .await
        .unwrap();
    proto::GetChangeResponse::decode(bytes.as_slice()).unwrap()
}

#[tokio::test]
async fn recorded_plan_is_typed_source_bound_and_not_a_validation_claim() {
    let (dir, repo) = fixture();
    let work = intent(&repo, "Governing work", None);
    let hash = save(&repo, "work");
    let graph = ProvenanceGraph::builder("session-a", "pi")
        .plan_id(&work.id)
        .add_change_explained(hash)
        .build();
    repo.save_provenance_graph(&graph).unwrap();
    drop(repo);
    let database = dir.path().join(".atomic/atomic.redb");
    let before = std::fs::read(&database).unwrap();
    let context = get(dir.path(), hash, true).await.intent_context.unwrap();
    assert_eq!(context.schema_version, 1);
    assert_eq!(context.change_hash.unwrap().value, hash.0);
    assert_eq!(context.plan_id.as_deref(), Some(work.id.as_str()));
    assert_eq!(context.status, "recorded");
    assert_eq!(context.validation_status, "not_checked");
    let binding = context
        .plans
        .iter()
        .find(|plan| plan.source == "provenance_graph")
        .unwrap();
    assert_eq!(binding.plan_id, work.id);
    assert_eq!(binding.session_id, "session-a");
    let resolved = binding.intent.as_ref().unwrap();
    assert_eq!(resolved.uid, work.uid);
    assert_eq!(resolved.title, "Governing work");
    assert_eq!(resolved.path, format!(".vault/{}", work.intent_file));
    assert!(get(dir.path(), hash, false).await.intent_context.is_none());
    assert_eq!(before, std::fs::read(database).unwrap());
}

#[tokio::test]
async fn legacy_intent_edit_and_creation_link_do_not_invent_a_plan_id() {
    use atomic_core::change::{Atom, GraphOp, Insertion, Local};
    use atomic_core::{ChangePosition, EdgeFlags, Position};
    let (dir, repo) = fixture();
    let work = intent(&repo, "Legacy hello world", Some("legacy-session"));
    let path = format!(".vault/{}", work.intent_file);
    let change = Change::new(
        ChangeHeader::new("legacy edit"),
        vec![GraphOp::Edit {
            change: Atom::Insertion(Insertion {
                predecessors: vec![],
                successors: vec![],
                flag: EdgeFlags::BLOCK,
                start: ChangePosition::new(0),
                end: ChangePosition::new(1),
                inode: Position::new(None, ChangePosition::new(0)),
            }),
            local: Local::new(&path, 1),
            encoding: None,
        }],
        b"x".to_vec(),
        vec![],
    );
    let hash = repo.save_change(&change).unwrap();
    let graph = ProvenanceGraph::builder("legacy-session", "pi")
        .add_change_explained(hash)
        .add_node(node(
            "edit",
            ProvenanceNodeKind::Commitment,
            Some(serde_json::json!({"file_path":path,"operation":"edit"}).to_string()),
            None,
        ))
        .add_node(node(
            "patch",
            ProvenanceNodeKind::PatchProposal,
            None,
            Some(hash),
        ))
        .add_edge(ProvenanceEdge {
            from: "edit".into(),
            to: "patch".into(),
            kind: ProvenanceEdgeKind::CommittedVia,
        })
        .build();
    let graph_hash = repo.save_provenance_graph(&graph).unwrap();
    drop(repo);
    let database = dir.path().join(".atomic/atomic.redb");
    let before = std::fs::read(&database).unwrap();
    let context = get(dir.path(), hash, true).await.intent_context.unwrap();
    assert_eq!(context.status, "missing");
    assert!(context.plan_id.is_none());
    assert!(context.plans.is_empty());
    assert_eq!(context.associated_intents.len(), 1);
    let association = &context.associated_intents[0];
    assert_eq!(association.intent.as_ref().unwrap().id, work.id);
    assert_eq!(association.commitment_node_ids, ["edit"]);
    assert_eq!(association.patch_node_ids, ["patch"]);
    assert_eq!(association.provenance_hashes[0].value, graph_hash.0);
    assert_eq!(
        before,
        std::fs::read(database).unwrap(),
        "query must not backfill, sign or change vault/session data"
    );
    let repo = Repository::open_readonly(dir.path()).unwrap();
    assert!(repo
        .load_provenance_graph(&graph_hash)
        .unwrap()
        .plan_id
        .is_none());
}

#[tokio::test]
async fn stored_turn_plan_is_supported_without_borrowing_another_turns_plan() {
    use atomic_core::change::session::SessionTurn;
    let (dir, repo) = fixture();
    let first = intent(&repo, "First", None);
    let second = intent(&repo, "Second", None);
    let hash = save(&repo, "first");
    let other = save(&repo, "second");
    let graph = ProvenanceGraph::builder("shared-session", "pi")
        .timestamp(1)
        .add_change_explained(hash)
        .build();
    let graph_hash = Hash::of(&graph.serialize().unwrap());
    repo.publish_provenance_checkpoint(
        &graph,
        SessionTurn {
            session_id: "shared-session".into(),
            turn_number: 0,
            goal: None,
            provenance_hash: graph_hash,
            change_hashes: vec![hash],
            previous_provenance: None,
            timestamp: 1,
            plan_id: Some(first.uid.clone()),
            todos: vec![],
            boundary_start: None,
            boundary_end: None,
            outcome: None,
        },
    )
    .unwrap();
    let next = ProvenanceGraph::builder("shared-session", "pi")
        .timestamp(2)
        .previous(graph_hash)
        .plan_id(&second.id)
        .add_change_explained(other)
        .build();
    repo.save_provenance_graph(&next).unwrap();
    drop(repo);
    let context = get(dir.path(), hash, true).await.intent_context.unwrap();
    assert_eq!(context.status, "recorded");
    assert_eq!(context.plan_id.as_deref(), Some(first.uid.as_str()));
    assert_eq!(context.plans.len(), 1);
    assert_eq!(context.plans[0].source, "session_turn");
    assert_eq!(context.plans[0].turn_number, Some(0));
    assert_eq!(context.plans[0].intent.as_ref().unwrap().id, first.id);
    assert!(!context.plans.iter().any(|plan| plan.plan_id == second.id));
}

#[tokio::test]
async fn conflicting_governing_plans_are_ambiguous_while_uid_aliases_are_not() {
    let (dir, repo) = fixture();
    let first = intent(&repo, "First", None);
    let second = intent(&repo, "Second", None);
    let conflict = save(&repo, "conflict");
    let alias = save(&repo, "alias");
    for (session, plan, hash) in [
        ("a", &first.id, conflict),
        ("b", &second.id, conflict),
        ("c", &first.id, alias),
        ("d", &first.uid, alias),
    ] {
        repo.save_provenance_graph(
            &ProvenanceGraph::builder(session, "pi")
                .plan_id(plan)
                .add_change_explained(hash)
                .build(),
        )
        .unwrap();
    }
    drop(repo);
    let context = get(dir.path(), conflict, true)
        .await
        .intent_context
        .unwrap();
    assert_eq!(context.status, "ambiguous");
    assert!(context.plan_id.is_none());
    assert!(context.plans.len() >= 2);
    let context = get(dir.path(), alias, true).await.intent_context.unwrap();
    assert_eq!(context.status, "recorded");
    assert!(context.plan_id.is_some());
    assert!(context
        .plans
        .iter()
        .all(|plan| plan.intent.as_ref().unwrap().uid == first.uid));
}

#[tokio::test]
async fn another_turn_in_same_session_is_not_a_governing_link_for_an_unbound_change() {
    let (dir, repo) = fixture();
    let work = intent(&repo, "Later work", Some("same-session"));
    let first = save(&repo, "unbound first");
    let second = save(&repo, "bound later");
    let first_graph = ProvenanceGraph::builder("same-session", "pi")
        .timestamp(1)
        .add_change_explained(first)
        .build();
    let previous = repo.save_provenance_graph(&first_graph).unwrap();
    repo.save_provenance_graph(
        &ProvenanceGraph::builder("same-session", "pi")
            .timestamp(2)
            .previous(previous)
            .plan_id(&work.id)
            .add_change_explained(second)
            .build(),
    )
    .unwrap();
    drop(repo);
    let context = get(dir.path(), first, true).await.intent_context.unwrap();
    assert_eq!(context.status, "missing");
    assert!(context.plan_id.is_none());
    assert!(context.plans.is_empty());
}

#[tokio::test]
async fn unresolved_current_vault_metadata_does_not_hide_an_actual_recorded_plan_id() {
    let (dir, repo) = fixture(); let hash = save(&repo, "unresolved intent metadata");
    repo.save_provenance_graph(&ProvenanceGraph::builder("recorded-session", "pi").plan_id("MISSING::author::999").add_change_explained(hash).build()).unwrap();
    drop(repo);
    let context = get(dir.path(), hash, true).await.intent_context.unwrap();
    assert_eq!(context.plan_id.as_deref(), Some("MISSING::author::999"));
    assert_eq!(context.status, "partial"); assert!(!context.warnings.is_empty());
    assert!(context.plans.iter().all(|plan| plan.intent.is_none()));
    assert_eq!(context.validation_status, "not_checked");
}
