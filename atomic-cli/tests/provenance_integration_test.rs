//! Integration tests for `atomic provenance {trace,show}`, driving the real CLI
//! binary end-to-end against a temporary repository with a real, in-process
//! `ProvenanceGraph` saved via `atomic-repository` (NOT the forbidden
//! `atomic-agent` capture path).
//!
//! These prove the PROJECTION path end-to-end:
//!   1. a REAL `ProvenanceGraph` is persisted with `repo.save_provenance_graph`,
//!   2. `atomic provenance show` projects it UNSIGNED on the fly (the baseline's
//!      "signable, not signed" default),
//!   3. `atomic provenance show --sign` emits the signable artifact, and its proof
//!      verifies via `verify_prov` against the resolved identity's public key,
//!   4. `atomic provenance trace` prints the human flywheel chain, and
//!   5. compute-on-demand: no new files appear and the provenance-graph count is
//!      unchanged after trace/show.
//!
//! The REV_DEPS fallback is exercised too: a graph whose explained change was
//! never made internal is invisible to REV_DEPS, and the disk-scan fallback
//! still finds it.
//!
//! Windows is excluded: the tests isolate the identity store by pointing `HOME`
//! at a temp dir, but on Windows `dirs::home_dir()` resolves via the system API
//! (FOLDERID_Profile) and ignores the env var, so the store cannot be redirected
//! from a test. An explicit identity-store override (env/flag) would lift this.
#![cfg(not(windows))]

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Output};

use atomic_canonical::did::did_for_public_key;
use atomic_canonical::prov::verify_prov;
use atomic_core::change::provenance_graph::{ProvenanceNode, ProvenanceNodeKind};
use atomic_core::change::ProvenanceGraph;
use atomic_core::types::{Base32, Hash};
use atomic_identity::IdentityStore;
use atomic_repository::Repository;
use serde_json::Value;
use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

/// Run `atomic <args>` inside `repo_dir` with `HOME` pointed at `home_dir` so the
/// identity store resolves to a test-owned location.
fn atomic(repo_dir: &Path, home_dir: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(repo_dir)
        .env("HOME", home_dir)
        .output()
        .expect("run atomic")
}

/// A prov node so the graph is non-trivial (the projection ignores nodes, but a
/// real graph carries them).
fn goal_node() -> ProvenanceNode {
    ProvenanceNode {
        id: "s-1".into(),
        kind: ProvenanceNodeKind::Goal,
        timestamp: 1_735_689_600_000,
        summary: "Fix the auth bug".into(),
        detail: None,
        change_hash: None,
        tool_name: None,
        tool_call_id: None,
        duration_ms: None,
        classified: false,
        confidence: None,
        consolidated_from: Vec::new(),
    }
}

/// Recursively collect every file path under `root` (relative), for a
/// before/after "nothing was written" assertion.
fn snapshot_files(root: &Path) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    fn walk(dir: &Path, base: &Path, set: &mut BTreeSet<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, base, set);
            } else if let Ok(rel) = path.strip_prefix(base) {
                set.insert(rel.to_string_lossy().to_string());
            }
        }
    }
    walk(root, root, &mut set);
    set
}

/// Create a default identity in a test-owned identity store rooted under
/// `home_dir/.atomic/identities`, matching `IdentityStore::open_default()` when
/// `HOME=home_dir`. Returns the identity's public key for verification.
fn create_default_identity(home_dir: &Path) -> atomic_identity::keypair::PublicKey {
    let store_root = home_dir.join(".atomic").join("identities");
    let mut store = IdentityStore::open(&store_root).expect("open identity store");
    let keypair = atomic_identity::keypair::KeyPair::generate();
    let identity = atomic_identity::identity::Identity::new("tester", &keypair);
    store
        .save_with_keypair(&identity, &keypair, None)
        .expect("save identity");
    store.set_default(&identity.id).expect("set default");
    keypair.public
}

/// Record a file into a fresh repo and return a real, internal change `Hash`.
///
/// `atomic init` itself records baseline changes ("Initialize repository",
/// "Initialize vault"), so the repo has several internal changes; any one of
/// them is a valid target for the projection (REV_DEPS works because it is
/// internal). We take the first.
fn repo_with_internal_change(repo_dir: &Path, home_dir: &Path) -> Hash {
    assert!(atomic(repo_dir, home_dir, &["init"]).status.success());
    std::fs::write(repo_dir.join("file.txt"), b"v1\n").unwrap();
    assert!(atomic(repo_dir, home_dir, &["add", "file.txt"])
        .status
        .success());
    assert!(atomic(repo_dir, home_dir, &["record", "-m", "rec"])
        .status
        .success());

    let repo = Repository::open(repo_dir).expect("open repo");
    let mut changes = repo.iter_changes();
    changes
        .next()
        .expect("at least one recorded change")
        .expect("iter change")
}

#[test]
fn provenance_show_defaults_unsigned_and_sign_flag_verifies_and_writes_nothing() {
    let repo_tmp = TempDir::new().unwrap();
    let home_tmp = TempDir::new().unwrap();
    let repo_dir = repo_tmp.path();
    let home_dir = home_tmp.path();

    let pubkey = create_default_identity(home_dir);
    let change_hash = repo_with_internal_change(repo_dir, home_dir);

    // Save a REAL ProvenanceGraph explaining the recorded change, via the
    // repository path (NOT the atomic-agent capture path).
    {
        let repo = Repository::open(repo_dir).expect("open repo");
        let graph = ProvenanceGraph::builder("session-abc", "claude-code")
            .agent_display_name("Claude Code")
            .agent_vendor("anthropic")
            .add_node(goal_node())
            .add_change_explained(change_hash)
            .build();
        repo.save_provenance_graph(&graph).expect("save graph");
    }

    // Snapshot the repo tree; trace/show must not add or remove any files.
    let before = snapshot_files(repo_dir);
    let graphs_before = {
        let repo = Repository::open(repo_dir).expect("open repo");
        repo.find_provenance_for_change_scan(&change_hash)
            .expect("scan")
            .len()
    };

    // `show` — emits the signed PROV JSON-LD to stdout.
    let show = atomic(
        repo_dir,
        home_dir,
        &["provenance", "show", &change_hash.to_base32()],
    );
    assert!(
        show.status.success(),
        "provenance show failed: {}",
        String::from_utf8_lossy(&show.stderr)
    );
    let stdout = String::from_utf8(show.stdout).unwrap();
    let value: Value = serde_json::from_str(&stdout).expect("show emits valid JSON-LD");

    // Shape assertions.
    assert_eq!(
        value.get("@id").and_then(Value::as_str),
        Some(format!("urn:atomic:provgraph:{}", change_hash.to_base32()).as_str())
    );
    let graph_nodes = value.get("@graph").and_then(Value::as_array).unwrap();
    assert_eq!(graph_nodes.len(), 3);
    let activity = graph_nodes
        .iter()
        .find(|n| n.get("@type").and_then(Value::as_str) == Some("prov:Activity"))
        .unwrap();
    assert_eq!(
        activity.get("generated"),
        Some(&serde_json::json!([format!(
            "urn:atomic:change:{}",
            change_hash.to_base32()
        )]))
    );
    // `used` is omitted when empty (unknown links omitted, never invented).
    assert!(activity.get("used").is_none());
    let agent = graph_nodes
        .iter()
        .find(|n| n.get("@type").and_then(Value::as_str) == Some("prov:SoftwareAgent"))
        .unwrap();
    let agent_id = agent.get("@id").and_then(Value::as_str).unwrap();
    assert_eq!(agent_id, "urn:atomic:agent:claude");
    assert!(!agent_id.starts_with("did:"), "agent id must NOT be a did");
    assert_eq!(
        agent.get("label").and_then(Value::as_str),
        Some("Claude Code")
    );

    // Default `show` is UNSIGNED — the baseline's "signable, not signed" unit.
    assert!(
        value.get("proof").is_none(),
        "default show must be unsigned"
    );
    assert!(value.get("attributedTo").is_none());
    assert!(value.get("contentHash").is_none());

    // `show --sign` emits the SIGNABLE artifact: it verifies against the resolved
    // key and carries the top-level attributedTo/contentHash/proof envelope.
    let signed_out = atomic(
        repo_dir,
        home_dir,
        &["provenance", "show", "--sign", &change_hash.to_base32()],
    );
    assert!(
        signed_out.status.success(),
        "provenance show --sign failed: {}",
        String::from_utf8_lossy(&signed_out.stderr)
    );
    let signed: Value = serde_json::from_str(&String::from_utf8(signed_out.stdout).unwrap())
        .expect("show --sign emits valid JSON-LD");
    verify_prov(&signed, &pubkey).expect("signed PROV proof must verify");
    assert_eq!(
        signed.get("attributedTo").and_then(Value::as_str),
        Some(did_for_public_key(&pubkey).as_str())
    );
    assert!(signed.get("proof").is_some());
    assert!(signed
        .get("contentHash")
        .and_then(Value::as_str)
        .is_some_and(|h| h.starts_with("blake3:")));

    // `trace` — prints the human chain (change -> activity -> generated -> ...).
    let trace = atomic(
        repo_dir,
        home_dir,
        &["provenance", "trace", &change_hash.to_base32()],
    );
    assert!(
        trace.status.success(),
        "provenance trace failed: {}",
        String::from_utf8_lossy(&trace.stderr)
    );
    let trace_out = String::from_utf8(trace.stdout).unwrap();
    assert!(trace_out.contains("activity"), "trace shows the activity");
    assert!(
        trace_out.contains("generated"),
        "trace shows generated edge"
    );
    assert!(
        trace_out.contains(&format!("urn:atomic:change:{}", change_hash.to_base32())),
        "trace shows the generated change urn"
    );

    // Compute-on-demand: nothing was written.
    let after = snapshot_files(repo_dir);
    assert_eq!(before, after, "trace/show must not write any files");
    let graphs_after = {
        let repo = Repository::open(repo_dir).expect("open repo");
        repo.find_provenance_for_change_scan(&change_hash)
            .expect("scan")
            .len()
    };
    assert_eq!(
        graphs_before, graphs_after,
        "provenance-graph count must be unchanged after trace/show"
    );
}

#[test]
fn provenance_trace_uses_scan_fallback_when_rev_deps_missing() {
    let repo_tmp = TempDir::new().unwrap();
    let home_tmp = TempDir::new().unwrap();
    let repo_dir = repo_tmp.path();
    let home_dir = home_tmp.path();

    create_default_identity(home_dir);
    // Init a real repo (needed so `provenance` finds a repository root), but do
    // NOT record the change the graph explains — so it is never internal and
    // REV_DEPS is empty for it.
    assert!(atomic(repo_dir, home_dir, &["init"]).status.success());

    // A change hash that was never recorded => not internal => invisible to
    // REV_DEPS. The graph is still saved to the change store on disk.
    let phantom_change = Hash::of(b"never-recorded-change");
    {
        let repo = Repository::open(repo_dir).expect("open repo");
        let graph = ProvenanceGraph::builder("session-scan", "opencode")
            .agent_display_name("OpenCode")
            .add_node(goal_node())
            .add_change_explained(phantom_change)
            .build();
        repo.save_provenance_graph(&graph).expect("save graph");

        // Confirm the premise: REV_DEPS finds nothing, the scan finds it.
        assert!(
            repo.find_provenance_for_change(&phantom_change)
                .expect("rev_deps lookup")
                .is_empty(),
            "REV_DEPS must be empty for a non-internal change"
        );
        assert_eq!(
            repo.find_provenance_for_change_scan(&phantom_change)
                .expect("scan")
                .len(),
            1,
            "the disk scan must find the graph"
        );
    }

    // The CLI must find it via the fallback and print the chain.
    let trace = atomic(
        repo_dir,
        home_dir,
        &["provenance", "trace", &phantom_change.to_base32()],
    );
    assert!(
        trace.status.success(),
        "provenance trace (fallback) failed: {}",
        String::from_utf8_lossy(&trace.stderr)
    );
    let out = String::from_utf8(trace.stdout).unwrap();
    assert!(
        out.contains(&format!("urn:atomic:change:{}", phantom_change.to_base32())),
        "fallback trace shows the generated change urn"
    );
}
