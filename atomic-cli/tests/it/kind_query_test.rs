//! User-defined kinds seen through `atomic query`, driven through the binary.
#![cfg(not(windows))]

use std::fs;
use std::io::Write as _;
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

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(ATOMIC_BIN);
        command
            .args(args)
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("ATOMIC_HOME", self.home.path().join(".atomic"))
            .env("ATOMIC_SERVICE", "local")
            .env_remove("ATOMIC_RPC")
            .env("ATOMIC_NONINTERACTIVE", "1")
            .env("NO_COLOR", "1");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run atomic")
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

const RFC_DEFINITION: &str = r#"---
name: rfc
specVersion: 1
title: Request for Comments
fields:
  decidedBy:
    type: ref
    target: [adr]
    edge: decidedBy
---
"#;

const REFERENCES: &str = "https://atomic.dev/ns/workflow#references";
const CONCEPT: &str = "https://atomic.dev/ns/workflow#Concept";

/// The authoring path, start to finish: `kind init`, edit, `add`, `record`, then
/// `kind new`. Nothing here deflates by hand or reindexes.
fn recorded_adr_fixture() -> (Fixture, String, Value, Value) {
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
    let first = new_adr(&f, "Sign changes with Ed25519", &[]);
    let supersedes = format!("supersedes={}", first["uid"].as_str().unwrap());
    let second = new_adr(
        &f,
        "Sign with Ed448",
        &["--set", &supersedes, "--set", "status=accepted"],
    );
    let hub = f.json(&["kind", "show", "adr", "--json"])["kg_id"]
        .as_str()
        .unwrap()
        .to_string();
    (f, hub, first, second)
}

fn iri_of(created: &Value) -> &str {
    created["kg_id"].as_str().unwrap()
}

fn sorted_ids(nodes: &Value) -> Vec<&str> {
    let mut ids: Vec<&str> = nodes
        .as_array()
        .unwrap_or_else(|| panic!("no node list: {nodes}"))
        .iter()
        .map(|node| node["id"].as_str().unwrap())
        .collect();
    ids.sort_unstable();
    ids
}

fn sorted<'a>(ids: &[&'a str]) -> Vec<&'a str> {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids
}

fn node_named<'a>(graph: &'a Value, id: &str) -> &'a Value {
    graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == id)
        .unwrap_or_else(|| panic!("no node {id}: {graph:#}"))
}

fn edge_between<'a>(graph: &'a Value, from: &str, to: &str) -> &'a Value {
    graph["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|edge| edge["from_id"] == from && edge["to_id"] == to)
        .unwrap_or_else(|| panic!("no edge {from} -> {to}: {graph:#}"))
}

#[test]
fn neighbors_show_kind_instances_and_their_relations() {
    let (f, hub, first, second) = recorded_adr_fixture();
    let (first_id, second_id) = (iri_of(&first), iri_of(&second));

    let graph = f.json(&["query", "neighbors", second_id, "--json"]);
    assert_eq!(
        sorted_ids(&graph["nodes"]),
        sorted(&[first_id, second_id, hub.as_str()]),
        "{graph:#}"
    );

    // One class for every kind node; membership is metadata and a real triple.
    for node in graph["nodes"].as_array().unwrap() {
        assert_eq!(node["types"], serde_json::json!([CONCEPT]), "{node:#}");
        assert_eq!(node["source"], "vault", "{node:#}");
    }
    let node = node_named(&graph, second_id);
    assert_eq!(node["label"], "Sign with Ed448");
    assert_eq!(node["summary"], "Sign with Ed448");
    let metadata = &node["metadata"];
    let uid = second["uid"].as_str().unwrap();
    let vault_path = format!("kinds/adr/{uid}.md");
    assert_eq!(metadata["kind_name"], "adr");
    assert_eq!(metadata["uid"], uid);
    assert_eq!(metadata["vault_path"], vault_path.as_str());
    assert_eq!(metadata["derived_from_vault_path"], vault_path.as_str());
    assert_eq!(metadata["field.status"], "accepted");
    // A ref's value is the edge, not a copy of it on the node.
    assert!(metadata.get("field.supersedes").is_none(), "{metadata:#}");
    // The retired node shape.
    for retired in ["kind", "status", "conforms", "violation_codes", "urn"] {
        assert!(metadata.get(retired).is_none(), "{retired}: {metadata:#}");
    }

    let supersedes = edge_between(&graph, second_id, first_id);
    assert_eq!(supersedes["kind"], REFERENCES);
    assert_eq!(
        supersedes["metadata"]["fields"],
        serde_json::json!(["supersedes"])
    );
    assert_eq!(
        supersedes["metadata"]["edges"],
        serde_json::json!(["supersedes"])
    );
    assert_eq!(
        supersedes["metadata"]["derived_from_vault_path"],
        vault_path.as_str()
    );
    let membership = edge_between(&graph, second_id, &hub);
    assert_eq!(membership["kind"], REFERENCES);
    assert_eq!(
        membership["metadata"]["edges"],
        serde_json::json!(["@kind"])
    );

    // The older ADR sees the same relation from the other end.
    let graph = f.json(&["query", "neighbors", first_id, "--json"]);
    assert_eq!(
        sorted_ids(&graph["nodes"]),
        sorted(&[first_id, second_id, hub.as_str()]),
        "{graph:#}"
    );
    assert_eq!(
        edge_between(&graph, second_id, first_id)["metadata"]["edges"],
        serde_json::json!(["supersedes"])
    );
    // A default the writer applied is an ordinary value by now.
    assert_eq!(
        node_named(&graph, first_id)["metadata"]["field.status"],
        "proposed"
    );

    let text = f.ok(&["query", "neighbors", second_id]);
    assert!(
        text.contains(&format!(
            "{second_id} \u{2192}[{REFERENCES} supersedes]\u{2192} {first_id}"
        )),
        "{text}"
    );
}

/// The hub answers "every ADR in the KG", and describes the definition.
#[test]
fn the_hub_lists_every_instance_and_describes_its_definition() {
    let (f, hub, first, second) = recorded_adr_fixture();
    let (first_id, second_id) = (iri_of(&first), iri_of(&second));

    let graph = f.json(&["query", "neighbors", &hub, "--json"]);
    assert_eq!(
        sorted_ids(&graph["nodes"]),
        sorted(&[first_id, second_id, hub.as_str()]),
        "{graph:#}"
    );
    assert_eq!(graph["edges"].as_array().unwrap().len(), 2, "{graph:#}");
    for instance in [first_id, second_id] {
        assert_eq!(
            edge_between(&graph, instance, &hub)["metadata"]["edges"],
            serde_json::json!(["@kind"])
        );
    }

    let node = node_named(&graph, &hub);
    assert_eq!(node["types"], serde_json::json!([CONCEPT]));
    assert_eq!(node["label"], "Architecture Decision Record");
    let metadata = &node["metadata"];
    assert_eq!(metadata["kind_definition"], "adr");
    assert_eq!(metadata["definition_status"], "registered");
    assert_eq!(
        metadata["declared_fields"],
        serde_json::json!(["status", "supersedes"])
    );
    assert_eq!(
        metadata["declared_edges"],
        serde_json::json!(["supersedes"])
    );
    assert_eq!(metadata["embed"], false);
    assert_eq!(metadata["context"], false);
    assert_eq!(metadata["problems"], serde_json::json!([]));
    assert_eq!(metadata["vault_path"], "kinds/adr/_kind.md");
    assert_eq!(metadata["derived_from_vault_path"], "kinds/adr/_kind.md");
    assert!(metadata.get("status").is_none(), "{metadata:#}");
}

#[test]
fn search_and_graph_find_kind_instances_by_title() {
    let (f, hub, first, second) = recorded_adr_fixture();
    let (first_id, second_id) = (iri_of(&first), iri_of(&second));

    let hits = f.json(&["query", "search", "Ed448", "--json"]);
    assert_eq!(sorted_ids(&hits), [second_id], "{hits:#}");

    let hits = f.json(&["query", "search", "Architecture", "--json"]);
    assert_eq!(sorted_ids(&hits), [hub.as_str()], "{hits:#}");

    let graph = f.json(&["query", "graph", "Ed448", "--kinds", CONCEPT, "--json"]);
    assert_eq!(
        sorted_ids(&graph["nodes"]),
        sorted(&[first_id, second_id, hub.as_str()]),
        "{graph:#}"
    );
    let supersedes = edge_between(&graph, second_id, first_id);
    assert_eq!(supersedes["kind"], REFERENCES);
    assert_eq!(
        supersedes["metadata"]["edges"],
        serde_json::json!(["supersedes"])
    );

    // The one custom class: `--kinds adr` cannot name a kind, it names nothing.
    let graph = f.json(&["query", "graph", "Ed448", "--kinds", "adr", "--json"]);
    assert_eq!(graph["nodes"], serde_json::json!([]), "{graph:#}");
}

/// `-t` stops at `atomic:Concept`, which ADRs share with RFCs and with labels, so
/// selecting one kind is the client's filter on `metadata.kind_name`.
#[test]
fn search_separates_kinds_by_kind_name_and_not_by_type() {
    let (f, _hub, first, second) = recorded_adr_fixture();
    let (first_id, second_id) = (iri_of(&first), iri_of(&second));
    f.write_vault("kinds/rfc/_kind.md", RFC_DEFINITION);
    f.ok(&["vault", "sync"]);
    let rfc = f.json(&["kind", "new", "rfc", "--title", "Sign nothing", "--json"]);
    labelled_intent(&f, "sign");

    let hits = f.json(&["query", "search", "sign", "-t", CONCEPT, "--json"]);
    let label_stub = hits
        .as_array()
        .unwrap()
        .iter()
        .find(|hit| hit["label"] == "sign")
        .unwrap_or_else(|| panic!("the label is a Concept as well: {hits:#}"));
    assert!(
        label_stub["metadata"].get("kind_name").is_none(),
        "{hits:#}"
    );

    let ids = |kind: &str| {
        let mut ids: Vec<&str> = hits
            .as_array()
            .unwrap()
            .iter()
            .filter(|hit| hit["metadata"]["kind_name"] == kind)
            .map(|hit| hit["id"].as_str().unwrap())
            .collect();
        ids.sort_unstable();
        ids
    };
    assert_eq!(ids("adr"), sorted(&[first_id, second_id]), "{hits:#}");
    assert_eq!(ids("rfc"), [iri_of(&rfc)], "{hits:#}");
}

/// Status through a plan: the hub's incoming references, filtered on the
/// `field.` key. `filter` takes the binding's bare name and only binds, so the
/// answer here is its step count, and `nodes` keeps every step's output.
#[test]
fn a_plan_filters_a_kinds_instances_on_a_field() {
    let (f, hub, first, second) = recorded_adr_fixture();
    let plan = serde_json::json!({
        "schema_version": 1,
        "steps": [
            {"type": "kg_neighbors", "node_id": hub, "predicate": REFERENCES,
             "direction": "incoming", "bind": "adrs"},
            {"type": "filter", "input": "adrs", "field": "field.status",
             "equals": "accepted", "bind": "accepted"},
            {"type": "filter", "input": "adrs", "field": "kind_name",
             "equals": "adr", "bind": "instances"},
        ],
    });
    let mut child = f
        .command(&["query", "plan", "--json"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(plan.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();

    let counts: Vec<u64> = result["step_stats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["result_count"].as_u64().unwrap())
        .collect();
    assert_eq!(counts, [3, 1, 2], "{result:#}");
    assert_eq!(
        sorted_ids(&result["nodes"]),
        sorted(&[iri_of(&first), iri_of(&second), hub.as_str()]),
        "{result:#}"
    );
}

/// `kind list` answers per view from files; the KG answers repository-wide. They
/// agree here, where one view holds everything.
#[test]
fn kind_list_and_the_hub_agree_on_a_single_view() {
    let (f, hub, first, second) = recorded_adr_fixture();
    let list = f.json(&["kind", "list", "adr", "--json"]);
    let listed = list
        .as_array()
        .or_else(|| list["instances"].as_array())
        .unwrap_or_else(|| panic!("{list:#}"));
    let mut uids: Vec<&str> = listed
        .iter()
        .map(|row| row["uid"].as_str().unwrap())
        .collect();
    uids.sort_unstable();
    assert_eq!(
        uids,
        sorted(&[
            first["uid"].as_str().unwrap(),
            second["uid"].as_str().unwrap()
        ])
    );

    let graph = f.json(&["query", "neighbors", &hub, "--json"]);
    let mut from_hub: Vec<&str> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["metadata"]["uid"].as_str())
        .collect();
    from_hub.sort_unstable();
    assert_eq!(from_hub, uids, "{graph:#}");
    f.assert_validates();
}
