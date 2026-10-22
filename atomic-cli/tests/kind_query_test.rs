//! User-defined kinds seen through `atomic query`, driven through the binary:
//! `kind new` must leave instances, their namespaced edges and the kind
//! definition reachable by `query neighbors`, `query search` and
//! `query graph --kinds` without a manual `query reindex`.
#![cfg(not(windows))]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

const ADR_DEFINITION: &str = r#"---
name: adr
specVersion: 1
title: Architecture Decision Record
fields:
  status:
    type: enum
    values: [proposed, accepted, superseded]
    required: true
    default: proposed
  supersedes:
    type: "ref[]"
    target: [adr]
    edge: SUPERSEDES
body:
  required: true
---
Use an ADR for any decision that constrains future code.
"#;

struct Fixture {
    repo: TempDir,
    home: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Fixture {
            repo: TempDir::new().unwrap(),
            home: TempDir::new().unwrap(),
        };
        fixture.ok(&["init"]);
        fixture
    }

    fn root(&self) -> &Path {
        self.repo.path()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(ATOMIC_BIN)
            .args(args)
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("ATOMIC_HOME", self.home.path().join(".atomic"))
            .env("ATOMIC_SERVICE", "local")
            .env_remove("ATOMIC_RPC")
            .env("ATOMIC_NONINTERACTIVE", "1")
            .env("NO_COLOR", "1")
            .output()
            .expect("run atomic")
    }

    /// Run, assert success, return stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?} failed ({:?}):\n{}\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.ok(args);
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{args:?}: {e}\n{out}"))
    }

    /// `kind new adr ... --json`, returning the new instance's ULID.
    fn new_adr(&self, extra: &[&str]) -> String {
        let mut args = vec!["kind", "new", "adr", "--body-file", "decision.md", "--json"];
        args.extend_from_slice(extra);
        let created = self.json(&args);
        created["uid"].as_str().unwrap().to_string()
    }
}

fn ids(nodes: &Value) -> Vec<&str> {
    let mut ids: Vec<&str> = nodes
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect();
    ids.sort();
    ids
}

/// A recorded `adr` kind with two instances, the second superseding the first.
fn adr_fixture() -> (Fixture, String, String) {
    let f = Fixture::new();
    f.ok(&["kind", "init", "adr"]);
    fs::write(f.root().join(".vault/kinds/adr/_kind.md"), ADR_DEFINITION).unwrap();
    f.ok(&["add", ".vault/kinds/adr/_kind.md"]);
    f.ok(&[
        "record",
        "-m",
        "feat: add adr kind",
        ".vault/kinds/adr/_kind.md",
    ]);
    fs::write(f.root().join("decision.md"), "We sign.").unwrap();
    let first = f.new_adr(&["--title", "Sign changes with Ed25519"]);
    let second = f.new_adr(&[
        "--title",
        "Sign with Ed448",
        "--set",
        &format!("supersedes={first}"),
        "--set",
        "status=accepted",
    ]);
    (f, first, second)
}

#[test]
fn neighbors_show_kind_instances_and_namespaced_edges() {
    let (f, first, second) = adr_fixture();
    let (first_id, second_id) = (format!("adr:{first}"), format!("adr:{second}"));

    for subject in [&first_id, &second_id] {
        let graph = f.json(&["query", "neighbors", subject, "--json"]);
        assert_eq!(
            ids(&graph["nodes"]),
            {
                let mut want = vec![first_id.as_str(), second_id.as_str()];
                want.sort();
                want
            },
            "{graph:#}"
        );
        for node in graph["nodes"].as_array().unwrap() {
            assert_eq!(node["kind"], "adr", "{node:#}");
        }
        let edges = graph["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1, "{graph:#}");
        assert_eq!(edges[0]["from_id"], second_id.as_str());
        assert_eq!(edges[0]["to_id"], first_id.as_str());
        assert_eq!(edges[0]["kind"], "adr.SUPERSEDES");
    }

    let text = f.ok(&["query", "neighbors", &second_id]);
    assert!(
        text.contains(&format!("{second_id} →[adr.SUPERSEDES]→ {first_id}")),
        "{text}"
    );

    let definition = f.json(&["query", "neighbors", "kind:adr", "--json"]);
    let node = &definition["nodes"][0];
    assert_eq!(node["id"], "kind:adr", "{definition:#}");
    assert_eq!(node["kind"], "kind");
    assert_eq!(node["metadata"]["status"], "registered");
    assert_eq!(
        node["metadata"]["edges"],
        serde_json::json!(["adr.SUPERSEDES"])
    );
}

#[test]
fn search_and_graph_find_kind_instances_by_title() {
    let (f, _first, second) = adr_fixture();
    let second_id = format!("adr:{second}");

    let hits = f.json(&["query", "search", "Ed448", "--json"]);
    assert_eq!(ids(&hits), vec![second_id.as_str()], "{hits:#}");

    let filtered = f.json(&["query", "search", "--kind", "adr", "Sign", "--json"]);
    assert_eq!(filtered.as_array().unwrap().len(), 2, "{filtered:#}");

    let definition = f.json(&[
        "query",
        "search",
        "--kind",
        "kind",
        "Architecture",
        "--json",
    ]);
    assert_eq!(ids(&definition), vec!["kind:adr"], "{definition:#}");

    let graph = f.json(&["query", "graph", "--kinds", "adr,kind", "--json", "Ed448"]);
    assert!(
        ids(&graph["nodes"]).contains(&second_id.as_str()),
        "{graph:#}"
    );
    let edges = graph["edges"].as_array().unwrap();
    assert!(
        edges.iter().any(|e| e["kind"] == "adr.SUPERSEDES"),
        "{graph:#}"
    );
}
