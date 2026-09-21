//! CB-6A end-to-end: signed bindings, trust policy, and packed-tree privacy.
//!
//! Proves through the real CLI binary and real Git plumbing:
//! - a signed binding publishes to a create-only `refs/atomic/bindings/...` ref,
//! - an identical retry is idempotent,
//! - `atomic git bridge binding verify` recomputes content and applies the
//!   trust policy (unknown signer → explicit UNTRUSTED),
//! - no transcripts, prompts, or unhashed private bodies enter any packed Git
//!   object, while root verification still succeeds afterwards.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use atomic_core::change::attestation::{AttestAgent, Attestation};
use atomic_core::change::ProvenanceGraph;
use atomic_core::change::{PromptContent, Provenance};
use atomic_identity::keypair::{KeyPair, SecretKey};
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

/// Hex-encoded deterministic signing key file (test-owned, never the global
/// identity store).
fn write_key_file(root: &Path, name: &str, seed: u8) -> std::path::PathBuf {
    let mut secret = [0u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        *byte = seed.wrapping_add((index as u8) * 7 + 11);
    }
    let hex: String = secret.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = root.join(name);
    fs::write(&path, hex).expect("write key file");
    path
}

fn setup_fixture(root: &Path, home: &Path) {
    {
        let git = git2::Repository::init(root).expect("init Git");
        fs::write(root.join("tracked.txt"), b"tracked bytes\n").expect("write file");
        let mut index = git.index().expect("open index");
        index
            .add_path(Path::new("tracked.txt"))
            .expect("stage file");
        index.write().expect("write index");
        let tree_oid = index.write_tree().expect("write tree");
        let tree = git.find_tree(tree_oid).expect("find tree");
        let signature = git2::Signature::now("Test", "test@example.com").expect("signature");
        git.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .expect("commit");
    }

    // Import the Git history into Atomic and anchor the bridge baseline.
    let import = atomic(root, home, &["git", "import", "--no-vault"]);
    assert_init(&import);
    let reconcile = atomic(root, home, &["git", "bridge", "reconcile"]);
    assert_init(&reconcile);

    // Create a real recorded edit so a causal operation exists to bind.
    fs::write(root.join("tracked.txt"), b"tracked bytes\nmore\n").expect("edit file");
    let add = atomic(root, home, &["add", "tracked.txt"]);
    assert_init(&add);
    let record = atomic(root, home, &["record", "-m", "record tracked file"]);
    assert_init(&record);
}

fn assert_init(output: &Output) {
    assert!(
        output.status.success(),
        "setup failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn binding_ref_names(git_dir: &Path) -> Vec<String> {
    let git = git2::Repository::open(git_dir).expect("open git");
    let mut refs = Vec::new();
    for reference in git.references().expect("list refs") {
        let reference = reference.expect("reference");
        if let Some(name) = reference.name() {
            if name.starts_with("refs/atomic/bindings/") {
                refs.push(name.to_string());
            }
        }
    }
    refs.sort();
    refs
}

#[test]
fn publish_is_create_only_idempotent_and_verifies_under_trust_policy() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let root = temp.path().join("repo");
    fs::create_dir_all(&root).expect("repo");
    setup_fixture(&root, &home);

    let key_file = write_key_file(temp.path(), "binding-key.hex", 0x30);

    // Publish through the CLI.
    let publish = atomic(
        &root,
        &home,
        &[
            "git",
            "bridge",
            "binding",
            "publish",
            "--key-file",
            key_file.to_str().unwrap(),
        ],
    );
    assert_success(&publish, "binding publish");

    let refs = binding_ref_names(&root);
    assert_eq!(refs.len(), 1, "exactly one binding ref: {refs:?}");
    assert!(refs[0].starts_with("refs/atomic/bindings/"));

    // Identical retry: idempotent.
    let retry = atomic(
        &root,
        &home,
        &[
            "git",
            "bridge",
            "binding",
            "publish",
            "--key-file",
            key_file.to_str().unwrap(),
        ],
    );
    assert_success(&retry, "idempotent retry");
    let retry_text = format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&retry.stdout),
        String::from_utf8_lossy(&retry.stderr)
    );
    assert!(
        retry_text.contains("changed nothing"),
        "retry must be an idempotent no-op: {retry_text}"
    );
    assert_eq!(binding_ref_names(&root), refs, "create-only: no second ref");

    // The binding id: read from the stored bindings through the library,
    // then release the handle before the CLI re-opens the database writable.
    let id_hex = {
        let repo = atomic_repository::Repository::open_readonly(&root).expect("open atomic");
        let ids = repo.binding_ids().expect("binding ids");
        assert_eq!(ids.len(), 1, "one binding stored");
        ids[0].to_hex()
    };

    // Unknown signer: content recomputes, provenance is UNTRUSTED and the
    // command fails closed.
    let untrusted = atomic(
        &root,
        &home,
        &["git", "bridge", "binding", "verify", &id_hex],
    );
    assert!(
        !untrusted.status.success(),
        "an unknown signer must fail the gate"
    );
    let untrusted_text = format!(
        "{}{}",
        String::from_utf8_lossy(&untrusted.stdout),
        String::from_utf8_lossy(&untrusted.stderr)
    );
    assert!(untrusted_text.contains("recomputed-ok"), "{untrusted_text}");
    assert!(untrusted_text.contains("UNTRUSTED"), "{untrusted_text}");

    // Add the signer to the repository trust policy and reload.
    let keypair = {
        let hex = fs::read_to_string(&key_file).unwrap();
        let mut secret = [0u8; 32];
        for (index, byte) in secret.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap();
        }
        KeyPair::from_secret_key(SecretKey::from_bytes(&secret))
    };
    let did = atomic_canonical::did::did_for_public_key(&keypair.public);
    fs::write(
        root.join(".atomic/config.toml"),
        format!("[git.trust]\nsigners = [\"{did}\"]\n"),
    )
    .unwrap();

    let trusted = atomic(
        &root,
        &home,
        &["git", "bridge", "binding", "verify", &id_hex],
    );
    assert_success(&trusted, "verify under trust policy");
    let trusted_text = String::from_utf8_lossy(&trusted.stdout).to_string();
    assert!(trusted_text.contains("trusted"), "{trusted_text}");
    assert!(!trusted_text.contains("UNTRUSTED"), "{trusted_text}");
}

#[test]
fn packed_git_objects_contain_no_private_sentinels() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let root = temp.path().join("repo");
    fs::create_dir_all(&root).expect("repo");
    setup_fixture(&root, &home);
    let key_file = write_key_file(temp.path(), "binding-key.priv", 0x31);

    // Seed distinctive private sentinels into the Atomic side: a full private
    // prompt inside recorded provenance, and an attestation whose notes carry
    // private session material. If any bridge path ever copied private
    // sidecars into Git, these bytes would appear in packed objects.
    const PROMPT_SENTINEL: &str = "PROMPT-SENTINEL-CB6A-do-not-leak-7f3a";
    const ATTESTATION_SENTINEL: &str = "ATTESTATION-NOTES-SENTINEL-CB6A-51de";
    let repo = atomic_repository::Repository::open(&root).expect("open atomic");

    let mut provenance = Provenance::default();
    provenance.vendor = atomic_core::change::AIVendor::Anthropic;
    provenance.model = "claude-sonnet-4-5".to_string();
    provenance.prompt = PromptContent::Full(PROMPT_SENTINEL.to_string());
    let mut change = repo
        .load_change(&latest_change_hash(&repo))
        .expect("load recorded change");
    change.add_provenance(provenance);
    repo.save_change(&change).expect("save private change");

    let mut attestation = Attestation::builder(
        "sentinel-session",
        AttestAgent::new("claude-code", "Claude Code", "anthropic"),
    )
    .cost_usd(0.42)
    .build();
    attestation.notes = Some(ATTESTATION_SENTINEL.to_string());
    repo.save_attestation(&attestation)
        .expect("save attestation");
    drop(repo);

    // Publish the binding, then pack the Git repository aggressively.
    let publish = atomic(
        &root,
        &home,
        &[
            "git",
            "bridge",
            "binding",
            "publish",
            "--key-file",
            key_file.to_str().unwrap(),
        ],
    );
    assert_success(&publish, "publish for privacy test");

    let gc = Command::new("git")
        .args(["gc", "--aggressive", "--prune=now"])
        .current_dir(&root)
        .output()
        .expect("git gc");
    assert!(
        gc.status.success(),
        "git gc failed: {}",
        String::from_utf8_lossy(&gc.stderr)
    );

    // Walk every object Git stores (packed and loose, decompressed) and grep
    // for the private sentinels.
    let all_objects = Command::new("git")
        .args(["cat-file", "--batch-all-objects", "--batch"])
        .current_dir(&root)
        .output()
        .expect("cat-file --batch-all-objects");
    assert!(all_objects.status.success(), "git cat-file failed");
    assert!(!all_objects.stdout.is_empty(), "git objects must exist");
    for sentinel in [PROMPT_SENTINEL.as_bytes(), ATTESTATION_SENTINEL.as_bytes()] {
        assert!(
            !contains_subslice(&all_objects.stdout, sentinel),
            "private sentinel bytes leaked into the packed Git object store"
        );
    }

    // Root verification still succeeds after packing: content recomputes,
    // and the stored binding still decodes and verifies.
    let id_hex = {
        let repo = atomic_repository::Repository::open_readonly(&root).expect("open atomic");
        let ids = repo.binding_ids().expect("binding ids");
        assert!(!ids.is_empty(), "binding stored");
        ids[0].to_hex()
    };
    let verify = atomic(
        &root,
        &home,
        &["git", "bridge", "binding", "verify", &id_hex],
    );
    // Verify intentionally fails the gate (unknown signer) but must have
    // recomputed the content first: provenance UNTRUSTED, not content.
    let verify_text = format!(
        "{}{}",
        String::from_utf8_lossy(&verify.stdout),
        String::from_utf8_lossy(&verify.stderr)
    );
    assert!(
        verify_text.contains("recomputed-ok"),
        "content recomputation must succeed after packing: {verify_text}"
    );
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The most recent change hash on the current view.
fn latest_change_hash(repo: &atomic_repository::Repository) -> atomic_core::Hash {
    let history = repo
        .log(atomic_repository::HistoryOptions::default())
        .expect("view history");
    history
        .into_iter()
        .next()
        .map(|entry| entry.hash)
        .expect("at least one recorded change")
}
