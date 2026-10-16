//! Integration tests for the Atomic-native session ledger.
//!
//! Covers the ATOM-15 work items end-to-end against temporary repositories:
//!   1. content-addressed session manifests (publish, head, ingest),
//!   2. session forking at a turn boundary with immutable parent linkage,
//!   3. idempotent index rebuild from stored provenance graphs,
//!   4. cross-repository session sync: manifest ingest + provenance-driven
//!      ledger rebuild in a repository that never saw the original JSON.
//!
//! Windows is excluded for the same identity-store redirection reason as the
//! provenance integration tests.
#![cfg(not(windows))]
use std::path::Path;
use std::process::{Command, Output};

use atomic_core::change::provenance_graph::{ProvenanceNode, ProvenanceNodeKind};
use atomic_core::change::session::SessionTodo;
use atomic_core::change::ProvenanceGraph;
use atomic_core::kg::{
    iri,
    ontology::{Class as C, Predicate as P},
};
use atomic_core::pristine::vault::{KgEdge, KgNode};
use atomic_core::pristine::{KgMutTxnT, KgTxnT};
use atomic_core::types::{Base32, Hash};
use atomic_core::MutTxnT;
use atomic_repository::Repository;
use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(repo_dir: &Path, home_dir: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(repo_dir)
        .env("HOME", home_dir)
        .output()
        .expect("run atomic")
}

fn init_repo(repo_dir: &Path, home_dir: &Path) {
    assert!(atomic(repo_dir, home_dir, &["init"]).status.success());
}

fn goal_node(id: &str, summary: &str) -> ProvenanceNode {
    ProvenanceNode {
        id: id.into(),
        kind: ProvenanceNodeKind::Goal,
        timestamp: 1_735_689_600_000,
        summary: summary.into(),
        detail: None,
        change_hash: None,
        tool_name: None,
        tool_call_id: None,
        duration_ms: None,
        classified: false,
        confidence: None,
        consolidated_from: Vec::new(),
    }
}

fn save_turn_graph(
    repo: &Repository,
    session_id: &str,
    goal: &str,
    change_seed: u8,
    previous: Option<Hash>,
) -> Hash {
    let builder = ProvenanceGraph::builder(session_id, "opencode")
        .add_node(goal_node(&format!("{}-goal", session_id), goal))
        .add_change_explained(Hash::of(&[change_seed]));
    let builder = match previous {
        Some(prev) => builder.previous(prev),
        None => builder,
    };
    repo.save_provenance_graph(&builder.build())
        .expect("save provenance graph")
}

#[test]
fn session_manifest_is_content_addressed_and_ingestible() {
    let repo_tmp = TempDir::new().unwrap();
    let home_tmp = TempDir::new().unwrap();
    let repo_dir = repo_tmp.path();
    let home_dir = home_tmp.path();
    init_repo(repo_dir, home_dir);

    let repo = Repository::open(repo_dir).expect("open repo");
    let p0 = save_turn_graph(&repo, "sess-manifest", "turn zero", 1, None);
    let p1 = save_turn_graph(&repo, "sess-manifest", "turn one", 2, Some(p0));

    // save_provenance_graph publishes the manifest automatically.
    let head = repo
        .get_session_head("sess-manifest")
        .expect("head query")
        .expect("head exists");
    let manifest = repo
        .get_session_manifest(&head)
        .expect("manifest query")
        .expect("manifest exists");

    assert_eq!(manifest.session_id, "sess-manifest");
    assert_eq!(manifest.turns.len(), 2);
    assert_eq!(manifest.turns[0].provenance_hash, p0);
    assert_eq!(manifest.turns[1].provenance_hash, p1);
    assert_eq!(manifest.turns[1].previous_provenance, Some(p0));
    assert_eq!(manifest.goal_provenance, Some(p0));
    assert_eq!(manifest.parent_session, None);

    // The manifest content hash is deterministic — re-serializing yields the
    // same identity, and ingesting the same bytes twice is idempotent.
    let bytes = manifest.to_bytes();
    let decoded = atomic_core::change::session::SessionManifest::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.content_hash(), head);
    let again = repo.ingest_session_manifest(&decoded).expect("ingest");
    assert_eq!(again, head);
}

#[test]
fn session_order_and_manifest_do_not_depend_on_ingestion_order() {
    let a_tmp = TempDir::new().unwrap();
    let b_tmp = TempDir::new().unwrap();
    let home_tmp = TempDir::new().unwrap();
    init_repo(a_tmp.path(), home_tmp.path());
    init_repo(b_tmp.path(), home_tmp.path());

    let parent = ProvenanceGraph::builder("ordered-session", "opencode")
        .timestamp(100)
        .add_node(goal_node("parent-goal", "parent"))
        .build();
    let parent_hash = Hash::of(&parent.serialize().unwrap());
    let child = ProvenanceGraph::builder("ordered-session", "opencode")
        .timestamp(200)
        .previous(parent_hash)
        .add_node(goal_node("child-goal", "child"))
        .build();
    let child_hash = Hash::of(&child.serialize().unwrap());

    let repo_a = Repository::open(a_tmp.path()).unwrap();
    repo_a.save_provenance_graph(&parent).unwrap();
    repo_a.save_provenance_graph(&child).unwrap();

    let repo_b = Repository::open(b_tmp.path()).unwrap();
    let project = repo_b.project_id().unwrap();
    let incoming = iri::mint_memory(&project, "incoming-link");
    let outgoing = iri::mint_memory(&project, "outgoing-link");
    let turn0 = iri::mint_turn(&project, "ordered-session", 0);
    let turn1 = iri::mint_turn(&project, "ordered-session", 1);
    repo_b.save_provenance_graph(&child).unwrap();
    {
        let mut txn = repo_b.pristine().write_txn().unwrap();
        txn.upsert_kg_node(&KgNode::new(&incoming, C::Memory, "incoming", "vault"))
            .unwrap();
        txn.upsert_kg_node(&KgNode::new(&outgoing, C::Memory, "outgoing", "vault"))
            .unwrap();
        txn.upsert_kg_edge(&KgEdge::new(&incoming, &turn0, P::References))
            .unwrap();
        txn.upsert_kg_edge(&KgEdge::new(&turn0, &outgoing, P::References))
            .unwrap();
        txn.commit().unwrap();
    }
    repo_b.save_provenance_graph(&parent).unwrap();

    let head_a = repo_a.get_session_head("ordered-session").unwrap();
    let head_b = repo_b.get_session_head("ordered-session").unwrap();
    assert_eq!(head_a, head_b, "manifest hash changed with ingestion order");

    let (_, turns_b) = repo_b
        .get_session_ledger("ordered-session")
        .unwrap()
        .unwrap();
    assert_eq!(turns_b[0].provenance_hash, parent_hash);
    assert_eq!(turns_b[1].provenance_hash, child_hash);

    let txn = repo_b.pristine().read_txn().unwrap();
    let child_edges = txn.get_kg_edges_from(&turn1).unwrap();
    assert!(child_edges
        .iter()
        .any(|edge| { edge.kind == P::ProvWasInformedBy && edge.to_id == turn0 }));
    let parent_edges = txn.get_kg_edges_from(&turn0).unwrap();
    assert!(!parent_edges
        .iter()
        .any(|edge| edge.kind == P::ProvWasInformedBy));
    assert!(child_edges
        .iter()
        .any(|edge| { edge.kind == P::References && edge.to_id == outgoing }));
    let child_incoming = txn.get_kg_edges_to(&turn1).unwrap();
    assert!(child_incoming
        .iter()
        .any(|edge| { edge.kind == P::References && edge.from_id == incoming }));
    assert!(!parent_edges.iter().any(|edge| edge.kind == P::References));
}

#[test]
fn session_fork_inherits_prefix_and_links_parent_manifest() {
    let repo_tmp = TempDir::new().unwrap();
    let home_tmp = TempDir::new().unwrap();
    let repo_dir = repo_tmp.path();
    let home_dir = home_tmp.path();
    init_repo(repo_dir, home_dir);

    let repo = Repository::open(repo_dir).expect("open repo");
    let p0 = save_turn_graph(&repo, "parent", "parent turn zero", 1, None);
    let p1 = save_turn_graph(&repo, "parent", "parent turn one", 2, Some(p0));
    let _p2 = save_turn_graph(&repo, "parent", "parent turn two", 3, Some(p1));

    // Fork at turn 1 — the child inherits turns 0 and 1 only.
    let (parent_manifest, child_manifest) = repo
        .fork_session("parent", 1, "child")
        .expect("fork session");

    let (_, child_turns) = repo
        .get_session_ledger("child")
        .expect("child ledger query")
        .expect("child ledger exists");
    assert_eq!(child_turns.len(), 2);
    assert_eq!(child_turns[0].turn_number, 0);
    assert_eq!(child_turns[1].turn_number, 1);
    assert!(child_turns.iter().all(|t| t.session_id == "child"));

    // The child manifest records immutable fork lineage.
    let manifest = repo
        .get_session_manifest(&child_manifest)
        .expect("child manifest query")
        .expect("child manifest exists");
    assert_eq!(manifest.parent_session, Some(parent_manifest));
    assert_eq!(manifest.fork_turn, Some(1));

    // The parent's ledger is untouched by the fork.
    let (parent_record, parent_turns) = repo
        .get_session_ledger("parent")
        .expect("parent ledger query")
        .expect("parent ledger exists");
    assert_eq!(parent_record.turn_count, 3);
    assert_eq!(parent_turns.len(), 3);

    // A new turn on the child appends after the inherited prefix.
    let child_latest = child_turns[1].provenance_hash;
    let c2 = save_turn_graph(&repo, "child", "child new work", 9, Some(child_latest));
    let (_, child_turns_after) = repo
        .get_session_ledger("child")
        .expect("child ledger query")
        .expect("child ledger exists");
    assert_eq!(child_turns_after.len(), 3);
    assert_eq!(child_turns_after[2].turn_number, 2);
    assert_eq!(child_turns_after[2].provenance_hash, c2);
    assert_eq!(child_turns_after[2].previous_provenance, Some(child_latest));
    let child_head = repo.get_session_head("child").unwrap().unwrap();
    let child_head = repo.get_session_manifest(&child_head).unwrap().unwrap();
    assert_eq!(child_head.parent_session, Some(parent_manifest));
    assert_eq!(child_head.fork_turn, Some(1));

    // Forking beyond the parent's last turn is rejected.
    assert!(repo.fork_session("parent", 42, "child2").is_err());
}

#[test]
fn session_rebuild_is_idempotent_and_tolerates_corruption() {
    let repo_tmp = TempDir::new().unwrap();
    let home_tmp = TempDir::new().unwrap();
    let repo_dir = repo_tmp.path();
    let home_dir = home_tmp.path();
    init_repo(repo_dir, home_dir);

    {
        let repo = Repository::open(repo_dir).expect("open repo");
        let p0 = save_turn_graph(&repo, "rebuild", "turn zero", 1, None);
        let _p1 = save_turn_graph(&repo, "rebuild", "turn one", 2, Some(p0));

        // Everything is already indexed: rebuild must skip, never duplicate.
        let (indexed, skipped, corrupt) = repo.rebuild_session_index().expect("rebuild");
        assert_eq!(indexed, 0);
        assert_eq!(skipped, 2);
        assert_eq!(corrupt, 0);

        let (_, turns) = repo
            .get_session_ledger("rebuild")
            .expect("ledger query")
            .expect("ledger exists");
        assert_eq!(turns.len(), 2, "rebuild must not duplicate turns");

        let mut txn = repo.pristine().write_txn().unwrap();
        let project = repo.project_id().unwrap();
        let turn0 = iri::mint_turn(&project, "rebuild", 0);
        let turn1 = iri::mint_turn(&project, "rebuild", 1);
        txn.del_kg_edge(&turn1, &turn0, P::ProvWasInformedBy.iri())
            .unwrap();
        txn.commit().unwrap();

        let (indexed, skipped, corrupt) = repo.rebuild_session_index().expect("repair rebuild");
        assert_eq!((indexed, skipped, corrupt), (0, 2, 0));
        let txn = repo.pristine().read_txn().unwrap();
        assert!(txn
            .get_kg_edges_from(&turn1)
            .unwrap()
            .iter()
            .any(|edge| { edge.kind == P::ProvWasInformedBy && edge.to_id == turn0 }));
    }

    // A corrupt provenance file is reported, not fatal.

    let bogus = Hash::of(b"bogus-corrupt-provenance");
    let bogus_str = bogus.to_base32();
    let dir = repo_dir.join(".atomic/changes").join(&bogus_str[..2]);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{}.provenance", bogus_str)),
        b"not a provenance graph",
    )
    .unwrap();

    {
        let repo = Repository::open(repo_dir).expect("open repo");
        let (indexed, skipped, corrupt) = repo.rebuild_session_index().expect("rebuild");
        assert_eq!(indexed, 0);
        assert_eq!(skipped, 2);
        assert_eq!(corrupt, 1);
    }

    // CLI surface works.
    let out = atomic(repo_dir, home_dir, &["session", "rebuild"]);
    assert!(
        out.status.success(),
        "session rebuild failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("Indexed: 0"));
    assert!(stdout.contains("Corrupt (skipped): 1"));
}

#[test]
fn session_cross_repo_sync_rebuilds_ledger_from_provenance() {
    // Repository A authors a session; repository B receives the manifest and
    // provenance graphs (the pull path) and rebuilds the full ledger without
    // ever seeing A's `.atomic/sessions/*.json` file.
    let a_tmp = TempDir::new().unwrap();
    let b_tmp = TempDir::new().unwrap();
    let home_tmp = TempDir::new().unwrap();
    init_repo(a_tmp.path(), home_tmp.path());
    init_repo(b_tmp.path(), home_tmp.path());

    let repo_a = Repository::open(a_tmp.path()).expect("open repo A");
    let p0 = save_turn_graph(&repo_a, "sync-session", "sync turn zero", 1, None);
    let p1 = save_turn_graph(&repo_a, "sync-session", "sync turn one", 2, Some(p0));

    let manifest_hash = repo_a
        .get_session_head("sync-session")
        .expect("head query")
        .expect("head exists");
    let manifest = repo_a
        .get_session_manifest(&manifest_hash)
        .expect("manifest query")
        .expect("manifest exists");

    // Repository B: ingest the manifest (portable identity), then ingest the
    // provenance graphs — the pull path's save_provenance_graph rebuilds the
    // session index and publishes the manifest locally.
    let repo_b = Repository::open(b_tmp.path()).expect("open repo B");
    assert!(!repo_b.has_provenance_graph(&p0));
    assert!(repo_b.get_session_ledger("sync-session").unwrap().is_none());

    let ingested = repo_b.ingest_session_manifest(&manifest).expect("ingest");
    assert_eq!(
        ingested, manifest_hash,
        "content-addressed identity is portable"
    );

    for turn in &manifest.turns {
        let graph = repo_a
            .load_provenance_graph(&turn.provenance_hash)
            .expect("load from A");
        repo_b
            .save_provenance_graph(&graph)
            .expect("save into B (pull path)");
    }

    // B's locally published manifest matches A's exactly — deterministic
    // content addressing across repositories.
    {
        let (record_b, turns_b) = repo_b
            .get_session_ledger("sync-session")
            .expect("ledger query on B")
            .expect("ledger exists on B");
        assert_eq!(record_b.session_id, "sync-session");
        assert_eq!(turns_b.len(), 2);
        assert_eq!(turns_b[0].provenance_hash, p0);
        assert_eq!(turns_b[1].provenance_hash, p1);
        assert_eq!(turns_b[1].previous_provenance, Some(p0));
        assert_eq!(
            repo_b.get_session_head("sync-session").unwrap(),
            Some(manifest_hash)
        );
    }
    drop(repo_b);
    drop(repo_a);

    // The CLI reads B's rebuilt ledger.
    let show = atomic(
        b_tmp.path(),
        home_tmp.path(),
        &["session", "show", "sync-session"],
    );
    assert!(
        show.status.success(),
        "session show failed on B: {}",
        String::from_utf8_lossy(&show.stderr)
    );
    let stdout = String::from_utf8(show.stdout).unwrap();
    assert!(stdout.contains("Session sync-session"));
    assert!(stdout.contains("Turn 0"));
    assert!(stdout.contains("Turn 1"));
    assert!(stdout.contains("Goal marker: sync turn zero"));
}

#[test]
fn session_rebuild_restores_a_fork_from_its_manifest() {
    let a_tmp = TempDir::new().unwrap();
    let b_tmp = TempDir::new().unwrap();
    let home_tmp = TempDir::new().unwrap();
    init_repo(a_tmp.path(), home_tmp.path());
    init_repo(b_tmp.path(), home_tmp.path());

    let repo_a = Repository::open(a_tmp.path()).unwrap();
    let p0 = save_turn_graph(&repo_a, "manifest-parent", "turn zero", 1, None);
    save_turn_graph(&repo_a, "manifest-parent", "turn one", 2, Some(p0));
    let (parent_manifest, child_manifest) = repo_a
        .fork_session("manifest-parent", 1, "manifest-child")
        .unwrap();
    let child = repo_a
        .get_session_manifest(&child_manifest)
        .unwrap()
        .unwrap();
    repo_a
        .upsert_session_lifecycle("empty-parent", None, None, None)
        .unwrap();
    let (empty_parent_manifest, empty_child_manifest) = repo_a
        .fork_session("empty-parent", 0, "empty-child")
        .unwrap();
    let empty_child = repo_a
        .get_session_manifest(&empty_child_manifest)
        .unwrap()
        .unwrap();

    let repo_b = Repository::open(b_tmp.path()).unwrap();
    repo_b.ingest_session_manifest(&child).unwrap();
    repo_b.ingest_session_manifest(&empty_child).unwrap();
    assert!(repo_b
        .get_session_ledger("manifest-child")
        .unwrap()
        .is_none());
    assert!(repo_b.get_session_ledger("empty-child").unwrap().is_none());

    let (indexed, skipped, corrupt) = repo_b.rebuild_session_index().unwrap();
    assert_eq!((indexed, skipped, corrupt), (2, 0, 0));

    let (_, turns) = repo_b
        .get_session_ledger("manifest-child")
        .unwrap()
        .expect("child ledger rebuilt from manifest");
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].provenance_hash, child.turns[0].provenance_hash);
    assert_eq!(turns[1].provenance_hash, child.turns[1].provenance_hash);

    let rebuilt_head = repo_b.get_session_head("manifest-child").unwrap().unwrap();
    let rebuilt = repo_b.get_session_manifest(&rebuilt_head).unwrap().unwrap();
    assert_eq!(rebuilt.parent_session, Some(parent_manifest));
    assert_eq!(rebuilt.fork_turn, Some(1));

    let (empty_record, empty_turns) = repo_b
        .get_session_ledger("empty-child")
        .unwrap()
        .expect("empty child ledger rebuilt from manifest");
    assert_eq!(empty_record.turn_count, 0);
    assert!(empty_turns.is_empty());
    let txn = repo_b.pristine().read_txn().unwrap();
    assert!(txn
        .get_kg_node(&iri::mint_session(
            &repo_b.project_id().unwrap(),
            "empty-child"
        ))
        .unwrap()
        .is_some());
    drop(txn);
    let empty_head = repo_b.get_session_head("empty-child").unwrap().unwrap();
    let empty_rebuilt = repo_b.get_session_manifest(&empty_head).unwrap().unwrap();
    assert_eq!(empty_rebuilt.parent_session, Some(empty_parent_manifest));
    assert_eq!(empty_rebuilt.fork_turn, Some(0));
}

#[test]
fn session_reregistration_is_idempotent() {
    let repo_tmp = TempDir::new().unwrap();
    let home_tmp = TempDir::new().unwrap();
    let repo_dir = repo_tmp.path();
    let home_dir = home_tmp.path();
    init_repo(repo_dir, home_dir);

    let repo = Repository::open(repo_dir).expect("open repo");
    let graph = ProvenanceGraph::builder("dup-session", "opencode")
        .add_node(goal_node("dup-goal", "duplicate registration"))
        .add_change_explained(Hash::of(b"dup-change"))
        .build();

    // Same graph saved twice (upload path + listener/refetch path race):
    // the file write is idempotent by content hash, and the session index
    // must dedup the second registration.
    let h1 = repo.save_provenance_graph(&graph).expect("first save");
    let head_after_first = repo.get_session_head("dup-session").unwrap();
    let h2 = repo.save_provenance_graph(&graph).expect("second save");
    assert_eq!(h1, h2);

    let (record, turns) = repo
        .get_session_ledger("dup-session")
        .expect("ledger query")
        .expect("ledger exists");
    assert_eq!(record.turn_count, 1, "re-registration duplicated a turn");
    assert_eq!(turns.len(), 1);
    assert_eq!(record.latest_provenance, Some(h1));
    // Manifest republication after the deduped save is content-identical.
    assert_eq!(
        repo.get_session_head("dup-session").unwrap(),
        head_after_first
    );
}

#[test]
fn session_lifecycle_reconciles_view_and_status_metadata() {
    let repo_tmp = TempDir::new().unwrap();
    let home_tmp = TempDir::new().unwrap();
    let repo_dir = repo_tmp.path();
    let home_dir = home_tmp.path();
    init_repo(repo_dir, home_dir);

    let repo = Repository::open(repo_dir).expect("open repo");

    // Session start: record created with view metadata, active status.
    repo.upsert_session_lifecycle(
        "lifecycle",
        Some("agent-lifecycle".into()),
        Some("dev".into()),
        None,
    )
    .expect("lifecycle start");
    let (record, _) = repo
        .get_session_ledger("lifecycle")
        .expect("ledger query")
        .expect("record exists");
    assert_eq!(record.view_name.as_deref(), Some("agent-lifecycle"));
    assert_eq!(record.parent_view.as_deref(), Some("dev"));
    assert_eq!(record.ended_at, None);

    // A recorded turn must survive later lifecycle updates.
    save_turn_graph(&repo, "lifecycle", "work happened", 1, None);

    // Session end: ended_at set, ledger data preserved.
    repo.upsert_session_lifecycle("lifecycle", None, None, Some(1_735_700_000))
        .expect("lifecycle end");
    let (record, turns) = repo
        .get_session_ledger("lifecycle")
        .expect("ledger query")
        .expect("record exists");
    assert_eq!(record.ended_at, Some(1_735_700_000));
    assert_eq!(record.turn_count, 1);
    assert_eq!(turns.len(), 1);
    assert_eq!(record.view_name.as_deref(), Some("agent-lifecycle"));

    // Session resume: ended marker cleared, ledger still intact.
    repo.upsert_session_lifecycle(
        "lifecycle",
        Some("agent-lifecycle".into()),
        Some("dev".into()),
        None,
    )
    .expect("lifecycle resume");
    let (record, turns) = repo
        .get_session_ledger("lifecycle")
        .expect("ledger query")
        .expect("record exists");
    assert_eq!(record.ended_at, None);
    assert_eq!(turns.len(), 1);
}

#[test]
fn session_ledger_emits_rdf_taxonomy() {
    let repo_tmp = TempDir::new().unwrap();
    let home_tmp = TempDir::new().unwrap();
    let repo_dir = repo_tmp.path();
    let home_dir = home_tmp.path();
    init_repo(repo_dir, home_dir);

    let mut repo = Repository::open(repo_dir).expect("open repo");
    let parent = repo.current_view().to_owned();
    repo.create_draft_view("agent-kg-session", &parent).unwrap();
    let project = repo.project_id().unwrap();
    let session_id = iri::mint_session(&project, "kg-session");
    let turn0_id = iri::mint_turn(&project, "kg-session", 0);
    let turn1_id = iri::mint_turn(&project, "kg-session", 1);
    let todo_id = iri::mint_todo(&project, "kg-session", "todo-rdf");
    let change0 = Hash::of(&[1u8]);
    let p0 = repo
        .save_provenance_graph(
            &ProvenanceGraph::builder("kg-session", "opencode")
                .plan_id("ATOM-20")
                .todos(vec![
                    SessionTodo {
                        id: "todo-rdf".into(),
                        content: "Emit RDF plan and todo links".into(),
                        status: "completed".into(),
                        priority: "high".into(),
                    },
                    SessionTodo {
                        id: "external-collision".into(),
                        content: "Keep an unrelated global node".into(),
                        status: "pending".into(),
                        priority: "normal".into(),
                    },
                ])
                .add_node(goal_node("kg-goal", "kg turn zero"))
                .add_change_explained(change0)
                .build(),
        )
        .expect("save contextual provenance");
    let _p1 = save_turn_graph(&repo, "kg-session", "kg turn one", 2, Some(p0));

    // Session view context is distinct from change membership.
    repo.upsert_session_lifecycle(
        "kg-session",
        Some("agent-kg-session".into()),
        Some("dev".into()),
        None,
    )
    .expect("lifecycle");

    let txn = repo.pristine().read_txn().expect("read txn");

    // Session node: typed, lifecycle metadata.
    let session = txn
        .get_kg_node(&session_id)
        .expect("kg read")
        .expect("session node");
    let meta = session.metadata.as_ref().expect("session metadata");
    assert!(session.has_type(C::Session));
    assert!(meta.get("rdf_type").is_none());
    assert_eq!(meta["view"], "agent-kg-session");
    assert_eq!(meta["status"], "active");

    // Turn + provenance nodes typed; the taxonomy chains present.
    let turn1 = txn
        .get_kg_node(&turn1_id)
        .expect("kg read")
        .expect("turn node");
    assert!(turn1.has_type(C::SessionTurn));

    let session_edges = txn.get_kg_edges_from(&session_id).expect("kg read");
    assert!(session_edges
        .iter()
        .any(|e| e.kind == P::HasTurn && e.to_id == turn0_id));
    assert!(session_edges.iter().any(|e| e.kind == P::SessionViewContext
        && e.to_id == iri::mint_view(&project, "agent-kg-session")));
    assert!(!session_edges
        .iter()
        .any(|e| matches!(e.kind, P::HasPlan | P::DirectlyOnView)));

    let turn0_edges = txn.get_kg_edges_from(&turn0_id).expect("kg read");
    assert!(turn0_edges
        .iter()
        .any(|e| e.kind == P::TurnExplainedBy && e.to_id == iri::mint_provenance(&p0)));
    assert!(turn0_edges
        .iter()
        .any(|e| e.kind == P::GeneratedChange && e.to_id == iri::mint_change(&change0)));
    // An unresolvable historical plan is a gap, never a guessed intent IRI.
    assert!(!turn0_edges.iter().any(|e| e.kind == P::HasPlan));
    assert!(txn
        .get_kg_node(&iri::mint_finding(
            &project,
            "UnresolvedReference",
            &turn0_id
        ))
        .unwrap()
        .is_some());
    assert!(turn0_edges
        .iter()
        .any(|e| { e.kind == P::HasTodo && e.to_id == todo_id }));
    let todo = txn
        .get_kg_node(&todo_id)
        .expect("kg read")
        .expect("todo node");
    assert!(todo.has_type(C::Todo));
    assert_eq!(todo.metadata.as_ref().unwrap()["status"], "completed");

    // Turn 1 informed by turn 0.
    let turn1_edges = txn.get_kg_edges_from(&turn1_id).expect("kg read");
    assert!(turn1_edges
        .iter()
        .any(|e| e.kind == P::ProvWasInformedBy && e.to_id == turn0_id));

    drop(txn);

    // Fork: derivation edge with the boundary in metadata.
    let (parent_manifest, _child) = repo
        .fork_session("kg-session", 1, "kg-child")
        .expect("fork");
    let txn = repo.pristine().read_txn().expect("read txn");
    let child_edges = txn
        .get_kg_edges_from(&iri::mint_session(&project, "kg-child"))
        .expect("kg read");
    let fork_edge = child_edges
        .iter()
        .find(|e| e.kind == P::ProvWasDerivedFrom && e.to_id == session_id)
        .expect("fork edge");
    let fork_meta = fork_edge.metadata.as_ref().expect("fork metadata");
    assert_eq!(fork_meta["fork_turn"], 1);
    assert_eq!(
        fork_meta["parent_manifest"],
        parent_manifest.to_base32().as_str()
    );

    // Inherited prefix emits under the child session too.
    assert!(txn
        .get_kg_node(&iri::mint_turn(&project, "kg-child", 1))
        .expect("kg read")
        .is_some());
    drop(txn);

    // Repair a missing derived todo on an inherited-only child, preserving
    // an unrelated resource owned by a different producer.
    let mut txn = repo.pristine().write_txn().expect("write txn");
    txn.del_kg_node(&iri::mint_todo(&project, "kg-child", "todo-rdf"))
        .unwrap();
    let unrelated_id = iri::mint_todo(&project, "external-session", "external-collision");
    txn.upsert_kg_node(&KgNode::new(
        &unrelated_id,
        C::Todo,
        "Owned by another index",
        "external_index",
    ))
    .unwrap();
    txn.commit().unwrap();

    repo.rebuild_session_index().expect("rebuild");
    let txn = repo.pristine().read_txn().expect("read txn");
    assert!(txn
        .get_kg_node(&iri::mint_todo(&project, "kg-child", "todo-rdf"))
        .unwrap()
        .is_some());
    let unrelated = txn
        .get_kg_node(&unrelated_id)
        .unwrap()
        .expect("unrelated colliding node must survive");
    assert_eq!(unrelated.source, "external_index");
    drop(txn);

    let child_head = repo.get_session_head("kg-child").unwrap().unwrap();
    let child_manifest = repo.get_session_manifest(&child_head).unwrap().unwrap();
    assert_eq!(child_manifest.parent_session, Some(parent_manifest));
    assert_eq!(child_manifest.fork_turn, Some(1));
}

#[test]
fn todo_ids_are_scoped_to_their_session() {
    let repo_tmp = TempDir::new().unwrap();
    let home_tmp = TempDir::new().unwrap();
    init_repo(repo_tmp.path(), home_tmp.path());
    let repo = Repository::open(repo_tmp.path()).unwrap();

    for (session_id, content) in [("todo-a", "First session"), ("todo-b", "Second session")] {
        repo.save_provenance_graph(
            &ProvenanceGraph::builder(session_id, "opencode")
                .todos(vec![SessionTodo {
                    id: "t-1".into(),
                    content: content.into(),
                    status: "pending".into(),
                    priority: "normal".into(),
                }])
                .build(),
        )
        .unwrap();
    }

    let txn = repo.pristine().read_txn().unwrap();
    let project = repo.project_id().unwrap();
    let first = txn
        .get_kg_node(&iri::mint_todo(&project, "todo-a", "t-1"))
        .unwrap()
        .expect("first session todo");
    let second = txn
        .get_kg_node(&iri::mint_todo(&project, "todo-b", "t-1"))
        .unwrap()
        .expect("second session todo");
    assert_eq!(first.label, "First session");
    assert_eq!(second.label, "Second session");
    assert_eq!(first.metadata.unwrap()["session_id"], "todo-a");
    assert_eq!(second.metadata.unwrap()["session_id"], "todo-b");
    assert!(txn.get_kg_node("todo:t-1").unwrap().is_none());
}

#[test]
fn session_fork_cli_creates_child_session() {
    let repo_tmp = TempDir::new().unwrap();
    let home_tmp = TempDir::new().unwrap();
    let repo_dir = repo_tmp.path();
    let home_dir = home_tmp.path();
    init_repo(repo_dir, home_dir);

    {
        let repo = Repository::open(repo_dir).expect("open repo");
        let p0 = save_turn_graph(&repo, "cli-parent", "cli turn zero", 1, None);
        save_turn_graph(&repo, "cli-parent", "cli turn one", 2, Some(p0));
    }

    let fork = atomic(
        repo_dir,
        home_dir,
        &[
            "session",
            "fork",
            "cli-parent",
            "--at-turn",
            "0",
            "--child",
            "cli-child",
        ],
    );
    assert!(
        fork.status.success(),
        "session fork failed: {}",
        String::from_utf8_lossy(&fork.stderr)
    );
    let stdout = String::from_utf8(fork.stdout).unwrap();
    assert!(stdout.contains("Forked session cli-parent"));
    assert!(stdout.contains("Child session: cli-child"));

    let show = atomic(repo_dir, home_dir, &["session", "show", "cli-child"]);
    assert!(show.status.success());
    let stdout = String::from_utf8(show.stdout).unwrap();
    assert!(stdout.contains("Session cli-child"));
    assert!(stdout.contains("Turns: 1"));
    assert!(stdout.contains("Forked at turn: 0"));
    assert!(stdout.contains("Parent manifest:"));
}
