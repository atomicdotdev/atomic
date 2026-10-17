//! `atomic intent attest --prepare` / `--signed`: attesting an intent with a
//! key that is not on this machine. The identity here is public-only — as an
//! agent identity is inside a sandbox whose key lives with a signing
//! service — so plain `attest` can't sign; `--prepare` says what to sign,
//! the key holder signs it, and `--signed` checks and records the result.

use std::path::Path;
use std::process::{Command, Output};

use atomic_identity::{Identity, IdentityStore, IdentityType, IdentityUsage, KeyPair};

fn atomic(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atomic"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("ATOMIC_CONFIG_DIR", home.join(".atomic"))
        .output()
        .unwrap()
}

fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A repo with one intent, a default human identity, and a public-only
/// agent identity whose key only the test holds.
fn setup() -> (tempfile::TempDir, tempfile::TempDir, String, KeyPair) {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    ok(&atomic(
        home.path(),
        repo.path(),
        &[
            "identity",
            "new",
            "ada",
            "--email",
            "ada@example.com",
            "--set-default",
        ],
    ));
    ok(&atomic(home.path(), repo.path(), &["init"]));
    ok(&atomic(
        home.path(),
        repo.path(),
        &["intent", "new", "Add a greeting"],
    ));

    let key = KeyPair::generate();
    let agent = Identity::builder("ada+bot")
        .identity_type(IdentityType::Agent)
        .usage(IdentityUsage::Bot)
        .public_key(key.public.clone())
        .build()
        .unwrap();
    IdentityStore::open(&home.path().join(".atomic").join("identities"))
        .unwrap()
        .save(&agent)
        .unwrap();

    let list = ok(&atomic(
        home.path(),
        repo.path(),
        &["intent", "list", "--json"],
    ));
    let intents: serde_json::Value = serde_json::from_str(&list).unwrap();
    let id = intents
        .as_array()
        .and_then(|a| a.first())
        .and_then(|i| i.get("id"))
        .and_then(|v| v.as_str())
        .expect("one intent")
        .to_string();
    (home, repo, id, key)
}

/// What the key holder does with `--prepare`'s output.
fn sign_elsewhere(prepared: &str, key: &KeyPair) -> serde_json::Value {
    let prepared: serde_json::Value = serde_json::from_str(prepared).unwrap();
    let bytes = data_encoding::BASE64
        .decode(prepared["signingBytes"].as_str().unwrap().as_bytes())
        .unwrap();
    let signature = atomic_identity::signing::Signer::new(key).sign(&bytes);
    atomic_canonical::proof::attach_proof(prepared["document"].clone(), &key.public, &signature)
}

#[test]
fn an_intent_is_attested_by_a_key_held_elsewhere() {
    let (home, repo, id, key) = setup();
    let (home, repo) = (home.path(), repo.path());

    // No key here: plain attest can't sign as the agent.
    let out = atomic(
        home,
        repo,
        &["intent", "attest", &id, "--identity", "ada+bot"],
    );
    assert!(!out.status.success(), "attest without a key must fail");

    let prepared = ok(&atomic(
        home,
        repo,
        &[
            "intent",
            "attest",
            &id,
            "--identity",
            "ada+bot",
            "--prepare",
        ],
    ));
    let signed = sign_elsewhere(&prepared, &key);
    let file = home.join("signed.json");
    std::fs::write(&file, signed.to_string()).unwrap();
    ok(&atomic(
        home,
        repo,
        &[
            "intent",
            "attest",
            &id,
            "--identity",
            "ada+bot",
            "--signed",
            file.to_str().unwrap(),
        ],
    ));

    let report: serde_json::Value = serde_json::from_str(&ok(&atomic(
        home,
        repo,
        &["intent", "validate", &id, "--json"],
    )))
    .unwrap();
    assert_eq!(report["conforms"], true, "{report}");
    assert_eq!(
        signed["attributedTo"],
        atomic_canonical::did::did_for_public_key(&key.public)
    );
}

#[test]
fn a_signature_by_another_key_is_refused() {
    let (home, repo, id, _key) = setup();
    let (home, repo) = (home.path(), repo.path());
    let prepared = ok(&atomic(
        home,
        repo,
        &[
            "intent",
            "attest",
            &id,
            "--identity",
            "ada+bot",
            "--prepare",
        ],
    ));
    let signed = sign_elsewhere(&prepared, &KeyPair::generate());
    let file = home.join("signed.json");
    std::fs::write(&file, signed.to_string()).unwrap();
    let out = atomic(
        home,
        repo,
        &[
            "intent",
            "attest",
            &id,
            "--identity",
            "ada+bot",
            "--signed",
            file.to_str().unwrap(),
        ],
    );
    assert!(
        !out.status.success(),
        "a signature by the wrong key must not be recorded"
    );
}
