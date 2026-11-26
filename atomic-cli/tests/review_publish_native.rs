//! Provider-free native publication, explicit degradation, and insertion linkage.
use atomic_core::{
    change::ChangeHeader,
    pristine::OperationTxnT,
    types::{Base32, Hash},
};
use atomic_identity::{Identity, IdentityStore, KeyPair};
use atomic_repository::{record::RecordOptions, tracking::TrackingOptions, Repository};
use serde_json::Value;
use std::{
    path::PathBuf,
    process::{Command, Output},
};

struct Fixture {
    root: tempfile::TempDir,
    global: tempfile::TempDir,
    base: String,
}
impl Fixture {
    fn new(callers: usize) -> Self {
        let root = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(root.path()).unwrap();
        repo.init_vault().unwrap();
        let base = repo.current_view().to_owned();
        let wc = repo.require_working_copy_id().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("src/lib.rs"),
            "mod consumers;\npub fn work(a:i32) {}\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("src/consumers.rs"),
            (0..callers)
                .map(|index| format!("pub fn consumer_{index}() {{ crate::work(1); }}\n"))
                .collect::<String>(),
        )
        .unwrap();
        repo.add(wc, "src", TrackingOptions::default()).unwrap();
        repo.add(wc, ".vault", TrackingOptions::default()).unwrap();
        record(&repo);
        repo.create_draft_view("draft", &base).unwrap();
        repo.switch_view(wc, "draft").unwrap();
        std::fs::write(
            root.path().join("src/lib.rs"),
            "mod consumers;\npub fn work(a:i32,b:i32) { todo!(); }\n",
        )
        .unwrap();
        record(&repo);
        drop(repo);
        Self { root, global, base }
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_atomic"))
            .current_dir(self.root.path())
            .env("ATOMIC_CONFIG_DIR", self.global.path())
            .env("PATH", self.global.path().join("no-executables"))
            .env("ATOMIC_SESSION_ID", "../must-not-be-used")
            .env_remove("OPENAI_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("GITHUB_TOKEN")
            .env("OPENAI_BASE_URL", "http://127.0.0.1:1")
            .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:1")
            .args(args)
            .output()
            .unwrap()
    }
    fn json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn plan(&self) -> Value {
        self.json(&[
            "review",
            "publish",
            "--from",
            "draft",
            "--to",
            &self.base,
            "--dry-run",
            "--json",
        ])
    }
    fn signer(&self) -> Identity {
        let pair = KeyPair::generate();
        let identity = Identity::new("reviewer", &pair);
        let mut store = IdentityStore::open(&self.global.path().join("identities")).unwrap();
        store.save_with_keypair(&identity, &pair, None).unwrap();
        store.set_default(&identity.id).unwrap();
        let repo = Repository::open(self.root.path()).unwrap();
        let path = repo.dot_dir().join("config.toml");
        let mut config = atomic_config::RepoConfig::load(&path).unwrap();
        config.git.trust.signers = vec![atomic_canonical::did::did_for_public_key(
            &identity.public_key,
        )];
        config.save(&path).unwrap();
        identity
    }
    fn evaluate(&self, mut plan: Value, not_ready: bool) -> PathBuf {
        let evaluation = &mut plan["evaluation_template"];
        for pass in evaluation["passes"].as_array_mut().unwrap() {
            pass["evaluated"] = Value::Bool(true);
            pass["rationale"] = Value::String(
                "Responsible test signer evaluated the complete recorded pass and its limits"
                    .into(),
            );
            for issue in pass["accepted_degradations"].as_array_mut().unwrap() {
                issue["rationale"]=Value::String("Known degradation explicitly accepted for this fixture's bounded rollout; compatibility follow-up is required".into());
            }
        }
        if not_ready {
            evaluation["passes"][0]["disposition"] = Value::String("not_ready".into());
            evaluation["passes"][0]["accepted_degradations"] = Value::Array(vec![]);
        }
        let path = self.global.path().join("evaluation.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
        path
    }
}
fn record(repo: &Repository) -> Hash {
    *repo
        .record(
            repo.require_working_copy_id().unwrap(),
            ChangeHeader::new("native publication fixture"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true)
                .enrich_kg(false),
        )
        .unwrap()
        .hash()
}
fn document(html: &str) -> Value {
    serde_json::from_str(
        html.split("id=\"review-data\"")
            .nth(1)
            .unwrap()
            .split_once('>')
            .unwrap()
            .1
            .split("</script>")
            .next()
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn native_publish_retains_all_findings_rationales_and_unready_evidence_without_any_agent() {
    let f = Fixture::new(137);
    let repo = Repository::open_readonly(f.root.path()).unwrap();
    let database = repo.dot_dir().join("atomic.redb");
    drop(repo);
    let before = std::fs::read(&database).unwrap();
    let plan = f.plan();
    assert_eq!(std::fs::read(&database).unwrap(), before);
    assert!(
        !f.global.path().join("identities").exists(),
        "Dry-run must not create credentials"
    );
    assert_eq!(plan["scope"]["source"]["view"], "draft");
    assert_eq!(plan["scope"]["target"]["view"], f.base);
    assert_eq!(plan["readiness"], "not_ready");
    assert_eq!(plan["report"]["schema_version"], 5);
    assert_eq!(
        plan["report"]["graph"]["risk"]["impact"][0]["issues"]
            .as_array()
            .unwrap()
            .len(),
        137
    );
    let local = f.json(&["review", "--from", "draft", "--to", &f.base, "--json"]);
    assert_eq!(
        local["graph"]["findings"],
        plan["report"]["graph"]["findings"]
    );
    let evaluation = f.evaluate(plan, false);
    let identity = f.signer();
    // A physical working-copy switch must not change the normalized evidence.
    let mut repo = Repository::open(f.root.path()).unwrap();
    repo.switch_view(repo.require_working_copy_id().unwrap(), &f.base)
        .unwrap();
    drop(repo);
    let args = [
        "review",
        "publish",
        "--from",
        "draft",
        "--to",
        f.base.as_str(),
        "--evaluation",
        evaluation.to_str().unwrap(),
        "--identity",
        "reviewer",
        "--yes",
        "--json",
    ];
    // Another process owns the same evidence/operation boundary: neither
    // publication nor insertion may race past it or retain a partial decision.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(f.root.path().join(".atomic/bridge.lock"))
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&lock).unwrap();
    let contended = f.run(&args);
    assert!(!contended.status.success());
    fs2::FileExt::unlock(&lock).unwrap();
    assert!(Repository::open_readonly(f.root.path())
        .unwrap()
        .native_review_decisions()
        .unwrap()
        .is_empty());
    let published = f.json(&args);
    assert!(published.get("path").is_none());
    assert!(published["vault_path"]
        .as_str()
        .unwrap()
        .starts_with("review-decisions/"));
    assert!(
        !f.root.path().join(".vault/review-decisions").exists(),
        "Retaining an attestation must not dirty the working copy"
    );
    assert_eq!(published["status"], "attested");
    assert_eq!(published["review"]["readiness"], "not_ready");
    assert_eq!(published["review"]["disposition"], "accepted_degradation");
    assert_eq!(
        published["document"]["report"]["graph"]["findings"],
        local["graph"]["findings"]
    );
    assert_eq!(published["agent_execution"], false);
    assert_eq!(published["document"]["independent_approval"], false);
    atomic_canonical::proof::verify_value(&published["document"], &identity.public_key).unwrap();
    let repeated = f.json(&args);
    assert_eq!(
        repeated["review"]["decision"], published["review"]["decision"],
        "Exact repeat must reuse the immutable attestation"
    );
    let output = f.run(&[
        "review",
        "publish",
        "--from",
        "draft",
        "--to",
        &f.base,
        "--evaluation",
        evaluation.to_str().unwrap(),
        "--identity",
        "reviewer",
        "--yes",
        "--html",
        "--no-open",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let html = String::from_utf8(output.stdout).unwrap();
    if let Some(path) = std::env::var_os("ATOMIC_REVIEW_NATIVE_PUBLICATION_ARTIFACT") {
        std::fs::write(path, &html).unwrap();
    }
    let data = document(&html);
    assert_eq!(
        data["review_decision"]["disposition"],
        "accepted_degradation"
    );
    assert_eq!(data["review_decision"]["readiness"], "not_ready");
    assert_eq!(
        data["view"]["graph"]["findings"],
        local["graph"]["findings"]
    );
    let compressed = data_encoding::BASE64
        .decode(data["report_archive"]["data"].as_str().unwrap().as_bytes())
        .unwrap();
    let archived: Value =
        serde_json::from_reader(flate2::read::GzDecoder::new(compressed.as_slice())).unwrap();
    assert_eq!(archived, published["document"]);
    let inspect = f.json(&["review", "publish", "inspect"]);
    assert_eq!(inspect.as_array().unwrap().len(), 1);
    assert_eq!(inspect[0]["validation"]["currently_applicable"], true);
    fs2::FileExt::try_lock_exclusive(&lock).unwrap();
    let contended_insert = f.run(&["insert", "view", "draft", "--to", &f.base]);
    fs2::FileExt::unlock(&lock).unwrap();
    assert!(!contended_insert.status.success());
    assert_eq!(
        f.json(&["review", "publish", "inspect"])[0]["validation"]["currently_applicable"],
        true,
        "A contended insertion must not mutate the scope or consume the decision"
    );
    let status = f.run(&["status", "--short"]);
    let inserted = f.run(&["insert", "view", "draft", "--to", &f.base]);
    assert!(
        inserted.status.success(),
        "{}\nPre-insert status:\n{}",
        String::from_utf8_lossy(&inserted.stderr),
        String::from_utf8_lossy(&status.stdout)
    );
    assert!(
        String::from_utf8_lossy(&inserted.stderr)
            .contains(published["review"]["decision"].as_str().unwrap()),
        "Insertion must surface the attested risk decision"
    );
    let repo = Repository::open_readonly(f.root.path()).unwrap();
    let hash =
        Hash::from_base32(published["review"]["decision"].as_str().unwrap().as_bytes()).unwrap();
    assert!(repo
        .pristine()
        .read_txn()
        .unwrap()
        .list_operations()
        .unwrap()
        .iter()
        .any(|op| op.payload().evidence.contains(&hash)));
    assert!(!repo.dot_dir().join("review-jobs").exists());
    let store = IdentityStore::open_existing(&f.global.path().join("identities")).unwrap();
    assert_eq!(store.list().unwrap().len(), 1);
    assert!(!f.global.path().join("identities/delegations").exists());
}

#[test]
fn yes_missing_or_partial_evaluation_cannot_publish_or_hide_findings() {
    let f = Fixture::new(2);
    let plan = f.plan();
    let output = f.run(&[
        "review", "publish", "--from", "draft", "--to", &f.base, "--yes", "--json",
    ]);
    assert!(!output.status.success());
    assert!(!f.global.path().join("identities").exists());
    let path = f.global.path().join("unfilled.json");
    std::fs::write(&path, serde_json::to_vec(&plan).unwrap()).unwrap();
    let output = f.run(&[
        "review",
        "publish",
        "--from",
        "draft",
        "--to",
        &f.base,
        "--evaluation",
        path.to_str().unwrap(),
        "--yes",
    ]);
    assert!(!output.status.success());
    let mut plan = plan;
    plan["evaluation_template"]["report_hash"] = Value::String("foreign-or-stale".into());
    let path = f.evaluate(plan, false);
    let output = f.run(&[
        "review",
        "publish",
        "--from",
        "draft",
        "--to",
        &f.base,
        "--evaluation",
        path.to_str().unwrap(),
        "--yes",
    ]);
    assert!(!output.status.success());
    assert!(Repository::open_readonly(f.root.path())
        .unwrap()
        .native_review_decisions()
        .unwrap()
        .is_empty());
    for flag in ["--runner", "--timeout-seconds", "--resume", "--session"] {
        let output = f.run(&["review", "publish", flag, "forbidden"]);
        assert!(!output.status.success());
    }
}

#[test]
fn attested_not_ready_remains_visible_and_does_not_allow_insertion() {
    let f = Fixture::new(2);
    let plan = f.plan();
    let path = f.evaluate(plan, true);
    f.signer();
    let published = f.json(&[
        "review",
        "publish",
        "--from",
        "draft",
        "--to",
        &f.base,
        "--evaluation",
        path.to_str().unwrap(),
        "--identity",
        "reviewer",
        "--yes",
        "--json",
    ]);
    assert_eq!(published["review"]["disposition"], "not_ready");
    let output = f.run(&["insert", "view", "draft", "--to", &f.base]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not_ready"));
    let report = f.json(&["review", "--from", "draft", "--to", &f.base, "--json"]);
    assert!(!report["graph"]["findings"].as_array().unwrap().is_empty());
}
