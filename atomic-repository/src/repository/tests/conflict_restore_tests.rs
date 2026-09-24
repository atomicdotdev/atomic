//! CB-8B: exact fresh-store conflict restoration from the complete pack
//! (RFC §8.3, intent ac-1).
//!
//! The round-trip contract: a conflicted view's complete conflict object
//! (identities, base/sides, modes/kinds, claimant/name-edge identities,
//! graph metadata) rides the binding's `conflicts.pack`; after TOTAL loss of
//! the original Atomic store, a fresh store restores the exact same conflict
//! state — deep, store-independent equality — and every incomplete or
//! tampered input is refused: a marker-plus-hash snapshot without a pack, a
//! stale pack, a corrupted pack, and missing sides.

use std::fs;
use std::path::PathBuf;

use atomic_core::change::ChangeHeader;
use atomic_core::types::Base32;
use atomic_core::Hash;

use atomic_objects::{ObjectFamily, ObjectRecord};

use crate::git_binding::{
    BindingSigner, CausalOrigin, GitObjectFormat, GitOid, GitStateBinding, GitStateBindingPayload,
    BINDING_VERSION, CONFLICTS_PACK_BLOB_NAME,
};
use crate::repository::conflict_object::ConflictSetObject;
use crate::Repository;

use super::{RecordOptions, TestRepository};
use crate::CrossViewInsertOptions;

fn test_keypair(seed: u8) -> atomic_identity::keypair::KeyPair {
    let mut secret = [0u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        *byte = seed.wrapping_add((index as u8) * 7 + 11);
    }
    atomic_identity::keypair::KeyPair::from_secret_key(
        atomic_identity::keypair::SecretKey::from_bytes(&secret),
    )
}

/// A publisher with an unresolved order conflict on `f.txt` (concurrent
/// same-position inserts from two views), its conflict object, marker bytes,
/// and a signed binding over a real conflict snapshot commit carrying the
/// complete `conflicts.pack`.
struct ConflictedPublisher {
    _dir: tempfile::TempDir,
    repo: TestRepository,
    git: git2::Repository,
    binding: GitStateBinding,
    conflict_set: ConflictSetObject,
    conflict_hash: Hash,
    root: PathBuf,
    view: String,
}

fn publisher_conflicted(seed: u8, with_pack: bool, corrupt_pack: bool) -> ConflictedPublisher {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let git = git2::Repository::init(&root).expect("init git");
    fs::write(root.join("f.txt"), "line1\nline2\nline3\nline4\nline5\n").expect("base file");
    fs::write(root.join("keep.txt"), "keep\n").expect("keep file");
    let mut repo = super::TestRepository::new(Repository::init(&root).expect("init atomic"));

    // Base: both files recorded on the shared view `dev`.
    repo.add("f.txt", Default::default()).expect("track f");
    repo.add("keep.txt", Default::default())
        .expect("track keep");
    repo.record(ChangeHeader::new("base"), RecordOptions::default())
        .expect("record base");

    // Draft `feature`: edit A after line1.
    repo.create_view_from("feature", "dev")
        .expect("create view");
    repo.switch_view("feature").expect("switch");
    fs::write(
        root.join("f.txt"),
        "line1\nAAA-inserted\nline2\nline3\nline4\nline5\n",
    )
    .expect("edit A");
    repo.record(ChangeHeader::new("edit A"), RecordOptions::default())
        .expect("record edit A");

    // Shared `dev`: edit B after line1 (same position → conflict).
    repo.switch_view("dev").expect("switch");
    fs::write(
        root.join("f.txt"),
        "line1\nBBB-inserted\nline2\nline3\nline4\nline5\n",
    )
    .expect("edit B");
    repo.record(ChangeHeader::new("edit B"), RecordOptions::default())
        .expect("record edit B");

    // Insert feature into dev: the concurrent inserts conflict; the
    // materialization persists the conflict rows and marker bytes.
    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .expect("insert with conflict");
    repo.materialize().expect("materialize conflict");

    // Capture the complete conflict object and its marker bytes.
    let (conflict_set, _markers) = repo
        .capture_view_conflict_set_with_markers("dev")
        .expect("capture conflict set")
        .expect("conflicts present");
    assert_eq!(conflict_set.files.len(), 1, "f.txt is conflicted");
    assert!(
        conflict_set.files[0].entries[0].sides.len() >= 2,
        "an order conflict carries both sides"
    );
    let conflict_hash = conflict_set.hash().expect("conflict hash");

    // The conflict snapshot commit: a real Git commit whose tree is the
    // marker projection of the conflicted state and whose message names
    // `atomic-conflict <hash>`.
    let policy =
        crate::repository::ConversionPolicy::new(atomic_core::operation::GitHashAlgorithm::Sha1);
    let projection = repo
        .prepare_conflict_snapshot_projection("dev", &policy)
        .expect("conflict snapshot projection");
    let commit_oid = write_tree_and_commit(&git, &projection.project, &conflict_hash);

    // The binding over the snapshot commit, with the view's real closure
    // identity (SetId + Merkle order) — the values exact resurrection
    // verifies.
    let ordered: Vec<Hash> = repo
        .effective_history(Some("dev"))
        .expect("effective history")
        .into_iter()
        .map(|entry| entry.hash)
        .collect();
    let identity = repo.view_identity("dev").expect("view identity");
    let keypair = test_keypair(seed);
    let binding = bind_snapshot_commit_with_identity(
        &git,
        commit_oid,
        ordered,
        identity.set_id,
        identity.merkle,
        &keypair,
    );

    if with_pack {
        let mut bytes = conflict_set.canonical_bytes().expect("canonical bytes");
        if corrupt_pack {
            let last = bytes.len() - 1;
            bytes[last] ^= 0xff;
        }
        let published = repo.publish_binding_with_conflicts_pack(
            repo.working_copy(),
            &git,
            &binding,
            None,
            &bytes,
        );
        if corrupt_pack {
            assert!(
                published.is_err(),
                "a corrupted conflicts.pack must be refused at publication"
            );
        } else {
            published.expect("publish binding with conflicts pack");
        }
    } else {
        repo.publish_binding(repo.working_copy(), &git, &binding, None)
            .expect("publish binding");
    }

    ConflictedPublisher {
        _dir: dir,
        repo,
        git,
        binding,
        conflict_set,
        conflict_hash,
        root,
        view: "dev".to_string(),
    }
}

/// Write a [`crate::ProjectTree`]'s objects into a Git ODB and create a
/// parentless commit over the root tree whose message names the conflict set.
fn write_tree_and_commit(
    git: &git2::Repository,
    project: &crate::ProjectTree,
    conflict_hash: &Hash,
) -> git2::Oid {
    let odb = git.odb().expect("odb");
    for (oid, object) in project.git.objects.iter() {
        let git2_kind = match object.kind {
            crate::GitObjectKind::Blob => git2::ObjectType::Blob,
            crate::GitObjectKind::Tree => git2::ObjectType::Tree,
        };
        let written = odb.write(git2_kind, &object.bytes).expect("write object");
        assert_eq!(
            written.as_bytes(),
            oid.as_bytes(),
            "the projected object identity must be content-addressed"
        );
    }
    let tree_oid = git2::Oid::from_bytes(project.git.root.as_bytes()).expect("tree oid");
    let tree = git.find_tree(tree_oid).expect("snapshot tree");
    let signature = git2::Signature::now("CB-8B", "cb8b@example.com").expect("signature");
    let message = format!(
        "Atomic conflict snapshot\n\natomic-conflict {}\n",
        conflict_hash.to_base32()
    );
    git.commit(None, &signature, &signature, &message, &tree, &[])
        .expect("snapshot commit")
}

fn bind_snapshot_commit_with_identity(
    git: &git2::Repository,
    commit_oid: git2::Oid,
    ordered: Vec<Hash>,
    set_id: atomic_core::types::SetId,
    merkle_state: atomic_core::types::Merkle,
    keypair: &atomic_identity::keypair::KeyPair,
) -> GitStateBinding {
    let commit = git.find_commit(commit_oid).expect("snapshot commit");
    let raw_commit = git
        .odb()
        .expect("odb")
        .read(commit_oid)
        .expect("read commit")
        .data()
        .to_vec();
    let mut payload = GitStateBindingPayload {
        version: BINDING_VERSION,
        git_object_format: GitObjectFormat::Sha1,
        git_commit: GitOid::from_hex(&commit_oid.to_string()).expect("commit oid"),
        git_tree: GitOid::from_hex(&commit.tree_id().to_string()).expect("tree oid"),
        git_parents: Vec::new(),
        raw_commit_object: Some(raw_commit),
        set_id,
        merkle_state,
        view_hint: Some("dev".to_string()),
        ordered_changes: ordered,
        closure_root: Hash::from_bytes([0u8; 32]),
        operation: atomic_core::types::OperationId::from_bytes([13u8; 32]),
        origin: CausalOrigin::ExactAtomicResurrection,
        loss: Vec::new(),
        provenance_roots: Vec::new(),
        attestation_roots: Vec::new(),
        signer: BindingSigner::for_keypair(keypair),
    };
    payload.closure_root = payload.compute_closure_root();
    GitStateBinding::sign(payload, keypair).expect("sign binding")
}

/// Copy every object of a commit's tree (recursively) plus the commit
/// itself from one Git ODB into another — the "fresh clone" transport.
fn copy_commit_objects(source: &git2::Repository, target: &git2::Repository, commit: git2::Oid) {
    fn copy_tree(source: &git2::Repository, target: &git2::Repository, tree: git2::Oid) {
        let tree_object = source.find_tree(tree).expect("tree");
        for entry in tree_object.iter() {
            match entry.kind() {
                Some(git2::ObjectType::Tree) => copy_tree(source, target, entry.id()),
                Some(git2::ObjectType::Blob) => copy_object(source, target, entry.id()),
                _ => {}
            }
        }
        copy_object(source, target, tree);
    }
    fn copy_object(source: &git2::Repository, target: &git2::Repository, oid: git2::Oid) {
        let odb = source.odb().expect("odb");
        let object = odb.read(oid).expect("read object");
        let kind = object.kind();
        let data = object.data().to_vec();
        let _ = target
            .odb()
            .expect("odb")
            .write(kind, &data)
            .expect("write object");
    }
    let commit_object = source.find_commit(commit).expect("commit");
    copy_tree(source, target, commit_object.tree_id());
    copy_object(source, target, commit);
}

#[test]
fn fresh_store_restores_the_exact_conflict_state_from_the_complete_pack() {
    let publisher = publisher_conflicted(0x71, true, false);
    let view = publisher.view.clone();
    let conflict_set = publisher.conflict_set.clone();
    let conflict_hash = publisher.conflict_hash;

    // ── Fresh store: total loss of the original Atomic store. ────────────
    let fresh_dir = tempfile::TempDir::new().unwrap();
    let fresh_git = git2::Repository::init(fresh_dir.path()).expect("init fresh git");
    copy_commit_objects(&publisher.git, &fresh_git, {
        git2::Oid::from_bytes(publisher.binding.payload().git_commit.as_bytes())
            .expect("snapshot oid")
    });
    // The binding commit + its conflicts.pack blob ride the binding ref;
    // publish them into the fresh Git ODB through the same create-only
    // ref machinery by replaying the publication there.
    let fresh = {
        let repo = Repository::init_with_view(fresh_dir.path(), &view).expect("init fresh");
        super::TestRepository::new(repo)
    };
    let pack = fresh_repo_pack_bytes(&publisher, &conflict_set);
    let republished = fresh
        .repo
        .publish_binding_with_conflicts_pack(
            fresh.working_copy(),
            &fresh_git,
            &publisher.binding,
            None,
            &pack,
        )
        .expect("publish binding into the fresh store's Git");
    assert!(matches!(
        republished,
        crate::BindingPublication::Published { .. }
    ));

    // ── Resurrect the closure through an Atomic-remote stand-in. ─────────
    struct Source {
        changes_dir: PathBuf,
    }
    impl crate::BindingChangeSource for Source {
        fn fetch_changes(&mut self, wanted: &[Hash]) -> Result<Vec<ObjectRecord>, String> {
            let store = crate::ChangeStore::open_existing(self.changes_dir.clone(), 8)
                .map_err(|error| error.to_string())?;
            let mut records = Vec::new();
            for hash in wanted {
                let bytes = match fs::read(store.change_path(hash)) {
                    Ok(bytes) => bytes,
                    Err(_) => continue,
                };
                records.push(ObjectRecord::new(
                    ObjectFamily::Change,
                    hash.to_base32(),
                    bytes,
                ));
            }
            Ok(records)
        }
    }
    let mut source = Source {
        changes_dir: publisher.root.join(".atomic/changes"),
    };
    fresh
        .repo
        .resurrect_binding_exact(&fresh_git, &publisher.binding, &view, Some(&mut source))
        .expect("exact resurrection in the fresh store");

    // ── The deep conflict-state proof (RFC §8.3 round-trip). ─────────────
    fresh.materialize().expect("materialize the restored view");
    let (restored_object, restored_markers) = fresh
        .capture_view_conflict_set_with_markers(&view)
        .expect("capture restored")
        .expect("restored conflicts");
    // The restored markers carry the same conflict region: both sides, the
    // same opening/closing markers. The renderer's SIDE PRESENTATION ORDER
    // is store-local (SCC tie-breaks use store-internal ids), so the two
    // stores may present the sides in the opposite order — the conflict
    // identity, sides, and metadata are what must restore exactly.
    let restored_bytes = restored_markers.get("f.txt").cloned().unwrap_or_default();
    let publisher_bytes = fresh_repo_publisher_markers(&publisher, "f.txt");
    for side_bytes in ["AAA-inserted", "BBB-inserted"] {
        assert!(
            restored_bytes
                .windows(side_bytes.len())
                .any(|w| w == side_bytes.as_bytes()),
            "restored markers must carry side {side_bytes}: {}",
            String::from_utf8_lossy(&restored_bytes)
        );
        assert!(
            publisher_bytes
                .windows(side_bytes.len())
                .any(|w| w == side_bytes.as_bytes()),
            "publisher markers must carry side {side_bytes}"
        );
    }
    assert!(
        restored_bytes.starts_with(b"line1\n>>>>>>>")
            && restored_bytes
                .windows(16)
                .any(|w| w == b"<<<<<<< 1\nline2\n"),
        "the restored marker region must be structurally identical: {}",
        String::from_utf8_lossy(&restored_bytes)
    );
    assert_eq!(
        restored_object.normalized(),
        conflict_set.normalized(),
        "restored conflict object must deep-equal the packed object;\nrestored: {restored_object:?}\npacked: {conflict_set:?}"
    );
    fresh
        .repo
        .verify_restored_conflict_state(&view, &conflict_set)
        .expect("restored conflict state equals the packed conflict object");

    assert_eq!(conflict_set.hash().expect("hash"), conflict_hash);

    // The restored store reports the conflict like the original.
    let conflicts = fresh.list_conflicts().expect("list conflicts");
    assert!(
        conflicts.iter().any(|(path, _)| path == "f.txt"),
        "the fresh store must surface f.txt as conflicted: {conflicts:?}"
    );
    let _ = CONFLICTS_PACK_BLOB_NAME;
}

fn fresh_repo_pack_bytes(
    publisher: &ConflictedPublisher,
    conflict_set: &ConflictSetObject,
) -> Vec<u8> {
    let _ = publisher;
    conflict_set.canonical_bytes().expect("canonical bytes")
}

fn fresh_repo_publisher_markers(publisher: &ConflictedPublisher, path: &str) -> Vec<u8> {
    let (_, markers) = publisher
        .repo
        .capture_view_conflict_set_with_markers(&publisher.view)
        .expect("capture publisher markers")
        .expect("conflicts");
    markers.get(path).cloned().unwrap_or_default()
}

#[test]
fn marker_plus_hash_without_a_pack_is_refused_at_restore() {
    let publisher = publisher_conflicted(0x72, false, false);
    let view = publisher.view.clone();

    let fresh_dir = tempfile::TempDir::new().unwrap();
    let fresh_git = git2::Repository::init(fresh_dir.path()).expect("init fresh git");
    copy_commit_objects(&publisher.git, &fresh_git, {
        git2::Oid::from_bytes(publisher.binding.payload().git_commit.as_bytes())
            .expect("snapshot oid")
    });
    let fresh = super::TestRepository::new(
        Repository::init_with_view(fresh_dir.path(), &view).expect("init fresh"),
    );
    fresh
        .repo
        .publish_binding(fresh.working_copy(), &fresh_git, &publisher.binding, None)
        .expect("publish pack-less binding");

    struct Source {
        changes_dir: PathBuf,
    }
    impl crate::BindingChangeSource for Source {
        fn fetch_changes(&mut self, wanted: &[Hash]) -> Result<Vec<ObjectRecord>, String> {
            let store = crate::ChangeStore::open_existing(self.changes_dir.clone(), 8)
                .map_err(|error| error.to_string())?;
            let mut records = Vec::new();
            for hash in wanted {
                let bytes = match fs::read(store.change_path(hash)) {
                    Ok(bytes) => bytes,
                    Err(_) => continue,
                };
                records.push(ObjectRecord::new(
                    ObjectFamily::Change,
                    hash.to_base32(),
                    bytes,
                ));
            }
            Ok(records)
        }
    }
    let mut source = Source {
        changes_dir: publisher.root.join(".atomic/changes"),
    };
    let error = fresh
        .repo
        .resurrect_binding_exact(&fresh_git, &publisher.binding, &view, Some(&mut source))
        .expect_err("a conflict snapshot without a pack must refuse");
    let message = error.to_string();
    assert!(
        message.contains("conflicts.pack") || message.contains("marker bytes plus a"),
        "the refusal must name the missing pack, not silently restore markers:\n{message}"
    );
}

#[test]
fn a_corrupted_conflicts_pack_is_refused_at_publication() {
    let publisher = publisher_conflicted(0x73, true, true);
    // The fixture asserts publication refusal for the corrupted bytes.
    let _ = publisher;
}

#[test]
fn a_stale_conflict_pack_diverges_from_the_snapshot_header() {
    let publisher = publisher_conflicted(0x74, true, false);
    let view = publisher.view.clone();
    let conflict_set = publisher.conflict_set.clone();

    // Tamper with the conflict state: publish a pack whose identity differs
    // from the snapshot commit's header (a stale or forged pack).
    let mut tampered = conflict_set.clone();
    if let Some(file) = tampered.files.first_mut() {
        file.entries[0].line = Some(99);
    }
    let tampered_bytes = tampered.canonical_bytes().expect("tampered bytes");

    let fresh_dir = tempfile::TempDir::new().unwrap();
    let fresh_git = git2::Repository::init(fresh_dir.path()).expect("init fresh git");
    copy_commit_objects(&publisher.git, &fresh_git, {
        git2::Oid::from_bytes(publisher.binding.payload().git_commit.as_bytes())
            .expect("snapshot oid")
    });
    let fresh = super::TestRepository::new(
        Repository::init_with_view(fresh_dir.path(), &view).expect("init fresh"),
    );
    fresh
        .repo
        .publish_binding_with_conflicts_pack(
            fresh.working_copy(),
            &fresh_git,
            &publisher.binding,
            None,
            &tampered_bytes,
        )
        .expect("the tampered pack still decodes; publication proceeds structurally");

    struct Source {
        changes_dir: PathBuf,
    }
    impl crate::BindingChangeSource for Source {
        fn fetch_changes(&mut self, wanted: &[Hash]) -> Result<Vec<ObjectRecord>, String> {
            let store = crate::ChangeStore::open_existing(self.changes_dir.clone(), 8)
                .map_err(|error| error.to_string())?;
            let mut records = Vec::new();
            for hash in wanted {
                let bytes = match fs::read(store.change_path(hash)) {
                    Ok(bytes) => bytes,
                    Err(_) => continue,
                };
                records.push(ObjectRecord::new(
                    ObjectFamily::Change,
                    hash.to_base32(),
                    bytes,
                ));
            }
            Ok(records)
        }
    }
    let mut source = Source {
        changes_dir: publisher.root.join(".atomic/changes"),
    };
    let error = fresh
        .repo
        .resurrect_binding_exact(&fresh_git, &publisher.binding, &view, Some(&mut source))
        .expect_err("a pack whose identity diverges from the header must refuse");
    let message = error.to_string();
    assert!(
        message.contains("does not match") || message.contains("stale or forged"),
        "the refusal must name the identity divergence:\n{message}"
    );
}

#[test]
fn unrepresentable_conflict_states_refuse_projection_without_flattening() {
    // Path-case collision: two conflicted paths differing only by case on a
    // case-insensitive policy must refuse projection.
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let _git = git2::Repository::init(&root).expect("init git");
    let repo = super::TestRepository::new(Repository::init(&root).expect("init atomic"));

    let policy =
        crate::repository::ConversionPolicy::new(atomic_core::operation::GitHashAlgorithm::Sha1);
    // An empty conflict set is trivially representable.
    let empty = ConflictSetObject::empty();
    assert!(repo
        .conflict_projection_representability(&empty, &policy)
        .is_representable());

    // A case collision under a case-INSENSITIVE platform capability refuses.
    let mut policy_insensitive = policy.clone();
    policy_insensitive.platform.case_sensitive = false;
    let colliding = ConflictSetObject {
        version: crate::repository::conflict_object::CONFLICT_SET_VERSION,
        files: vec![
            crate::repository::ConflictFileObject {
                path: b"src/Lib.rs".to_vec(),
                inode: 1,
                entries: vec![crate::repository::ConflictEntryObject {
                    kind: crate::repository::ConflictEntryKind::Name,
                    line: Some(1),
                    base: None,
                    sides: Vec::new(),
                    claimants: vec![crate::repository::ConflictClaimantObject {
                        change: Hash::of(b"one"),
                        inode: 1,
                        path: b"src/Lib.rs".to_vec(),
                        directory: false,
                    }],
                }],
            },
            crate::repository::ConflictFileObject {
                path: b"src/lib.rs".to_vec(),
                inode: 2,
                entries: vec![crate::repository::ConflictEntryObject {
                    kind: crate::repository::ConflictEntryKind::Name,
                    line: Some(1),
                    base: None,
                    sides: Vec::new(),
                    claimants: vec![crate::repository::ConflictClaimantObject {
                        change: Hash::of(b"two"),
                        inode: 2,
                        path: b"src/lib.rs".to_vec(),
                        directory: false,
                    }],
                }],
            },
        ],
    };
    match repo.conflict_projection_representability(&colliding, &policy_insensitive) {
        crate::repository::ConflictRepresentability::PathCaseCollision { paths } => {
            assert_eq!(paths.len(), 2, "both case variants are named");
        }
        other => panic!("a case collision must refuse on an incapable filesystem: {other:?}"),
    }
    // The same collision on a case-SENSITIVE platform is representable (the
    // Git tree carries both paths; only incapable filesystems refuse).
    assert!(repo
        .conflict_projection_representability(&colliding, &policy)
        .is_representable());
}
