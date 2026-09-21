//! CB-6C end-to-end: exact resurrection through the real CLI and real Git.
//!
//! Proves through the real binary and real Git plumbing (RFC §5.1–5.2, §8.6):
//! - a fresh clone fetches the verified closure (pack fallback) and resurrects
//!   the exact original change hashes, Merkle order, and Merkle state from the
//!   binding — no synthesized substitute,
//! - the projection proof runs before publication,
//! - tampering with the published binding fails closed.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(root: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .output()
        .expect("run atomic")
}

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn hex_of_oid(oid: &atomic_core::operation::GitObjectId) -> String {
    oid.as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A publisher: Git baseline, imported bridge baseline, three recorded changes,
/// and a bound projection commit whose tree is the projected closure (built
/// through the projection API — the same facts `git bridge reconcile` exports).
fn setup_publisher(root: &Path, home: &Path) -> (git2::Oid, String) {
    // A native Atomic repository (no Git, no Git import): every closure member
    // is a hash-authoritative V3 change without unhashed sections, so the
    // bounded pack can carry it (RFC §12.13), and no bridge baseline is needed.
    assert_success(&atomic(root, home, &["init"]), "atomic init");

    for (name, message) in [("a.txt", "one"), ("b.txt", "two"), ("c.txt", "three")] {
        fs::write(root.join(name), format!("{message}\n")).expect("edit file");
        assert_success(&atomic(root, home, &["add", name]), "add");
        assert_success(&atomic(root, home, &["record", "-m", message]), "record");
    }

    // Materialize the projected closure as a real Git commit over the
    // projected tree so the binding has a bound commit the proof can verify.
    let project = {
        let repo = atomic_repository::Repository::open_readonly(root).expect("open atomic");
        let policy = atomic_repository::ConversionPolicy::new(
            atomic_core::operation::GitHashAlgorithm::Sha1,
        );
        let view = repo.current_view().to_string();
        repo.project_tree(&view, &policy).expect("project tree")
    };
    let git = git2::Repository::init(root).expect("init git");
    {
        let odb = git.odb().expect("odb");
        for (expected, object) in project.git.objects.iter() {
            let kind = match object.kind {
                atomic_repository::GitObjectKind::Blob => git2::ObjectType::Blob,
                atomic_repository::GitObjectKind::Tree => git2::ObjectType::Tree,
            };
            let written = odb.write(kind, &object.bytes).expect("write object");
            assert_eq!(written.as_bytes(), expected.as_bytes());
        }
    }
    let tree_oid = git2::Oid::from_bytes(project.git.root.as_bytes()).expect("tree oid");
    let tree = git.find_tree(tree_oid).expect("find tree");
    let signature = git2::Signature::now("Publisher", "pub@example.com").expect("signature");
    let commit = git
        .commit(
            Some("refs/heads/master"),
            &signature,
            &signature,
            "bound projection",
            &tree,
            &[],
        )
        .expect("bind commit");
    git.set_head("refs/heads/master").expect("set head");
    (commit, hex_of_oid(&project.git.root))
}

/// Assemble the bounded changes.pack for the closure from the local store.
fn changes_pack_for(
    repo: &atomic_repository::Repository,
    ordered: &[atomic_core::Hash],
) -> Vec<u8> {
    let mut records = Vec::with_capacity(ordered.len());
    for hash in ordered {
        let bytes = fs::read(repo.change_store().change_path(hash)).expect("read change");
        records.push(atomic_objects::ObjectRecord::new(
            atomic_objects::ObjectFamily::Change,
            atomic_core::Hash::of(&bytes).to_hex(),
            bytes,
        ));
    }
    atomic_repository::git_binding::assemble_changes_pack(
        &records,
        &atomic_repository::git_binding::BindingPackLimits::default_limits(),
    )
    .expect("assemble pack")
}

/// Publish a signed binding for `commit` (tree `tree_hex`) over the view's
/// effective closure with the pack attached — the repository-level contract of
/// `git bridge binding publish`.
fn publish_binding_for(root: &Path, home: &Path, commit: &git2::Oid, tree_hex: &str) -> String {
    let _ = home;
    let repo = atomic_repository::Repository::open(root).expect("open atomic");
    let working_copy = repo.require_working_copy_id().expect("working copy");
    let view = repo.desired_view_name(working_copy).expect("desired view");
    let git = git2::Repository::open(root).expect("open git");
    let identity = repo.view_identity(&view).expect("view identity");
    let history: Vec<atomic_core::Hash> = repo
        .effective_history(Some(&view))
        .expect("history")
        .iter()
        .map(|entry| entry.hash)
        .collect();
    // The whole recorded closure: native changes without unhashed sections.
    let ordered: Vec<atomic_core::Hash> = history;
    let mut merkle = atomic_core::types::Merkle::ZERO;
    for hash in &ordered {
        merkle = merkle.next(hash);
    }
    let raw_commit = git
        .odb()
        .expect("odb")
        .read(*commit)
        .expect("read commit")
        .data()
        .to_vec();
    let parents: Vec<atomic_repository::git_binding::GitOid> = git
        .find_commit(*commit)
        .expect("commit")
        .parent_ids()
        .map(|oid| {
            atomic_repository::git_binding::GitOid::from_hex(&oid.to_string()).expect("parent")
        })
        .collect();
    let mut secret = [0u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        *byte = 0x60u8.wrapping_add((index as u8) * 5 + 3);
    }
    let signer = atomic_identity::keypair::KeyPair::from_secret_key(
        atomic_identity::keypair::SecretKey::from_bytes(&secret),
    );
    let operation = {
        use atomic_repository::OperationHeadState;
        match repo
            .operation_log(
                atomic_core::operation::OperationScope::WorkingCopy(working_copy),
                Some(1),
                false,
            )
            .expect("op log")
            .head_state
        {
            OperationHeadState::Single(head) => head,
            other => panic!("expected a single operation head, got {other:?}"),
        }
    };
    let mut payload = atomic_repository::git_binding::GitStateBindingPayload {
        version: atomic_repository::git_binding::BINDING_VERSION,
        git_object_format: atomic_repository::git_binding::GitObjectFormat::Sha1,
        git_commit: atomic_repository::git_binding::GitOid::from_hex(&commit.to_string())
            .expect("commit oid"),
        git_tree: atomic_repository::git_binding::GitOid::from_hex(tree_hex).expect("tree oid"),
        git_parents: parents,
        raw_commit_object: Some(raw_commit),
        set_id: identity.set_id,
        merkle_state: merkle,
        view_hint: Some(view.clone()),
        ordered_changes: ordered.clone(),
        closure_root: atomic_core::Hash::from_bytes([0u8; 32]),
        operation,
        origin: atomic_repository::git_binding::CausalOrigin::ExactAtomicResurrection,
        loss: Vec::new(),
        provenance_roots: Vec::new(),
        attestation_roots: Vec::new(),
        signer: atomic_repository::git_binding::BindingSigner::for_keypair(&signer),
    };
    payload.closure_root = payload.compute_closure_root();
    let binding = atomic_repository::git_binding::GitStateBinding::sign(payload, &signer)
        .expect("sign binding");
    let pack = changes_pack_for(&repo, &ordered);
    repo.publish_binding_with_changes_pack(working_copy, &git, &binding, None, &pack)
        .expect("publish binding");
    binding.id().to_hex()
}

fn effective_hashes(root: &Path) -> Vec<String> {
    use atomic_core::types::Base32;
    let repo = atomic_repository::Repository::open_readonly(root).expect("open atomic");
    let view = repo.current_view().to_string();
    repo.effective_history(Some(&view))
        .expect("effective history")
        .iter()
        .map(|entry| entry.hash.to_base32())
        .collect()
}

/// The target view's members via the read-only API (hash-authoritative).
fn view_members(root: &Path, view: &str) -> Vec<String> {
    use atomic_core::types::Base32;
    let repo = atomic_repository::Repository::open_readonly(root).expect("open atomic");
    repo.effective_history(Some(view))
        .expect("effective history")
        .iter()
        .map(|entry| entry.hash.to_base32())
        .collect()
}

fn transfer_to_client(publisher: &Path, remote_dir: &Path, client: &Path) {
    let run_git = |dir: &Path, args: &[&str], context: &str| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|error| panic!("{context}: {error}"));
        assert!(
            out.status.success(),
            "{context} failed: {}",
            output_text(&out)
        );
    };
    run_git(
        publisher,
        &[
            "push",
            remote_dir.to_str().unwrap(),
            "refs/heads/*:refs/heads/*",
            "refs/atomic/bindings/*:refs/atomic/bindings/*",
        ],
        "push branch + binding namespace",
    );
    run_git(
        publisher.parent().unwrap(),
        &[
            "clone",
            remote_dir.to_str().unwrap(),
            client.file_name().unwrap().to_str().unwrap(),
        ],
        "clone to client",
    );
    run_git(
        client,
        &[
            "config",
            "--add",
            "remote.origin.fetch",
            "+refs/atomic/bindings/*:refs/atomic/bindings/*",
        ],
        "configure binding fetch refspec",
    );
    run_git(client, &["fetch", "origin"], "fetch binding refs");
}

fn make_bare_remote(temp: &TempDir) -> std::path::PathBuf {
    let remote_dir = temp.path().join("remote.git");
    fs::create_dir_all(&remote_dir).expect("remote dir");
    let git_init = Command::new("git")
        .args(["init", "--bare", remote_dir.to_str().unwrap()])
        .output()
        .expect("init bare");
    assert!(git_init.status.success(), "bare init failed");
    remote_dir
}

#[test]
fn fresh_clone_resurrects_the_exact_closure_through_the_published_binding() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let publisher = temp.path().join("publisher");
    fs::create_dir_all(&publisher).expect("publisher");

    let (commit, tree_hex) = setup_publisher(&publisher, &home);
    let id = publish_binding_for(&publisher, &home, &commit, &tree_hex);

    // Transfer through real Git to a fresh client.
    let remote_dir = make_bare_remote(&temp);
    let client = temp.path().join("client");
    transfer_to_client(&publisher, &remote_dir, &client);

    // The client initializes its Atomic repository, ties the published
    // binding into its store (transport), and its bridge baseline import then
    // takes the resurrection path over synthesis.
    assert_success(&atomic(&client, &home, &["init"]), "client atomic init");
    {
        let repo = atomic_repository::Repository::open(&client).expect("client atomic");
        let git = git2::Repository::open(&client).expect("client git");
        let mut id_bytes = [0u8; 32];
        for (index, chunk) in id.as_bytes().chunks(2).enumerate() {
            id_bytes[index] = u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16).unwrap();
        }
        let binding = repo
            .load_published_binding(
                &git,
                &atomic_repository::git_binding::BindingId::from_bytes(id_bytes),
            )
            .expect("load published binding")
            .expect("binding present on the fetched ref");
        repo.store_binding(&binding).expect("store binding");
    }
    assert_success(
        &atomic(&client, &home, &["git", "import", "--no-vault"]),
        "client git import (resurrection over synthesis)",
    );

    // The imported branch view carries the publisher's exact closure, in order.
    let publisher_hashes = effective_hashes(&publisher);
    let member_hashes = view_members(&client, "master");
    assert_eq!(
        member_hashes, publisher_hashes,
        "the fresh clone must carry the exact original closure in Merkle order"
    );

    // The CLI resurrect subcommand restores the same closure into a second
    // view (verified adoption; already-present changes are reported, not
    // re-applied).
    assert_success(
        &atomic(&client, &home, &["view", "create", "fresh"]),
        "create target view",
    );
    assert_success(
        &atomic(&client, &home, &["git", "bridge", "binding", "fetch", &id]),
        "binding fetch",
    );
    let resurrect = atomic(
        &client,
        &home,
        &[
            "git",
            "bridge",
            "binding",
            "resurrect",
            &id,
            "--to-view",
            "fresh",
        ],
    );
    let resurrect_text = output_text(&resurrect);
    assert!(
        resurrect.status.success(),
        "resurrection must succeed: {resurrect_text}"
    );
    let fresh_members = view_members(&client, "fresh");
    assert_eq!(
        fresh_members, publisher_hashes,
        "the second view must carry the exact original closure in Merkle order"
    );
}

/// The default view name on the client side.
const DEFAULT_VIEW_NAME: &str = "dev";

#[test]
fn a_tampered_published_binding_fails_resurrection_closed() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let publisher = temp.path().join("publisher");
    fs::create_dir_all(&publisher).expect("publisher");

    let (commit, tree_hex) = setup_publisher(&publisher, &home);
    let id = publish_binding_for(&publisher, &home, &commit, &tree_hex);

    let remote_dir = make_bare_remote(&temp);
    let client = temp.path().join("client");
    transfer_to_client(&publisher, &remote_dir, &client);

    // Tamper: flip a byte inside the published binding blob and commit the
    // tampered tree over the create-only ref.
    {
        let repo = git2::Repository::open(&client).expect("open client git");
        let reference = repo
            .find_reference(&format!("refs/atomic/bindings/{}/{}", &id[..2], &id))
            .expect("binding ref");
        let binding_commit = reference.peel_to_commit().expect("peel");
        let tree = binding_commit.tree().expect("tree");
        let entry = tree.get_name("binding.cbor").expect("binding blob");
        let blob = repo.find_blob(entry.id()).expect("blob");
        let mut bytes = blob.content().to_vec();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        let new_blob = repo.blob(&bytes).expect("rewrite blob");
        let mut builder = repo.treebuilder(Some(&tree)).expect("treebuilder");
        builder
            .insert("binding.cbor", new_blob, 0o100644)
            .expect("insert");
        let new_tree_oid = builder.write().expect("tree");
        let new_tree = repo.find_tree(new_tree_oid).expect("tree");
        let signature = git2::Signature::now("Attacker", "attacker@example.com").expect("sig");
        let _ = repo.commit(
            Some(reference.name().expect("ref name")),
            &signature,
            &signature,
            "tampered",
            &new_tree,
            &[&binding_commit],
        );
    }

    assert_success(
        &atomic(&client, &home, &["git", "import", "--no-vault"]),
        "client git import",
    );
    assert_success(
        &atomic(&client, &home, &["view", "create", "fresh"]),
        "create target view",
    );
    let members_before = view_members(&client, "fresh");
    let fetch = atomic(&client, &home, &["git", "bridge", "binding", "fetch", &id]);
    assert!(
        !fetch.status.success(),
        "a tampered binding must fail verification before any closure fetch: {}",
        output_text(&fetch)
    );
    let resurrect = atomic(
        &client,
        &home,
        &[
            "git",
            "bridge",
            "binding",
            "resurrect",
            &id,
            "--to-view",
            "fresh",
        ],
    );
    let text = output_text(&resurrect);
    assert!(
        !resurrect.status.success(),
        "a tampered binding must never resurrect: {text}"
    );
    assert!(
        text.contains("does not match Git")
            || text.contains("fails cryptography")
            || text.contains("carries invalid binding bytes"),
        "the refusal must name the verification failure: {text}"
    );
    // Nothing was adopted into the target view: it keeps only the membership
    // it inherited at creation time.
    let members = view_members(&client, "fresh");
    assert_eq!(
        members, members_before,
        "a refused resurrection must not publish membership"
    );
}
