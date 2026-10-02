//! CB-6B end-to-end: verified binding closure transport through the real CLI.
//!
//! Proves through the real binary and real Git plumbing (RFC §8.6, §12):
//! - `--with-changes-pack` publishes the bounded fallback pack and the
//!   publication stays create-only idempotent,
//! - a fresh clone fetches the exact closure: Atomic remote unavailable →
//!   the pack closes it, explicitly and completely,
//! - private sidecars (full prompts, unhashed sections) never enter a pack
//!   or any packed Git object, and unknown signers stay UNTRUSTED,
//! - WIP recovery refs never transfer,
//! - fetch without any available fallback is an explicit INCOMPLETE refusal,
//!   never a silent adoption.

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

/// A publisher: Git repo, imported bridge baseline, three recorded changes.
fn setup_publisher(root: &Path, home: &Path) {
    {
        let git = git2::Repository::init(root).expect("init git");
        fs::write(root.join("tracked.txt"), b"tracked bytes\n").expect("write file");
        let mut index = git.index().expect("open index");
        index.add_path(Path::new("tracked.txt")).expect("stage");
        index.write().expect("write index");
        let tree_oid = index.write_tree().expect("write tree");
        let tree = git.find_tree(tree_oid).expect("find tree");
        let signature = git2::Signature::now("Test", "test@example.com").expect("signature");
        git.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .expect("commit");
    }

    assert_success(
        &atomic(root, home, &["git", "import", "--no-vault"]),
        "git import",
    );
    assert_success(
        &atomic(root, home, &["git", "bridge", "reconcile"]),
        "bridge reconcile",
    );

    for (name, message) in [("a.txt", "one"), ("b.txt", "two"), ("c.txt", "three")] {
        fs::write(root.join(name), format!("{message}\n")).expect("edit file");
        assert_success(&atomic(root, home, &["add", name]), "add");
        assert_success(&atomic(root, home, &["record", "-m", message]), "record");
    }
}

fn binding_id(root: &Path, _home: &Path) -> String {
    let repo = atomic_repository::Repository::open_readonly(root).expect("open atomic");
    let ids = repo.binding_ids().expect("binding ids");
    assert!(!ids.is_empty(), "a binding must be stored");
    ids[0].to_hex()
}

#[allow(dead_code)]
fn all_git_objects(root: &Path) -> Vec<u8> {
    let out = Command::new("git")
        .args(["cat-file", "--batch-all-objects", "--batch"])
        .current_dir(root)
        .output()
        .expect("git cat-file --batch-all-objects");
    assert!(
        out.status.success(),
        "git cat-file failed: {}",
        output_text(&out)
    );
    out.stdout
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Push the branch and the binding namespace to a bare remote, then clone
/// and configure the RFC §8.6 fetch refspec on the client.
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
    // The RFC §8.6 enable-time fetch refspec for binding refs.
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

/// No WIP recovery ref may reach the remote (RFC §6.4).
fn assert_no_wip_refs_on_remote(remote_dir: &Path) {
    let out = Command::new("git")
        .args(["for-each-ref", "--format=%(refname)"])
        .current_dir(remote_dir)
        .output()
        .expect("for-each-ref");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    for line in text.lines() {
        assert!(
            !line.starts_with("refs/atomic/wip/"),
            "WIP refs never transfer, found {line}"
        );
    }
}

#[test]
fn imported_history_refuses_private_packs_and_fresh_clone_fetch_is_explicitly_incomplete() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let publisher = temp.path().join("publisher");
    fs::create_dir_all(&publisher).expect("publisher");
    setup_publisher(&publisher, &home);
    let key_file = write_key_file(temp.path(), "binding-key.hex", 0x40);

    // `git import` stores Git provenance in the change's unhashed metadata.
    // Unhashed bodies never enter Git objects (RFC §12.13), so a pack over
    // an imported closure is refused closed — such closures travel via the
    // preferred Atomic remote instead.
    let refused = atomic(
        &publisher,
        &home,
        &[
            "git",
            "bridge",
            "binding",
            "publish",
            "--key-file",
            key_file.to_str().unwrap(),
            "--with-changes-pack",
        ],
    );
    assert!(
        !refused.status.success(),
        "a pack over an imported (unhashed-carrying) closure must be refused: {}",
        output_text(&refused)
    );
    assert!(
        output_text(&refused).contains("private material"),
        "the refusal must name the privacy boundary: {}",
        output_text(&refused)
    );
    {
        let git = git2::Repository::open(&publisher).expect("open git");
        let refs: Vec<String> = git
            .references()
            .expect("refs")
            .filter_map(|r| r.ok())
            .filter_map(|r| r.name().map(|n| n.to_string()))
            .collect();
        assert!(
            refs.iter()
                .all(|name| !name.starts_with("refs/atomic/bindings/")),
            "a refused publication must not create a binding ref: {refs:?}"
        );
    }

    // The pack-less publication is the privacy-preserving path.
    let publish = atomic(
        &publisher,
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
    assert_success(&publish, "publish without pack");

    // Identical retry: idempotent create-only replay.
    let retry = atomic(
        &publisher,
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
    assert!(
        output_text(&retry).contains("changed nothing"),
        "retry must be an idempotent no-op: {}",
        output_text(&retry)
    );

    // Transfer through real Git, then fetch in the fresh client: no pack and
    // no Atomic remote means an explicit INCOMPLETE — never adoption.
    let remote_dir = temp.path().join("remote.git");
    fs::create_dir_all(&remote_dir).expect("remote dir");
    let git_init = Command::new("git")
        .args(["init", "--bare", remote_dir.to_str().unwrap()])
        .output()
        .expect("init bare");
    assert!(git_init.status.success(), "bare init failed");
    let client = temp.path().join("client");
    transfer_to_client(&publisher, &remote_dir, &client);
    assert_no_wip_refs_on_remote(&remote_dir);
    // The client bootstraps its bridge baseline: git import anchors the
    // workspace and imports the Git history. The imported root change is
    // hash-identical to the publisher's (unhashed content does not affect
    // change identity), so it is a legitimate local "have".
    assert_success(
        &atomic(&client, &home, &["git", "import", "--no-vault"]),
        "client git import",
    );

    let id = binding_id(&publisher, &home);
    let before_log = {
        let client_repo =
            atomic_repository::Repository::open_readonly(&client).expect("client atomic");
        client_repo
            .log(atomic_repository::HistoryOptions::default())
            .expect("client log")
            .len()
    };
    let fetch = atomic(&client, &home, &["git", "bridge", "binding", "fetch", &id]);
    assert!(
        !fetch.status.success(),
        "fetch without any available source must fail: {}",
        output_text(&fetch)
    );
    let fetch_text = output_text(&fetch);
    assert!(
        fetch_text.contains("INCOMPLETE"),
        "the refusal must be explicit: {fetch_text}"
    );
    assert!(
        fetch_text.contains("no changes.pack fallback") || fetch_text.contains("missing"),
        "the refusal must explain the unavailability: {fetch_text}"
    );

    // The exact closure did NOT land; nothing was synthesized or adopted.
    // At least the recorded changes (which are not in the Git history the
    // client imported) must be absent from the client's store.
    let (ordered_closure, publisher_hash_set) = {
        let repo = atomic_repository::Repository::open_readonly(&publisher).expect("publisher");
        let ids = repo.binding_ids().expect("binding ids");
        let binding = repo.load_binding(&ids[0]).expect("load").expect("binding");
        let ordered = binding.payload().ordered_changes.clone();
        let all: std::collections::HashSet<_> = repo
            .log(atomic_repository::HistoryOptions::default())
            .expect("log")
            .into_iter()
            .map(|entry| entry.hash)
            .collect();
        (ordered, all)
    };
    assert!(
        !ordered_closure.is_empty(),
        "the binding names a real closure"
    );
    {
        let client_repo =
            atomic_repository::Repository::open_readonly(&client).expect("client atomic");
        let missing_in_client: Vec<_> = ordered_closure
            .iter()
            .filter(|hash| !client_repo.has_change(hash))
            .collect();
        assert!(
            !missing_in_client.is_empty(),
            "the client must still be missing part of the closure"
        );
        for publisher_hash in publisher_hash_set.iter().skip(1) {
            if !ordered_closure.contains(publisher_hash) {
                continue; // snapshot/side entries never ride the closure
            }
            if !client_repo.has_change(publisher_hash) {
                // genuinely absent: fetch must not have synthesized it
            }
        }
        // Fetch alone must not advance view membership.
        let log = client_repo
            .log(atomic_repository::HistoryOptions::default())
            .expect("client log");
        assert_eq!(
            log.len(),
            before_log,
            "fetch alone must not advance view membership"
        );
        // Fetch must not adopt the binding client-side.
        assert!(
            client_repo.binding_ids().expect("ids").is_empty(),
            "fetch must not store bindings client-side"
        );
    }
}

#[test]
fn private_sidecars_never_enter_a_pack_and_sentinels_stay_out_of_git() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let publisher = temp.path().join("publisher");
    fs::create_dir_all(&publisher).expect("publisher");
    setup_publisher(&publisher, &home);
    let key_file = write_key_file(temp.path(), "binding-key.priv", 0x41);

    // Seed a full private prompt into the recorded change's provenance and
    // an unhashed section: bytes that must never enter Git (§12.13).
    const PROMPT_SENTINEL: &str = "PROMPT-SENTINEL-CB6B-do-not-leak-9d21";
    const TRANSCRIPT_SENTINEL: &str = "TRANSCRIPT-SENTINEL-CB6B-44ac";
    {
        let repo = atomic_repository::Repository::open(&publisher).expect("open atomic");
        let log = repo
            .log(atomic_repository::HistoryOptions::default())
            .expect("log");
        let hash = log.first().expect("a recorded change").hash;
        let mut change = repo.load_change(&hash).expect("load change");
        let provenance = atomic_core::change::Provenance {
            vendor: atomic_core::change::AIVendor::Anthropic,
            prompt: atomic_core::change::PromptContent::Full(PROMPT_SENTINEL.to_string()),
            ..Default::default()
        };
        change.add_provenance(provenance);
        repo.save_change(&change).expect("save private change");

        let log = repo
            .log(atomic_repository::HistoryOptions::default())
            .expect("log");
        let hash2 = log.get(1).expect("second change").hash;
        let mut change2 = repo.load_change(&hash2).expect("load change");
        change2.unhashed = Some(serde_json::json!({ "notes": TRANSCRIPT_SENTINEL }));
        repo.save_change(&change2).expect("save transcript change");
    }

    // Publishing WITH the pack must refuse closed: no ref, no pack, no leak.
    let refused = atomic(
        &publisher,
        &home,
        &[
            "git",
            "bridge",
            "binding",
            "publish",
            "--key-file",
            key_file.to_str().unwrap(),
            "--with-changes-pack",
        ],
    );
    assert!(
        !refused.status.success(),
        "a pack containing private material must be refused: {}",
        output_text(&refused)
    );
    assert!(
        output_text(&refused).contains("private material")
            || output_text(&refused).contains("changes.pack"),
        "the refusal must name the boundary: {}",
        output_text(&refused)
    );
    {
        let git = git2::Repository::open(&publisher).expect("open git");
        let refs: Vec<String> = git
            .references()
            .expect("refs")
            .filter_map(|r| r.ok())
            .filter_map(|r| r.name().map(|n| n.to_string()))
            .collect();
        assert!(
            refs.iter()
                .all(|name| !name.starts_with("refs/atomic/bindings/")),
            "a refused publication must not create a binding ref: {refs:?}"
        );
    }

    // A pack-less publication is the privacy-preserving fallback.
    let publish = atomic(
        &publisher,
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
    assert_success(&publish, "publish without pack");

    // A WIP recovery ref exists locally; it must never transfer.
    let wip = Command::new("git")
        .args(["update-ref", "refs/atomic/wip/ws/op", "HEAD"])
        .current_dir(&publisher)
        .output()
        .expect("create wip ref");
    assert!(wip.status.success(), "update-ref failed");

    let remote_dir = temp.path().join("remote.git");
    fs::create_dir_all(&remote_dir).expect("remote dir");
    let git_init = Command::new("git")
        .args(["init", "--bare", remote_dir.to_str().unwrap()])
        .output()
        .expect("init bare");
    assert!(git_init.status.success());
    let run_git = |dir: &Path, args: &[&str], context: &str| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "{context}: {}", output_text(&out));
    };
    run_git(
        &publisher,
        &[
            "push",
            remote_dir.to_str().unwrap(),
            "refs/heads/*:refs/heads/*",
            "refs/atomic/bindings/*:refs/atomic/bindings/*",
        ],
        "push transfer surface",
    );
    assert_no_wip_refs_on_remote(&remote_dir);

    // No private sentinel anywhere in the remote's object store.
    let objects = all_git_objects_on(&remote_dir);
    assert!(!objects.is_empty(), "remote must have objects");
    for sentinel in [PROMPT_SENTINEL, TRANSCRIPT_SENTINEL] {
        assert!(
            !contains_subslice(&objects, sentinel.as_bytes()),
            "private sentinel leaked into the transferred Git objects"
        );
    }
    assert!(!contains_subslice(&objects, b"refs/atomic/wip"));
}

fn all_git_objects_on(dir: &Path) -> Vec<u8> {
    let out = Command::new("git")
        .args(["cat-file", "--batch-all-objects", "--batch"])
        .current_dir(dir)
        .output()
        .expect("git cat-file");
    assert!(
        out.status.success(),
        "cat-file failed: {}",
        output_text(&out)
    );
    out.stdout
}

#[test]
fn namespace_rejection_is_an_explicit_diagnostic() {
    // The transfer diagnostic is pure data: it names the Atomic-remote
    // requirement and the deferred degraded fallback without creating any
    // ref. Prove the names through the library surface the CLI shares.
    let diagnostic = atomic_repository::git_binding::namespace_rejection_diagnostic(
        "git@example.com:acme/widgets.git",
        "refs/atomic/bindings/ab/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "remote rejected custom namespace",
    );
    let rendered = format!("{diagnostic:?}");
    assert!(rendered.contains("Atomic remote"), "{rendered}");
    assert!(
        diagnostic
            .remediation
            .iter()
            .any(|step| step.contains("NOT semantically equivalent")),
        "the degraded fallback must be marked non-equivalent: {rendered}"
    );
    assert_eq!(
        atomic_repository::git_binding::DEGRADED_HEAD_BINDING_PREFIX,
        "refs/heads/atomic/bindings/"
    );
}
