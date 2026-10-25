use atomic_core::{
    change::{Change, ChangeHeader, ProvenanceGraph},
    kg::iri,
    pristine::VaultEntryType,
};
use atomic_repository::Repository;
use std::{
    path::Path,
    process::{Command, Output},
};

const UID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const KEY: &str = "TEST::tester::1";

fn command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_atomic"));
    command
        .current_dir(root)
        .args(args)
        .env("ATOMIC_SERVICE", "local")
        .env_remove("ATOMIC_RPC");
    command
}
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn fixture(root: &Path) -> String {
    let repo = Repository::init(root).unwrap();
    repo.init_vault().unwrap();
    repo.vault_store(&format!("intents/{UID}/intent.md"), VaultEntryType::Intent,
        b":::why\nInspect exact lineage.\n:::\n".to_vec(),
        serde_json::json!({"uid":UID,"id":KEY,"title":"Graph </script><script>alert(1)</script>","kind":"feature","status":"in_progress"}).to_string()).unwrap();
    repo.vault_reconcile_intent_manifest().unwrap();
    let mut bytes = vec![];
    let mut change = Change::empty(ChangeHeader::new("Exact recorded change"));
    let mut provenance = atomic_core::change::Provenance::builder()
        .vendor(atomic_core::change::AIVendor::OpenAI)
        .model("graph-test-model")
        .tool(atomic_core::change::AITool::cli("graph-test-agent"))
        .request_id("graph-test-usage")
        .cost_usd(0.125)
        .build();
    provenance.tokens = atomic_core::change::TokenUsage::full(100, 40, 10, 60, 20);
    change.add_provenance(provenance);
    let hash = change.serialize(&mut bytes).unwrap();
    repo.import_change_bytes(&hash, &bytes).unwrap();
    repo.save_provenance_graph(
        &ProvenanceGraph::builder("graph-test-session", "opencode")
            .plan_id(UID)
            .add_change_explained(hash)
            .build(),
    )
    .unwrap();
    repo.rebuild_kg(true).unwrap();
    let root = iri::mint_intent(&repo.project_id().unwrap(), UID).unwrap();
    assert_eq!(repo.intent_graph(KEY, 100).unwrap().root, root);
    assert_eq!(repo.intent_graph(&root, 100).unwrap().root, root);
    root
}

#[test]
fn graph_resolves_ids_renders_all_formats_and_preserves_database() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let canonical = fixture(root);
    let database = root.join(".atomic/atomic.redb");
    let before = std::fs::read(&database).unwrap();
    let shown = success(command(root, &["intent", "show", KEY]).output().unwrap());
    assert!(shown.starts_with(&format!("Intent {KEY} — ")));
    assert!(shown.contains("├── Metadata\n│   ├── Status: in_progress\n│   └── Kind: feature\n"));
    assert!(shown.contains("├── Why\n│   └── Inspect exact lineage.\n"));
    assert!(shown.ends_with("    └── Signed: no\n"));
    assert_eq!(shown.matches("Kind: feature").count(), 1);
    let json_show: serde_json::Value = serde_json::from_str(&success(
        command(root, &["intent", "show", KEY, "--json"])
            .output()
            .unwrap(),
    ))
    .unwrap();
    assert_eq!(json_show["status"], "in_progress");
    assert!(json_show["@id"].as_str().unwrap().contains(UID));
    let mut expected = None;
    for id in [KEY, UID, canonical.as_str()] {
        let output = success(
            command(root, &["intent", "graph", id, "--json"])
                .output()
                .unwrap(),
        );
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["root"], canonical);
        assert_eq!(json["implementing_changes"].as_array().unwrap().len(), 1);
        assert_eq!(json["truncated"], false);
        let root_node = json["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["id"] == canonical)
            .unwrap();
        assert_eq!(
            root_node["metadata"]["ai_usage"]["cost"]["amount"],
            "0.125000"
        );
        assert_eq!(
            root_node["metadata"]["ai_usage"]["contributors"][0]["model"],
            "graph-test-model"
        );
        let change_id = json["implementing_changes"][0].as_str().unwrap();
        let neighbors: serde_json::Value = serde_json::from_str(&success(
            command(root, &["query", "neighbors", change_id, "--json"])
                .output()
                .unwrap(),
        ))
        .unwrap();
        let change_node = neighbors["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["id"] == change_id)
            .unwrap();
        assert_eq!(
            change_node["metadata"]["ai_usage"]["cost"]["amount"],
            "0.125000"
        );
        if let Some(expected) = &expected {
            assert_eq!(&json, expected);
        }
        expected = Some(json);
    }
    let text = success(command(root, &["intent", "graph", KEY]).output().unwrap());
    assert!(text.contains("├── Implementing changes (1)"));
    assert!(!text.contains("atomic:generatedChange"));
    assert!(!text.contains("Summary: [context]"));
    let details = success(
        command(root, &["intent", "graph", KEY, "--details"])
            .output()
            .unwrap(),
    );
    assert!(details.contains("├── Implementing changes (1)"));
    assert!(details.contains("atomic:generatedChange →"));
    assert!(details.contains(&format!("ID: {canonical}")));
    let dot = success(
        command(root, &["intent", "graph", KEY, "--dot"])
            .output()
            .unwrap(),
    );
    assert!(dot.contains("digraph query"));
    assert!(dot.contains("atomic:implements"));
    let html_path = root.join("intent.html");
    success(
        command(
            root,
            &[
                "intent",
                "graph",
                KEY,
                "--html",
                "--no-open",
                "-o",
                html_path.to_str().unwrap(),
            ],
        )
        .output()
        .unwrap(),
    );
    let html = std::fs::read_to_string(html_path).unwrap();
    assert!(html.contains("<!DOCTYPE html>"));
    assert!(!html.contains("</script><script>alert(1)</script>"));
    assert!(html.contains("\\u003c/script\\u003e"));
    assert!(html.contains("id=\"node-inspector\""));
    assert!(html.contains("createGraphInspector"));
    assert!(html.contains("canvas.addEventListener('click'"));
    assert!(html.contains("Incoming relationships"));
    let capped = success(
        command(
            root,
            &["intent", "graph", KEY, "--json", "--max-nodes", "1"],
        )
        .output()
        .unwrap(),
    );
    let capped: serde_json::Value = serde_json::from_str(&capped).unwrap();
    assert_eq!(capped["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(capped["truncated"], true);
    assert_eq!(
        std::fs::read(database).unwrap(),
        before,
        "graph reads must not rebuild or mutate the KG"
    );
}

/// Run explicitly on machines with Chrome; normal tests never launch GUI apps.
#[test]
#[ignore = "requires Chrome; set ATOMIC_TEST_CHROME and run with --ignored"]
fn graph_inspector_dom_interactions_in_chrome() {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/graph_inspector_test.html");
    let dom = chrome_dom(&fixture, "1200,900");
    assert!(dom.contains("data-test-status=\"passed\""), "{dom}");
}

fn chrome_dom(path: &Path, window_size: &str) -> String {
    let chrome = std::env::var_os("ATOMIC_TEST_CHROME")
        .expect("set ATOMIC_TEST_CHROME to a Chrome/Chromium executable");
    let profile = tempfile::tempdir().unwrap();
    let url = url::Url::from_file_path(path).unwrap();
    let output = std::process::Command::new(chrome)
        .args([
            "--headless=new",
            "--disable-gpu",
            "--no-first-run",
            "--disable-background-networking",
            "--disable-extensions",
            "--dump-dom",
            "--virtual-time-budget=3000",
            "--allow-file-access-from-files",
        ])
        .arg(format!("--window-size={window_size}"))
        .arg(format!("--user-data-dir={}", profile.path().display()))
        .arg(url.as_str())
        .output()
        .unwrap();
    success(output)
}

#[test]
#[ignore = "requires Chrome and D3; set ATOMIC_TEST_CHROME and optionally ATOMIC_TEST_D3"]
fn exported_graph_canvas_selection_drag_zoom_and_responsive_layout() {
    let directory = tempfile::tempdir().unwrap();
    fixture(directory.path());
    let mut html = success(
        command(
            directory.path(),
            &["intent", "graph", KEY, "--html", "--no-open"],
        )
        .output()
        .unwrap(),
    );
    if let Some(d3) = std::env::var_os("ATOMIC_TEST_D3") {
        let url = url::Url::from_file_path(std::fs::canonicalize(d3).unwrap()).unwrap();
        html = html.replace("https://cdn.jsdelivr.net/npm/d3@7", url.as_str());
    }
    html = html.replace("<head>", "<head><script>window.graphTestErrors = []; window.addEventListener('error', function(event) { window.graphTestErrors.push(event.message || 'resource error'); });</script>");
    html = html.replace(
        "</body>",
        &format!(
            "<script>{}</script></body>",
            include_str!("fixtures/graph_canvas_test.js")
        ),
    );
    let path = directory.path().join("canvas-inspector.html");
    std::fs::write(&path, html).unwrap();
    for size in ["1200,900", "600,900"] {
        let dom = chrome_dom(&path, size);
        assert!(
            dom.contains("data-canvas-test-status=\"passed\""),
            "{size}: {dom}"
        );
    }
}

#[test]
fn graph_rejects_unknown_intents_conflicting_formats_and_invalid_limits() {
    let directory = tempfile::tempdir().unwrap();
    fixture(directory.path());
    for args in [
        vec!["intent", "graph", "TEST::tester::999", "--json"],
        vec!["intent", "graph", KEY, "--json", "--html"],
        vec!["intent", "graph", KEY, "--max-nodes", "0"],
        vec!["intent", "graph", KEY, "--max-nodes", "10001"],
        vec!["intent", "graph", KEY, "--no-open"],
    ] {
        assert!(!command(directory.path(), &args)
            .output()
            .unwrap()
            .status
            .success());
    }
}

#[test]
fn graph_distinguishes_explicit_declaration_from_captured_implementation() {
    use atomic_repository::IntentUpdateOptions;
    let directory = tempfile::tempdir().unwrap();
    fixture(directory.path());
    let repo = Repository::open(directory.path()).unwrap();
    let mut bytes = vec![];
    let hash = Change::empty(ChangeHeader::new("Mixed parent change"))
        .serialize(&mut bytes)
        .unwrap();
    repo.import_change_bytes(&hash, &bytes).unwrap();
    let entry = repo.vault_intent_show(KEY).unwrap();
    let body = format!("{}\n::ref{{to={} edge=declaresImplementation declarationEvidence=\"Author explicitly repairs a lost child association; no original plan provenance is claimed.\"}}\n", String::from_utf8(entry.content_bytes).unwrap(), iri::mint_change(&hash));
    repo.vault_intent_update(
        KEY,
        IntentUpdateOptions {
            content: Some(body),
            force: true,
            ..Default::default()
        },
    )
    .unwrap();
    drop(repo);
    let json: serde_json::Value = serde_json::from_str(&success(
        command(directory.path(), &["intent", "graph", KEY, "--json"])
            .output()
            .unwrap(),
    ))
    .unwrap();
    assert_eq!(json["implementing_changes"].as_array().unwrap().len(), 2);
    assert_eq!(json["captured_changes"].as_array().unwrap().len(), 1);
    assert_eq!(
        json["declared_changes"],
        serde_json::json!([iri::mint_change(&hash)])
    );
    let text = success(
        command(directory.path(), &["intent", "graph", KEY])
            .output()
            .unwrap(),
    );
    assert!(text.contains("[declared; original turn unproven]"));
    let details = success(
        command(directory.path(), &["intent", "graph", KEY, "--details"])
            .output()
            .unwrap(),
    );
    assert!(details.contains("declared implementation; original turn provenance not established"));
    assert!(details.contains("Declaration evidence: Author explicitly repairs"));
}

/// The demo-2 failure: real, fully recorded source and intent artifacts exist,
/// but there is no plan_id, implementation assertion, or retrospective claim.
#[cfg(unix)]
#[test]
fn graph_exposes_real_recording_evidence_without_plan_id_or_database_mutation() {
    use atomic_core::types::Base32;
    use std::fs;
    let directory = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let root = directory.path();
    let run = |args: &[&str]| {
        success(
            command(root, args)
                .env("HOME", home.path())
                .env("ATOMIC_CONFIG_DIR", home.path().join(".atomic"))
                .env("ATOMIC_NONINTERACTIVE", "1")
                .env_remove("ATOMIC_SESSION_ID")
                .env_remove("ATOMIC_AGENT_IDENTITY")
                .output()
                .unwrap(),
        )
    };
    run(&[
        "identity",
        "new",
        "tester",
        "--email",
        "tester@example.invalid",
        "--set-default",
    ]);
    run(&["init"]);
    {
        let repo = Repository::open(root).unwrap();
        repo.vault_store(&format!("intents/{UID}/intent.md"), VaultEntryType::Intent,
            b":::why\nBuild the CLI.\n:::\n".to_vec(),
            serde_json::json!({"uid":UID,"id":KEY,"title":"Build the CLI","kind":"feature","status":"in_progress"}).to_string()).unwrap();
        repo.vault_reconcile_intent_manifest().unwrap();
    }
    let artifact_path = format!(".vault/intents/{UID}/intent.md");
    fs::create_dir_all(root.join(format!(".vault/intents/{UID}"))).unwrap();
    fs::write(root.join(&artifact_path), format!("---\nid: {KEY}\nuid: {UID}\ntitle: Build the CLI\nstatus: in_progress\n---\n:::why\nBuild the CLI.\n:::\n")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/cli.ts"), "console.log('Hello, world!');\n").unwrap();
    run(&["add", &artifact_path, "src/cli.ts"]);
    run(&["record", "-m", "Record the CLI and its intent"]);
    let log: serde_json::Value =
        serde_json::from_str(&run(&["log", "-n", "1", "-f", "json"])).unwrap();
    let recorded_hash =
        atomic_core::types::Hash::from_base32(log[0]["hash"].as_str().unwrap().as_bytes()).unwrap();
    let recorded = iri::mint_change(&recorded_hash);
    // A later change shares the source file but not the intent artifact.
    fs::write(root.join("src/cli.ts"), "console.log('Unrelated edit');\n").unwrap();
    run(&["record", "-m", "Another change to the shared source file"]);
    {
        let repo = Repository::open(root).unwrap();
        repo.rebuild_kg(true).unwrap();
    }
    let database = root.join(".atomic/atomic.redb");
    let before = fs::read(&database).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&run(&["intent", "graph", KEY, "--json"])).unwrap();
    assert_eq!(json["recording_changes"], serde_json::json!([recorded]));
    assert_eq!(json["implementing_changes"], serde_json::json!([]));
    assert_eq!(json["captured_changes"], serde_json::json!([]));
    assert_eq!(json["declared_changes"], serde_json::json!([]));
    assert!(json["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["metadata"]["path"] == "src/cli.ts"));
    let text = run(&["intent", "graph", KEY]);
    assert!(text.contains("0 implementing · 1 recording"));
    assert!(text.contains("Intent recording evidence (not implementation) (1)"));
    let details = run(&["intent", "graph", KEY, "--details"]);
    assert!(details.contains("records intent artifact; implementation attribution not asserted"));
    assert!(!text.contains("No implementing changes are linked"));
    let dot = run(&["intent", "graph", KEY, "--dot"]);
    assert!(dot.contains("records intent artifact"));
    let html = run(&["intent", "graph", KEY, "--html", "--no-open"]);
    assert!(html.contains("1 intent-artifact recording"));
    assert_eq!(
        fs::read(database).unwrap(),
        before,
        "semantic graph reads must not write to the repository"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_has_local_reactor_parity_through_shared_read_endpoints() {
    use libatomic::atomic::{
        daemon_service_server::DaemonServiceServer,
        knowledge_service_server::KnowledgeServiceServer, vault_service_server::VaultServiceServer,
    };
    use libatomic::daemon::{
        services::DaemonImpl,
        services_agent::{KnowledgeImpl, VaultImpl},
        state::DaemonState,
    };
    use std::sync::Arc;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
    fixture(&root);
    let local = success(
        command(&root, &["intent", "graph", KEY, "--json"])
            .output()
            .unwrap(),
    );
    let local_show = success(command(&root, &["intent", "show", KEY]).output().unwrap());
    // Keep the Unix socket short enough for sockaddr_un on macOS.
    let socket_dir = tempfile::Builder::new()
        .prefix("ig-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let socket = socket_dir.path().join("s");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let state = Arc::new(DaemonState::new());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(DaemonServiceServer::new(DaemonImpl {
                state: state.clone(),
            }))
            .add_service(KnowledgeServiceServer::new(KnowledgeImpl {
                state: state.clone(),
            }))
            .add_service(VaultServiceServer::new(VaultImpl { state }))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::UnixListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            ),
    );
    let (output, show_output) = tokio::task::spawn_blocking(move || {
        let graph = command(&root, &["intent", "graph", KEY, "--json"])
            .env("ATOMIC_SERVICE", "reactor")
            .env("ATOMIC_DAEMON_SOCKET", &socket)
            .output()
            .unwrap();
        let show = command(&root, &["intent", "show", KEY])
            .env("ATOMIC_SERVICE", "reactor")
            .env("ATOMIC_DAEMON_SOCKET", &socket)
            .output()
            .unwrap();
        (graph, show)
    })
    .await
    .unwrap();
    let _ = stop.send(());
    server.await.unwrap().unwrap();
    let reactor = success(output);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&local).unwrap(),
        serde_json::from_str::<serde_json::Value>(&reactor).unwrap()
    );
    assert_eq!(success(show_output), local_show);
}
