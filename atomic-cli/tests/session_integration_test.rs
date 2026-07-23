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
use atomic_core::pristine::ontology::{edge_kind, entity_type, predicate};
use atomic_core::pristine::KgTxnT;
use atomic_core::types::{Base32, Hash};
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

    let repo = Repository::open(repo_dir).expect("open repo");
    let change0 = Hash::of(&[1u8]);
    let p0 = repo
        .save_provenance_graph(
            &ProvenanceGraph::builder("kg-session", "opencode")
                .plan_id("ATOM-20")
                .todos(vec![SessionTodo {
                    id: "todo-rdf".into(),
                    content: "Emit RDF plan and todo links".into(),
                    status: "completed".into(),
                    priority: "high".into(),
                }])
                .add_node(goal_node("kg-goal", "kg turn zero"))
                .add_change_explained(change0)
                .build(),
        )
        .expect("save contextual provenance");
    let _p1 = save_turn_graph(&repo, "kg-session", "kg turn one", 2, Some(p0));

    // Lifecycle: view metadata + status ride the session node, ON_VIEW edge.
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
        .get_kg_node("session:kg-session")
        .expect("kg read")
        .expect("session node");
    let meta = session.metadata.as_ref().expect("session metadata");
    assert_eq!(meta["rdf_type"], entity_type::SESSION);
    assert_eq!(meta["view"], "agent-kg-session");
    assert_eq!(meta["status"], "active");

    // Turn + provenance nodes typed; the taxonomy chains present.
    let turn1 = txn
        .get_kg_node("session:kg-session/turn:1")
        .expect("kg read")
        .expect("turn node");
    assert_eq!(
        turn1.metadata.as_ref().unwrap()["rdf_type"],
        entity_type::SESSION_TURN
    );

    let session_edges = txn
        .get_kg_edges_from("session:kg-session")
        .expect("kg read");
    assert!(session_edges
        .iter()
        .any(|e| e.kind == predicate::HAD_MEMBER && e.to_id == "session:kg-session/turn:0"));
    assert!(session_edges
        .iter()
        .any(|e| e.kind == edge_kind::ON_VIEW && e.to_id == "view:agent-kg-session"));
    assert!(session_edges
        .iter()
        .any(|e| e.kind == predicate::HAD_PLAN && e.to_id == "intent:ATOM-20"));

    let turn0_edges = txn
        .get_kg_edges_from("session:kg-session/turn:0")
        .expect("kg read");
    assert!(turn0_edges.iter().any(|e| e.kind == predicate::EXPLAINED_BY
        && e.to_id == format!("provenance:{}", p0.to_base32())));
    let change0_short = &change0.to_base32()[..12];
    assert!(turn0_edges
        .iter()
        .any(|e| e.kind == predicate::GENERATED && e.to_id == format!("change:{}", change0_short)));
    assert!(turn0_edges
        .iter()
        .any(|e| e.kind == predicate::HAD_PLAN && e.to_id == "intent:ATOM-20"));
    assert!(turn0_edges
        .iter()
        .any(|e| e.kind == predicate::HAS_TODO && e.to_id == "todo:todo-rdf"));
    let todo = txn
        .get_kg_node("todo:todo-rdf")
        .expect("kg read")
        .expect("todo node");
    assert_eq!(
        todo.metadata.as_ref().unwrap()["rdf_type"],
        entity_type::TODO
    );
    assert_eq!(todo.metadata.as_ref().unwrap()["status"], "completed");

    // Turn 1 informed by turn 0.
    let turn1_edges = txn
        .get_kg_edges_from("session:kg-session/turn:1")
        .expect("kg read");
    assert!(turn1_edges
        .iter()
        .any(|e| e.kind == predicate::WAS_INFORMED_BY && e.to_id == "session:kg-session/turn:0"));

    drop(txn);

    // Fork: derivation edge with the boundary in metadata.
    let (parent_manifest, _child) = repo
        .fork_session("kg-session", 1, "kg-child")
        .expect("fork");
    let txn = repo.pristine().read_txn().expect("read txn");
    let child_edges = txn.get_kg_edges_from("session:kg-child").expect("kg read");
    let fork_edge = child_edges
        .iter()
        .find(|e| e.kind == predicate::WAS_DERIVED_FROM && e.to_id == "session:kg-session")
        .expect("fork edge");
    let fork_meta = fork_edge.metadata.as_ref().expect("fork metadata");
    assert_eq!(fork_meta["fork_turn"], 1);
    assert_eq!(
        fork_meta["parent_manifest"],
        parent_manifest.to_base32().as_str()
    );

    // Inherited prefix emits under the child session too.
    assert!(txn
        .get_kg_node("session:kg-child/turn:1")
        .expect("kg read")
        .is_some());
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
