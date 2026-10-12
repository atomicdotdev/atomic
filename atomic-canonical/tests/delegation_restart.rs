//! Store-backed grant lookup in fresh operating-system processes.
//!
//! These tests cover local selection, not server authorization. In particular,
//! an internally valid grant does not establish that its issuer is trusted.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use atomic_canonical::{delegation, jcs};
use atomic_identity::delegation::{Delegation, DelegationPermission, DelegationScope};
use atomic_identity::{Identity, IdentityStore, IdentityType, KeyPair};
use chrono::Utc;
use serde_json::{json, Value};
use tempfile::TempDir;

struct Fixture {
    directory: TempDir,
    store: IdentityStore,
    human: Identity,
    human_key: KeyPair,
    agent: Identity,
}

impl Fixture {
    fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let store = IdentityStore::open(directory.path()).unwrap();
        let human_key = KeyPair::generate();
        let human = Identity::new("issuer", &human_key);
        let agent_key = KeyPair::generate();
        let agent = Identity::builder("issuer+runner")
            .identity_type(IdentityType::Agent)
            .public_key(agent_key.public)
            .delegated_by(human.id)
            .build()
            .unwrap();
        // Lookup needs public identity data only. No secret key is sent to a
        // child or included in the retained process observations.
        store.save(&human).unwrap();
        store.save(&agent).unwrap();
        Self {
            directory,
            store,
            human,
            human_key,
            agent,
        }
    }

    fn certificate(&self, issued: chrono::DateTime<Utc>, expired: bool) -> Value {
        let scope = DelegationScope::builder()
            .permission(DelegationPermission::Record)
            .project("acme/project")
            .server("https://staging.example.test")
            .view("review/*")
            .build();
        let mut terms = Delegation::new(&self.human, &self.agent, scope);
        terms.issued = issued;
        // Recompute the identifier when fixing the issue time, just as the
        // constructor does for its clock-derived time.
        terms.id = atomic_identity::delegation::DelegationId::from_delegation_data(
            &self.human.id,
            &self.agent.id,
            issued,
        );
        terms.expires = Some(if expired {
            Utc::now() - chrono::Duration::days(1)
        } else {
            Utc::now() + chrono::Duration::days(1)
        });
        delegation::mint(&self.human, &self.human_key, &terms)
    }

    fn save(&self, document: &Value) -> String {
        let parsed = delegation::verify(document, &self.human.public_key).unwrap();
        let id = parsed.id.to_base32();
        self.store
            .save_delegation(&id, &delegation::encode_for_storage(document).unwrap())
            .unwrap();
        id
    }

    fn lookup(&self) -> Value {
        subprocess_lookup(self.directory.path())
    }
}

fn subprocess_lookup(store_path: &Path) -> Value {
    let capture = TempDir::new().unwrap();
    let stdout_path = capture.path().join("stdout");
    let stderr_path = capture.path().join("stderr");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "delegation_lookup_worker",
            "--nocapture",
        ])
        .env("ATOMIC_NATIVE_TEST_STORE", store_path)
        .stdin(Stdio::null())
        .stdout(fs::File::create(&stdout_path).unwrap())
        .stderr(fs::File::create(&stderr_path).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("delegation lookup worker timed out");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = fs::read_to_string(stdout_path).unwrap();
    let stderr = fs::read_to_string(stderr_path).unwrap();
    assert!(status.success(), "worker failed: {stdout}\n{stderr}");
    let record = stdout
        .lines()
        .find_map(|line| line.strip_prefix("ATOMIC_NATIVE_LOOKUP="))
        .expect("worker must emit one lookup result");
    let result: Value = serde_json::from_str(record).unwrap();
    assert_ne!(
        result["pid"].as_u64().unwrap(),
        u64::from(std::process::id())
    );
    // The retained harness keeps stdout, including these exact observations.
    println!("ATOMIC_NATIVE_LOOKUP={record}");
    result
}

#[test]
#[ignore = "subprocess entry point; invoked by the process-level tests"]
fn delegation_lookup_worker() {
    let root = std::env::var_os("ATOMIC_NATIVE_TEST_STORE").expect("worker store path");
    let store = IdentityStore::open(Path::new(&root)).unwrap();
    let agent = store.load_by_name("issuer+runner").unwrap();
    let loaded = delegation::load_for_delegate(&store, &agent).unwrap();
    let active = delegation::active_for_delegate(&store, &agent);
    let result = json!({
        "pid": std::process::id(),
        "stored": store.list_delegations().unwrap().len(),
        "verified_for_delegate": loaded.len(),
        "locally_revoked": loaded.iter().filter(|d| d.revoked_locally).count(),
        "active_id": active.as_ref().map(|d| &d.id),
        "scope": active.as_ref().map(|d| &d.delegation.scope),
        "allows_record": active.as_ref().is_some_and(|d| d.delegation.scope.has_permission(DelegationPermission::Record)),
        "allows_push": active.as_ref().is_some_and(|d| d.delegation.scope.has_permission(DelegationPermission::Push)),
        "allows_staging": active.as_ref().is_some_and(|d| d.delegation.scope.allows_server("https://staging.example.test")),
        "allows_production": active.as_ref().is_some_and(|d| d.delegation.scope.allows_server("https://production.example.test")),
        "allows_other_project": active.as_ref().is_some_and(|d| d.delegation.scope.allows_project("acme/other")),
        "allows_other_view": active.as_ref().is_some_and(|d| d.delegation.scope.allows_view("release/main")),
        "coverage": "local-store-selection-only"
    });
    println!("ATOMIC_NATIVE_LOOKUP={result}");
}

#[test]
fn exact_retry_and_process_restart_preserve_a_narrow_grant() {
    let fixture = Fixture::new();
    let doc = fixture.certificate(Utc::now() - chrono::Duration::hours(1), false);
    let id = fixture.save(&doc);
    let before = fixture.lookup();
    assert_eq!(
        fixture.save(&doc),
        id,
        "retry must keep the same identifier"
    );
    let after = fixture.lookup();
    assert_ne!(before["pid"], after["pid"]);
    for result in [&before, &after] {
        assert_eq!(result["stored"], 1);
        assert_eq!(result["verified_for_delegate"], 1);
        assert_eq!(result["active_id"], id);
        assert_eq!(result["scope"], doc["scope"]);
        assert_eq!(result["allows_record"], true);
        for refused in [
            "allows_push",
            "allows_production",
            "allows_other_project",
            "allows_other_view",
        ] {
            assert_eq!(result[refused], false, "scope widened at {refused}");
        }
        assert_eq!(result["allows_staging"], true);
    }
}

#[test]
fn revocation_survives_restart_and_does_not_revoke_a_distinct_renewal() {
    let fixture = Fixture::new();
    let old = fixture.certificate(Utc::now() - chrono::Duration::hours(2), false);
    let old_id = fixture.save(&old);
    assert_eq!(fixture.lookup()["active_id"], old_id);
    let old_terms = delegation::parse(&old).unwrap();
    let revocation = delegation::mint_revocation(
        &fixture.human,
        &fixture.human_key,
        &old_terms.id,
        Some("rotate grant"),
    );
    delegation::verify_revocation(&revocation, &fixture.human.public_key).unwrap();
    fixture
        .store
        .save_revocation(&old_id, &serde_json::to_string(&revocation).unwrap())
        .unwrap();
    let revoked = fixture.lookup();
    assert_eq!(revoked["active_id"], Value::Null);
    assert_eq!(revoked["locally_revoked"], 1);

    let renewed = fixture.certificate(Utc::now() - chrono::Duration::hours(1), false);
    let new_id = fixture.save(&renewed);
    assert_ne!(old_id, new_id);
    for _ in 0..2 {
        let result = fixture.lookup();
        assert_eq!(result["stored"], 2);
        assert_eq!(result["active_id"], new_id);
        assert_eq!(result["locally_revoked"], 1);
    }
}

#[test]
fn expired_and_corrupt_grants_stay_inactive_after_restart() {
    let fixture = Fixture::new();
    let expired = fixture.certificate(Utc::now() - chrono::Duration::days(2), true);
    fixture.save(&expired);
    let initial = fixture.lookup();
    assert_eq!(initial["verified_for_delegate"], 1);
    assert_eq!(initial["active_id"], Value::Null);

    let valid = fixture.certificate(Utc::now() - chrono::Duration::hours(1), false);
    let mut changed = valid.clone();
    changed["scope"]["projects"] = json!(["*"]);
    fixture
        .store
        .save_delegation("changed-scope", &serde_json::to_string(&changed).unwrap())
        .unwrap();
    // A repeated signed member must be rejected before a JSON parser picks a
    // reading, including when read back from persistent storage.
    let raw = serde_json::to_string(&valid).unwrap();
    let duplicate = format!("{{\"scope\":{{}},{}", &raw[1..]);
    assert!(jcs::admit_document(duplicate.as_bytes()).is_err());
    fixture
        .store
        .save_delegation("ambiguous-json", &duplicate)
        .unwrap();
    for _ in 0..2 {
        let result = fixture.lookup();
        assert_eq!(result["stored"], 3);
        assert_eq!(result["verified_for_delegate"], 1);
        assert_eq!(result["active_id"], Value::Null);
    }
    // Restore a valid control: rejecting corrupt inputs must not make the
    // entire store unusable for this agent.
    let valid_id = fixture.save(&valid);
    let result = fixture.lookup();
    assert_eq!(result["stored"], 4);
    assert_eq!(result["verified_for_delegate"], 2);
    assert_eq!(result["active_id"], valid_id);
}

#[test]
fn local_selection_does_not_establish_external_issuer_authority() {
    let fixture = Fixture::new();
    let doc = fixture.certificate(Utc::now() - chrono::Duration::hours(1), false);
    let id = fixture.save(&doc);
    let unrelated_key = KeyPair::generate();
    assert!(delegation::verify(&doc, &unrelated_key.public).is_err());
    assert!(delegation::verify(&doc, &fixture.human.public_key).is_ok());
    // Deliberate positive boundary control: local self-contained selection
    // accepts the valid certificate. Only a separate externally keyed check
    // can establish that the issuer is the one a relying party trusts.
    let result = fixture.lookup();
    assert_eq!(result["active_id"], id);
    assert_eq!(result["coverage"], "local-store-selection-only");

    let other_key = KeyPair::generate();
    let other_agent = Identity::builder("other-runner")
        .identity_type(IdentityType::Agent)
        .public_key(other_key.public)
        .build()
        .unwrap();
    let other_terms = Delegation::new(&fixture.human, &other_agent, DelegationScope::full());
    let other_doc = delegation::mint(&fixture.human, &fixture.human_key, &other_terms);
    fixture.save(&other_doc);
    let result = fixture.lookup();
    assert_eq!(result["stored"], 2);
    assert_eq!(result["verified_for_delegate"], 1);
    assert_eq!(
        result["active_id"], id,
        "another subject's grant must not replace this one"
    );
}
