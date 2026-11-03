//! Successful JSON commands emit one document without changing operation effects.
#![cfg(not(windows))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;

struct Fixture {
    repo: TempDir,
    home: TempDir,
}

impl Fixture {
    fn empty() -> Self {
        Self {
            repo: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn native() -> Self {
        let fixture = Self::empty();
        fixture.ok(&["init", "--vault", "--view", "main"]);
        fixture
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_atomic"));
        // Do not inherit the developer's managed run or daemon transport.
        for (name, _) in std::env::vars_os() {
            let text = name.to_string_lossy();
            if text.starts_with("ATOMIC_") || text.starts_with("OPENCODE_") {
                command.env_remove(name);
            }
        }
        command
            .args(args)
            .current_dir(self.repo.path())
            .env("HOME", self.home.path())
            .env("ATOMIC_HOME", self.home.path().join(".atomic"))
            .env("ATOMIC_CONFIG_DIR", self.home.path().join(".atomic"))
            .env("ATOMIC_SERVICE", "local")
            .env_remove("ATOMIC_RPC")
            .env("ATOMIC_NONINTERACTIVE", "1")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .env("NO_COLOR", "1");
        command
    }

    fn output(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run atomic")
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.output(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let text = self.ok(args);
        serde_json::from_str(&text).unwrap_or_else(|error| {
            panic!("{args:?} must emit exactly one JSON document: {error}: {text}")
        })
    }

    fn identity(&self) {
        self.ok(&[
            "identity",
            "new",
            "auditor",
            "--email",
            "audit@example.invalid",
            "--set-default",
        ]);
    }

    fn git(&self, args: &[&str]) {
        let output = Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .current_dir(self.repo.path())
            .env("HOME", self.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
    }
}

struct MockServer {
    url: String,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl MockServer {
    fn new(status: u16, response: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(error) => panic!("accept mock request: {error}"),
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request = String::new();
                    reader.read_line(&mut request).unwrap();
                    let mut length = 0;
                    loop {
                        let mut header = String::new();
                        reader.read_line(&mut header).unwrap();
                        if header.trim().is_empty() {
                            break;
                        }
                        if let Some((name, value)) = header.split_once(':') {
                            if name.eq_ignore_ascii_case("content-length") {
                                length = value.trim().parse().unwrap();
                            }
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    let body = if body.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_slice(&body).unwrap()
                    };
                    requests
                        .lock()
                        .expect("requests lock poisoned")
                        .push((request, body));
                    let body = response.to_string();
                    write!(stream, "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }
            })
        };
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let result = self.worker.take().expect("mock worker exists").join();
        if !thread::panicking() {
            result.expect("mock server panicked");
        }
    }
}

#[test]
fn attest_json_returns_the_persisted_canonical_node() {
    let fixture = Fixture::native();
    fixture.identity();
    let created = fixture.ok(&["intent", "new", "JSON output contract"]);
    let id = created
        .lines()
        .next()
        .unwrap()
        .strip_prefix("Created intent: ")
        .unwrap();
    let node = fixture.json(&["intent", "attest", id, "--json"]);
    assert!(node["proof"].is_object(), "{node}");
    assert_eq!(node, fixture.json(&["intent", "show", id, "--json"]));
    assert!(fixture
        .ok(&["intent", "attest", id])
        .starts_with("Attested intent:"));

    let created = fixture.json(&[
        "memory",
        "new",
        "--kind",
        "lesson",
        "--text",
        "JSON is one document.",
        "--json",
    ]);
    let id = created["id"].as_str().unwrap();
    let node = fixture.json(&["memory", "attest", id, "--json"]);
    assert!(node["proof"].is_object(), "{node}");
    assert_eq!(node, fixture.json(&["memory", "show", id, "--json"]));
    assert!(fixture
        .ok(&["memory", "attest", id])
        .starts_with("Attested memory:"));
}

#[test]
fn adoption_json_has_no_replacement_notice_on_stdout() {
    let fixture = Fixture::native();
    let project = atomic_core::kg::project_id::ProjectId::generate().to_base32();
    let server = MockServer::new(
        200,
        json!({"bound": true, "identity": {
            "@context": {"atomic": "https://atomic.dev/ns/workflow#", "dcterms": "http://purl.org/dc/terms/"},
            "@type": "atomic:ProjectIdentity", "atomic:projectId": project,
            "atomic:schemaVersion": 1, "dcterms:created": "2026-10-27T00:00:00Z"
        }}),
    );
    fixture.ok(&["remote", "add", "mock", &format!("{}/code", server.url)]);
    for _ in 0..2 {
        let result = fixture.json(&["repo", "identity", "adopt", "--remote", "mock", "--json"]);
        assert_eq!(result["adopted"], project);
    }
}

#[test]
fn git_diff_json_covers_clean_unstaged_and_staged_layers() {
    let fixture = Fixture::empty();
    fixture.git(&["init", "-q", "-b", "main"]);
    fixture.git(&["config", "user.name", "JSON Audit"]);
    fixture.git(&["config", "user.email", "audit@example.invalid"]);
    fixture.git(&["config", "commit.gpgsign", "false"]);
    let file = fixture.repo.path().join("tracked.txt");
    std::fs::write(&file, "before\n").unwrap();
    fixture.git(&["add", "tracked.txt"]);
    fixture.git(&["commit", "-q", "-m", "baseline"]);
    fixture.ok(&["git", "import", "--no-vault"]);
    fixture.ok(&["git", "shim", "reconcile"]);
    let clean = fixture.json(&["diff", "--git", "--json"]);
    assert_eq!(clean["schema_version"], 1);
    assert_eq!(clean["files"], json!([]));
    std::fs::write(&file, "after\n").unwrap();
    let changed = fixture.json(&["diff", "--git", "--json"]);
    assert_eq!(changed["files"].as_array().unwrap().len(), 1);
    fixture.git(&["add", "tracked.txt"]);
    let staged = fixture.json(&["diff", "--git", "--cached", "--json"]);
    assert_eq!(staged["files"].as_array().unwrap().len(), 1);
    assert!(fixture
        .ok(&["diff", "--git", "--cached"])
        .starts_with("# atomic-diff-git v1"));
}

#[test]
fn conflicting_json_modes_fail_before_opening_a_repository() {
    let fixture = Fixture::empty();
    for args in [
        &["status", "--json", "--no-reconcile"][..],
        &["triage", "review", "--json", "--html", "--no-open"][..],
    ] {
        let output = fixture.output(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("conflicting-args"),
            "{output:?}"
        );
        assert!(!fixture.repo.path().join(".atomic").exists());
    }
}

#[test]
fn unsupported_entities_fail_cleanly_only_in_json_mode() {
    let fixture = Fixture::native();
    for prefix in [&["query"][..], &["vault", "query"][..]] {
        let mut args = prefix.to_vec();
        args.extend(["entities", "notes.txt", "--json"]);
        let output = fixture.output(&args);
        assert!(!output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Unsupported file type"),
            "{output:?}"
        );
    }
    assert!(fixture
        .ok(&["query", "entities", "notes.txt"])
        .contains("Unsupported file type"));
}

#[test]
fn recorded_code_search_json_does_not_require_an_ambient_index() {
    let fixture = Fixture::native();
    for prefix in [&["query"][..], &["vault", "query"][..]] {
        let mut args = prefix.to_vec();
        args.extend(["code", "no-match-json-fixture", "--json"]);
        let result = fixture.json(&args);
        assert_eq!(result["matches"], json!([]));
    }
}

#[test]
fn empty_graph_json_creates_and_replaces_the_requested_file() {
    let fixture = Fixture::native();
    let file = fixture.repo.path().join("graph.json");
    for prefix in [&["query"][..], &["vault", "query"][..]] {
        for existing in [false, true] {
            if existing {
                std::fs::write(&file, "stale result").unwrap();
            } else if file.exists() {
                std::fs::remove_file(&file).unwrap();
            }
            let mut args = prefix.to_vec();
            args.extend([
                "graph",
                "no-match-json-fixture",
                "--json",
                "--output",
                file.to_str().unwrap(),
            ]);
            let output = fixture.output(&args);
            assert!(output.status.success(), "{output:?}");
            assert!(output.stdout.is_empty(), "{output:?}");
            let result: Value =
                serde_json::from_slice(&std::fs::read(&file).expect("JSON output file")).unwrap();
            assert_eq!(result, json!({"nodes": [], "edges": []}));
        }
    }
    assert_eq!(
        fixture.json(&["query", "graph", "no-match-json-fixture", "--json"]),
        json!({"nodes": [], "edges": []})
    );
    let output = fixture.output(&[
        "query",
        "graph",
        "no-match-json-fixture",
        "--json",
        "--output",
        fixture.repo.path().to_str().unwrap(),
    ]);
    assert!(
        !output.status.success(),
        "writing JSON to a directory must fail: {output:?}"
    );
    assert!(output.stdout.is_empty(), "{output:?}");
}

#[test]
fn hook_json_selects_one_document_instead_of_the_adapter_reply() {
    let fixture = Fixture::native();
    for verb in ["pre-invocation", "post-tool-use"] {
        for json_output in [true, false] {
            let input = json!({"conversationId": "json-hook-fixture", "workspacePaths": [fixture.repo.path()], "stepIdx": 1});
            let mut args = vec!["agent", "hooks", "agy", verb, "--foreground"];
            if json_output {
                args.push("--json");
            }
            let mut child = fixture
                .command(&args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.to_string().as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{output:?}");
            let node: Value =
                serde_json::from_slice(&output.stdout).expect("one hook result object");
            if json_output {
                assert_eq!(node["session_id"], "json-hook-fixture");
                assert!(node["recorded"].is_boolean());
            } else {
                assert_eq!(node, json!({}), "preserve the adapter reply without --json");
            }
        }
    }
}

#[test]
fn json_agent_creation_enrolls_and_binds_like_human_creation() {
    let fixture = Fixture::native();
    fixture.identity();
    let server = MockServer::new(
        200,
        json!({"success": true, "data": {
            "id": uuid::Uuid::new_v4(), "name": "agent", "did": "did:atomic:test",
            "parentIdentityId": uuid::Uuid::new_v4(), "softwareAgent": null,
            "status": "active", "createdAt": "2026-10-27T00:00:00Z"
        }}),
    );
    fixture.ok(&["server", "add", "mock", &server.url]);
    for (name, json_output) in [("json-bot", true), ("human-bot", false)] {
        let mut args = vec!["identity", "agent", "create", name, "--server", "mock"];
        if json_output {
            args.push("--json");
        }
        let output = fixture.ok(&args);
        if json_output {
            let certificate: Value = serde_json::from_str(&output).expect("certificate JSON only");
            assert!(certificate["proof"].is_object());
        }
        let config =
            std::fs::read_to_string(fixture.home.path().join(".atomic/config.toml")).unwrap();
        assert!(
            config.contains(&format!("agent_identity = \"auditor+{name}\"")),
            "{config}"
        );
    }
    let requests = server.requests.lock().expect("requests lock poisoned");
    assert_eq!(
        requests.len(),
        2,
        "both output modes must enroll: {requests:?}"
    );
    for (request, body) in requests.iter() {
        assert!(request.starts_with("POST /identities/agents "), "{request}");
        assert!(body["certificate"]["proof"].is_object(), "{body}");
    }
}

#[test]
fn json_agent_creation_keeps_enrollment_failures_on_stderr() {
    for status in [404, 500] {
        let fixture = Fixture::native();
        fixture.identity();
        let server = MockServer::new(status, json!({"error": "enrollment unavailable"}));
        fixture.ok(&["server", "add", "mock", &server.url]);
        let output = fixture.output(&[
            "identity", "agent", "create", "bot", "--server", "mock", "--json",
        ]);
        assert!(
            output.status.success(),
            "preserve partial-success semantics: {output:?}"
        );
        let certificate: Value =
            serde_json::from_slice(&output.stdout).expect("certificate JSON only");
        assert!(certificate["proof"].is_object());
        assert!(
            !output.stderr.is_empty(),
            "failed enrollment must be reported: {output:?}"
        );
        assert_eq!(
            server
                .requests
                .lock()
                .expect("requests lock poisoned")
                .len(),
            1
        );
        let config =
            std::fs::read_to_string(fixture.home.path().join(".atomic/config.toml")).unwrap();
        assert!(
            !config.contains("agent_identity ="),
            "failed enrollment must not bind: {config}"
        );
    }
}

#[test]
fn local_json_agent_creation_does_not_enroll_or_bind() {
    let fixture = Fixture::native();
    fixture.identity();
    let server = MockServer::new(500, json!({"error": "must not be contacted"}));
    fixture.ok(&["server", "add", "mock", &server.url]);
    let certificate = fixture.json(&[
        "identity", "agent", "create", "offline", "--server", "mock", "--local", "--json",
    ]);
    assert!(certificate["proof"].is_object());
    assert!(server
        .requests
        .lock()
        .expect("requests lock poisoned")
        .is_empty());
    let config = std::fs::read_to_string(fixture.home.path().join(".atomic/config.toml")).unwrap();
    assert!(
        !config.contains("agent_identity ="),
        "offline creation must not bind: {config}"
    );
}
