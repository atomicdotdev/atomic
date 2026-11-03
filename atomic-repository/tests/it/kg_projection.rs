use atomic_core::{
    change::{Change, ChangeHeader, ProvenanceGraph},
    kg::{
        iri,
        ontology::{Class as C, Predicate as P},
    },
    pristine::{KgTxnT, VaultEntryType},
    types::{Base32, Merkle},
};
use atomic_repository::{Repository, UnrecordOptions, ViewManifest};

const UID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

#[test]
fn session_dates_accept_both_epoch_units_without_a_kg_upgrade() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    for (session, timestamp, expected) in [
        ("seconds", 1_792_920_379, "2026-10-25T09:26:19+00:00"),
        (
            "milliseconds",
            1_792_920_379_589,
            "2026-10-25T09:26:19.589+00:00",
        ),
    ] {
        let graph = ProvenanceGraph::builder(session, "opencode")
            .timestamp(timestamp)
            .build();
        let bytes = graph.serialize().unwrap();
        let hash = repo.save_provenance_graph(&graph).unwrap();
        let id = iri::mint_turn(&repo.project_id().unwrap(), session, 0);
        assert_eq!(
            repo.vault_kg_node(&id).unwrap().unwrap().metadata.unwrap()["created_at"],
            expected
        );
        assert_eq!(
            repo.load_provenance_graph(&hash)
                .unwrap()
                .serialize()
                .unwrap(),
            bytes
        );
    }
    assert_eq!(
        repo.pristine().read_txn().unwrap().kg_generation().unwrap(),
        2
    );
}

#[test]
fn readonly_queries_correct_cached_turn_dates_without_reindexing_or_writes() {
    use atomic_core::pristine::tables::{KG_INDEX_META, KG_NODES};
    use redb::ReadableDatabase;

    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    let graph = ProvenanceGraph::builder("legacy-timestamp", "opencode")
        .timestamp(1_792_920_379_589)
        .build();
    let hash = repo.save_provenance_graph(&graph).unwrap();
    let manifest_head = repo.get_session_head("legacy-timestamp").unwrap();
    let id = iri::mint_turn(&repo.project_id().unwrap(), "legacy-timestamp", 0);
    let mut cached = repo.vault_kg_node(&id).unwrap().unwrap();
    cached.metadata.as_mut().unwrap()["created_at"] =
        serde_json::json!("+58785-05-21T06:46:29+00:00");
    let cached_bytes = serde_json::to_vec(&cached).unwrap();
    let db_path = repo.dot_dir().join("atomic.redb");
    drop(repo);
    {
        let db = redb::Database::open(&db_path).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_table(KG_NODES)
            .unwrap()
            .insert(id.as_str(), cached_bytes.as_slice())
            .unwrap();
        txn.commit().unwrap();
    }

    let repo = Repository::open_readonly(directory.path()).unwrap();
    let expected = "2026-10-25T09:26:19.589+00:00";
    assert_eq!(
        repo.vault_kg_node(&id).unwrap().unwrap().metadata.unwrap()["created_at"],
        expected
    );
    let snapshot = repo.workflow_snapshot().unwrap();
    let turn = snapshot.nodes.iter().find(|node| node.id == id).unwrap();
    assert_eq!(turn.metadata.as_ref().unwrap()["created_at"], expected);
    assert_eq!(snapshot.generation, 2);
    assert_eq!(
        repo.get_session_head("legacy-timestamp").unwrap(),
        manifest_head
    );
    assert_eq!(repo.load_provenance_graph(&hash).unwrap(), graph);
    drop(repo);

    // Both the old cached date and the generation marker remain byte-for-byte
    // untouched: correcting the returned date must not require writable open.
    let db = redb::Database::open(&db_path).unwrap();
    let txn = db.begin_read().unwrap();
    assert_eq!(
        txn.open_table(KG_NODES)
            .unwrap()
            .get(id.as_str())
            .unwrap()
            .unwrap()
            .value(),
        cached_bytes
    );
    assert_eq!(
        txn.open_table(KG_INDEX_META)
            .unwrap()
            .get("kg_generation")
            .unwrap()
            .unwrap()
            .value(),
        2
    );
}

#[test]
fn references_never_take_ownership_of_another_intents_children() {
    for reference_first in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        repo.init_vault().unwrap();
        let project = repo.project_id().unwrap();
        let owner = format!("intents/{UID}/intent.md");
        let reference = || {
            repo.vault_store(
                "memory/reference.md",
                VaultEntryType::Memory,
                b"Reference, not ownership.".to_vec(),
                serde_json::json!({"derivedFrom":["urn:atomic:ac:target-ac", "urn:atomic:task:target-task"]}).to_string(),
            ).unwrap();
        };
        if reference_first {
            reference();
        }
        repo.vault_store(
            &owner,
            VaultEntryType::Intent,
            b":::why\nOwner.\n:::\n:::acceptance-criterion{#target-ac status=met verifiedBy=review evidence=test}\nAuthoritative acceptance text.\n:::\n:::task{#target-task status=done criteria=target-ac}\nAuthoritative task text.\n:::\n".to_vec(),
            serde_json::json!({"uid":UID,"id":"TEST::owner::1","title":"Owner","status":"in_progress"}).to_string(),
        ).unwrap();
        if !reference_first {
            reference();
        }
        let ac = iri::mint_child(
            &project,
            UID,
            iri::ChildKind::AcceptanceCriterion,
            "target-ac",
        )
        .unwrap();
        let task = iri::mint_child(&project, UID, iri::ChildKind::Task, "target-task").unwrap();
        let assert_children = || {
            for (id, label, status) in [
                (&ac, "Authoritative acceptance text.", "met"),
                (&task, "Authoritative task text.", "done"),
            ] {
                let node = repo.vault_kg_node(id).unwrap().unwrap();
                assert_eq!(node.source, "vault");
                assert_eq!(node.label, label);
                let metadata = node.metadata.unwrap();
                assert_eq!(metadata["status"], status);
                assert_eq!(metadata["derived_from_vault_path"], owner);
            }
            let node = repo.vault_kg_node(&ac).unwrap().unwrap();
            assert_eq!(node.metadata.as_ref().unwrap()["evidence"], "test");
        };
        assert_children();
        repo.vault_reindex_kg().unwrap();
        assert_children();
        repo.rebuild_kg(true).unwrap();
        assert_children();
        repo.vault_delete("memory/reference.md").unwrap();
        assert_children();
        reference();
        repo.vault_delete(&owner).unwrap();
        assert!(repo.vault_kg_node(&ac).unwrap().is_none());
        assert!(repo.vault_kg_node(&task).unwrap().is_none());
        let source = iri::mint_memory(&project, "reference");
        assert!(repo
            .vault_kg_node(&iri::mint_finding(&project, "UnresolvedReference", &source))
            .unwrap()
            .is_some());
    }
}

#[test]
fn received_changes_repair_vault_references_without_changing_view_membership() {
    for change_first in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        repo.init_vault().unwrap();
        let change = Change::empty(ChangeHeader::new("late dependency"));
        let mut bytes = Vec::new();
        let hash = change.serialize(&mut bytes).unwrap();
        if change_first {
            repo.import_change_bytes(&hash, &bytes).unwrap();
        }
        repo.vault_store(
            "memory/source.md",
            VaultEntryType::Memory,
            b"Source.".to_vec(),
            serde_json::json!({"derivedFrom":[format!("urn:atomic:change:{}", hash.to_base32())]})
                .to_string(),
        )
        .unwrap();
        let project = repo.project_id().unwrap();
        let source = iri::mint_memory(&project, "source");
        let finding = iri::mint_finding(&project, "UnresolvedReference", &source);
        assert_eq!(
            repo.vault_kg_node(&finding).unwrap().is_some(),
            !change_first
        );
        for _ in 0..2 {
            repo.import_change_bytes(&hash, &bytes).unwrap();
            let snapshot = repo.workflow_snapshot().unwrap();
            assert!(snapshot.edges.iter().any(|edge| edge.from_id == source
                && edge.to_id == iri::mint_change(&hash)
                && edge.kind == P::ProvWasDerivedFrom));
            assert!(repo.vault_kg_node(&finding).unwrap().is_none());
            assert!(!snapshot
                .edges
                .iter()
                .any(|edge| edge.from_id == iri::mint_change(&hash)
                    && edge.kind == P::DirectlyOnView));
        }
    }
}

#[test]
fn late_dependencies_repair_change_edges_and_remove_only_resolved_findings() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    repo.init_kg().unwrap();
    let mut objects = Vec::new();
    for name in ["first parent", "second parent"] {
        let mut bytes = Vec::new();
        let hash = Change::empty(ChangeHeader::new(name))
            .serialize(&mut bytes)
            .unwrap();
        objects.push((hash, bytes));
    }
    // Change headers contain timestamps, so fixture creation order is not a
    // canonical dependency order. Serialization correctly rejects unsorted hashes.
    objects.sort_by_key(|(hash, _)| *hash);
    let child = Change::new(
        ChangeHeader::new("child"),
        vec![],
        vec![],
        objects.iter().map(|(hash, _)| *hash).collect(),
    );
    let mut bytes = Vec::new();
    let hash = child.serialize(&mut bytes).unwrap();
    repo.import_change_bytes(&hash, &bytes).unwrap();
    let project = repo.project_id().unwrap();
    let child = iri::mint_change(&hash);
    let finding = iri::mint_finding(&project, "UnresolvedReference", &child);
    assert!(repo.vault_kg_node(&finding).unwrap().is_some());
    for (i, (hash, bytes)) in objects.iter().enumerate() {
        repo.import_change_bytes(hash, bytes).unwrap();
        assert!(repo
            .workflow_snapshot()
            .unwrap()
            .edges
            .iter()
            .any(|edge| edge.from_id == child
                && edge.to_id == iri::mint_change(hash)
                && edge.kind == P::DirectlyDependsOnChange));
        assert_eq!(repo.vault_kg_node(&finding).unwrap().is_some(), i == 0);
    }
}

#[test]
fn canonical_references_are_exact_and_ambiguous_legacy_aliases_fail_closed() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    repo.init_vault().unwrap();
    for name in ["A", "a"] {
        repo.vault_store(
            &format!("memory/{name}.md"),
            VaultEntryType::Memory,
            name.as_bytes().to_vec(),
            "{}".into(),
        )
        .unwrap();
    }
    let project = repo.project_id().unwrap();
    let source = iri::mint_memory(&project, "source");
    let upper = iri::mint_memory(&project, "A");
    repo.vault_store(
        "memory/source.md",
        VaultEntryType::Memory,
        b"Exact canonical target".to_vec(),
        serde_json::json!({"derivedFrom":[upper]}).to_string(),
    )
    .unwrap();
    for full in [false, true] {
        if full {
            repo.rebuild_kg(true).unwrap();
        }
        let edges = repo.workflow_snapshot().unwrap().edges;
        assert!(edges.iter().any(|edge| edge.from_id == source
            && edge.to_id == upper
            && edge.kind == P::ProvWasDerivedFrom));
        assert!(!edges.iter().any(|edge| edge.from_id == source
            && edge.to_id == iri::mint_memory(&project, "a")
            && edge.kind == P::ProvWasDerivedFrom));
    }
    repo.vault_store(
        "memory/source.md",
        VaultEntryType::Memory,
        b"Ambiguous legacy alias".to_vec(),
        serde_json::json!({"derivedFrom":["urn:atomic:memory:a"]}).to_string(),
    )
    .unwrap();
    assert!(!repo
        .workflow_snapshot()
        .unwrap()
        .edges
        .iter()
        .any(|edge| edge.from_id == source && edge.kind == P::ProvWasDerivedFrom));
    assert!(repo
        .vault_kg_node(&iri::mint_finding(&project, "UnresolvedReference", &source))
        .unwrap()
        .is_some());
    repo.vault_delete("memory/a.md").unwrap();
    assert!(repo
        .workflow_snapshot()
        .unwrap()
        .edges
        .iter()
        .any(|edge| edge.from_id == source
            && edge.to_id == upper
            && edge.kind == P::ProvWasDerivedFrom));
    assert!(repo
        .vault_kg_node(&iri::mint_finding(&project, "UnresolvedReference", &source))
        .unwrap()
        .is_none());
}

#[test]
fn later_intent_ingestion_repairs_session_links_and_deletion_restores_gap_findings() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    repo.init_vault().unwrap();
    let hash = atomic_core::types::Hash::of(b"historical generated change");
    repo.save_provenance_graph(
        &ProvenanceGraph::builder("late-plan", "opencode")
            .plan_id(UID)
            .add_change_explained(hash)
            .build(),
    )
    .unwrap();
    let project = repo.project_id().unwrap();
    let change = iri::mint_change(&hash);
    let missing = iri::mint_finding(&project, "MissingPlanId", &change);
    assert!(repo.vault_kg_node(&missing).unwrap().is_some());
    let path = format!("intents/{UID}/intent.md");
    repo.vault_store(&path, VaultEntryType::Intent, b":::why\nResolve an old turn.\n:::\n".to_vec(),
        serde_json::json!({"uid":UID,"id":"TEST::alice::1","title":"Late plan","status":"in_progress","kind":"feature"}).to_string()).unwrap();
    repo.vault_reconcile_intent_manifest().unwrap();
    repo.vault_reindex_kg().unwrap();
    let intent = iri::mint_intent(&project, UID).unwrap();
    assert!(repo
        .workflow_snapshot()
        .unwrap()
        .edges
        .iter()
        .any(|edge| edge.from_id == change && edge.kind == P::Implements && edge.to_id == intent));
    assert!(repo.vault_kg_node(&missing).unwrap().is_none());
    repo.vault_delete(&path).unwrap();
    assert!(repo.vault_kg_node(&intent).unwrap().is_none());
    assert!(repo.vault_kg_node(&missing).unwrap().is_some());
    assert!(!repo
        .workflow_snapshot()
        .unwrap()
        .edges
        .iter()
        .any(|edge| edge.from_id == change && edge.kind == P::Implements));
}

#[test]
fn legacy_memory_references_resolve_case_insensitively_and_repair_when_targets_arrive() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    repo.init_vault().unwrap();
    repo.vault_store(
        "memory/source.md",
        VaultEntryType::Memory,
        b"source".to_vec(),
        serde_json::json!({"derivedFrom":["urn:atomic:memory:TaRgEt"]}).to_string(),
    )
    .unwrap();
    let project = repo.project_id().unwrap();
    let source = iri::mint_memory(&project, "source");
    assert!(repo
        .vault_kg_node(&iri::mint_finding(&project, "UnresolvedReference", &source))
        .unwrap()
        .is_some());
    repo.vault_store(
        "memory/target.md",
        VaultEntryType::Memory,
        b"target".to_vec(),
        "{}".into(),
    )
    .unwrap();
    let snapshot = repo.workflow_snapshot().unwrap();
    assert!(snapshot.edges.iter().any(|edge| edge.from_id == source
        && edge.to_id == iri::mint_memory(&project, "target")
        && edge.kind == P::ProvWasDerivedFrom));
    assert!(repo
        .vault_kg_node(&iri::mint_finding(&project, "UnresolvedReference", &source))
        .unwrap()
        .is_none());
    assert!(snapshot
        .nodes
        .iter()
        .all(|node| node.types.iter().all(|class| C::from_iri(class).is_some())));
}

#[test]
fn membership_only_manifest_unrecord_and_reinsert_update_direct_view_facts() {
    let directory = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(directory.path()).unwrap();
    repo.init_kg().unwrap();
    repo.create_shared_view("mirror").unwrap();
    let change = Change::new(
        ChangeHeader::new("Already stored object"),
        vec![],
        vec![],
        vec![],
    );
    let mut bytes = Vec::new();
    let hash = change.serialize(&mut bytes).unwrap();
    repo.import_change_bytes(&hash, &bytes).unwrap();
    let project = repo.project_id().unwrap();
    let change_id = iri::mint_change(&hash);
    let view = iri::mint_view(&project, "mirror");
    let membership = |repo: &Repository| {
        repo.workflow_snapshot().unwrap().edges.iter().any(|edge| {
            edge.from_id == change_id && edge.to_id == view && edge.kind == P::DirectlyOnView
        })
    };
    assert!(!membership(&repo));
    let manifest = ViewManifest {
        name: "mirror".into(),
        scope: atomic_core::pristine::ViewScope::Shared,
        parent: None,
        changes: vec![hash],
        state: Merkle::ZERO.next(&hash),
    };
    assert_eq!(repo.reconcile_view_manifest(&manifest).unwrap().replayed, 1);
    assert!(membership(&repo));
    assert_eq!(repo.reconcile_view_manifest(&manifest).unwrap().replayed, 0);
    repo.unrecord(
        &hash,
        UnrecordOptions {
            view: Some("mirror".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!membership(&repo));
    assert!(repo
        .pristine()
        .read_txn()
        .unwrap()
        .get_kg_node(&change_id)
        .unwrap()
        .is_some());
    repo.reinsert_change_on_view("mirror", &hash, None).unwrap();
    assert!(membership(&repo));
}
