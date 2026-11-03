use atomic_core::change::{
    Atom, Change, ChangeHeader, EdgeUpdate, FileOps, GraphOp, Insertion, Local, NewEdge,
};
use atomic_core::crdt::TrunkId;
use atomic_core::kg::iri;
use atomic_core::types::{Base32, ChangePosition, EdgeFlags, GraphNode, Hash, NodeId, Position};
use atomic_repository::patch_evidence::{
    PatchEvidenceLimits, PatchEvidenceStatus, RemovedPreimageStatus, MAX_EVIDENCE_CONTENT_BYTES,
    MAX_EVIDENCE_OBJECT_BYTES, MAX_EVIDENCE_OPERATIONS, MAX_EVIDENCE_OPERATION_BYTES,
};
use atomic_repository::{execute_plan, parse_plan, QueryPlan, QueryStep, Repository};

fn fixture(content: &[u8]) -> Change {
    let insertion = Insertion {
        predecessors: vec![],
        successors: vec![],
        flag: EdgeFlags::BLOCK,
        start: ChangePosition::new(0),
        end: ChangePosition::new(content.len() as u64),
        inode: Position::new(None, ChangePosition::new(0)),
    };
    Change::with_file_ops(
        ChangeHeader::builder().message("recorded evidence").build(),
        vec![GraphOp::Edit {
            change: Atom::Insertion(insertion),
            local: Local::new("evidence.txt", 1),
            encoding: None,
        }],
        vec![FileOps::create(
            TrunkId::new(NodeId::new(1), 0),
            "evidence.txt".into(),
            None,
        )],
        content.to_vec(),
        vec![],
    )
}

#[test]
fn patch_evidence_is_recorded_and_unchanged_after_working_copy_edits() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let change = fixture(b"original\0\xff\n");
    let hash = repo.save_change(&change).unwrap();
    let change_iri = iri::mint_change(&hash);
    let plan = parse_plan(
        &serde_json::json!({
            "schema_version": 1,
            "steps": [{"type": "patch_evidence", "change_iri": change_iri}]
        })
        .to_string(),
    )
    .unwrap();
    let before = execute_plan(&repo, &plan).unwrap();
    std::fs::write(dir.path().join("evidence.txt"), b"MUTABLE UNRELATED BYTES").unwrap();
    let after = execute_plan(&repo, &plan).unwrap();
    assert_eq!(before.patch_evidence, after.patch_evidence);
    assert!(after.nodes.is_empty());
    assert!(after.edges.is_empty());
    assert!(after.content.is_empty());
    let evidence = &after.patch_evidence[0];
    assert_eq!(evidence.status, PatchEvidenceStatus::Found);
    assert_eq!(
        evidence.graph_ops.as_ref().unwrap().operations,
        change.hashed.hunks
    );
    assert_eq!(
        evidence.file_ops.as_ref().unwrap().operations,
        change.file_ops()
    );
    assert_eq!(
        evidence.content_blob.as_ref().unwrap().bytes,
        change.contents
    );
    assert!(!evidence.content_blob.as_ref().unwrap().truncated);
    assert_eq!(
        evidence.removed_preimage,
        RemovedPreimageStatus::Unsupported
    );
    // Exercise the verified disk path too, not only the in-memory store cache.
    drop(repo);
    let reopened = Repository::open(dir.path()).unwrap();
    assert_eq!(
        evidence,
        &reopened
            .patch_evidence(&change_iri, PatchEvidenceLimits::default())
            .unwrap()
    );
}

#[test]
fn patch_evidence_rejects_noncanonical_iris_and_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let hash = Hash::of(b"identity");
    let canonical = iri::mint_change(&hash);
    for invalid in [
        hash.to_base32(),
        canonical.to_lowercase(),
        canonical[..canonical.len() - 1].into(),
        format!("{canonical}A"),
        format!("{canonical}#fragment"),
        iri::mint_provenance(&hash),
        "change:ABC".into(),
        "$results".into(),
    ] {
        assert!(
            repo.patch_evidence(&invalid, PatchEvidenceLimits::default())
                .is_err(),
            "{invalid}"
        );
    }
    // 52 Base32 characters with non-zero padding bits are not a canonical hash.
    let mut noncanonical_padding = canonical.clone();
    noncanonical_padding.pop();
    noncanonical_padding.push('B');
    assert!(repo
        .patch_evidence(&noncanonical_padding, PatchEvidenceLimits::default())
        .is_err());
    assert_eq!(
        repo.patch_evidence(&canonical, PatchEvidenceLimits::default())
            .unwrap()
            .status,
        PatchEvidenceStatus::Missing
    );
}

#[test]
fn patch_evidence_missing_is_not_empty_or_unsupported_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let missing = repo
        .patch_evidence(
            &iri::mint_change(&Hash::of(b"absent")),
            PatchEvidenceLimits::default(),
        )
        .unwrap();
    assert_eq!(missing.status, PatchEvidenceStatus::Missing);
    assert!(missing.graph_ops.is_none());
    assert!(missing.file_ops.is_none());
    assert!(missing.content_blob.is_none());
    let hash = repo
        .save_change(&Change::empty(
            ChangeHeader::builder().message("empty").build(),
        ))
        .unwrap();
    let empty = repo
        .patch_evidence(&iri::mint_change(&hash), PatchEvidenceLimits::default())
        .unwrap();
    assert_eq!(empty.status, PatchEvidenceStatus::Found);
    let operations = empty.file_ops.unwrap();
    assert_eq!(operations.total, 0);
    assert!(!operations.truncated);
    assert_eq!(empty.content_blob.unwrap().total_bytes, 0);
}

#[test]
fn patch_evidence_preserves_removed_references_without_inventing_preimage_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let dependency = repo.save_change(&fixture(b"historical preimage")).unwrap();
    let deletion = EdgeUpdate {
        inode: Position::new(Some(dependency), ChangePosition::new(0)),
        edges: vec![NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: Position::new(Some(dependency), ChangePosition::new(0)),
            to: GraphNode::new(
                Some(dependency),
                ChangePosition::new(0),
                ChangePosition::new(19),
            ),
            introduced_by: Some(dependency),
        }],
    };
    let change = Change::with_file_ops(
        ChangeHeader::builder().message("delete evidence").build(),
        vec![GraphOp::FileDel {
            del: deletion.clone(),
            contents: Some(deletion),
            path: "evidence.txt".into(),
            encoding: None,
        }],
        vec![FileOps::delete(
            TrunkId::new(NodeId::new(1), 0),
            "evidence.txt".into(),
        )],
        vec![],
        vec![dependency],
    );
    let hash = repo.save_change(&change).unwrap();
    std::fs::write(dir.path().join("evidence.txt"), b"not historical evidence").unwrap();
    let evidence = repo
        .patch_evidence(&iri::mint_change(&hash), PatchEvidenceLimits::default())
        .unwrap();
    assert_eq!(evidence.graph_ops.unwrap().operations, change.hashed.hunks);
    assert_eq!(evidence.file_ops.unwrap().operations, change.file_ops());
    assert!(evidence.content_blob.unwrap().bytes.is_empty());
    assert_eq!(
        evidence.removed_preimage,
        RemovedPreimageStatus::Unsupported
    );
}

#[test]
fn patch_evidence_enforces_count_byte_and_content_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let change = fixture(&vec![0xff; MAX_EVIDENCE_CONTENT_BYTES + 1]);
    let hash = repo.save_change(&change).unwrap();
    let change_iri = iri::mint_change(&hash);
    let limits = PatchEvidenceLimits {
        max_operations: 1,
        max_operation_bytes: MAX_EVIDENCE_OPERATION_BYTES,
        max_content_bytes: 3,
    };
    let bounded = repo.patch_evidence(&change_iri, limits).unwrap();
    assert_eq!(bounded.graph_ops.as_ref().unwrap().operations.len(), 1);
    assert!(bounded.file_ops.as_ref().unwrap().truncated);
    let blob = bounded.content_blob.unwrap();
    assert_eq!(blob.bytes, vec![0xff; 3]);
    assert_eq!(blob.total_bytes, change.contents.len());
    assert!(blob.truncated);
    let none = repo
        .patch_evidence(
            &change_iri,
            PatchEvidenceLimits {
                max_operations: 0,
                max_operation_bytes: 0,
                max_content_bytes: 0,
            },
        )
        .unwrap();
    assert!(none.graph_ops.unwrap().truncated);
    assert!(none.file_ops.unwrap().truncated);
    assert!(none.content_blob.unwrap().truncated);
    // A single operation never leaks past the JSON byte budget or gets split.
    let graph_bytes = serde_json::to_vec(&change.hashed.hunks[0]).unwrap().len();
    let exact = repo
        .patch_evidence(
            &change_iri,
            PatchEvidenceLimits {
                max_operation_bytes: graph_bytes,
                ..limits
            },
        )
        .unwrap();
    assert_eq!(exact.graph_ops.unwrap().operations.len(), 1);
    let tight = repo
        .patch_evidence(
            &change_iri,
            PatchEvidenceLimits {
                max_operation_bytes: 1,
                ..limits
            },
        )
        .unwrap();
    assert!(tight.graph_ops.unwrap().operations.is_empty());
    assert!(tight.file_ops.unwrap().operations.is_empty());
    for invalid in [
        PatchEvidenceLimits {
            max_operations: MAX_EVIDENCE_OPERATIONS + 1,
            ..limits
        },
        PatchEvidenceLimits {
            max_operation_bytes: MAX_EVIDENCE_OPERATION_BYTES + 1,
            ..limits
        },
        PatchEvidenceLimits {
            max_content_bytes: MAX_EVIDENCE_CONTENT_BYTES + 1,
            ..limits
        },
    ] {
        assert!(repo.patch_evidence(&change_iri, invalid).is_err());
    }
}

#[test]
fn patch_evidence_oversized_source_is_explicitly_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let hash = Hash::of(b"oversized");
    let path = repo.change_store().change_path(&hash);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::File::create(path)
        .unwrap()
        .set_len(MAX_EVIDENCE_OBJECT_BYTES + 1)
        .unwrap();
    let result = repo
        .patch_evidence(&iri::mint_change(&hash), PatchEvidenceLimits::default())
        .unwrap();
    assert_eq!(result.status, PatchEvidenceStatus::Unsupported);
    assert!(result.content_blob.is_none());
}

#[test]
fn patch_evidence_corrupt_or_wrong_hash_object_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let actual = repo.save_change(&fixture(b"authentic")).unwrap();
    let wrong = Hash::of(b"wrong identity");
    let wrong_path = repo.change_store().change_path(&wrong);
    std::fs::create_dir_all(wrong_path.parent().unwrap()).unwrap();
    std::fs::copy(repo.change_store().change_path(&actual), &wrong_path).unwrap();
    assert!(repo
        .patch_evidence(&iri::mint_change(&wrong), PatchEvidenceLimits::default())
        .is_err());
    std::fs::write(wrong_path, b"not an ATOM object").unwrap();
    assert!(repo
        .patch_evidence(&iri::mint_change(&wrong), PatchEvidenceLimits::default())
        .is_err());
}

#[test]
fn patch_evidence_query_step_has_strict_fields_missing_status_and_fanout_bound() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let change_iri = iri::mint_change(&Hash::of(b"missing"));
    let step = QueryStep::PatchEvidence {
        change_iri: change_iri.clone(),
        limits: PatchEvidenceLimits::default(),
    };
    let result = execute_plan(
        &repo,
        &QueryPlan {
            schema_version: 1,
            steps: vec![step.clone()],
        },
    )
    .unwrap();
    assert_eq!(
        result.patch_evidence[0].status,
        PatchEvidenceStatus::Missing
    );
    assert_eq!(result.step_stats[0].result_count, 0);
    assert!(execute_plan(
        &repo,
        &QueryPlan {
            schema_version: 1,
            steps: vec![step; 9]
        }
    )
    .is_err());
    for extra in [
        serde_json::json!({"path": "evidence.txt"}),
        serde_json::json!({"bind": "x"}),
    ] {
        let mut value = serde_json::json!({"type": "patch_evidence", "change_iri": change_iri});
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(parse_plan(
            &serde_json::json!({"schema_version": 1, "steps": [value]}).to_string()
        )
        .is_err());
    }
    assert!(parse_plan(&serde_json::json!({"schema_version":1,"steps":[{"type":"patch_evidence","change_iri":change_iri,"limits":{"unknown":1}}]}).to_string()).is_err());
}
