//! User-defined kinds seen through `atomic query`, driven through the binary.
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
    default: proposed
    required: true
  supersedes:
    type: "ref[]"
    target: [adr]
    edge: supersedes
---
Use an ADR for any decision that constrains future code.
"#;

/// A field named after every metadata key `atomic_shacl::snapshot_ntriples`
/// reads, and every key vault GC and file search read, each with a value of a
/// type the adapter would reject if it ever saw it.
const ADAPTER_KEYS_DEFINITION: &str = r#"---
name: tricky
specVersion: 1
fields:
  status:
    type: integer
  priority:
    type: "integer[]"
  finding_code:
    type: boolean
  object_availability:
    type: integer
  projection_completeness:
    type: integer
  project_id:
    type: integer
  turn_number:
    type: string
  schema_version:
    type: string
  vault_path:
    type: integer
  path:
    type: string
  derived_from_vault_path:
    type: string
---
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

    fn write_vault(&self, rel: &str, content: &str) {
        let path = self.root().join(".vault").join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// `query validate` exits 0, and it evaluated the shapes.
    fn assert_validates(&self) {
        let report = self.json(&["query", "validate", "--format", "json"]);
        assert_eq!(report["conforms"], true, "{report}");
        assert!(
            report["target_coverage"]
                .as_array()
                .is_some_and(|coverage| !coverage.is_empty()),
            "{report}"
        );
    }
}

/// `kind new adr` with a title, returning the `--json` output.
fn new_adr(f: &Fixture, title: &str, extra: &[&str]) -> Value {
    let mut args = vec!["kind", "new", "adr", "--title", title, "--json"];
    args.extend_from_slice(extra);
    f.json(&args)
}

/// An intent with `labels`, deflated from disk, returning its IRI.
fn labelled_intent(f: &Fixture, labels: &str) -> String {
    const INTENT_UID: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TV";
    f.write_vault(
        "intents/t-a-1/intent.md",
        &format!(
            "---\nuid: {INTENT_UID}\nid: T::a::1\ntitle: Labelled\nkind: feature\n\
             status: backlog\nlabels: [{labels}]\n---\nBody.\n"
        ),
    );
    f.ok(&["vault", "sync"]);
    let repo = atomic_repository::Repository::open_readonly(f.root()).unwrap();
    atomic_core::kg::iri::mint_intent(&repo.project_id().unwrap(), INTENT_UID).unwrap()
}

fn adr_fixture() -> (Fixture, Value, Value) {
    let f = Fixture::new();
    f.write_vault("kinds/adr/_kind.md", ADR_DEFINITION);
    f.ok(&["vault", "sync"]);
    let first = new_adr(&f, "Use redb", &[]);
    let supersedes = format!("supersedes={}", first["uid"].as_str().unwrap());
    let second = new_adr(&f, "Use sqlite", &["--set", &supersedes]);
    (f, first, second)
}

/// `kg_id` is the IRI `query` answers to, in the form the identity tests pin, and
/// oxrdf accepts it through `query validate`.
#[test]
fn kg_id_is_the_iri_query_answers_to() {
    let (f, first, second) = adr_fixture();
    let hub = f.json(&["kind", "show", "adr", "--json"]);
    let project = hub["kg_id"]
        .as_str()
        .and_then(|id| id.strip_suffix(":concept:kind.adr"))
        .unwrap_or_else(|| panic!("{hub}"));
    assert_eq!(
        first["kg_id"],
        format!("{project}:concept:adr.{}", first["uid"].as_str().unwrap())
    );

    let neighbors = f.json(&[
        "query",
        "neighbors",
        second["kg_id"].as_str().unwrap(),
        "--json",
    ]);
    let edges = neighbors["edges"].as_array().unwrap();
    let to = |target: &Value| {
        edges
            .iter()
            .find(|edge| edge["to_id"] == *target)
            .unwrap_or_else(|| panic!("no edge to {target}: {neighbors}"))
    };
    assert_eq!(
        to(&first["kg_id"])["metadata"]["edges"],
        serde_json::json!(["supersedes"])
    );
    assert_eq!(
        to(&hub["kg_id"])["metadata"]["edges"],
        serde_json::json!(["@kind"])
    );

    f.assert_validates();
    // A focused validation parses the IRI on the way in as well as out.
    f.ok(&[
        "query",
        "validate",
        "--node",
        second["kg_id"].as_str().unwrap(),
    ]);
}

/// The label shows from either end, and an edge with no label prints as it did.
#[test]
fn neighbors_text_labels_custom_relations() {
    let (f, first, second) = adr_fixture();
    let references = "https://atomic.dev/ns/workflow#references";
    let (first_id, second_id) = (
        first["kg_id"].as_str().unwrap(),
        second["kg_id"].as_str().unwrap(),
    );

    let text = f.ok(&["query", "neighbors", second_id]);
    assert!(
        text.contains(&format!(
            "{second_id} \u{2192}[{references} supersedes]\u{2192} {first_id}"
        )),
        "{text}"
    );
    assert!(text.contains(&format!("[{references} @kind]")), "{text}");
    let text = f.ok(&["query", "neighbors", first_id]);
    assert!(
        text.contains(&format!(
            "{second_id} \u{2192}[{references} supersedes]\u{2192} {first_id}"
        )),
        "{text}"
    );

    // A built-in edge keeps its old rendering, with no label after the predicate.
    let intent = labelled_intent(&f, "storage");
    let text = f.ok(&["query", "neighbors", &intent]);
    assert!(
        text.contains("\u{2192}[https://atomic.dev/ns/workflow#hasLabel]\u{2192}"),
        "{text}"
    );
}

/// No field value reaches RDF, whatever the field is called: each of these would
/// fail `snapshot_ntriples` under `?` if it landed on the key of the same name.
#[test]
fn query_validate_ignores_fields_named_after_adapter_keys() {
    const UID: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TR";
    let f = Fixture::new();
    f.write_vault("kinds/tricky/_kind.md", ADAPTER_KEYS_DEFINITION);
    f.write_vault(
        &format!("kinds/tricky/{UID}.md"),
        &format!(
            "---\nuid: {UID}\ntitle: \"quote\\\" back\\\\ line\\n bell\\a end\"\nstatus: 3\n\
             priority: [1, 2]\nfinding_code: true\nobject_availability: 1\n\
             projection_completeness: 2\nproject_id: 4\nturn_number: x\nschema_version: y\n\
             vault_path: 5\npath: elsewhere\nderived_from_vault_path: memory/notes.md\n---\n\
             Body.\n"
        ),
    );
    f.ok(&["vault", "sync"]);
    let report = f.json(&["kind", "validate", "--json"]);
    assert_eq!(report["ok"], true, "{report}");
    f.assert_validates();

    let hub = f.json(&["kind", "show", "tricky", "--json"]);
    let hub = hub["kg_id"].as_str().unwrap();
    let id = format!(
        "{}:concept:tricky.{UID}",
        hub.strip_suffix(":concept:kind.tricky").unwrap()
    );
    let neighbors = f.json(&["query", "neighbors", &id, "--json"]);
    let node = neighbors["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == id.as_str())
        .unwrap_or_else(|| panic!("{neighbors}"));
    assert_eq!(node["metadata"]["field.status"], 3);
    assert!(node["metadata"]["status"].is_null(), "{node}");
    assert_eq!(
        node["metadata"]["derived_from_vault_path"],
        format!("kinds/tricky/{UID}.md")
    );
}

/// A repository holding everything that can go wrong with a kind still validates.
#[test]
fn query_validate_passes_over_broken_kinds() {
    const UID: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TR";
    const UID2: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TS";
    const UID3: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TT";
    let f = Fixture::new();
    f.write_vault("kinds/adr/_kind.md", ADR_DEFINITION);
    f.write_vault(
        "kinds/broken/_kind.md",
        "---\nname: broken\nspecVersion: 2\n---\n",
    );
    f.write_vault("kinds/noyaml/_kind.md", "no frontmatter\n");
    f.write_vault(
        "kinds/intent/_kind.md",
        "---\nname: intent\nspecVersion: 1\n---\n",
    );
    f.write_vault(
        &format!("kinds/broken/{UID}.md"),
        &format!("---\nuid: {UID}\n---\nBody.\n"),
    );
    f.write_vault(
        &format!("kinds/ghost/{UID}.md"),
        &format!("---\nuid: {UID}\n---\nBody.\n"),
    );
    f.write_vault(
        &format!("kinds/adr/{UID}.md"),
        &format!("---\nuid: {UID}\nstatus: [unclosed\n---\nBody.\n"),
    );
    f.write_vault(
        &format!("kinds/adr/{UID2}.md"),
        &format!(
            "---\nuid: {UID2}\ntitle: \"\"\nstatus: maybe\nowner: me\n\
             supersedes: [{UID}, {UID3}, nope]\n---\nSee [[missing]].\n"
        ),
    );
    f.write_vault("kinds/adr/notes.md", "---\nname: x\n---\nStray.\n");
    f.ok(&["vault", "sync"]);
    // Labels spelling a hub, a live instance and an inert one.
    labelled_intent(&f, &format!("kind.adr, adr.{UID2}, adr.{UID}"));
    f.assert_validates();

    f.ok(&["query", "reindex", "--full"]);
    f.assert_validates();
}
