//! All three passes, with the complete JSON finding set in offline HTML.
use atomic_core::{
    change::{session::SessionTurn, ChangeHeader, ProvenanceGraph},
    types::{Base32, Hash},
};
use atomic_repository::{
    record::RecordOptions, tracking::TrackingOptions, IntentCreateOptions, IntentUpdateOptions,
    Repository,
};
use serde_json::Value;
use std::{path::Path, process::Command};

fn record(repo: &Repository) -> Hash {
    *repo
        .record(
            repo.require_working_copy_id().unwrap(),
            ChangeHeader::new("comparison passes fixture"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true)
                .enrich_kg(false),
        )
        .unwrap()
        .hash()
}
fn add(repo: &Repository, path: &str, text: &str) {
    let full = repo.root().join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, text).unwrap();
    repo.add(
        repo.require_working_copy_id().unwrap(),
        path,
        TrackingOptions::default(),
    )
    .unwrap();
}
fn governance(repo: &Repository, hash: Hash, plan: &str) {
    let session = format!("comparison-{plan}");
    let graph = ProvenanceGraph::builder(&session, "test")
        .timestamp(10)
        .plan_id(plan)
        .add_change_explained(hash)
        .build();
    let provenance_hash = Hash::of(&graph.serialize().unwrap());
    repo.publish_provenance_checkpoint(
        &graph,
        SessionTurn {
            session_id: session,
            turn_number: 0,
            goal: None,
            provenance_hash,
            change_hashes: vec![hash],
            previous_provenance: None,
            timestamp: 10,
            plan_id: Some(plan.into()),
            todos: vec![],
            boundary_start: None,
            boundary_end: None,
            outcome: None,
        },
    )
    .unwrap();
}

#[test]
fn missing_recorded_criteria_use_explicit_context_and_ambiguous_governance_is_not_guessed() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    repo.init_vault().unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    let from = repo.current_view().to_owned();
    let create = || {
        repo.vault_intent_create(IntentCreateOptions {
            title: "Current-vault-only criterion".into(),
            priority: None,
            assignee: None,
            labels: vec![],
            session_id: None,
            turn_id: None,
            kind: None,
        })
        .unwrap()
    };
    let intent = create();
    let other = create();
    repo.vault_intent_update(&intent.id,IntentUpdateOptions {content:Some(format!(":::why\nContext only.\n:::\n:::acceptance-criterion{{#{}-ac-1 status=unmet}}\nCURRENT_CONTEXT_NOT_RECORDED_PROOF\n:::\n",intent.uid)),..Default::default()}).unwrap();
    add(&repo, "lib.rs", "pub fn work() {}\n");
    record(&repo);
    repo.create_draft_view("after", &from).unwrap();
    repo.switch_view(wc, "after").unwrap();
    std::fs::write(dir.path().join("lib.rs"), "pub fn work() { let n=1; }\n").unwrap();
    let hash = record(&repo);
    governance(&repo, hash, &intent.uid);
    drop(repo);
    let report = review(dir.path(), &from, "after");
    let criterion = &report["view_comparison"]["conformance"]["criteria"][0];
    assert!(criterion["before"].is_null() && criterion["after"].is_null());
    assert_eq!(
        criterion["current_context"]["source_kind"],
        "current_vault_context"
    );
    assert_eq!(
        report["graph"]["criteria"][0]["review_assessment"],
        "unknown"
    );
    let repo = Repository::open(dir.path()).unwrap();
    governance(&repo, hash, &other.uid);
    drop(repo);
    let report = review(dir.path(), &from, "after");
    assert!(report["graph"]["criteria"].as_array().unwrap().is_empty());
    assert_eq!(
        report["view_comparison"]["conformance"]["changes"][0]["intent_context"]["status"],
        "ambiguous"
    );
    assert!(report["view_comparison"]["conformance"]["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|note| note.as_str().unwrap().contains("ambiguous")));
}

#[test]
fn blast_radius_compares_final_states_not_an_intermediate_breaking_patch() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    repo.init_vault().unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    let from = repo.current_view().to_owned();
    let source = "fn work(a:i32) {}\nfn consumer() { work(1); }\n";
    add(&repo, "lib.rs", source);
    record(&repo);
    repo.create_draft_view("after", &from).unwrap();
    repo.switch_view(wc, "after").unwrap();
    std::fs::write(
        dir.path().join("lib.rs"),
        "fn work(a:i32,b:i32) {}\nfn consumer() { work(1); }\n",
    )
    .unwrap();
    record(&repo);
    std::fs::write(dir.path().join("lib.rs"), source).unwrap();
    record(&repo);
    drop(repo);
    let report = review(dir.path(), &from, "after");
    assert!(report["graph"]["risk"]["impact"][0]["issues"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        report["graph"]["risk"]["assessment"]["rating"],
        "not_applicable"
    );
    assert_eq!(
        report["view_comparison"]["conformance"]["changes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
fn command(root: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_atomic"))
        .current_dir(root)
        .env("PATH", root.join("no-executables"))
        .env("ATOMIC_SESSION_ID", "../must-not-be-used")
        .env("OPENAI_BASE_URL", "http://127.0.0.1:1")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:1")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn review(root: &Path, before: &str, after: &str) -> Value {
    serde_json::from_str(&command(
        root,
        &["review", "--from", after, "--to", before, "--json"],
    ))
    .unwrap()
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

fn scoped_intent(repo: &Repository, title: &str) -> atomic_repository::IntentCreateResult {
    repo.vault_intent_create(IntentCreateOptions {
        title: title.into(),
        priority: None,
        assignee: None,
        labels: vec![],
        session_id: None,
        turn_id: None,
        kind: None,
    })
    .unwrap()
}

#[test]
fn unlinked_view_intents_are_scoped_to_the_requested_view_not_the_active_view() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    repo.init_vault().unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    let base = repo.current_view().to_owned();
    let parent = scoped_intent(&repo, "PARENT_SCOPE_MUST_NOT_APPEAR");
    repo.vault_intent_update(&parent.id,IntentUpdateOptions {content:Some(format!(":::why\nParent work.\n:::\n:::acceptance-criterion{{#{}-ac-1 status=unmet}}\nPARENT_CRITERION_MUST_NOT_APPEAR\n:::\n",parent.uid)),..Default::default()}).unwrap();
    let parent_path = format!(".vault/{}", parent.intent_file);
    let parent_source = std::fs::read_to_string(dir.path().join(&parent_path)).unwrap();
    add(&repo, &parent_path, &parent_source);
    add(&repo, "lib.rs", "pub fn before() {}\n");
    record(&repo);
    repo.create_draft_view("target", &base).unwrap();
    repo.create_draft_view("foreign", &base).unwrap();
    repo.switch_view(wc, "target").unwrap();
    std::fs::write(dir.path().join("lib.rs"), "pub fn after() {}\n").unwrap();
    record(&repo);
    // Neither of these view assignments has a recorded governing-plan link,
    // and neither intent document has been added to the VCS graph.
    let work = scoped_intent(&repo, "View-scoped API intent");
    repo.vault_intent_update(&work.id,IntentUpdateOptions {content:Some(format!(":::why\nTarget work.\n:::\n:::acceptance-criterion{{#{}-ac-1 status=unmet}}\nFIRST_VIEW_SCOPED_CRITERION\n:::\n:::acceptance-criterion{{#{}-ac-2 status=unmet}}\nSECOND_VIEW_SCOPED_CRITERION\n:::\n:::task{{#{}-1 status=open criteria={}-ac-1}}\nImplement target work.\n::file-ref{{path=lib.rs}}\n:::\n",work.uid,work.uid,work.uid,work.uid)),..Default::default()}).unwrap();
    let empty = scoped_intent(&repo, "View-scoped checklist without criteria");
    repo.vault_intent_update(
        &empty.id,
        IntentUpdateOptions {
            content: Some(":::why\nNo criteria have been defined yet.\n:::\n".into()),
            ..Default::default()
        },
    )
    .unwrap();
    repo.switch_view(wc, "foreign").unwrap();
    let foreign = scoped_intent(&repo, "FOREIGN_SCOPE_MUST_NOT_APPEAR");
    repo.vault_intent_update(&foreign.id,IntentUpdateOptions {content:Some(format!(":::why\nForeign work.\n:::\n:::acceptance-criterion{{#{}-ac-1 status=unmet}}\nFOREIGN_CRITERION_MUST_NOT_APPEAR\n:::\n",foreign.uid)),..Default::default()}).unwrap();
    std::fs::write(
        dir.path().join("lib.rs"),
        "DIRTY_ACTIVE_VIEW_NOT_TARGET_SOURCE",
    )
    .unwrap();
    let db = repo.dot_dir().join("atomic.redb");
    let active = repo.dot_dir().join("current_view");
    let wc_before = repo.working_copy_record(wc).unwrap();
    drop(repo);
    let db_before = std::fs::read(&db).unwrap();
    let active_before = std::fs::read(&active).unwrap();
    let report = review(dir.path(), &base, "target");
    let context = &report["view_comparison"]["conformance"];
    assert_eq!(context["scope"]["view"], "target");
    assert_eq!(context["intents"].as_array().unwrap().len(), 2);
    assert_eq!(context["criteria"].as_array().unwrap().len(), 2);
    assert_eq!(report["graph"]["passes"][1]["status"], "partial");
    assert!(context["criteria"]
        .as_array()
        .unwrap()
        .iter()
        .all(|criterion| criterion["governing_changes"]
            .as_array()
            .unwrap()
            .is_empty()
            && criterion["current_context"]["source_kind"] == "current_vault_context"));
    assert!(report["graph"]["criteria"]
        .as_array()
        .unwrap()
        .iter()
        .all(|criterion| criterion["review_assessment"] == "unknown"));
    let snapshot: Value = serde_json::from_str(&command(
        dir.path(),
        &["review", "--view", "target", "--json"],
    ))
    .unwrap();
    assert_eq!(
        snapshot["view_snapshot"]["conformance"]["scope"]["view"],
        "target"
    );
    assert_eq!(snapshot["graph"]["criteria"].as_array().unwrap().len(), 2);
    assert_eq!(snapshot["graph"]["risk"]["basis"], "view_snapshot");
    let reverse = review(dir.path(), "target", &base);
    assert_eq!(
        reverse["view_comparison"]["conformance"]["intents"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        reverse["view_comparison"]["conformance"]["intents"][0]["human_key"],
        parent.id
    );
    let same = review(dir.path(), "target", "target");
    assert_eq!(same["graph"]["passes"][1]["status"], "partial");
    for (args, artifact) in [
        (
            vec![
                "review",
                "--from",
                "target",
                "--to",
                base.as_str(),
                "--html",
                "--no-open",
            ],
            "ATOMIC_REVIEW_SCOPED_INTENTS_ARTIFACT",
        ),
        (
            vec!["review", "--view", "target", "--html", "--no-open"],
            "ATOMIC_REVIEW_SCOPED_VIEW_ARTIFACT",
        ),
    ] {
        let html = command(dir.path(), &args);
        if let Some(path) = std::env::var_os(artifact) {
            std::fs::write(path, &html).unwrap();
        }
        for forbidden in [
            "PARENT_SCOPE_MUST_NOT_APPEAR",
            "FOREIGN_SCOPE_MUST_NOT_APPEAR",
            "PARENT_CRITERION_MUST_NOT_APPEAR",
            "FOREIGN_CRITERION_MUST_NOT_APPEAR",
            "DIRTY_ACTIVE_VIEW_NOT_TARGET_SOURCE",
        ] {
            assert!(!html.contains(forbidden), "Leaked {forbidden}");
        }
        let doc = document(&html);
        assert_eq!(
            doc["view"]["graph"]["criteria"].as_array().unwrap().len(),
            2
        );
    }
    assert_eq!(std::fs::read(db).unwrap(), db_before);
    assert_eq!(std::fs::read(active).unwrap(), active_before);
    let repo = Repository::open_readonly(dir.path()).unwrap();
    assert_eq!(repo.current_view(), "foreign");
    assert_eq!(repo.working_copy_record(wc).unwrap(), wc_before);
}

#[test]
fn recorded_view_assignment_survives_dirty_or_missing_current_vault_definitions() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    repo.init_vault().unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    let base = repo.current_view().to_owned();
    add(&repo, "lib.rs", "pub fn code() {}\n");
    record(&repo);
    repo.create_draft_view("target", &base).unwrap();
    repo.switch_view(wc, "target").unwrap();
    let work = scoped_intent(&repo, "Recorded view-assigned work");
    repo.vault_intent_update(&work.id,IntentUpdateOptions {content:Some(format!(":::why\nRecorded target work.\n:::\n:::acceptance-criterion{{#{}-ac-1 status=unmet}}\nPINNED_VIEW_ASSIGNMENT_CRITERION\n:::\n",work.uid)),..Default::default()}).unwrap();
    let path = format!(".vault/{}", work.intent_file);
    let source = std::fs::read_to_string(dir.path().join(&path)).unwrap();
    add(&repo, &path, &source);
    record(&repo);
    repo.vault_intent_update(
        &work.id,
        IntentUpdateOptions {
            title: Some("DIRTY_SCOPE_TITLE_NOT_RENDERED".into()),
            content: Some(":::why\nDIRTY_SCOPE_BODY_NOT_RENDERED\n:::\n".into()),
            ..Default::default()
        },
    )
    .unwrap();
    drop(repo);
    for before_delete in [true, false] {
        if !before_delete {
            let repo = Repository::open(dir.path()).unwrap();
            repo.vault_intent_delete(&work.id).unwrap();
        }
        let report = review(dir.path(), &base, "target");
        let context = &report["view_comparison"]["conformance"];
        assert_eq!(context["intents"].as_array().unwrap().len(), 1);
        assert_eq!(
            context["intents"][0]["title"],
            "Recorded view-assigned work"
        );
        assert_eq!(context["intents"][0]["source_kind"], "recorded_view");
        assert_eq!(
            context["criteria"][0]["after"]["text"],
            "PINNED_VIEW_ASSIGNMENT_CRITERION"
        );
        assert_eq!(
            context["criteria"][0]["governing_changes"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        let html = command(
            dir.path(),
            &[
                "review",
                "--from",
                "target",
                "--to",
                &base,
                "--html",
                "--no-open",
            ],
        );
        assert!(
            !html.contains("DIRTY_SCOPE_TITLE_NOT_RENDERED")
                && !html.contains("DIRTY_SCOPE_BODY_NOT_RENDERED")
        );
    }
}

#[test]
fn pinned_conformance_and_semantic_blast_radius_export_every_contract_finding() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    repo.init_vault().unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    let from = repo.current_view().to_owned();
    let intent = repo
        .vault_intent_create(IntentCreateOptions {
            title: "Preserve the direct consumer contract".into(),
            priority: None,
            assignee: None,
            labels: vec![],
            session_id: None,
            turn_id: None,
            kind: None,
        })
        .unwrap();
    let body = |status: &str| {
        format!(":::why\nPreserve compatibility.\n:::\n:::acceptance-criterion{{#{}-ac-1 status={status} verifiedBy=author evidence=claimed}}\nRECORDED_CONTRACT_CRITERION: consumers can still call work with one argument.\n:::\n:::task{{#{}-1 status=done criteria={}-ac-1}}\nImplement the API update.\n::file-ref{{path=src/lib.rs}}\n:::\n",intent.uid,intent.uid,intent.uid)
    };
    repo.vault_intent_update(
        &intent.id,
        IntentUpdateOptions {
            content: Some(body("unmet")),
            ..Default::default()
        },
    )
    .unwrap();
    let intent_path = format!(".vault/{}", intent.intent_file);
    let intent_bytes = std::fs::read_to_string(dir.path().join(&intent_path)).unwrap();
    add(&repo, &intent_path, &intent_bytes);
    add(
        &repo,
        "src/lib.rs",
        "mod consumer;\npub fn work(x:i32) {}\n",
    );
    let consumers = (0..137)
        .map(|index| format!("pub fn consumer_{index}() {{ crate::work(1); }}\n"))
        .collect::<String>();
    add(&repo, "src/consumer.rs", &consumers);
    record(&repo);
    repo.create_draft_view("after", &from).unwrap();
    repo.switch_view(wc, "after").unwrap();
    repo.vault_intent_update(
        &intent.id,
        IntentUpdateOptions {
            content: Some(body("met")),
            ..Default::default()
        },
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "mod consumer;\npub fn work(x:i32,y:i32) {}\n",
    )
    .unwrap();
    let changed = record(&repo);
    governance(&repo, changed, &intent.uid);
    repo.switch_view(wc, &from).unwrap();
    // Neither dirty code nor later current-vault definitions may replace a
    // pinned recorded definition. No review command/test evidence is executed.
    repo.vault_intent_update(
        &intent.id,
        IntentUpdateOptions {
            content: Some(":::why\nDIRTY_CRITERION_MUST_NOT_REPLACE_RECORDED_SOURCE\n:::\n".into()),
            ..Default::default()
        },
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "DIRTY_SOURCE_MUST_NOT_BE_ANALYZED",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("untracked.rs"),
        "UNTRACKED_CALLER_MUST_NOT_BE_ANALYZED",
    )
    .unwrap();
    let database = repo.dot_dir().join("atomic.redb");
    let active = repo.dot_dir().join("current_view");
    let expected_wc = repo.working_copy_record(wc).unwrap();
    drop(repo);
    let db_before = std::fs::read(&database).unwrap();
    let active_before = std::fs::read(&active).unwrap();

    let report = review(dir.path(), &from, "after");
    assert_eq!(report["graph"]["passes"][1]["status"], "partial");
    assert_eq!(report["graph"]["passes"][2]["status"], "partial");
    let criterion = &report["view_comparison"]["conformance"]["criteria"][0];
    assert_eq!(criterion["before"]["status"], "unmet");
    assert_eq!(criterion["after"]["status"], "met");
    assert_eq!(criterion["after"]["source_kind"], "recorded_view");
    assert!(criterion["current_context"].is_null());
    assert_eq!(criterion["governing_changes"][0], changed.to_base32());
    assert_eq!(
        criterion["after"]["tasks"][0]["files"][0]["path"],
        "src/lib.rs"
    );
    assert_eq!(
        report["graph"]["criteria"][0]["review_assessment"], "unknown",
        "Author met is not independent acceptance proof"
    );
    let risk = &report["graph"]["risk"];
    assert_eq!(risk["assessment"]["rating"], "high");
    let impact = &risk["impact"][0];
    assert_eq!(impact["callers_after"].as_array().unwrap().len(), 137);
    assert_eq!(impact["issues"].as_array().unwrap().len(), 137);
    assert_eq!(impact["checked_call_contracts"], 137);
    let findings: Vec<_> = report["graph"]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|finding| {
            finding["passes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|pass| pass == "blast_radius_risk")
        })
        .collect();
    assert_eq!(findings.len(), 137, "No numbered-finding truncation at 100");
    assert!(findings
        .iter()
        .all(|finding| finding["finding"]["introduced_by_change"]["status"] == "unknown"));
    let reverse = review(dir.path(), "after", &from);
    assert!(reverse["graph"]["risk"]["impact"][0]["issues"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        reverse["view_comparison"]["conformance"]["changes"][0]["direction"],
        "removed_from_after"
    );
    assert_eq!(
        reverse["view_comparison"]["conformance"]["criteria"][0]["before"]["status"],
        "met"
    );
    let same = review(dir.path(), &from, &from);
    assert_eq!(
        same["graph"]["risk"]["assessment"]["rating"],
        "not_applicable"
    );

    let html = command(
        dir.path(),
        &[
            "review",
            "--from",
            "after",
            "--to",
            &from,
            "--html",
            "--no-open",
        ],
    );
    if let Some(path) = std::env::var_os("ATOMIC_REVIEW_PASSES_ARTIFACT") {
        std::fs::write(path, &html).unwrap();
    }
    for forbidden in [
        "DIRTY_SOURCE_MUST_NOT_BE_ANALYZED",
        "DIRTY_CRITERION_MUST_NOT_REPLACE_RECORDED_SOURCE",
        "UNTRACKED_CALLER_MUST_NOT_BE_ANALYZED",
    ] {
        assert!(!html.contains(forbidden));
    }
    let doc = document(&html);
    assert_eq!(
        doc["view"]["graph"]["findings"],
        report["graph"]["findings"]
    );
    assert_eq!(
        doc["view"]["view_comparison"]["conformance"],
        report["view_comparison"]["conformance"]
    );
    let callers = doc["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| {
            source["path"] == "src/consumer.rs"
                && source["change"] == report["view_comparison"]["source_ref"]
        })
        .unwrap();
    assert_eq!(callers["after"]["text"], consumers);
    let compressed = data_encoding::BASE64
        .decode(doc["report_archive"]["data"].as_str().unwrap().as_bytes())
        .unwrap();
    let archive: Value =
        serde_json::from_reader(flate2::read::GzDecoder::new(compressed.as_slice())).unwrap();
    assert_eq!(archive, report);
    let terminal = command(dir.path(), &["review", "--from", "after", "--to", &from]);
    assert!(
        terminal.contains("RECORDED_CONTRACT_CRITERION")
            && terminal.contains("137 current bound caller(s)")
    );
    assert_eq!(std::fs::read(database).unwrap(), db_before);
    assert_eq!(std::fs::read(active).unwrap(), active_before);
    let repo = Repository::open_readonly(dir.path()).unwrap();
    assert_eq!(repo.current_view(), from);
    assert_eq!(repo.working_copy_record(wc).unwrap(), expected_wc);
}
