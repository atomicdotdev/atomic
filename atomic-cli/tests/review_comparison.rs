//! Paired recorded views: identity, local observations, source parity, no writes.
use atomic_core::{change::ChangeHeader, pristine::ViewScope, types::Base32};
use atomic_repository::{record::RecordOptions, tracking::TrackingOptions, Repository};
use serde_json::Value;
use std::{collections::BTreeSet, path::Path, process::Command};

fn record(repo: &Repository) {
    repo.record(
        repo.require_working_copy_id().unwrap(),
        ChangeHeader::new("comparison fixture"),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
            .enrich_kg(false),
    )
    .unwrap();
}
fn write(repo: &Repository, path: &str, text: &str) {
    let full = repo.root().join(path);
    let exists = full.exists();
    std::fs::write(full, text).unwrap();
    if !exists {
        repo.add(
            repo.require_working_copy_id().unwrap(),
            path,
            TrackingOptions::default(),
        )
        .unwrap();
    }
}
fn command(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_atomic"))
        .current_dir(root)
        .env("PATH", root.join("no-executables"))
        .env("ATOMIC_SESSION_ID", "../must-not-be-used")
        .env("OPENAI_API_KEY", "must-not-be-used")
        .env("ANTHROPIC_API_KEY", "must-not-be-used")
        .env("OPENAI_BASE_URL", "http://127.0.0.1:1")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:1")
        .args(args)
        .output()
        .unwrap()
}
fn review(root: &Path, from: &str, to: &str) -> Value {
    let output = command(root, &["review", "--from", from, "--to", to, "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
fn file<'a>(report: &'a Value, path: &str) -> &'a Value {
    report["view_comparison"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == path)
        .unwrap_or_else(|| panic!("Missing {path} in {}", report["view_comparison"]["files"]))
}
fn states(report: &Value, path: &str) -> BTreeSet<String> {
    file(report, path)["finding_states"]
        .as_object()
        .unwrap()
        .values()
        .map(|s| s["status"].as_str().unwrap().to_owned())
        .collect()
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
fn compare_recorded_views_in_both_directions_without_touching_the_working_copy() {
    let directory = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(directory.path()).unwrap();
    repo.init_vault().unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    let from = repo.current_view().to_owned();
    for (path, text) in [
        ("same.rs", "pub fn same() { todo!(); }\n"),
        ("old.rs", "pub fn renamed() { todo!(); }\n"),
        ("removed.rs", "pub fn removed() { todo!(); }\n"),
        ("fixed.rs", "pub fn fixed() { todo!(); }\n"),
        (
            "changed.rs",
            "pub fn changed() { todo!(\"before\"); todo!(); }\n",
        ),
        ("metrics.rs", "pub fn metric(a: bool) -> bool { a }\n"),
        ("parse.rs", "pub fn parse() { todo!(); }\n"),
        ("notes.txt", "Unsupported recorded source\n"),
        ("replace.rs", "pub fn original() { todo!(); }\n"),
        (
            "overload.ts",
            "function f(a: string) { return a; }\nfunction f(a: number) { return a; }\n",
        ),
    ] {
        write(&repo, path, text);
    }
    for path in ["clone-a.ts", "clone-b.ts"] {
        write(&repo,path,"export function clone(value) { const a = value + 1; const b = a * 2; const c = b - value; if (c > 0) { return c + a; } else { return value - c; } }\n");
    }
    record(&repo);
    repo.create_draft_view("after", &from).unwrap();
    repo.switch_view(wc, "after").unwrap();
    write(&repo, "fixed.rs", "pub fn fixed() {}\n");
    write(
        &repo,
        "changed.rs",
        "pub fn changed() { todo!(\"after\"); todo!(); }\n",
    );
    write(
        &repo,
        "metrics.rs",
        "pub fn metric(a: bool) -> bool { if a { true } else { false } }\n",
    );
    write(&repo, "parse.rs", "pub fn parse( {\n");
    std::fs::rename(
        directory.path().join("old.rs"),
        directory.path().join("renamed.rs"),
    )
    .unwrap();
    repo.move_file(wc, "old.rs", "renamed.rs").unwrap();
    std::fs::remove_file(directory.path().join("removed.rs")).unwrap();
    std::fs::remove_file(directory.path().join("clone-b.ts")).unwrap();
    std::fs::remove_file(directory.path().join("replace.rs")).unwrap();
    record(&repo);
    write(&repo, "added.rs", "pub fn added() { todo!(); }\n");
    write(&repo, "replace.rs", "pub fn replacement() { todo!(); }\n");
    record(&repo);
    let expected_after = repo.get_view_info("after").unwrap().state.to_base32();
    repo.create_view_with_identity("empty", ViewScope::Shared, None)
        .unwrap();
    repo.switch_view(wc, &from).unwrap();
    std::fs::write(directory.path().join("same.rs"), "DIRTY_NOT_RECORDED\n").unwrap();
    write(&repo, "staged.rs", "STAGED_NOT_RECORDED\n");
    std::fs::write(
        directory.path().join("untracked.rs"),
        "UNTRACKED_NOT_RECORDED\n",
    )
    .unwrap();
    let wc_record = repo.working_copy_record(wc).unwrap();
    let database = repo.dot_dir().join("atomic.redb");
    let active_path = repo.dot_dir().join("current_view");
    let active_bytes = std::fs::read(&active_path).unwrap();
    drop(repo);
    let db_bytes = std::fs::read(&database).unwrap();

    let report = review(directory.path(), &from, "after");
    assert_eq!(report["schema_version"], 4);
    assert_eq!(report["selection"]["kind"], "view_comparison");
    assert_eq!(report["view_comparison"]["from"]["view"], from);
    assert_eq!(report["view_comparison"]["to"]["state"], expected_after);
    assert!(report["changes"].as_array().unwrap().is_empty());
    assert!(report["selected_roots"].as_array().unwrap().is_empty());
    assert!(report.get("view_snapshot").is_none());
    for (path, status) in [
        ("same.rs", "unchanged"),
        ("renamed.rs", "renamed"),
        ("removed.rs", "removed"),
        ("fixed.rs", "modified"),
        ("added.rs", "added"),
        ("notes.txt", "unavailable"),
        // Re-creation at this name is a recorded restore of the same inode.
        ("replace.rs", "modified"),
    ] {
        assert_eq!(file(&report, path)["status"], status, "{path}");
    }
    assert_eq!(
        file(&report, "renamed.rs")["before_inode"],
        file(&report, "renamed.rs")["after_inode"]
    );
    assert_eq!(
        file(&report, "replace.rs")["before_inode"],
        file(&report, "replace.rs")["after_inode"]
    );
    assert_eq!(
        states(&report, "same.rs"),
        BTreeSet::from(["existing".into()])
    );
    assert_eq!(
        states(&report, "renamed.rs"),
        BTreeSet::from(["existing".into()])
    );
    assert_eq!(states(&report, "added.rs"), BTreeSet::from(["new".into()]));
    assert_eq!(
        states(&report, "removed.rs"),
        BTreeSet::from(["resolved".into()])
    );
    assert_eq!(
        states(&report, "fixed.rs"),
        BTreeSet::from(["resolved".into()])
    );
    assert_eq!(
        states(&report, "changed.rs"),
        BTreeSet::from(["changed".into(), "existing".into()])
    );
    assert!(states(&report, "parse.rs").contains("unknown"));
    for finding in report["graph"]["findings"].as_array().unwrap() {
        assert!(finding["comparison"].is_object());
        assert_eq!(
            finding["finding"]["introduced_by_change"]["status"],
            "unknown"
        );
    }
    let metric = file(&report, "metrics.rs")["analysis"]["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["observation"]["metric"] == "cyclomatic_complexity")
        .unwrap();
    assert_eq!(metric["delta"], 1);
    let clones = file(&report, "clone-a.ts")["analysis"]["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["observation"]["metric"] == "duplicated_tokens")
        .unwrap();
    assert!(
        clones["observation"]["before"]["value"]
            .as_u64()
            .unwrap_or_else(|| panic!("Clone measurement: {clones}"))
            > 0
    );
    assert_eq!(clones["observation"]["after"]["value"], 0);
    assert!(
        clones["delta"].as_i64().unwrap() < 0,
        "Unchanged file metrics must retain side-wide clone context"
    );
    for m in file(&report, "overload.ts")["analysis"]["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["subject_identity_ambiguous"] == true)
    {
        assert!(m["delta"].is_null());
        assert_eq!(m["observation"]["before"]["status"], "unavailable");
    }
    let reverse = review(directory.path(), "after", &from);
    let metric = file(&reverse, "metrics.rs")["analysis"]["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["observation"]["metric"] == "cyclomatic_complexity")
        .unwrap();
    assert_eq!(metric["delta"], -1);
    assert_eq!(
        states(&reverse, "added.rs"),
        BTreeSet::from(["resolved".into()])
    );
    assert_eq!(file(&reverse, "old.rs")["status"], "renamed");
    let same = review(directory.path(), &from, &from);
    for file in same["view_comparison"]["files"].as_array().unwrap() {
        assert!(file["finding_states"]
            .as_object()
            .unwrap()
            .values()
            .all(|s| s["status"] == "existing"));
        assert!(file["analysis"]["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["delta"].is_null() || m["delta"] == 0));
    }
    let empty = review(directory.path(), "empty", "empty");
    assert!(empty["view_comparison"]["files"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(empty["graph"]["findings"].as_array().unwrap().is_empty());
    let add_all = review(directory.path(), "empty", &from);
    assert!(add_all["view_comparison"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .all(|f| f["status"] == "added"));

    let output = command(
        directory.path(),
        &[
            "review",
            "--from",
            &from,
            "--to",
            "after",
            "--html",
            "--no-open",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let html = String::from_utf8(output.stdout).unwrap();
    if let Some(path) = std::env::var_os("ATOMIC_REVIEW_COMPARISON_ARTIFACT") {
        std::fs::write(path, &html).unwrap();
    }
    assert!(html.contains("data:image/png;base64,"));
    for forbidden in [
        "DIRTY_NOT_RECORDED",
        "STAGED_NOT_RECORDED",
        "UNTRACKED_NOT_RECORDED",
    ] {
        assert!(!html.contains(forbidden));
    }
    let doc = document(&html);
    assert_eq!(doc["local_only"], true);
    let sources = doc["sources"].as_array().unwrap();
    let renamed = sources
        .iter()
        .find(|s| {
            s["change"] == file(&report, "renamed.rs")["source_ref"] && s["path"] == "renamed.rs"
        })
        .unwrap();
    assert_eq!(renamed["before"]["text"], "pub fn renamed() { todo!(); }\n");
    assert_eq!(renamed["before"], renamed["after"]);
    let removed = sources.iter().find(|s| s["path"] == "removed.rs").unwrap();
    assert_eq!(removed["after"]["status"], "absent");
    let added = sources.iter().find(|s| s["path"] == "added.rs").unwrap();
    assert_eq!(added["before"]["status"], "absent");
    let compressed = data_encoding::BASE64
        .decode(doc["report_archive"]["data"].as_str().unwrap().as_bytes())
        .unwrap();
    let restored: Value =
        serde_json::from_reader(flate2::read::GzDecoder::new(compressed.as_slice())).unwrap();
    assert_eq!(restored, report);
    let terminal = command(
        directory.path(),
        &["review", "--from", &from, "--to", "after"],
    );
    assert!(terminal.status.success());
    assert!(String::from_utf8_lossy(&terminal.stdout)
        .contains(&format!("BEFORE {from} -> AFTER after")));
    for args in [
        vec!["review", "--from", &from],
        vec!["review", "--to", "after"],
        vec!["review", "--from", "missing", "--to", "after"],
        vec!["review", "--from", &from, "--to", "after", "--view", &from],
        vec![
            "review", "--from", &from, "--to", "after", "--change", "unused",
        ],
        vec![
            "review", "--from", &from, "--to", "after", "--intent", "unused",
        ],
        vec![
            "review", "--from", &from, "--to", "after", "--turn", "unused:1",
        ],
        vec![
            "review",
            "--from",
            &from,
            "--to",
            "after",
            "--findings",
            "unused",
        ],
        vec![
            "review",
            "--from",
            &from,
            "--to",
            "after",
            "--risk-profile",
            "unused",
        ],
    ] {
        let output = command(directory.path(), &args);
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty());
    }
    assert_eq!(std::fs::read(database).unwrap(), db_bytes);
    assert_eq!(std::fs::read(active_path).unwrap(), active_bytes);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("same.rs")).unwrap(),
        "DIRTY_NOT_RECORDED\n"
    );
    let repo = Repository::open_readonly(directory.path()).unwrap();
    assert_eq!(repo.current_view(), from);
    assert_eq!(repo.working_copy_record(wc).unwrap(), wc_record);
}
