//! Whole-view audits are local recorded snapshots, not per-change history replay.

use atomic_core::{change::ChangeHeader, pristine::ViewScope, types::Base32};
use atomic_repository::{record::RecordOptions, tracking::TrackingOptions, Repository};
use serde_json::Value;
use std::{collections::BTreeSet, path::Path, process::Command};

fn record(repo: &Repository, message: &str) -> String {
    repo.record(
        repo.require_working_copy_id().unwrap(),
        ChangeHeader::new(message),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
            .enrich_kg(false),
    )
    .unwrap()
    .hash()
    .to_base32()
}
fn write(repo: &Repository, path: &str, text: &str) {
    let full = repo.root().join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    let tracked = full.exists();
    std::fs::write(full, text).unwrap();
    if !tracked {
        repo.add(
            repo.require_working_copy_id().unwrap(),
            path,
            TrackingOptions::default(),
        )
        .unwrap();
    }
}

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_atomic"));
    command
        .current_dir(root)
        .env("ATOMIC_SESSION_ID", "../ambient-session-must-not-be-used")
        .env("PATH", root.join("no-external-executables"))
        .env("OPENAI_API_KEY", "must-not-be-used")
        .env("ANTHROPIC_API_KEY", "must-not-be-used")
        .env("OPENAI_BASE_URL", "http://127.0.0.1:1")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:1");
    command
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
fn named_view_audits_live_snapshot_once_without_switching_or_reading_dirty_bytes() {
    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let outside = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(directory.path()).unwrap();
    repo.init_vault().unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    let active = repo.current_view().to_owned();
    write(&repo, "inherited.rs", "pub fn inherited() { todo!(); }\n");
    write(&repo, ".vault/generated.rs", "VAULT_NOT_CODE invalid {{{\n");
    write(
        &repo,
        "deleted.rs",
        "pub fn deleted_marker() { todo!(); }\n",
    );
    let base = record(&repo, "parent source");
    repo.create_draft_view("audit-target", &active).unwrap();
    repo.switch_view(wc, "audit-target").unwrap();
    write(
        &repo,
        "old-name.rs",
        "pub fn OBSOLETE_REVISION() { todo!(); }\n",
    );
    record(&repo, "old target source");
    let current = "pub fn current_revision() { todo!(); }\n";
    write(&repo, "old-name.rs", current);
    record(&repo, "current target source");
    std::fs::rename(
        directory.path().join("old-name.rs"),
        directory.path().join("renamed.rs"),
    )
    .unwrap();
    repo.move_file(wc, "old-name.rs", "renamed.rs").unwrap();
    std::fs::remove_file(directory.path().join("deleted.rs")).unwrap();
    write(&repo, "notes.txt", "unsupported source remains explicit\n");
    #[cfg(unix)]
    {
        std::fs::write(
            outside.path().join("secret.rs"),
            "pub fn EXTERNAL_SOURCE_MUST_NOT_BE_READ() { todo!(); }\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.rs"),
            directory.path().join("link.rs"),
        )
        .unwrap();
        repo.add(wc, "link.rs", TrackingOptions::default()).unwrap();
    }
    record(&repo, "rename delete and metadata");
    let expected_state = repo
        .get_view_info("audit-target")
        .unwrap()
        .state
        .to_base32();
    repo.create_draft_view("unrelated", &active).unwrap();
    repo.switch_view(wc, "unrelated").unwrap();
    write(
        &repo,
        "foreign.rs",
        "pub fn FOREIGN_VIEW_ONLY() { todo!(); }\n",
    );
    let foreign = record(&repo, "unrelated source");
    repo.switch_view(wc, &active).unwrap();
    std::fs::write(
        directory.path().join("inherited.rs"),
        "DIRTY_WORKTREE_NOT_RECORDED\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("untracked.rs"),
        "UNTRACKED_NOT_SOURCE\n",
    )
    .unwrap();
    write(&repo, "staged.rs", "STAGED_NOT_RECORDED\n");
    let database = repo.dot_dir().join("atomic.redb");
    let compatibility = repo.dot_dir().join("current_view");
    let wc_id = repo.dot_dir().join("working_copy_id");
    let expected_wc = repo.working_copy_record(wc).unwrap();
    drop(repo);
    let bytes = std::fs::read(&database).unwrap();
    let current_view = std::fs::read(&compatibility).unwrap();
    let id_bytes = std::fs::read(&wc_id).unwrap();
    let output = command(directory.path())
        .args(["review", "--view", "audit-target", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["selection"]["kind"], "view");
    assert_eq!(report["source"]["view"], "audit-target");
    assert_eq!(report["source"]["view_state"], expected_state);
    assert_eq!(report["source"]["working_copy_view"], active);
    assert!(report["changes"].as_array().unwrap().is_empty());
    assert!(report["selected_roots"].as_array().unwrap().is_empty());
    let snapshot = &report["view_snapshot"];
    let reference = snapshot["source_ref"].as_str().unwrap();
    assert!(reference.starts_with("urn:atomic:view-snapshot:"));
    let paths = snapshot["paths"].as_array().unwrap();
    for path in [
        "inherited.rs",
        "renamed.rs",
        "notes.txt",
        ".vault/generated.rs",
    ] {
        assert!(
            paths.contains(&Value::String(path.into())),
            "missing {path}"
        );
    }
    for path in [
        "deleted.rs",
        "old-name.rs",
        "foreign.rs",
        "staged.rs",
        "untracked.rs",
    ] {
        assert!(
            !paths.contains(&Value::String(path.into())),
            "unexpected {path}"
        );
    }
    let findings = snapshot["analysis"]["findings"].as_array().unwrap();
    assert!(findings.len() >= 2);
    for finding in findings {
        assert_eq!(finding["change"], reference);
        assert_eq!(finding["introduced_by_change"]["status"], "unknown");
        assert!(finding["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["kind"] != "diff"));
    }
    let metrics = snapshot["analysis"]["metrics"].as_array().unwrap();
    assert!(!metrics.is_empty());
    let mut identities = BTreeSet::new();
    for metric in metrics {
        assert!(
            identities.insert((
                metric["path"].to_string(),
                metric["observation"]["metric"].to_string(),
                metric["observation"]["subject"].to_string()
            )),
            "history duplicated a snapshot measurement"
        );
        assert_eq!(metric["observation"]["before"]["status"], "unavailable");
        assert!(metric["delta"].is_null());
    }
    let checks = snapshot["analysis"]["checks"].as_array().unwrap();
    #[cfg(unix)]
    assert!(checks.iter().any(|check| check["path"] == "link.rs"
        && check["status"] == "skipped"
        && check["detail"].as_str().unwrap().contains("symlink")));
    for card in atomic_audit::AUDIT_CARDS {
        assert!(checks.iter().any(|check| check["rule"] == card.id));
    }
    assert!(report["dependency_closure"]
        .as_array()
        .unwrap()
        .contains(&Value::String(base)));
    assert!(!report["dependency_closure"]
        .as_array()
        .unwrap()
        .contains(&Value::String(foreign)));
    assert_eq!(report["graph"]["risk"]["basis"], "view_snapshot");
    assert_eq!(report["graph"]["risk"]["assessment"]["rating"], "unknown");
    let output = command(directory.path())
        .args(["review", "--view", "audit-target", "--html", "--no-open"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let html = String::from_utf8(output.stdout).unwrap();
    if let Some(path) = std::env::var_os("ATOMIC_REVIEW_VIEW_ARTIFACT") {
        std::fs::write(path, &html).unwrap();
    }
    for forbidden in [
        "DIRTY_WORKTREE_NOT_RECORDED",
        "OBSOLETE_REVISION",
        "FOREIGN_VIEW_ONLY",
        "STAGED_NOT_RECORDED",
        "EXTERNAL_SOURCE_MUST_NOT_BE_READ",
    ] {
        assert!(!html.contains(forbidden));
    }
    let doc = document(&html);
    assert_eq!(doc["local_only"], true);
    assert_eq!(doc["view"]["view_snapshot"]["view"], "audit-target");
    assert_eq!(doc["audit_cards"].as_array().unwrap().len(), 10);
    assert_eq!(doc["metric_cards"].as_array().unwrap().len(), 10);
    let source = doc["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["path"] == "renamed.rs")
        .unwrap();
    assert_eq!(source["after"]["text"], current);
    assert_eq!(source["before"]["status"], "unavailable");
    let compressed = data_encoding::BASE64
        .decode(doc["report_archive"]["data"].as_str().unwrap().as_bytes())
        .unwrap();
    let embedded: Value =
        serde_json::from_reader(flate2::read::GzDecoder::new(compressed.as_slice())).unwrap();
    assert_eq!(embedded, report);
    let output = command(directory.path())
        .args(["review", "--view", "audit-target"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let terminal = String::from_utf8(output.stdout).unwrap();
    assert!(
        terminal.contains("Selected view snapshot: audit-target")
            && terminal.contains("View snapshot metrics")
    );
    assert_eq!(std::fs::read(&database).unwrap(), bytes);
    assert_eq!(std::fs::read(compatibility).unwrap(), current_view);
    assert_eq!(std::fs::read(wc_id).unwrap(), id_bytes);
    let repo = Repository::open_readonly(directory.path()).unwrap();
    assert_eq!(repo.current_view(), active);
    assert_eq!(repo.working_copy_record(wc).unwrap(), expected_wc);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("inherited.rs")).unwrap(),
        "DIRTY_WORKTREE_NOT_RECORDED\n"
    );
}

#[test]
fn empty_named_view_is_not_latest_change_and_selectors_fail_closed() {
    let directory = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(directory.path()).unwrap();
    repo.init_vault().unwrap();
    write(&repo, "live.rs", "fn active_only() { todo!(); }\n");
    let hash = record(&repo, "active-only source");
    repo.create_view_with_identity("empty", ViewScope::Shared, None)
        .unwrap();
    drop(repo);
    let output = command(directory.path())
        .args(["review", "--view", "empty", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["view_snapshot"]["paths"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(report["graph"]["findings"].as_array().unwrap().is_empty());
    let output = command(directory.path())
        .args(["review", "--view", "empty", "--html", "--no-open"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let html = String::from_utf8(output.stdout).unwrap();
    if let Some(path) = std::env::var_os("ATOMIC_REVIEW_EMPTY_VIEW_ARTIFACT") {
        std::fs::write(path, &html).unwrap();
    }
    let doc = document(&html);
    assert_eq!(doc["view"]["view_snapshot"]["file_count"], 0);
    assert_eq!(doc["audit_cards"].as_array().unwrap().len(), 10);
    assert_eq!(doc["metric_cards"].as_array().unwrap().len(), 10);
    for args in [
        vec!["review", "--view", "missing"],
        vec!["review", "--view", "empty", "--change", &hash],
        vec!["review", "--view", "empty", "--intent", "unused"],
        vec!["review", "--view", "empty", "--turn", "session:1"],
        vec!["review", "--view", "empty", "--findings", "unused.json"],
        vec!["review", "--view", "empty", "--risk-profile", "unused.json"],
        vec!["review", "--view", "empty", "publish", "--view", "empty"],
    ] {
        let output = command(directory.path()).args(args).output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}
