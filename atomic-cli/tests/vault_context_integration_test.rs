//! End-to-end contract tests for compact Vault candidate retrieval followed by
//! an exact-revision body pull through the real `atomic` CLI binary.

use std::path::Path;
use std::process::{Command, Output};

use atomic_core::pristine::vault::VaultEntryType;
use atomic_repository::Repository;
use serde_json::Value;
use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(dir: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run atomic")
}

fn write_memory(root: &Path, body: &str) {
    let repo = Repository::open(root).expect("open repository");
    repo.vault_store(
        "memory/auth-decision.md",
        VaultEntryType::Memory,
        body.as_bytes().to_vec(),
        r#"{"name":"auth-decision","status":"active","memoryKind":"lesson"}"#.to_string(),
    )
    .expect("store memory");
}

#[test]
fn compact_candidate_can_be_pulled_once_and_rejects_a_stale_revision() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    let init = atomic(root, &["init"]);
    assert!(
        init.status.success(),
        "atomic init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    write_memory(root, "Use RS256 signing for service tokens.");

    let candidates = atomic(
        root,
        &["vault", "context", "rs256", "--candidates-only", "--json"],
    );
    assert!(
        candidates.status.success(),
        "candidate retrieval failed: {}",
        String::from_utf8_lossy(&candidates.stderr)
    );
    let candidates: Value =
        serde_json::from_slice(&candidates.stdout).expect("candidate JSON contract");
    assert_eq!(candidates["mode"], "candidates");
    assert_eq!(
        candidates["candidate_authority"],
        "untrusted_historical_data"
    );
    assert!(candidates.get("context_markdown").is_none());
    assert!(candidates["candidates"][0].get("body").is_none());
    assert!(candidates["candidates"][0].get("preview").is_none());

    let candidate = &candidates["candidates"][0];
    let path = candidate["path"].as_str().expect("candidate path");
    let revision = candidate["revision_hash"]
        .as_str()
        .expect("candidate revision");

    let pull = atomic(
        root,
        &["vault", "show", path, "--revision", revision, "--json"],
    );
    assert!(
        pull.status.success(),
        "exact pull failed: {}",
        String::from_utf8_lossy(&pull.stderr)
    );
    let pull: Value = serde_json::from_slice(&pull.stdout).expect("body JSON contract");
    assert_eq!(pull["mode"], "vault_entry_body");
    assert_eq!(pull["content_authority"], "untrusted_historical_data");
    assert_eq!(pull["revision_hash"], revision);
    assert_eq!(pull["content"], "Use RS256 signing for service tokens.");

    let lowercase_revision = revision.to_ascii_lowercase();
    let lowercase_pull = atomic(
        root,
        &[
            "vault",
            "show",
            path,
            "--revision",
            &lowercase_revision,
            "--json",
        ],
    );
    assert!(
        lowercase_pull.status.success(),
        "lowercase Base32 revision should identify the same entry: {}",
        String::from_utf8_lossy(&lowercase_pull.stderr)
    );

    write_memory(root, "Use EdDSA signing for service tokens.");

    let stale_pull = atomic(
        root,
        &["vault", "show", path, "--revision", revision, "--json"],
    );
    assert_eq!(stale_pull.status.code(), Some(2));
    assert!(
        stale_pull.stdout.is_empty(),
        "stale pull must not leak a body"
    );
    assert!(
        String::from_utf8_lossy(&stale_pull.stderr).contains("Vault entry revision changed"),
        "stale pull should explain the revision mismatch: {}",
        String::from_utf8_lossy(&stale_pull.stderr)
    );
}
