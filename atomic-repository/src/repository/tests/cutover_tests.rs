use super::*;

use atomic_core::operation::{
    ActorRef, MetadataTarget, MetadataTransition, MetadataValue, OperationKind, OperationScope,
    RepoStateRef,
};
use atomic_core::pristine::{
    CapabilityMutTxnT, CapabilityTxnT, RefMapping, RefSyncStatus, RequiredRepositoryCapability,
    BRIDGE_CUTOVER_CAPABILITY, CHANGE_FORMAT_VNEXT_CAPABILITY, REF_MAPPING_VERSION,
    SUPPORTED_REPOSITORY_CAPABILITIES,
};
use atomic_core::operation::EffectReceiptKind;
use atomic_core::OperationId;

use crate::RepositoryError;

use std::fs;

use super::operation::{current_operation_timestamp_ms, working_copy_state_ref};

const CUTOVER_ID: &str = "git-bridge-cutover";
const VNEXT_ID: &str = "change-format-vnext";

// ── CB-13B R6 readiness fixtures ─────────────────────────────────────────

/// Create a real colocated Git repository with one deterministic commit and
/// return its HEAD oid. Replaces the empty-`.git`-passes corpus: an empty
/// directory, an arbitrary file, and a dangling symlink must all refuse.
/// The initial branch is the given view's mapped ref target, so a Shared
/// view's candidate binding resolves.
fn init_real_git_repo(root: &std::path::Path, view: &str) -> String {
    let run = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_DATE", "@1726732800 +0000")
            .env("GIT_COMMITTER_DATE", "@1726732800 +0000")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(&["init", "-b", view]);
    run(&["config", "user.name", "Atomic Test"]);
    run(&["config", "user.email", "atomic@example.com"]);
    std::fs::write(root.join("README.md"), "cutover readiness fixture\n").unwrap();
    run(&["add", "README.md"]);
    run(&["commit", "--no-gpg-sign", "-m", "initial"]);
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("resolve HEAD");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Install the two bridge evidence proofs on top of a real Git repository:
/// the verified workspace checkpoint (bridge-enabled equivalence, its
/// `git_head` matching the live HEAD) and the current view's candidate
/// binding (a stored ref mapping whose local ref exists in Git).
fn install_bridge_evidence(repo: &TestRepository, root: &std::path::Path, head_oid: &str) {
    use super::super::workspace_txn::{write_workspace_checkpoint, WorkspaceCheckpoint};
    let view = repo.current_view().to_string();
    let record = repo
        .working_copy_record(repo.require_working_copy_id().unwrap())
        .unwrap();
    let state = record.desired_state;
    write_workspace_checkpoint(
        root,
        &WorkspaceCheckpoint {
            version: 2,
            view: view.clone(),
            atomic_state: state.to_string(),
            git_head_symref: None,
            git_head: head_oid.to_string(),
            git_tree: "tree".to_string(),
            git_index_tree: None,
            git_index_digest: None,
        },
    )
    .unwrap();

    let (view_id, scope) = {
        let txn = repo.pristine.read_txn().unwrap();
        let view_state = txn.get_view(&view).unwrap().expect("current view exists");
        (view_state.id, view_state.kind)
    };
    let local_ref = super::super::ref_mapping::mapped_local_ref(scope, &view);
    repo.set_ref_mapping(
        repo.require_working_copy_id().unwrap(),
        &view,
        Some(RefMapping {
            version: REF_MAPPING_VERSION,
            view_id,
            view_name: view.clone(),
            scope: scope as u8,
            local_ref: local_ref.clone(),
            remote: None,
            last_observed_local: None,
            last_observed_remote: None,
            last_exported: None,
            last_exported_state: None,
            last_observed_atomic: None,
            status: RefSyncStatus::Synchronized,
        }),
    )
    .unwrap();
    // The bound ref must exist in the colocated repository for a real
    // binding proof; a Shared view publishes on refs/heads/<view>, which
    // the deterministic commit above created.
    if let Some(ref_name) = local_ref {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "--verify", &ref_name])
            .output()
            .expect("verify mapped ref");
        assert!(
            output.status.success(),
            "fixture must create the mapped ref {ref_name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn expect_cutover_refusal(repo: &TestRepository, needle: &str) {
    let error = match repo.plan_bridge_cutover() {
        Ok(_) => panic!("cutover must refuse on unverified readiness"),
        Err(RepositoryError::InvalidOperation { message }) => message,
        Err(other) => panic!("expected typed invalid-operation error, got {other}"),
    };
    assert!(
        error.contains(needle),
        "refusal must mention {needle:?}: {error}"
    );
    // NO fence: the refusal preserves the repository.
    assert_eq!(capability_row(repo, CUTOVER_ID), None);
}

fn capability_row(repo: &Repository, id: &str) -> Option<u32> {
    let txn = repo.pristine.read_txn().unwrap();
    txn.required_capability_version(id).unwrap()
}

fn cutover_operation_count(repo: &Repository, working_copy: WorkingCopyId) -> usize {
    let log = repo
        .operation_log(OperationScope::WorkingCopy(working_copy), None, false)
        .unwrap();
    log.entries
        .iter()
        .filter(|entry| entry.operation.payload().kind == OperationKind::Cutover)
        .count()
}

/// Prepare, apply and verify one journaled metadata operation carrying
/// exactly the given capability transition. This drives the real journaled
/// apply path — the same path replay and inverse recovery use.
fn apply_capability_lease(
    repo: &Repository,
    working_copy: WorkingCopyId,
    target: MetadataTarget,
    expected_old: MetadataValue,
    expected_new: MetadataValue,
) -> Result<OperationId, RepositoryError> {
    let operation_lock = repo.try_lock_operation(working_copy)?;
    let record = repo.working_copy_record(working_copy)?;
    let state = RepoStateRef {
        view: None,
        working_copy: Some(working_copy_state_ref(record)),
        git: None,
    };
    let operation = repo.prepare_metadata_operation(
        &operation_lock,
        OperationKind::Cutover,
        None,
        state.clone(),
        state,
        vec![MetadataTransition {
            target,
            expected_old,
            expected_new,
        }],
        Vec::new(),
        ActorRef::System {
            name: "cutover-regression".to_string(),
        },
        current_operation_timestamp_ms(),
    )?;
    repo.apply_operation_metadata_locked(&operation_lock, operation.id())?;
    repo.finalize_operation_verified(&operation_lock, operation.id())?;
    Ok(operation.id())
}

fn expect_invalid(result: Result<OperationId, RepositoryError>, needle: &str) {
    let error = match result {
        Ok(_) => panic!("capability lease must refuse"),
        Err(RepositoryError::InvalidOperation { message }) => message,
        Err(other) => panic!("expected typed invalid-operation error, got {other}"),
    };
    assert!(
        error.contains(needle),
        "error message must mention {needle:?}: {error}"
    );
}

#[test]
fn capability_lease_writes_exactly_the_requested_version() {
    // R3 regression (failing-before): a lease for Sequence(0) must write 0.
    // The previous implementation passed the range check but stored the
    // build's supported version (1) instead of the leased 0.
    let (_temp, repo) = create_temp_repo();
    let working_copy = repo.require_working_copy_id().unwrap();
    assert_eq!(capability_row(&repo, CUTOVER_ID), None);

    apply_capability_lease(
        &repo,
        working_copy,
        MetadataTarget::Capability {
            id: CUTOVER_ID.to_string(),
        },
        MetadataValue::Absent,
        MetadataValue::Sequence(0),
    )
    .unwrap();

    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(0));
}

#[test]
fn capability_lease_replays_idempotently_on_the_exact_version() {
    // Replay of an already-applied exact lease is AlreadyApplied: the row
    // keeps the leased value and the replay does not diverge. The previous
    // implementation stored 1 for a leased 0, so replay saw a third value.
    let (_temp, repo) = create_temp_repo();
    let working_copy = repo.require_working_copy_id().unwrap();
    let operation_id = apply_capability_lease(
        &repo,
        working_copy,
        MetadataTarget::Capability {
            id: CUTOVER_ID.to_string(),
        },
        MetadataValue::Absent,
        MetadataValue::Sequence(0),
    )
    .unwrap();
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(0));

    let operation_lock = repo.try_lock_operation(working_copy).unwrap();
    repo.apply_operation_metadata_locked(&operation_lock, operation_id)
        .unwrap();
    drop(operation_lock);

    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(0));
}

#[test]
fn capability_lease_lowers_to_the_leased_version_exactly() {
    // R3 inverse regression (failing-before): a lowering lease 1 → 0 must
    // write 0. The raise-only helper silently kept 1 and reported success.
    let (_temp, repo) = create_temp_repo();
    let working_copy = repo.require_working_copy_id().unwrap();
    repo.pristine
        .require_repository_capability(BRIDGE_CUTOVER_CAPABILITY)
        .unwrap();
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(1));

    apply_capability_lease(
        &repo,
        working_copy,
        MetadataTarget::Capability {
            id: CUTOVER_ID.to_string(),
        },
        MetadataValue::Sequence(1),
        MetadataValue::Sequence(0),
    )
    .unwrap();

    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(0));
}

#[test]
fn cutover_rollback_restores_the_exact_prior_state_through_the_journal() {
    // The cutover operation is journaled and verified, then rolled back
    // through the typed inverse: the requirement row returns to Absent.
    let (temp, mut repo) = create_temp_repo();
    let head = init_real_git_repo(temp.path(), &repo.current_view().to_string());
    install_bridge_evidence(&repo, temp.path(), &head);
    let working_copy = repo.require_working_copy_id().unwrap();

    let outcome = repo.execute_bridge_cutover().unwrap();
    let cutover_operation = outcome.operation.expect("cutover must be journaled");
    assert!(!outcome.already_fenced);
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(1));

    repo.rollback_bridge_cutover().unwrap();
    assert_eq!(capability_row(&repo, CUTOVER_ID), None);

    // The inverse is a distinct journal child of the cutover, not an edit.
    let log = repo
        .operation_log(OperationScope::WorkingCopy(working_copy), None, false)
        .unwrap();
    assert!(log.entries.iter().any(|entry| {
        entry.operation.id() == cutover_operation
            && entry.operation.payload().kind == OperationKind::Cutover
    }));
    assert!(log.entries.iter().any(|entry| {
        entry.operation.payload().kind == OperationKind::Undo
            && entry
                .operation
                .payload()
                .relation
                .is_some_and(|relation| matches!(
                    relation,
                    atomic_core::operation::OperationRelation::Undo { target }
                        if target == cutover_operation
                ))
    }));

    // The rollback's own undo child is not the cutover: a second rollback
    // refuses instead of silently re-enabling the fence.
    let error = match repo.rollback_bridge_cutover() {
        Ok(_) => panic!("rollback must refuse a non-cutover head"),
        Err(RepositoryError::InvalidOperation { message }) => message,
        Err(other) => panic!("expected typed invalid-operation error, got {other}"),
    };
    assert!(error.contains("not the cutover"), "{error}");
}

#[test]
fn capability_lease_refuses_unsupported_values_without_mutation() {
    let (_temp, repo) = create_temp_repo();
    let working_copy = repo.require_working_copy_id().unwrap();
    let cutover_target = MetadataTarget::Capability {
        id: CUTOVER_ID.to_string(),
    };

    // A version above this build's supported maximum refuses before the
    // operation is prepared or journaled.
    expect_invalid(
        apply_capability_lease(
            &repo,
            working_copy,
            cutover_target.clone(),
            MetadataValue::Absent,
            MetadataValue::Sequence(2),
        ),
        "supports only through version 1",
    );
    assert_eq!(capability_row(&repo, CUTOVER_ID), None);

    // An unknown capability refuses; the row is never stored.
    expect_invalid(
        apply_capability_lease(
            &repo,
            working_copy,
            MetadataTarget::Capability {
                id: "future-format".to_string(),
            },
            MetadataValue::Absent,
            MetadataValue::Sequence(1),
        ),
        "does not support",
    );
    assert_eq!(capability_row(&repo, "future-format"), None);

    // A version outside the representable range refuses before mutation.
    expect_invalid(
        apply_capability_lease(
            &repo,
            working_copy,
            cutover_target,
            MetadataValue::Absent,
            MetadataValue::Sequence(u64::MAX),
        ),
        "representable capability version range",
    );
    assert_eq!(capability_row(&repo, CUTOVER_ID), None);
}

#[test]
fn unknown_capability_deletion_fails_closed_and_preserves_the_row() {
    // Deleting an unknown requirement would bypass a newer build's fence
    // during rollback; this build must refuse and preserve the row.
    let (_temp, repo) = create_temp_repo();
    let working_copy = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine.write_txn().unwrap();
        txn.put_required_capability_exact("future-format", 1)
            .unwrap();
        txn.commit().unwrap();
    }
    assert_eq!(capability_row(&repo, "future-format"), Some(1));

    expect_invalid(
        apply_capability_lease(
            &repo,
            working_copy,
            MetadataTarget::Capability {
                id: "future-format".to_string(),
            },
            MetadataValue::Sequence(1),
            MetadataValue::Absent,
        ),
        "preserves the requirement row",
    );
    assert_eq!(capability_row(&repo, "future-format"), Some(1));
}

#[test]
fn bridge_cutover_is_journaled_verified_and_fences_legacy_writers() {
    let (temp, repo) = create_temp_repo();

    // Without a colocated Git repository the cutover refuses with
    // remediation and preserves the repository.
    let error = match repo.execute_bridge_cutover() {
        Ok(_) => panic!("cutover without a colocated Git repository must refuse"),
        Err(RepositoryError::InvalidOperation { message }) => message,
        Err(other) => panic!("expected typed invalid-operation error, got {other}"),
    };
    assert!(error.contains("colocated Git repository"), "{error}");
    assert_eq!(capability_row(&repo, CUTOVER_ID), None);

    let head = init_real_git_repo(temp.path(), &repo.current_view().to_string());
    install_bridge_evidence(&repo, temp.path(), &head);
    let working_copy = repo.require_working_copy_id().unwrap();

    // Audit before the fence: nothing requires cutover yet, and the
    // readiness observations prove the real colocated repository.
    let audit = repo.audit_bridge_cutover().unwrap();
    assert_eq!(audit.cutover_requirement, None);
    assert!(audit.unsupported_requirements.is_empty());
    assert_eq!(audit.colocated_git.form, ColocatedGitForm::Repository);
    assert!(audit.readiness_refusals.is_empty());
    assert!(audit.path_claim_schema_version.is_some());
    assert!(audit
        .surfaces
        .iter()
        .any(|surface| surface.disposition == CutoverAuditDisposition::PreservedImmutable));

    let outcome = repo.execute_bridge_cutover().unwrap();
    assert!(!outcome.already_fenced);
    let cutover_operation = outcome.operation.expect("cutover must be journaled");
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(1));

    // The journaled cutover completed with a verified receipt.
    let details = repo.operation_details(cutover_operation).unwrap();
    assert_eq!(details.verification, OperationVerificationState::Verified);
    assert_eq!(details.operation.payload().kind, OperationKind::Cutover);

    // The fence is structural: the legacy shadow writer lock refuses
    // before taking its lock or mutating any state.
    let error = match repo.try_lock_shadow_commit() {
        Ok(_) => panic!("legacy shadow lock must refuse after cutover"),
        Err(RepositoryError::LegacyShadowWriterFenced { capability }) => capability,
        Err(other) => panic!("expected typed fenced error, got {other}"),
    };
    assert!(error.contains(CUTOVER_ID), "{error}");
    repo.require_legacy_shadow_write_allowed().unwrap_err();

    // Re-execution is an idempotent already-fenced no-op: no second
    // Cutover operation is prepared.
    let again = repo.execute_bridge_cutover().unwrap();
    assert!(again.already_fenced);
    assert!(again.operation.is_none());
    assert_eq!(cutover_operation_count(&repo, working_copy), 1);
}

#[test]
fn bridge_cutover_rollback_lifts_the_fence_and_allows_legacy_writers() {
    let (temp, mut repo) = create_temp_repo();
    let head = init_real_git_repo(temp.path(), &repo.current_view().to_string());
    install_bridge_evidence(&repo, temp.path(), &head);

    repo.execute_bridge_cutover().unwrap();
    assert!(matches!(
        repo.try_lock_shadow_commit(),
        Err(RepositoryError::LegacyShadowWriterFenced { .. })
    ));
    repo.rollback_bridge_cutover().unwrap();

    assert_eq!(capability_row(&repo, CUTOVER_ID), None);
    let guard = repo.try_lock_shadow_commit().unwrap();
    assert!(guard.is_some());
    drop(guard);
}

#[test]
fn bridge_cutover_rollback_refuses_a_foreign_head() {
    let (temp, mut repo) = create_temp_repo();
    let head = init_real_git_repo(temp.path(), &repo.current_view().to_string());
    install_bridge_evidence(&repo, temp.path(), &head);
    let working_copy = repo.require_working_copy_id().unwrap();
    repo.execute_bridge_cutover().unwrap();

    // A later verified lease for a different capability moves the shared
    // repository head away from the cutover; the typed rollback must
    // refuse it instead of undoing the newer operation (CB-13B R2: the
    // shared head is the rollback authority, whatever worktree moved it).
    apply_capability_lease(
        &repo,
        working_copy,
        MetadataTarget::Capability {
            id: VNEXT_ID.to_string(),
        },
        MetadataValue::Absent,
        MetadataValue::Sequence(1),
    )
    .unwrap();
    let error = match repo.rollback_bridge_cutover() {
        Ok(_) => panic!("rollback must refuse a non-cutover head"),
        Err(RepositoryError::InvalidOperation { message }) => message,
        Err(other) => panic!("expected typed invalid-operation error, got {other}"),
    };
    assert!(error.contains("not the cutover"), "{error}");
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(1));
}

#[test]
fn cutover_rollback_keeps_newer_required_format_fences() {
    // CB-13B R2: the inverse deletes only the cutover row. A required
    // format fence (change-format-vnext) recorded before the cutover
    // survives the rollback: newer objects keep their format requirement.
    let (temp, mut repo) = create_temp_repo();
    let head = init_real_git_repo(temp.path(), &repo.current_view().to_string());
    install_bridge_evidence(&repo, temp.path(), &head);
    let working_copy = repo.require_working_copy_id().unwrap();

    apply_capability_lease(
        &repo,
        working_copy,
        MetadataTarget::Capability {
            id: VNEXT_ID.to_string(),
        },
        MetadataValue::Absent,
        MetadataValue::Sequence(1),
    )
    .unwrap();

    repo.execute_bridge_cutover().unwrap();
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(1));
    assert_eq!(capability_row(&repo, VNEXT_ID), Some(1));

    repo.rollback_bridge_cutover().unwrap();
    assert_eq!(capability_row(&repo, CUTOVER_ID), None);
    // The newer format fence is kept.
    assert_eq!(capability_row(&repo, VNEXT_ID), Some(1));
}

// ── CB-13B R4: final-schema census, goldens and compatibility matrix ────

fn surface<'a>(
    final_schema: &'a [CutoverSchemaAudit],
    item: &str,
) -> &'a CutoverSchemaAudit {
    final_schema
        .iter()
        .find(|surface| surface.item == item)
        .unwrap_or_else(|| panic!("final-schema census must name {item:?}"))
}

#[test]
fn final_schema_census_names_every_contract_surface() {
    // R4: the recorded inventory for extra_known, ViewState/SetId,
    // unrecord suffixes, raw RepoPath, conflicts, FILE_INDEX_V2, worktree
    // identity, operation versions, attributes and remote capabilities —
    // each observed live and classified.
    let (temp, repo) = create_temp_repo();
    let head = init_real_git_repo(temp.path(), &repo.current_view().to_string());
    install_bridge_evidence(&repo, temp.path(), &head);
    let audit = repo.audit_bridge_cutover().unwrap();

    let expected: &[(&str, CutoverAuditDisposition)] = &[
        ("extra_known", CutoverAuditDisposition::PreservedImmutable),
        ("view-state-set-id", CutoverAuditDisposition::PreservedImmutable),
        ("unrecord-suffixes", CutoverAuditDisposition::PreservedImmutable),
        ("raw-repo-path", CutoverAuditDisposition::PreservedImmutable),
        ("conflicts", CutoverAuditDisposition::PreservedImmutable),
        ("file-index-v2", CutoverAuditDisposition::RebuildableCache),
        ("worktree-identity", CutoverAuditDisposition::PreservedImmutable),
        ("operation-versions", CutoverAuditDisposition::PreservedImmutable),
        ("attributes", CutoverAuditDisposition::PreservedImmutable),
        ("remote-capabilities", CutoverAuditDisposition::Refused),
        ("git-sha-index-bindings", CutoverAuditDisposition::Required),
        ("trailer-bindings", CutoverAuditDisposition::PreservedImmutable),
        ("aggregate-tag-bindings", CutoverAuditDisposition::Refused),
        ("same-name-ambiguity", CutoverAuditDisposition::PreservedImmutable),
    ];
    assert_eq!(audit.final_schema.len(), expected.len());
    for (item, disposition) in expected {
        assert_eq!(
            surface(&audit.final_schema, item).disposition,
            *disposition,
            "{item} disposition"
        );
    }
    // The observations are real, not prose: one working copy, the current
    // view's set id present, no conflicts yet.
    assert!(surface(&audit.final_schema, "worktree-identity")
        .detail
        .contains("1 working-copy record(s)"));
    assert!(surface(&audit.final_schema, "view-state-set-id")
        .detail
        .contains("SetId is"));
    // The digest folds the census: changing it changes the evidence.
    let digest = audit.evidence_digest();
    let mut changed = audit.clone();
    changed.final_schema[0].detail = "tampered".to_string();
    assert_ne!(changed.evidence_digest(), digest);
}

#[test]
fn cutover_preserves_legacy_object_bytes_and_hashes() {
    // R4 migration golden: every legacy object the cutover does not own —
    // content-addressed change files, GIT_SHA_INDEX rows, the view state
    // and working-copy records — keeps its exact bytes and hash across
    // the fence.
    let (temp, repo) = create_temp_repo();
    let head = init_real_git_repo(temp.path(), &repo.current_view().to_string());
    install_bridge_evidence(&repo, temp.path(), &head);

    // A durable legacy change object (content-addressed bytes on disk).
    repo.pristine
        .require_repository_capability(CHANGE_FORMAT_VNEXT_CAPABILITY)
        .unwrap();
    let change = create_test_change("legacy object golden");
    let hash = repo.save_change(&change).unwrap();

    let change_file = repo
        .changes_dir()
        .join(&hash.to_base32()[..2])
        .join(format!("{}.change", hash.to_base32()));
    let before_bytes = fs::read(&change_file).unwrap();
    let before_conflicts = {
        let txn = repo.pristine.read_txn().unwrap();
        txn.snapshot_conflicts().unwrap()
    };
    let before_shas = {
        let txn = repo.pristine.read_txn().unwrap();
        use atomic_core::pristine::GitShaIndexTxnT;
        txn.list_git_shas().unwrap()
    };
    let before_state = {
        let txn = repo.pristine.read_txn().unwrap();
        let view = txn.get_view(repo.current_view()).unwrap().unwrap();
        view.state
    };

    repo.execute_bridge_cutover().unwrap();

    let after_bytes = fs::read(&change_file).unwrap();
    assert_eq!(after_bytes, before_bytes, "change bytes are immutable");
    // The registered hash still resolves to the same immutable bytes.
    let reloaded = repo.load_change(&hash).unwrap();
    assert_eq!(reloaded.hashed.header.message, change.hashed.header.message);
    let after_conflicts = {
        let txn = repo.pristine.read_txn().unwrap();
        txn.snapshot_conflicts().unwrap()
    };
    assert_eq!(after_conflicts, before_conflicts);
    let after_shas = {
        let txn = repo.pristine.read_txn().unwrap();
        use atomic_core::pristine::GitShaIndexTxnT;
        txn.list_git_shas().unwrap()
    };
    assert_eq!(after_shas, before_shas);
    let after_state = {
        let txn = repo.pristine.read_txn().unwrap();
        let view = txn.get_view(repo.current_view()).unwrap().unwrap();
        view.state
    };
    assert_eq!(after_state, before_state, "the view state is untouched");
}

#[test]
fn hook_manager_symlinks_and_hookspath_are_active_surfaces() {
    // Matrix cell: hook managers that symlink dispatchers into hooks/ and
    // a configured core.hooksPath are both active surfaces.
    let (temp, repo) = create_temp_repo();
    let head = init_real_git_repo(temp.path(), &repo.current_view().to_string());
    install_bridge_evidence(&repo, temp.path(), &head);

    // A symlinked hook dispatcher.
    let target = temp.path().join("manager-dispatcher.sh");
    fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(
            &target,
            temp.path().join(".git/hooks/pre-push"),
        )
        .unwrap();
    }
    let readiness = observe_colocated_git_readiness(temp.path());
    assert!(
        readiness
            .active_hooks
            .iter()
            .any(|hook| hook.contains("pre-push")),
        "{:?}",
        readiness.active_hooks
    );
    expect_cutover_refusal(&repo, "hook");

    // A configured core.hooksPath (hook manager lease).
    fs::remove_file(temp.path().join(".git/hooks/pre-push")).unwrap();
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(temp.path())
        .args(["config", "core.hooksPath", "/opt/hook-manager"])
        .output()
        .unwrap();
    assert!(output.status.success());
    expect_cutover_refusal(&repo, "core.hooksPath");
}

#[test]
fn cutover_lock_contention_returns_a_typed_retry_error() {
    // Matrix cell: lock contention — the cutover holds no mutation when
    // the common operation lock is contended.
    let (temp, repo) = create_temp_repo();
    let head = init_real_git_repo(temp.path(), &repo.current_view().to_string());
    install_bridge_evidence(&repo, temp.path(), &head);

    // Hold the common operation lock; the cutover's lock acquisition
    // refuses with the typed contention error before any mutation.
    let _holder = repo.try_lock_common_operation().unwrap();
    let error = match repo.execute_bridge_cutover() {
        Ok(_) => panic!("cutover must refuse while the common lock is held"),
        Err(RepositoryError::LockContended { .. }) => return,
        Err(other) => panic!("expected typed lock contention, got {other}"),
    };
    let _ = error;
}

#[test]
fn linked_worktrees_share_the_cutover_fence() {
    // Matrix cell: copied/linked IDs — a linked worktree shares the
    // common pristine, so the cutover fence applies there too.
    let directory = TempDir::new().unwrap();
    let primary = directory.path().join("primary");
    let linked = directory.path().join("linked");
    fs::create_dir_all(&primary).unwrap();
    let head = init_real_git_repo(&primary, "main");

    let repo = TestRepository::new(Repository::init_with_view(&primary, "main").unwrap());
    install_bridge_evidence(&repo, &primary, &head);
    repo.execute_bridge_cutover().unwrap();
    drop(repo);

    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&primary)
        .args(["worktree", "add", "-q", "-b", "linked-cutover-fence", linked.to_str().unwrap(), "main"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let linked_repo = Repository::open(&linked).unwrap();
    // The fence is shared: the linked worktree's legacy writer refuses.
    let error = linked_repo
        .require_legacy_shadow_write_allowed()
        .unwrap_err();
    assert!(
        matches!(error, RepositoryError::LegacyShadowWriterFenced { .. }),
        "{error}"
    );
}

#[test]
fn bridge_cutover_refuses_unsupported_requirements_without_mutation() {
    let (_temp, repo) = create_temp_repo();
    std::fs::create_dir_all(_temp.path().join(".git")).unwrap();

    // A newer build left a requirement row this build cannot satisfy.
    {
        let mut txn = repo.pristine.write_txn().unwrap();
        txn.put_required_capability_exact("future-format", 7)
            .unwrap();
        txn.commit().unwrap();
    }

    let audit = repo.audit_bridge_cutover().unwrap();
    assert_eq!(
        audit.unsupported_requirements,
        vec![RequiredRepositoryCapability {
            id: "future-format".to_string(),
            minimum_version: 7,
        }]
    );

    let error = match repo.plan_bridge_cutover() {
        Ok(_) => panic!("cutover with unsupported requirements must refuse"),
        Err(RepositoryError::InvalidOperation { message }) => message,
        Err(other) => panic!("expected typed invalid-operation error, got {other}"),
    };
    assert!(error.contains("future-format"), "{error}");
    assert!(error.contains("upgrade Atomic"), "{error}");
    // The repository is preserved: no cutover requirement was written.
    assert_eq!(capability_row(&repo, CUTOVER_ID), None);

    // The same row also fails closed at open: this build cannot adopt it.
    drop(repo);
    let result = Repository::open_readonly(_temp.path());
    assert!(matches!(
        result,
        Err(RepositoryError::UnsupportedRequiredCapabilities { .. })
    ));
}

#[test]
fn cutover_audit_evidence_digest_is_stable_and_sensitive() {
    let (_temp, repo) = create_temp_repo();
    let before = repo.audit_bridge_cutover().unwrap();
    let digest = before.evidence_digest();

    repo.pristine
        .require_repository_capability(BRIDGE_CUTOVER_CAPABILITY)
        .unwrap();
    let after = repo.audit_bridge_cutover().unwrap();

    assert_eq!(before.evidence_digest(), digest);
    assert_ne!(before.evidence_digest(), after.evidence_digest());
    assert_eq!(after.cutover_requirement, Some(1));
    // Sanity: the fence reuses the build's declared capability identity.
    assert_eq!(BRIDGE_CUTOVER_CAPABILITY.id(), CUTOVER_ID);
    assert_eq!(
        BRIDGE_CUTOVER_CAPABILITY.id(),
        SUPPORTED_REPOSITORY_CAPABILITIES[1].id()
    );
    assert_eq!(
        CHANGE_FORMAT_VNEXT_CAPABILITY.id(),
        SUPPORTED_REPOSITORY_CAPABILITIES[0].id()
    );
}

/// CB-13B R5 interleaving regression: the legacy writer observes the fence
/// Absent, the cutover then fences and releases the common lock, and the
/// writer finally acquires the now-free lock. The writer must fail closed
/// at that point instead of proceeding with a post-cutover legacy write.
/// Fails before the fix (the writer returned a held guard and would have
/// projected legacy shadow state after the fence landed) and passes after
/// (a typed fenced refusal under the held lock).
#[test]
fn shadow_writer_fails_closed_when_the_cutover_fences_between_observation_and_lock() {
    use std::sync::Arc;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    let (temp, repo) = create_temp_repo();
    let head = init_real_git_repo(temp.path(), &repo.current_view().to_string());
    install_bridge_evidence(&repo, temp.path(), &head);
    let repo = Arc::new(repo);
    assert_eq!(capability_row(&repo, CUTOVER_ID), None);

    let (at_window, at_window_rx) = channel();
    let (resume_tx, resume) = channel();
    let writer = {
        let repo = Arc::clone(&repo);
        std::thread::spawn(move || {
            super::super::materialize::install_shadow_fence_interleave_window(
                at_window, resume,
            );
            repo.try_lock_shadow_commit()
        })
    };

    // The writer passed the pre-lock fence observation (Absent) and is
    // paused inside the read-to-lock window.
    at_window_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("writer must reach the read-to-lock window");

    // The cutover fences and releases while the writer sits in the window.
    let outcome = repo.execute_bridge_cutover().unwrap();
    assert!(outcome.operation.is_some(), "cutover must fence");
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(1));

    resume_tx.send(()).expect("resume the writer");
    let acquired = writer.join().expect("writer thread must not panic");
    let error = match acquired {
        Ok(Some(_)) => panic!(
            "legacy writer must fail closed when the cutover fenced in the \
             read-to-lock window"
        ),
        Ok(None) => panic!("unexpected contention: the cutover had released the lock"),
        Err(RepositoryError::LegacyShadowWriterFenced { capability }) => capability,
        Err(other) => panic!("expected typed fenced error, got {other}"),
    };
    assert!(error.contains(CUTOVER_ID), "{error}");
}

// ── CB-13B R6: readiness is real, leased and projection-proven ──────────

#[test]
fn cutover_refuses_an_empty_git_directory() {
    // R6 (failing-before): any successful symlink_metadata on `.git` —
    // including an empty directory — qualified as a colocated repository
    // and the fence landed.
    let (temp, repo) = create_temp_repo();
    std::fs::create_dir_all(temp.path().join(".git")).unwrap();
    expect_cutover_refusal(&repo, "not a valid Git repository");
}

#[test]
fn cutover_refuses_an_arbitrary_file_at_git() {
    let (_temp, repo) = create_temp_repo();
    std::fs::write(_temp.path().join(".git"), "not a gitdir\n").unwrap();
    expect_cutover_refusal(&repo, "not a valid Git gitdir pointer");
}

#[test]
fn cutover_refuses_a_dangling_symlink_at_git() {
    let (temp, repo) = create_temp_repo();
    #[cfg(unix)]
    std::os::unix::fs::symlink(temp.path().join("nowhere"), temp.path().join(".git")).unwrap();
    expect_cutover_refusal(&repo, "does not resolve to a valid Git repository");
}

#[test]
fn cutover_refuses_a_real_repository_without_bridge_evidence() {
    // A real Git repository but no verified checkpoint and no candidate
    // binding: the shadow pipeline never published here.
    let (temp, repo) = create_temp_repo();
    init_real_git_repo(temp.path(), &repo.current_view().to_string());
    expect_cutover_refusal(&repo, "verified bridge checkpoint");
}

#[test]
fn cutover_refuses_active_hook_surfaces() {
    // The audit labels hook migration Refused; the surface must actually
    // refuse execution instead of prose-only.
    let (temp, repo) = create_temp_repo();
    let head = init_real_git_repo(temp.path(), &repo.current_view().to_string());
    install_bridge_evidence(&repo, temp.path(), &head);
    let hooks = temp.path().join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    expect_cutover_refusal(&repo, "hook");
}

/// Install an Atomic-owned advisory dispatcher with the exact marker the
/// CLI hook installer writes (shared marker constant).
fn install_owned_dispatcher(temp: &std::path::Path, name: &str, subcommand: &str) -> Vec<u8> {
    use std::os::unix::fs::PermissionsExt;
    let hooks = temp.join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let script = format!(
        "#!/bin/sh\n{}\n/usr/local/bin/atomic git bridge {subcommand} \"$@\" || true\nexit 0\n",
        observe_marker()
    );
    let hook = hooks.join(name);
    std::fs::write(&hook, &script).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    script.into_bytes()
}

fn observe_marker() -> &'static str {
    super::super::git_observation::ATOMIC_DISPATCHER_MARKER
}

#[test]
fn cutover_decommissions_owned_dispatchers_with_journaled_leases() {
    // CB-13B R2: the hook writer routes through the journaled cutover —
    // the Atomic-owned advisory dispatcher is removed through a
    // migration-effect lease (exact bytes expected-old, Absent
    // expected-new) with an Applied receipt; the foreign hook refuses.
    let (temp, repo) = create_temp_repo();
    let head = init_real_git_repo(temp.path(), &repo.current_view().to_string());
    install_bridge_evidence(&repo, temp.path(), &head);
    let hook_path = temp.path().join(".git/hooks/post-checkout");
    let bytes = install_owned_dispatcher(temp.path(), "post-checkout", "hook-post-checkout");

    let outcome = repo.execute_bridge_cutover().unwrap();
    let operation = outcome.operation.expect("cutover must be journaled");
    assert!(!hook_path.exists(), "the owned dispatcher is decommissioned");
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(1));

    // The decommission is journaled: the operation carries the effect and
    // its Applied receipt.
    let details = repo.operation_details(operation).unwrap();
    assert_eq!(details.operation.payload().delta.effects.len(), 1);
    assert_eq!(details.verification, OperationVerificationState::Verified);
    assert!(details.receipts.iter().any(|receipt| {
        receipt.payload().kind == EffectReceiptKind::Applied
    }));
    let _ = bytes;
}

#[test]
fn cutover_rollback_restores_decommissioned_hooks_byte_for_byte() {
    // CB-13B R2: the leased hook rollback — the typed undo replays the
    // swapped effect plans from the cutover's retained backups, restoring
    // the dispatcher's exact bytes and mode.
    let (temp, mut repo) = create_temp_repo();
    let head = init_real_git_repo(temp.path(), &repo.current_view().to_string());
    install_bridge_evidence(&repo, temp.path(), &head);
    let hook_path = temp.path().join(".git/hooks/post-checkout");
    let bytes = install_owned_dispatcher(temp.path(), "post-checkout", "hook-post-checkout");

    repo.execute_bridge_cutover().unwrap();
    assert!(!hook_path.exists());

    repo.rollback_bridge_cutover().unwrap();
    assert_eq!(capability_row(&repo, CUTOVER_ID), None);
    let restored = std::fs::read(&hook_path).expect("the dispatcher is restored");
    assert_eq!(restored, bytes, "the dispatcher restores byte-for-byte");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&hook_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "the restored hook stays executable");
    }
}

#[test]
fn restore_refuses_an_effect_bearing_operation() {
    // Pins the re-review's noted gap: restore of an effect-bearing
    // operation (the cutover's hook decommission) explicitly refuses
    // instead of restoring metadata without its external effects.
    let (temp, mut repo) = create_temp_repo();
    let head = init_real_git_repo(temp.path(), &repo.current_view().to_string());
    install_bridge_evidence(&repo, temp.path(), &head);
    install_owned_dispatcher(temp.path(), "post-checkout", "hook-post-checkout");

    let outcome = repo.execute_bridge_cutover().unwrap();
    let operation = outcome.operation.expect("cutover must be journaled");
    let working_copy = repo.require_working_copy_id().unwrap();

    let error = match repo.restore_operation(working_copy, operation) {
        Ok(_) => panic!("restore of an effect-bearing operation must refuse"),
        Err(RepositoryError::InvalidOperation { message }) => message,
        Err(other) => panic!("expected typed invalid-operation error, got {other}"),
    };
    assert!(error.contains("external effects"), "{error}");
    // The fence is untouched by the refused restore.
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(1));
}

#[test]
fn cutover_recovery_restores_decommissioned_hooks_after_interruption() {
    // Interruption between the fence and the receipt: the dispatcher is
    // already gone and the row is durable. The writable open's idempotent
    // recovery replays the inverse effects (the dispatcher's exact bytes
    // restore) and reverts the fence; a fresh cutover then succeeds.
    let (temp, repo) = create_temp_repo();
    let head = init_real_git_repo(temp.path(), &repo.current_view().to_string());
    install_bridge_evidence(&repo, temp.path(), &head);
    let hook_path = temp.path().join(".git/hooks/post-checkout");
    let bytes = install_owned_dispatcher(temp.path(), "post-checkout", "hook-post-checkout");

    super::super::cutover::install_cutover_interrupt_before_finalize();
    let error = repo.execute_bridge_cutover().unwrap_err();
    assert!(
        format!("{error}").contains("interrupted") || format!("{error}").contains("failpoint"),
        "{error}"
    );
    // The interruption left the fence durable and the dispatcher gone.
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(1));
    assert!(!hook_path.exists());

    drop(repo);
    // The writable open performs the idempotent recovery.
    let repo = TestRepository::new(Repository::open(temp.path()).unwrap());
    assert_eq!(
        capability_row(&repo, CUTOVER_ID),
        None,
        "recovery reverts the fence"
    );
    let restored = std::fs::read(&hook_path).expect("the dispatcher is restored");
    assert_eq!(restored, bytes, "the dispatcher restores byte-for-byte");

    // A fresh cutover then succeeds cleanly.
    repo.execute_bridge_cutover().unwrap();
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(1));
    assert!(!hook_path.exists());
}

#[test]
fn cutover_refuses_configured_remotes() {
    // The audit labels remote-capability negotiation Refused; a configured
    // remote must actually refuse execution.
    let (temp, repo) = create_temp_repo();
    let head = init_real_git_repo(temp.path(), &repo.current_view().to_string());
    install_bridge_evidence(&repo, temp.path(), &head);
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(temp.path())
        .args(["remote", "add", "origin", "https://example.com/repo.git"])
        .output()
        .expect("add remote");
    assert!(output.status.success());
    expect_cutover_refusal(&repo, "remote");
}

#[test]
fn cutover_fences_only_on_full_leased_readiness() {
    // Corpus replacement for the empty-`.git`-passes happy path: a real
    // Git repository with a verified checkpoint whose git_head matches the
    // live HEAD, a candidate binding that resolves, no hooks and no
    // remotes — only then does the fence land.
    let (temp, repo) = create_temp_repo();
    let head = init_real_git_repo(temp.path(), &repo.current_view().to_string());
    install_bridge_evidence(&repo, temp.path(), &head);

    let audit = repo.audit_bridge_cutover().unwrap();
    assert_eq!(audit.colocated_git.form, ColocatedGitForm::Repository);
    assert!(audit.readiness_refusals.is_empty(), "{:?}", audit.readiness_refusals);

    let outcome = repo.execute_bridge_cutover().unwrap();
    assert!(!outcome.already_fenced);
    assert!(outcome.operation.is_some());
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(1));
}

// ── CB-13B R7: resume requires exact-version plus lifecycle proof ───────

/// Prepare and apply one journaled cutover capability lease WITHOUT the
/// final Verified receipt: the interruption state between the fence and
/// finalize.
fn prepare_and_apply_capability_lease(
    repo: &Repository,
    working_copy: WorkingCopyId,
    expected_old: MetadataValue,
    expected_new: MetadataValue,
) -> Result<OperationId, RepositoryError> {
    let operation_lock = repo.try_lock_operation(working_copy)?;
    let record = repo.working_copy_record(working_copy)?;
    let state = RepoStateRef {
        view: None,
        working_copy: Some(working_copy_state_ref(record)),
        git: None,
    };
    let operation = repo.prepare_metadata_operation(
        &operation_lock,
        OperationKind::Cutover,
        None,
        state.clone(),
        state,
        vec![MetadataTransition {
            target: MetadataTarget::Capability {
                id: CUTOVER_ID.to_string(),
            },
            expected_old,
            expected_new,
        }],
        Vec::new(),
        ActorRef::System {
            name: "cutover-regression".to_string(),
        },
        current_operation_timestamp_ms(),
    )?;
    repo.apply_operation_metadata_locked(&operation_lock, operation.id())?;
    Ok(operation.id())
}

#[test]
fn cutover_refuses_a_foreign_fence_version_zero() {
    // R7 (failing-before): version 0 is valid in the exact-lease API, yet
    // the presence-only already-fenced branch returned no-op success for
    // it while the plan expected this build's version 1.
    let (_temp, repo) = create_temp_repo();
    let working_copy = repo.require_working_copy_id().unwrap();
    apply_capability_lease(
        &repo,
        working_copy,
        MetadataTarget::Capability {
            id: CUTOVER_ID.to_string(),
        },
        MetadataValue::Absent,
        MetadataValue::Sequence(0),
    )
    .unwrap();
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(0));

    let error = match repo.execute_bridge_cutover() {
        Ok(outcome) => panic!("foreign fence version 0 must not be a no-op success: {outcome:?}"),
        Err(RepositoryError::InvalidOperation { message }) => message,
        Err(other) => panic!("expected typed invalid-operation error, got {other}"),
    };
    assert!(error.contains("version is 0"), "{error}");
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(0));
}

#[test]
fn cutover_refuses_a_late_higher_fence_version() {
    let (_temp, repo) = create_temp_repo();
    {
        let mut txn = repo.pristine.write_txn().unwrap();
        txn.put_required_capability_exact(CUTOVER_ID, 2).unwrap();
        txn.commit().unwrap();
    }
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(2));

    let error = match repo.execute_bridge_cutover() {
        Ok(outcome) => panic!("a late higher fence must not be a no-op success: {outcome:?}"),
        Err(RepositoryError::InvalidOperation { message }) => message,
        Err(other) => panic!("expected typed invalid-operation error, got {other}"),
    };
    // A late higher version fails closed at the unsupported-requirement
    // gate: this build cannot satisfy it, so it refuses with remediation
    // instead of a no-op success.
    assert!(error.contains("version 2"), "{error}");
    assert!(error.contains("upgrade Atomic"), "{error}");
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(2));
}

#[test]
fn cutover_refuses_a_fence_without_an_owning_operation() {
    // R7 (failing-before): a durable row with no journal entry returned
    // no-op success; the fence's owning operation must exist.
    let (_temp, repo) = create_temp_repo();
    {
        let mut txn = repo.pristine.write_txn().unwrap();
        txn.put_required_capability_exact(CUTOVER_ID, 1).unwrap();
        txn.commit().unwrap();
    }
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(1));

    let error = match repo.execute_bridge_cutover() {
        Ok(outcome) => panic!("an unowned fence must not be a no-op success: {outcome:?}"),
        Err(RepositoryError::InvalidOperation { message }) => message,
        Err(other) => panic!("expected typed invalid-operation error, got {other}"),
    };
    assert!(error.contains("no owning Cutover operation"), "{error}");
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(1));
}

#[test]
fn cutover_resumes_an_interrupted_fence_before_the_receipt() {
    // Interruption after the fence, before the receipt: the row landed,
    // the owning operation never finalized. Execution resumes the owning
    // operation in place instead of a no-op success.
    let (temp, repo) = create_temp_repo();
    let head = init_real_git_repo(temp.path(), &repo.current_view().to_string());
    install_bridge_evidence(&repo, temp.path(), &head);
    let working_copy = repo.require_working_copy_id().unwrap();

    let operation = prepare_and_apply_capability_lease(
        &repo,
        working_copy,
        MetadataValue::Absent,
        MetadataValue::Sequence(1),
    )
    .unwrap();
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(1));
    let details = repo.operation_details(operation).unwrap();
    assert_ne!(details.verification, OperationVerificationState::Verified);

    let outcome = repo.execute_bridge_cutover().unwrap();
    assert!(outcome.resumed, "{outcome:?}");
    assert!(outcome.already_fenced);
    assert_eq!(outcome.operation, Some(operation));
    assert_eq!(capability_row(&repo, CUTOVER_ID), Some(1));
    let details = repo.operation_details(operation).unwrap();
    assert_eq!(details.verification, OperationVerificationState::Verified);
    // The resume did not prepare a second cutover.
    assert_eq!(cutover_operation_count(&repo, working_copy), 1);
}

#[test]
fn cutover_refuses_to_prepare_over_an_interrupted_fence_before_the_landing() {
    // Interruption before the fence: the owning operation is prepared but
    // nothing landed. A fresh execution refuses at the incomplete-head
    // gate instead of preparing a second cutover or no-op succeeding.
    let (temp, repo) = create_temp_repo();
    let head = init_real_git_repo(temp.path(), &repo.current_view().to_string());
    install_bridge_evidence(&repo, temp.path(), &head);
    let working_copy = repo.require_working_copy_id().unwrap();

    let operation_lock = repo.try_lock_operation(working_copy).unwrap();
    let record = repo.working_copy_record(working_copy).unwrap();
    let state = RepoStateRef {
        view: None,
        working_copy: Some(working_copy_state_ref(record)),
        git: None,
    };
    let operation = repo
        .prepare_metadata_operation(
            &operation_lock,
            OperationKind::Cutover,
            None,
            state.clone(),
            state,
            vec![MetadataTransition {
                target: MetadataTarget::Capability {
                    id: CUTOVER_ID.to_string(),
                },
                expected_old: MetadataValue::Absent,
                expected_new: MetadataValue::Sequence(1),
            }],
            Vec::new(),
            ActorRef::System {
                name: "cutover-regression".to_string(),
            },
            current_operation_timestamp_ms(),
        )
        .unwrap();
    drop(operation_lock);
    let details = repo.operation_details(operation.id()).unwrap();
    assert_eq!(details.verification, OperationVerificationState::Prepared);
    assert_eq!(capability_row(&repo, CUTOVER_ID), None);

    let error = match repo.execute_bridge_cutover() {
        Ok(outcome) => panic!(
            "an execution over an interrupted pre-fence head must refuse: {outcome:?}"
        ),
        Err(RepositoryError::InvalidOperation { message }) => message,
        Err(other) => panic!("expected typed invalid-operation error, got {other}"),
    };
    assert!(error.contains("incomplete"), "{error}");
    // Nothing landed: the repository is preserved.
    assert_eq!(capability_row(&repo, CUTOVER_ID), None);
}
