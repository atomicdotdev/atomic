//! Verify executable audit coverage through the real CLI, not just catalog flags.

use atomic_core::{change::ChangeHeader, types::Base32};
use atomic_repository::{record::RecordOptions, tracking::TrackingOptions, Repository};
use std::collections::BTreeSet;
use std::process::Command;

#[test]
fn repository_wide_ancestor_is_reference_only_in_json_and_html() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    repo.init_vault().unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    std::fs::create_dir_all(directory.path().join("src")).unwrap();
    // One ancestor spans the repository and holds payload that the selected
    // single-file change must never drag into either export, even compressed.
    let payload = "ANCESTOR_ONLY_FILE_TOKEN_PAYLOAD ".repeat(512);
    for index in 0..32 {
        let path = format!("ancestor-only-{index}.txt");
        std::fs::write(directory.path().join(&path), &payload).unwrap();
        repo.add(wc, &path, TrackingOptions::default()).unwrap();
    }
    let path = directory.path().join("src/lib.rs");
    std::fs::write(&path, "pub fn selected() {}\n").unwrap();
    repo.add(wc, "src/lib.rs", TrackingOptions::default())
        .unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
            .enrich_kg(false)
    };
    let ancestor = repo
        .record(
            wc,
            ChangeHeader::new(format!("{}\nANCESTOR_ONLY_MESSAGE_BODY", "🦀".repeat(300))),
            options(),
        )
        .unwrap()
        .hash()
        .to_base32();
    let source = "pub fn selected() { todo!(); }\n";
    std::fs::write(&path, source).unwrap();
    let selected = repo
        .record(wc, ChangeHeader::new("selected change"), options())
        .unwrap()
        .hash()
        .to_base32();
    std::fs::write(&path, "fn DIRTY_EXPORT_PAYLOAD() {}\n").unwrap();
    let database = repo.dot_dir().join("atomic.redb");
    drop(repo);
    let before = std::fs::read(&database).unwrap();
    let export = |flags: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_atomic"))
            .current_dir(directory.path())
            .env_remove("ATOMIC_SESSION_ID")
            .env("PATH", directory.path().join("no-external-executables"))
            .env("OPENAI_API_KEY", "must-not-be-used")
            .env("ANTHROPIC_API_KEY", "must-not-be-used")
            .env("OPENAI_BASE_URL", "http://127.0.0.1:1")
            .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:1")
            .env("LLM_MODEL", "must-not-be-used")
            .args(["review", "--change", &selected])
            .args(flags)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let json = export(&["--json"]);
    let report: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(report["schema_version"], 2);
    assert_eq!(report["selected_roots"], serde_json::json!([selected]));
    assert_eq!(report["dependency_only"], serde_json::json!([ancestor]));
    assert_eq!(
        report["dependency_closure"],
        serde_json::json!([ancestor, selected])
    );
    let references = report["dependency_references"].as_array().unwrap();
    assert_eq!(references.len(), 1);
    let reference = &references[0];
    assert_eq!(reference["hash"], ancestor);
    assert_eq!(reference["direct"], true);
    assert_eq!(reference["on_current_view"], true);
    assert_eq!(reference["summary"].as_str().unwrap(), "🦀".repeat(240));
    assert_eq!(
        reference
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        [
            "hash",
            "direct",
            "on_current_view",
            "summary",
            "dependencies"
        ]
        .into_iter()
        .collect()
    );
    let changes = report["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["hash"], selected);
    assert_eq!(changes[0]["paths"], serde_json::json!(["src/lib.rs"]));
    assert_eq!(changes[0]["dependencies"], serde_json::json!([ancestor]));
    assert!(changes[0]["recorded_patch"].is_object());
    assert!(changes[0]["diff"].as_str().unwrap().contains("todo!()"));
    assert!(!report["graph"]["findings"].as_array().unwrap().is_empty());
    assert!(report["graph"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node["kind"] == "metric"));
    for excluded in [
        "ANCESTOR_ONLY_FILE_TOKEN_PAYLOAD",
        "ANCESTOR_ONLY_MESSAGE_BODY",
        "ancestor-only-",
        "DIRTY_EXPORT_PAYLOAD",
    ] {
        assert!(
            !json.contains(excluded),
            "unexpected ancestor/dirty payload: {excluded}"
        );
    }
    let html = export(&["--html", "--no-open"]);
    // A generous bound detects repository-payload inclusion, not compression
    // efficiency. The decoded archive must also pass the exact JSON boundary.
    assert!(
        json.len() < payload.len() * 16,
        "{} bytes of JSON",
        json.len()
    );
    assert!(
        html.len() < payload.len() * 16,
        "{} bytes of HTML",
        html.len()
    );
    let data = html
        .split("id=\"review-data\"")
        .nth(1)
        .unwrap()
        .split_once('>')
        .unwrap()
        .1
        .split("</script>")
        .next()
        .unwrap();
    let document: serde_json::Value = serde_json::from_str(data).unwrap();
    assert_eq!(document["local_only"], true);
    assert_eq!(document["audit_cards"].as_array().unwrap().len(), 10);
    assert_eq!(document["metric_cards"].as_array().unwrap().len(), 10);
    assert!(document["view"]["graph"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|node| node["kind"] != "review_execution"));
    let compressed = data_encoding::BASE64
        .decode(
            document["report_archive"]["data"]
                .as_str()
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
    let embedded: serde_json::Value =
        serde_json::from_reader(flate2::read::GzDecoder::new(compressed.as_slice())).unwrap();
    assert_eq!(
        embedded, report,
        "HTML download must have exactly the JSON boundary"
    );
    let snapshots = document["sources"].as_array().unwrap();
    assert!(snapshots
        .iter()
        .all(|snapshot| snapshot["path"] == "src/lib.rs" && snapshot["reviewed_root"] == true));
    assert_eq!(snapshots[0]["after"]["text"], source);
    assert_eq!(before, std::fs::read(database).unwrap());
}

#[test]
fn all_seven_substantive_audits_run_without_supplied_findings() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    repo.init_vault().unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    std::fs::create_dir_all(directory.path().join("src")).unwrap();
    let path = directory.path().join("src/lib.rs");
    let mut source = String::from(
        r#"
fn masked() -> Result<(),()> { match work() { Ok(())=>Ok(()), Err(_)=>Ok(()) } }
fn duplicate(b:bool) { if b { work(1); } else { work(1); } }
fn implementation(x:i32)->i32 { x+1 }
fn middle(x:i32)->i32 { implementation(x) }
fn outer(x:i32)->i32 { middle(x) }
async fn locked() { let m=std::sync::Mutex::new(0); let guard=m.lock().unwrap(); work().await; drop(guard); }
#[test] fn weak_test() { let result=work(); assert_eq!(result,result); }
fn cast(bits:u32)->f32 { unsafe { std::mem::transmute::<u32,f32>(bits) } }
fn large(x:i32) {
"#,
    );
    for i in 0..15 {
        source.push_str(&format!("if x == {i} {{ work(); }}\n"));
    }
    for _ in 0..100 {
        source.push_str("work();\n");
    }
    source.push_str("}\n");
    std::fs::write(&path, &source).unwrap();
    repo.add(wc, "src/lib.rs", TrackingOptions::default())
        .unwrap();
    let hash = repo
        .record(
            wc,
            ChangeHeader::new("audit fixture"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true)
                .enrich_kg(false),
        )
        .unwrap()
        .hash()
        .to_base32();
    std::fs::write(&path, "fn unrelated_dirty_bytes() {}\n").unwrap();
    let database = repo.dot_dir().join("atomic.redb");
    drop(repo);
    let before = std::fs::read(&database).unwrap();
    let mut original_report = None;
    for json in [true, false] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_atomic"));
        command
            .current_dir(directory.path())
            .env_remove("ATOMIC_SESSION_ID")
            .args(["review", "--change", &hash]);
        if json {
            command.arg("--json");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        if json {
            let report: serde_json::Value = serde_json::from_str(&text).unwrap();
            let findings = report["graph"]["findings"].as_array().unwrap();
            let checks = report["changes"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|change| change["analysis"]["checks"].as_array().unwrap())
                .collect::<Vec<_>>();
            for card in atomic_audit::AUDIT_CARDS {
                assert!(
                    checks.iter().any(|check| check["rule"] == card.id),
                    "Missing local coverage for {}",
                    card.id
                );
            }
            let rules: BTreeSet<_> = findings
                .iter()
                .map(|finding| {
                    finding["finding"]["requirement_or_rule"]["id"]
                        .as_str()
                        .unwrap()
                })
                .collect();
            assert_eq!(
                rules,
                [
                    "anti-slop.failure-masking",
                    "anti-slop.duplicated-behavior",
                    "anti-slop.unnecessary-machinery",
                    "anti-slop.manageability",
                    "anti-slop.async-resource-safety",
                    "anti-slop.weak-verification",
                    "anti-slop.unsafe-shortcuts"
                ]
                .into_iter()
                .collect()
            );
            assert_eq!(findings.len(), 7);
            for finding in findings {
                assert_eq!(finding["finding"]["change"], hash);
                assert_eq!(finding["finding"]["introduced_by_change"]["status"], "yes");
                assert!(!finding["finding"]["evidence"]
                    .as_array()
                    .unwrap()
                    .is_empty());
            }
            original_report = Some(report);
        } else {
            for number in 1..=7 {
                assert!(text.contains(&format!("F{number} ·")));
            }
            assert!(text.contains("Audit/rule: anti-slop.failure-masking"));
            assert!(!text.contains("Not automatically audited:"));
        }
    }
    let destination = directory.path().join("review #1 100%.html");
    for to_file in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_atomic"));
        command
            .current_dir(directory.path())
            .env_remove("ATOMIC_SESSION_ID")
            .args(["review", "--change", &hash, "--html", "--no-open"]);
        if to_file {
            command.arg("--output").arg(&destination);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let html = if to_file {
            assert!(output.stdout.is_empty());
            if let Some(artifact) = std::env::var_os("ATOMIC_REVIEW_HTML_ARTIFACT") {
                std::fs::copy(&destination, artifact).unwrap();
            }
            std::fs::read_to_string(&destination).unwrap()
        } else {
            String::from_utf8(output.stdout).unwrap()
        };
        assert!(html.starts_with("<!doctype html>"));
        assert!(!html.contains("unrelated_dirty_bytes"));
        let encoded = html
            .split("id=\"review-data\"")
            .nth(1)
            .unwrap()
            .split_once('>')
            .unwrap()
            .1
            .split("</script>")
            .next()
            .unwrap();
        let document: serde_json::Value = serde_json::from_str(encoded).unwrap();
        let compressed = data_encoding::BASE64
            .decode(
                document["report_archive"]["data"]
                    .as_str()
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
        let embedded: serde_json::Value =
            serde_json::from_reader(flate2::read::GzDecoder::new(compressed.as_slice())).unwrap();
        assert_eq!(&embedded, original_report.as_ref().unwrap());
        assert_eq!(embedded["graph"]["findings"].as_array().unwrap().len(), 7);
        let snapshot = document["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|snapshot| snapshot["path"] == "src/lib.rs")
            .unwrap();
        assert_eq!(snapshot["after"]["text"], source);
    }
    assert_eq!(
        before,
        std::fs::read(database).unwrap(),
        "audit inspection must be read-only"
    );
}
