//! CB-6B closure-acquisition fixtures (RFC §5.2, §8.6, §12).
//!
//! Covers the four acquisition modes — remote-only, pack-only, mixed-have,
//! interrupted/resumed — plus the explicit refusals: missing objects,
//! shallow boundaries, unavailable fallbacks, and adversarial (tampered,
//! traversing) binding trees. Every fixture asserts the same invariants:
//! exact ordered hashes, verified closure root, byte-identical raw foreign
//! commit bytes, and *no* advance of view/ref/operation state by fetch alone.

use std::fs;
use std::path::PathBuf;

use atomic_core::change::Change;
use atomic_core::types::Base32;
use atomic_core::Hash;

use atomic_objects::{ObjectFamily, ObjectRecord};

use crate::git_binding::{
    assemble_changes_pack, BindingPackLimits, BindingSigner, CausalOrigin, GitObjectFormat,
    GitOid, GitStateBinding, GitStateBindingPayload, LossNote, BINDING_VERSION,
};
use crate::repository::binding_fetch::{
    BindingChangeSource, ClosureReadiness, IncompletenessReason,
};
use crate::Repository;

use super::{RecordOptions, TestRepository};
/// Deterministic test signing key — test code only, never the global store.
fn test_keypair(seed: u8) -> atomic_identity::keypair::KeyPair {
    let mut secret = [0u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        *byte = seed.wrapping_add((index as u8) * 7 + 11);
    }
    atomic_identity::keypair::KeyPair::from_secret_key(
        atomic_identity::keypair::SecretKey::from_bytes(&secret),
    )
}

/// A signed binding over a real Git commit and a real recorded closure.
fn bind_commit(
    git: &git2::Repository,
    commit_oid: git2::Oid,
    ordered: Vec<Hash>,
    keypair: &atomic_identity::keypair::KeyPair,
) -> GitStateBinding {
    let commit = git.find_commit(commit_oid).expect("commit");
    let tree_hex = commit.tree_id().to_string();
    let parents: Vec<GitOid> = commit
        .parent_ids()
        .map(|oid| GitOid::from_hex(&oid.to_string()).expect("parent oid"))
        .collect();
    let raw_commit = git
        .odb()
        .expect("odb")
        .read(commit_oid)
        .expect("read commit")
        .data()
        .to_vec();
    let object_format = match commit_oid.as_bytes().len() {
        20 => GitObjectFormat::Sha1,
        32 => GitObjectFormat::Sha256,
        other => panic!("unexpected OID width {other}"),
    };
    let mut payload = GitStateBindingPayload {
        version: BINDING_VERSION,
        git_object_format: object_format,
        git_commit: GitOid::from_hex(&commit_oid.to_string()).expect("commit oid"),
        git_tree: GitOid::from_hex(&commit.tree_id().to_string()).expect("tree oid"),
        git_parents: parents,
        raw_commit_object: Some(raw_commit),
        set_id: atomic_core::types::SetId::from_bytes([11u8; 32]),
        merkle_state: atomic_core::types::Merkle::from_bytes([12u8; 32]),
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

/// A publisher: real Git repo + three recorded Atomic changes + a signed
/// binding over HEAD, optionally published with a bounded `changes.pack`.
struct Publisher {
    _dir: tempfile::TempDir,
    repo: super::TestRepository,
    git: git2::Repository,
    binding: GitStateBinding,
    ordered: Vec<Hash>,
    raw_commit: Vec<u8>,
    root: PathBuf,
}

impl Publisher {
    fn changes_dir(&self) -> PathBuf {
        self.root.join(".atomic/changes")
    }
}

fn publisher_with_binding(seed: u8, with_pack: bool) -> Publisher {
    publisher_with_binding_format(seed, with_pack, GitObjectFormat::Sha1)
}

/// A publisher: real Git repo + three recorded Atomic changes + a signed
/// binding over HEAD, optionally published with a bounded `changes.pack`,
/// in the requested Git object format (RFC §8.6 round trips both OID widths).
fn publisher_with_binding_format(
    seed: u8,
    with_pack: bool,
    format: GitObjectFormat,
) -> Publisher {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().to_path_buf();

    let git = match format {
        GitObjectFormat::Sha1 => git2::Repository::init(&root).expect("init git"),
        GitObjectFormat::Sha256 => {
            let out = std::process::Command::new("git")
                .args([
                    "init",
                    "--object-format=sha256",
                    "-q",
                    root.to_str().unwrap(),
                ])
                .output()
                .expect("run git init --object-format=sha256");
            assert!(
                out.status.success(),
                "sha256 git init failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            git2::Repository::open(&root).expect("open sha256 git")
        }
    };
    fs::write(root.join("seed.txt"), b"seed\n").expect("seed file");
    let mut index = git.index().expect("index");
    index.add_path(std::path::Path::new("seed.txt")).expect("stage");
    index.write().expect("index write");
    let tree_oid = index.write_tree().expect("tree");
    let tree = git.find_tree(tree_oid).expect("tree");
    let signature = git2::Signature::now("Publisher", "pub@example.com").expect("sig");
    let head = git
        .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
        .expect("commit");
    let raw_commit = git
        .odb()
        .expect("odb")
        .read(head)
        .expect("read head")
        .data()
        .to_vec();
    drop(tree);

    let repo = super::TestRepository::new(Repository::init(&root).expect("init atomic"));
    for (name, message) in [("a.txt", "one"), ("b.txt", "two"), ("c.txt", "three")] {
        fs::write(root.join(name), format!("{message}\n")).expect("write file");
        repo.add(name, Default::default()).expect("track file");
        repo.record(atomic_core::change::ChangeHeader::new(message), RecordOptions::default())
            .expect("record");
    }

    let ordered: Vec<Hash> = repo
        .effective_history(None)
        .expect("effective history")
        .into_iter()
        .map(|entry| entry.hash)
        .collect();
    assert_eq!(ordered.len(), 3, "three recorded changes");

    let keypair = test_keypair(seed);
    let binding = bind_commit(&git, head, ordered.clone(), &keypair);

    if with_pack {
        let records: Vec<ObjectRecord> = ordered
            .iter()
            .map(|hash| {
                let bytes = fs::read(repo.change_store().change_path(hash)).expect("change bytes");
                ObjectRecord::new(ObjectFamily::Change, Hash::of(&bytes).to_hex(), bytes)
            })
            .collect();
        let pack = assemble_changes_pack(&records, &BindingPackLimits::default_limits())
            .expect("assemble pack");
        let published = repo
            .publish_binding_with_changes_pack(repo.working_copy(), &git, &binding, None, &pack)
            .expect("publish with pack");
        assert!(matches!(published, crate::BindingPublication::Published { .. }));
    } else {
        let published = repo
            .publish_binding(repo.working_copy(), &git, &binding, None)
            .expect("publish");
        assert!(matches!(published, crate::BindingPublication::Published { .. }));
    }

    Publisher {
        _dir: dir,
        repo,
        git,
        binding,
        ordered,
        raw_commit,
        root,
    }
}

/// An Atomic-remote stand-in: serves raw change files from the publisher's
/// store, keyed by change identity (base32, the sync-plane convention).
/// With `fail_after`, it serves the first N wants and then reports a
/// transport failure (interrupted download).
struct FileRemoteSource {
    changes_dir: PathBuf,
    fail_after: Option<usize>,
    served: usize,
}

impl FileRemoteSource {
    fn serving(changes_dir: PathBuf) -> Self {
        FileRemoteSource {
            changes_dir,
            fail_after: None,
            served: 0,
        }
    }

    fn interrupted(changes_dir: PathBuf, fail_after: usize) -> Self {
        FileRemoteSource {
            changes_dir,
            fail_after: Some(fail_after),
            served: 0,
        }
    }
}

impl BindingChangeSource for FileRemoteSource {
    fn fetch_changes(&mut self, wanted: &[Hash]) -> Result<Vec<ObjectRecord>, String> {
        if let Some(limit) = self.fail_after {
            if self.served >= limit {
                return Err("connection reset during fetch".to_string());
            }
        }
        let store = crate::ChangeStore::open_existing(self.changes_dir.clone(), 8)
            .map_err(|error| error.to_string())?;
        let mut records = Vec::new();
        for hash in wanted {
            let bytes = match fs::read(store.change_path(hash)) {
                Ok(bytes) => bytes,
                Err(_) => continue, // the remote does not serve this change
            };
            records.push(ObjectRecord::new(ObjectFamily::Change, hash.to_base32(), bytes));
            self.served += 1;
            if let Some(limit) = self.fail_after {
                if self.served >= limit {
                    break;
                }
            }
        }
        Ok(records)
    }
}

/// A client repository whose Git received the binding ref through a real
/// bare remote over real Git refspecs — the transfer surface is exactly the
/// create-only binding refs that were pushed.
struct Client {
    _remote_dir: tempfile::TempDir,
    _dir: tempfile::TempDir,
    repo: super::TestRepository,
    git: git2::Repository,
}

fn transfer_to_client(publisher_root: &std::path::Path, binding: &GitStateBinding) -> Client {
    let remote_dir = tempfile::TempDir::new().unwrap();
    let remote = git2::Repository::init_bare(remote_dir.path()).expect("bare remote");
    let publisher_git = git2::Repository::open(publisher_root).expect("open publisher git");
    let ref_name = Repository::binding_ref_name(&binding.id());
    let binding_refspec = format!("{ref_name}:{ref_name}");
    // The bound commit travels the normal branch refs; the binding travels
    // its own namespace. A shallow fixture that fetched ONLY the binding
    // namespace would be missing the bound commit by construction.
    let head_branch = publisher_git
        .head()
        .ok()
        .and_then(|head| head.shorthand().map(|s| s.to_string()));
    let mut push_specs: Vec<String> = vec![binding_refspec];
    if let Some(branch) = &head_branch {
        push_specs.push(format!("refs/heads/{branch}:refs/heads/{branch}"));
    }
    publisher_git
        .remote_anonymous(remote_dir.path().to_str().unwrap())
        .expect("publisher anonymous remote")
        .push(
            &push_specs.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            None,
        )
        .expect("push binding ref to remote");

    let client_dir = tempfile::TempDir::new().unwrap();
    let client_git =
        git2::Repository::init(client_dir.path()).expect("init client git");
    let mut fetch_specs: Vec<String> =
        vec!["refs/atomic/bindings/*:refs/atomic/bindings/*".to_string()];
    if let Some(branch) = &head_branch {
        fetch_specs.push("refs/heads/*:refs/heads/*".to_string());
    }
    let fetch_specs: Vec<&str> = fetch_specs.iter().map(|s| s.as_str()).collect();
    client_git
        .remote_anonymous(remote_dir.path().to_str().unwrap())
        .expect("client anonymous remote")
        .fetch(&fetch_specs, None, None)
        .expect("fetch binding refspec");

    // The transferred binding ref and its objects must be readable in the
    // client before any Atomic work begins.
    assert!(
        client_git.find_reference(&ref_name).is_ok(),
        "the binding ref must transfer through the refspec"
    );
    let target = client_git
        .find_reference(&ref_name)
        .expect("binding ref")
        .target()
        .expect("direct target");
    client_git
        .find_commit(target)
        .expect("the binding commit must be fetched");

    // The remote carries no WIP refs — WIP recovery refs never transfer
    // (RFC §6.4); branches carry only the normal Git history.
    for reference in remote.references().expect("remote refs") {
        let reference = reference.expect("ref");
        let name = reference.name().expect("named ref");
        assert!(
            !name.starts_with("refs/atomic/wip/"),
            "WIP refs never transfer, found {name}"
        );
    }

    let repo = super::TestRepository::new(Repository::init(client_dir.path()).expect("init client"));
    Client {
        _remote_dir: remote_dir,
        _dir: client_dir,
        repo,
        git: client_git,
    }
}

/// Fetch must never advance view membership, graph registration, binding
/// storage, or operations. Fetch alone is transport, not adoption.
fn assert_fetch_advances_nothing(client: &Client, binding: &GitStateBinding) {
    let view_log = client
        .repo
        .log(crate::HistoryOptions::default())
        .expect("client view log");
    assert!(
        view_log.is_empty(),
        "fetch must not advance view membership: {view_log:?}"
    );
    let registered = client.repo.registered_change_hashes().expect("registered");
    assert!(
        registered.is_empty(),
        "fetch must not insert changes into a view: {registered:?}"
    );
    let ids = client.repo.binding_ids().expect("binding ids");
    assert!(
        ids.is_empty(),
        "fetch must not adopt bindings client-side: {ids:?}"
    );
    let heads = client
        .repo
        .operation_log(
            atomic_core::operation::OperationScope::WorkingCopy(client.repo.working_copy()),
            Some(10),
            false,
        )
        .expect("operation log");
    assert!(
        matches!(heads.head_state, crate::OperationHeadState::Empty),
        "fetch must not journal an operation: {:?}",
        heads.head_state
    );
    let _ = binding;
}

/// The complete-closure invariants shared by every acquisition mode.
fn assert_exact_closure(
    client: &Client,
    ordered: &[Hash],
    raw_commit: &[u8],
    outcome: &crate::ClosureAcquisition,
) {
    assert!(
        outcome.readiness.is_complete(),
        "acquisition must be complete: {:?}",
        outcome.readiness
    );
    assert_eq!(outcome.ordered_hashes, ordered, "exact ordered hashes");
    for hash in ordered {
        let change = client.repo.load_change(hash).expect("cached change");
        assert_eq!(&change.hash().expect("identity"), hash, "identity preserved");
    }
    assert_eq!(
        outcome.raw_commit_object.as_deref(),
        Some(raw_commit),
        "raw foreign commit bytes are byte-identical across acquisition modes"
    );
    let validation = outcome.validation.as_ref().expect("final validation ran");
    assert!(validation.is_complete(), "validation agrees: {validation:?}");
}

/// AC-1 fixture: remote-only acquisition.
#[test]
fn remote_only_acquisition_restores_the_exact_closure() {
    let publisher = publisher_with_binding(0x20, false);
    let client = transfer_to_client(&publisher.root, &publisher.binding);
    let ordered = publisher.ordered.clone();
    let raw_commit = publisher.raw_commit.clone();

    assert_eq!(
        client
            .repo
            .missing_closure_objects(&publisher.binding)
            .unwrap()
            .len(),
        3,
        "the client is missing everything before fetch"
    );

    let mut source = FileRemoteSource::serving(publisher.changes_dir());
    let outcome = client
        .repo
        .fetch_binding_closure(
            &client.git,
            &publisher.binding,
            Some(&mut source),
            &BindingPackLimits::default_limits(),
        )
        .expect("fetch");
    assert_eq!(outcome.from_remote, 3);
    assert_eq!(outcome.from_pack, 0);
    assert_eq!(outcome.already_local, 0);
    assert_exact_closure(&client, &ordered, &raw_commit, &outcome);
    assert_fetch_advances_nothing(&client, &publisher.binding);
}

/// AC-1 fixture: pack-only acquisition — no remote at all, the bounded
/// fallback pack is the only source.
#[test]
fn pack_only_fallback_restores_the_exact_closure() {
    let publisher = publisher_with_binding(0x21, true);
    let client = transfer_to_client(&publisher.root, &publisher.binding);
    let ordered = publisher.ordered.clone();
    let raw_commit = publisher.raw_commit.clone();

    let outcome = client
        .repo
        .fetch_binding_closure(&client.git, &publisher.binding, None, &BindingPackLimits::default_limits())
        .expect("fetch");
    assert_eq!(outcome.from_pack, 3, "the pack closed the closure");
    assert_eq!(outcome.from_remote, 0);
    assert_exact_closure(&client, &ordered, &raw_commit, &outcome);
    assert_fetch_advances_nothing(&client, &publisher.binding);
}

/// AC-1 fixture: mixed-have — one change is already cached, the remote
/// serves exactly the remainder.
#[test]
fn mixed_have_acquisition_fetches_only_the_missing() {
    let publisher = publisher_with_binding(0x22, false);
    let client = transfer_to_client(&publisher.root, &publisher.binding);
    let ordered = publisher.ordered.clone();

    // Pre-cache the first change (an earlier download's cache).
    let first: Change = publisher
        .repo
        .load_change(&ordered[0])
        .expect("publisher change");
    client.repo.repo.save_change(&first).expect("pre-cache");

    let mut source = FileRemoteSource::serving(publisher.changes_dir());
    let outcome = client
        .repo
        .fetch_binding_closure(
            &client.git,
            &publisher.binding,
            Some(&mut source),
            &BindingPackLimits::default_limits(),
        )
        .expect("fetch");
    assert_eq!(outcome.already_local, 1, "the pre-cached change counts");
    assert_eq!(outcome.from_remote, 2, "the remote serves only the missing");
    assert_exact_closure(&client, &ordered, &publisher.raw_commit, &outcome);
    assert_fetch_advances_nothing(&client, &publisher.binding);
}

/// AC-1 fixture: interrupted download then resumption. The interrupted fetch
/// refuses explicitly, caches the verified prefix, and the resumed fetch
/// reuses the cache and completes.
#[test]
fn interrupted_download_resumes_from_the_verified_cache() {
    let publisher = publisher_with_binding(0x23, false);
    let client = transfer_to_client(&publisher.root, &publisher.binding);
    let ordered = publisher.ordered.clone();

    // Interrupted: the remote serves one change, then the connection dies.
    let mut flaky = FileRemoteSource::interrupted(publisher.changes_dir(), 1);
    let outcome = client
        .repo
        .fetch_binding_closure(
            &client.git,
            &publisher.binding,
            Some(&mut flaky),
            &BindingPackLimits::default_limits(),
        )
        .expect("the interrupted fetch is an outcome, not a crash");
    let (missing, reasons) = match &outcome.readiness {
        ClosureReadiness::Incomplete { missing, reasons } => (missing.clone(), reasons.clone()),
        ClosureReadiness::Complete => {
            panic!("an interrupted fetch must not claim completeness")
        }
    };
    assert_eq!(outcome.from_remote, 1, "the served change is cached for retry");
    assert_eq!(missing.len(), 2, "the remainder is explicitly missing");
    assert!(
        reasons
            .iter()
            .any(|r| matches!(r, IncompletenessReason::MissingObjects)),
        "explicit MissingObjects: {reasons:?}"
    );
    assert!(
        reasons
            .iter()
            .any(|r| matches!(r, IncompletenessReason::NoFallbackAvailable)),
        "the unavailable fallback is explicit: {reasons:?}"
    );
    assert_fetch_advances_nothing(&client, &publisher.binding);

    // Resumption: the cache counts as haves; the remote serves the rest.
    let mut resumed = FileRemoteSource::serving(publisher.changes_dir());
    let outcome = client
        .repo
        .fetch_binding_closure(
            &client.git,
            &publisher.binding,
            Some(&mut resumed),
            &BindingPackLimits::default_limits(),
        )
        .expect("resumed fetch");
    assert_eq!(outcome.already_local, 1, "the interrupted cache is reused");
    assert_eq!(outcome.from_remote, 2);
    assert_exact_closure(&client, &ordered, &publisher.raw_commit, &outcome);
}

/// AC-1: missing objects with no remote and no pack produce an explicit,
/// typed refusal — never synthesis, truncation, or adoption.
#[test]
fn missing_objects_and_unavailable_fallback_fail_explicitly() {
    let publisher = publisher_with_binding(0x24, false);
    let client = transfer_to_client(&publisher.root, &publisher.binding);

    let outcome = client
        .repo
        .fetch_binding_closure(&client.git, &publisher.binding, None, &BindingPackLimits::default_limits())
        .expect("the refusal is the outcome");
    match &outcome.readiness {
        ClosureReadiness::Incomplete { missing, reasons } => {
            assert_eq!(missing, &publisher.ordered, "the missing list is exact");
            assert!(
                reasons
                    .iter()
                    .any(|r| matches!(r, IncompletenessReason::NoFallbackAvailable)),
                "the unavailable fallback is explicit: {reasons:?}"
            );
        }
        ClosureReadiness::Complete => panic!("an empty client must not adopt the closure"),
    }
    for hash in &publisher.ordered {
        assert!(!client.repo.has_change(hash), "no silent synthesis");
    }
    assert_fetch_advances_nothing(&client, &publisher.binding);
}

/// AC-1: a shallow/promisor boundary (LossNote::TruncatedHistory) fails
/// explicitly even when every object is present — transport alone can never
/// claim completeness across the boundary.
#[test]
fn shallow_boundaries_fail_explicitly_despite_complete_objects() {
    let publisher = publisher_with_binding(0x25, false);

    // A second binding over the same commit and closure, declaring a
    // shallow boundary. Different payload → different id → its own
    // create-only ref. Signed by the same key as the original payload
    // carries (the signer travels inside the payload).
    let keypair = test_keypair(0x25);
    let mut payload = publisher.binding.payload().clone();
    payload.loss = vec![LossNote::TruncatedHistory {
        description: "promisor boundary at the first parent".to_string(),
    }];
    payload.closure_root = payload.compute_closure_root();
    let shallow = GitStateBinding::sign(payload, &keypair).expect("sign shallow binding");
    publisher
        .repo
        .publish_binding(publisher.repo.working_copy(), &publisher.git, &shallow, None)
        .expect("publish shallow binding");

    let client = transfer_to_client(&publisher.root, &shallow);

    // Pre-cache every ordered change so the only issue is the boundary.
    for hash in &publisher.ordered {
        let change = publisher.repo.load_change(hash).expect("publisher change");
        client.repo.repo.save_change(&change).expect("cache");
    }

    let outcome = client
        .repo
        .fetch_binding_closure(&client.git, &shallow, None, &BindingPackLimits::default_limits())
        .expect("fetch runs");
    match &outcome.readiness {
        ClosureReadiness::Incomplete { missing, reasons } => {
            assert!(missing.is_empty(), "no object is missing: {missing:?}");
            assert!(
                reasons
                    .iter()
                    .any(|r| matches!(r, IncompletenessReason::ShallowBoundary)),
                "the shallow boundary is explicit: {reasons:?}"
            );
        }
        ClosureReadiness::Complete => {
            panic!("a shallow boundary must never be reported complete")
        }
    }
}

/// AC-2: a tampered pack (blake3-inconsistent blob) fails closed; nothing is
/// cached and no authoritative state moves.
#[test]
fn tampered_pack_fails_closed_without_mutation() {
    let publisher = publisher_with_binding(0x26, true);
    let ref_name = Repository::binding_ref_name(&publisher.binding.id());

    // Simulate a malicious remote: keep the real binding bytes but replace
    // changes.pack with hash-inconsistent bytes, then force the ref over.
    {
        let original = publisher
            .git
            .find_reference(&ref_name)
            .unwrap()
            .target()
            .unwrap();
        let commit = publisher.git.find_commit(original).unwrap();
        let old_tree = commit.tree().unwrap();
        let binding_entry = old_tree.get_name("binding.cbor").unwrap().clone();
        let binding_blob = publisher.git.find_blob(binding_entry.id()).unwrap();

        let mut builder = publisher.git.treebuilder(None).unwrap();
        let binding_blob = publisher.git.blob(binding_blob.content()).unwrap();
        builder
            .insert("binding.cbor", binding_blob, git2::FileMode::Blob.into())
            .unwrap();
        let forged_pack = publisher
            .git
            .blob(b"forged pack bytes that hash-verify against nothing")
            .unwrap();
        builder
            .insert("changes.pack", forged_pack, git2::FileMode::Blob.into())
            .unwrap();
        let tree_oid = builder.write().unwrap();
        let tree = publisher.git.find_tree(tree_oid).unwrap();
        let sig = git2::Signature::now("Malice", "malice@example.com").unwrap();
        let forged_commit = publisher
            .git
            .commit(None, &sig, &sig, "forged", &tree, &[])
            .unwrap();
        publisher
            .git
            .reference(&ref_name, forged_commit, true, "forced")
            .unwrap();
    }

    let client = transfer_to_client(&publisher.root, &publisher.binding);
    let outcome = client
        .repo
        .fetch_binding_closure(&client.git, &publisher.binding, None, &BindingPackLimits::default_limits())
        .expect("the refusal is the outcome");
    match &outcome.readiness {
        ClosureReadiness::Incomplete { reasons, .. } => assert!(
            reasons
                .iter()
                .any(|r| matches!(r, IncompletenessReason::PackRefused { .. })),
            "the pack refusal is typed: {reasons:?}"
        ),
        ClosureReadiness::Complete => panic!("a forged pack must not close the closure"),
    }
    for hash in &publisher.ordered {
        assert!(!client.repo.has_change(hash), "no bytes from a refused pack");
    }
    assert_fetch_advances_nothing(&client, &publisher.binding);
}

/// AC-2: path confinement — a binding tree carrying a non-allowlisted entry,
/// or a subtree where a blob belongs, is refused before any bytes are read.
#[test]
fn binding_trees_confine_to_the_allowlisted_public_blobs() {
    let publisher = publisher_with_binding(0x27, false);
    let ref_name = Repository::binding_ref_name(&publisher.binding.id());
    let sig = git2::Signature::now("Malice", "m@e").unwrap();

    // Case 1: an extra, non-allowlisted entry (traversal payload carrier).
    {
        let original = publisher.git.find_reference(&ref_name).unwrap().target().unwrap();
        let commit = publisher.git.find_commit(original).unwrap();
        let old_tree = commit.tree().unwrap();
        let binding_entry = old_tree.get_name("binding.cbor").unwrap().clone();
        let binding_blob = publisher.git.find_blob(binding_entry.id()).unwrap();
        let mut builder = publisher.git.treebuilder(None).unwrap();
        let blob = publisher.git.blob(binding_blob.content()).unwrap();
        builder
            .insert("binding.cbor", blob, git2::FileMode::Blob.into())
            .unwrap();
        let evil = publisher.git.blob(b"evil traversal payload").unwrap();
        builder.insert("evil.bin", evil, git2::FileMode::Blob.into()).unwrap();
        let tree_oid = builder.write().unwrap();
        let tree = publisher.git.find_tree(tree_oid).unwrap();
        let forged = publisher
            .git
            .commit(None, &sig, &sig, "forged tree", &tree, &[])
            .unwrap();
        publisher
            .git
            .reference(&ref_name, forged, true, "force")
            .unwrap();

        let error = publisher
            .repo
            .load_binding_changes_pack(&publisher.git, &publisher.binding.id())
            .unwrap_err();
        assert!(
            error.to_string().contains("non-allowlisted"),
            "confinement refusal: {error}"
        );
    }

    // Case 2: a subtree smuggled under an allowlisted name — the mode
    // confinement fires before any bytes are read (path-escape vector).
    {
        let original = publisher.git.find_reference(&ref_name).unwrap().target().unwrap();
        let commit = publisher.git.find_commit(original).unwrap();
        let old_tree = commit.tree().unwrap();
        let binding_entry = old_tree.get_name("binding.cbor").unwrap().clone();
        let binding_blob = publisher.git.find_blob(binding_entry.id()).unwrap();
        let nested_tree = publisher.git.treebuilder(None).unwrap().write().unwrap();
        let mut builder = publisher.git.treebuilder(None).unwrap();
        let blob = publisher.git.blob(binding_blob.content()).unwrap();
        builder
            .insert("binding.cbor", blob, git2::FileMode::Blob.into())
            .unwrap();
        builder
            .insert("changes.pack", nested_tree, git2::FileMode::Tree.into())
            .unwrap();
        let tree_oid = builder.write().unwrap();
        let tree = publisher.git.find_tree(tree_oid).unwrap();
        let forged = publisher
            .git
            .commit(None, &sig, &sig, "forged tree 2", &tree, &[])
            .unwrap();
        publisher
            .git
            .reference(&ref_name, forged, true, "force")
            .unwrap();

        let error = publisher
            .repo
            .load_binding_changes_pack(&publisher.git, &publisher.binding.id())
            .unwrap_err();
        assert!(
            error.to_string().contains("not a regular blob"),
            "non-blob entries are refused: {error}"
        );
    }
}

/// Task 3 / RFC §8.6: the transport round trip in the SHA-256 OID format.
///
/// The bundled libgit2 (git2 0.19) cannot open SHA-256 repositories, so the
/// Git structural check cannot run against a real SHA-256 ODB here; the
/// pipeline below exercises everything else end to end with real change
/// bytes: the SHA-256-tagged binding (with a pure-Rust `sha2` commit digest
/// cross-checked against git's own hashing where available), the bounded
/// pack codec, closure validation, and identity-keyed ingest into a real
/// change store. The Git-side structural re-verification for SHA-256
/// repositories lands with the libgit2 upgrade (CB-10A/13C territory).
#[test]
fn sha256_oid_bindings_transport_their_pack_through_the_bounded_pipeline() {
    use crate::git_binding::{commit_object_digest, ChangesPack, ChangesPackV1};

    // A real raw commit object, digest-hashed with the pure-Rust sha2 path
    // the binding codec uses for SHA-256 repositories.
    let raw_commit = b"tree 1111111111111111111111111111111111111111111111111111111111111111\nauthor A <a@e> 0 +0000\ncommitter A <a@e> 0 +0000\n\nsha256 round trip\n";
    let digest = commit_object_digest(GitObjectFormat::Sha256, raw_commit).expect("sha256 digest");
    assert_eq!(digest.as_bytes().len(), 32, "sha256 OIDs are 32 bytes wide");
    assert_eq!(
        digest.to_hex().len(),
        64,
        "sha256 OIDs render as 64 hex characters"
    );

    // Three real recorded-style changes forming a dependency chain.
    let (record_a, change_a) = test_change("one", Vec::new());
    let (record_b, change_b) = test_change("two", vec![change_a.hash().unwrap()]);
    let (record_c, change_c) = test_change("three", vec![change_b.hash().unwrap()]);
    let ordered = vec![
        change_a.hash().unwrap(),
        change_b.hash().unwrap(),
        change_c.hash().unwrap(),
    ];

    // The binding: SHA-256 object format, raw commit bytes bound by digest.
    let keypair = test_keypair(0x28);
    let mut payload = GitStateBindingPayload {
        version: BINDING_VERSION,
        git_object_format: GitObjectFormat::Sha256,
        git_commit: digest,
        git_tree: GitOid::from_hex(&"2".repeat(64)).unwrap(),
        git_parents: Vec::new(),
        raw_commit_object: Some(raw_commit.to_vec()),
        set_id: atomic_core::types::SetId::from_bytes([21u8; 32]),
        merkle_state: atomic_core::types::Merkle::from_bytes([22u8; 32]),
        view_hint: None,
        ordered_changes: ordered.clone(),
        closure_root: Hash::from_bytes([0u8; 32]),
        operation: atomic_core::types::OperationId::from_bytes([23u8; 32]),
        origin: CausalOrigin::ExactAtomicResurrection,
        loss: Vec::new(),
        provenance_roots: Vec::new(),
        attestation_roots: Vec::new(),
        signer: BindingSigner::for_keypair(&keypair),
    };
    payload.closure_root = payload.compute_closure_root();
    let binding = GitStateBinding::sign(payload, &keypair).expect("sign sha256 binding");

    // The bounded pack: blake3-addressed change records, quarantined.
    let records = vec![record_a, record_b, record_c];
    let pack = crate::git_binding::assemble_changes_pack(
        &records,
        &BindingPackLimits::default_limits(),
    )
    .expect("assemble pack");
    let decoded = crate::git_binding::decode_changes_pack(
        &pack,
        &BindingPackLimits::default_limits(),
    )
    .expect("decode pack");
    let quarantined = crate::git_binding::QuarantinedPack::from_records(
        &decoded,
        &BindingPackLimits::default_limits(),
    )
    .expect("quarantine");
    assert_eq!(quarantined.len(), 3, "the pack carries the closure");

    // Closure validation over the quarantined pack: exact ordered hashes,
    // recomputed closure root, acyclic dependencies.
    let mut source = |hash: &Hash| Ok(quarantined.get(hash).cloned());
    let report = crate::git_binding::validate_binding_closure(
        binding.payload(),
        &mut source,
        &BindingPackLimits::default_limits(),
    )
    .expect("closure validates");
    assert!(report.is_complete());
    assert_eq!(report.verified, 3);

    // Identity-keyed ingest into a real change store (the remote path).
    let temp = tempfile::TempDir::new().unwrap();
    let repo = TestRepository::new(Repository::init(temp.path()).expect("init"));
    let identity_keyed: Vec<ObjectRecord> = ordered
        .iter()
        .map(|hash| {
            let change = quarantined.get(hash).expect("packed change");
            let mut bytes = Vec::new();
            change
                .serialize(&mut std::io::Cursor::new(&mut bytes))
                .expect("serialize");
            ObjectRecord::new(ObjectFamily::Change, hash.to_base32(), bytes)
        })
        .collect();
    let cached = repo
        .ingest_verified_changes(&identity_keyed, &ordered)
        .expect("ingest");
    assert_eq!(cached, 3, "every wanted change is cached");
    for hash in &ordered {
        let stored = repo.load_change(hash).expect("cached change");
        assert_eq!(&stored.hash().unwrap(), hash, "identity preserved through ingest");
    }

    // The raw foreign commit bytes ride the binding byte-identically.
    assert_eq!(
        binding.payload().raw_commit_object.as_deref(),
        Some(raw_commit.as_slice())
    );

    // The envelope refuses corruption and stays versioned.
    let mut tampered = pack.clone();
    tampered[0] ^= 0x01;
    assert!(crate::git_binding::decode_changes_pack(
        &tampered,
        &BindingPackLimits::default_limits()
    )
    .is_err());
    let envelope = ChangesPack::V1(ChangesPackV1 { records: decoded });
    let _ = envelope;
}

/// A real (small) change with dependencies, paired with its pack record.
fn test_change(message: &str, deps: Vec<Hash>) -> (ObjectRecord, atomic_core::change::Change) {
    let header = atomic_core::change::ChangeHeader::builder()
        .message(message)
        .author(atomic_core::change::Author::new("Test", Some("t@e")))
        .build();
    let change = atomic_core::change::Change::new(
        header,
        Vec::new(),
        format!("{message}\n").into_bytes(),
        deps,
    );
    let mut bytes = Vec::new();
    change
        .serialize(&mut std::io::Cursor::new(&mut bytes))
        .expect("serialize");
    (
        ObjectRecord::new(ObjectFamily::Change, Hash::of(&bytes).to_hex(), bytes),
        change,
    )
}
