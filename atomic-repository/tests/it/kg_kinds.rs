//! User-defined vault kinds and the knowledge graph's closed vocabulary.

use std::collections::{BTreeMap, BTreeSet};

use atomic_canonical::kind_spec::{
    is_reserved_kind_name, is_valid_kind_name, is_valid_ulid, RESERVED_KIND_NAMES,
};
use atomic_core::kg::iri;
use atomic_core::kg::ontology::{Class, Predicate as P};
use atomic_core::kg::project_id::ProjectId;
use atomic_core::pristine::{MutTxnT, VaultEntry, VaultEntryType, VaultMutTxnT};
use atomic_core::types::{Base32, Hash};
use atomic_core::{KgEdge, KgNode};
use atomic_repository::{KindDefinition, KindSource, Repository, WorkflowSnapshot};

/// `ToolResult` from `atomic:ToolResult`.
fn local_name(class: Class) -> &'static str {
    class.curie().strip_prefix("atomic:").unwrap()
}

/// `tool<separator>result` from `ToolResult`.
fn separated(local: &str, separator: char) -> String {
    let mut out = String::new();
    for c in local.chars() {
        if c.is_ascii_uppercase() && !out.is_empty() {
            out.push(separator);
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// `atomic-canonical` cannot depend on `atomic-core`, so this test is what
/// keeps `RESERVED_KIND_NAMES` equal to its rule: the normalized local name of
/// every ontology class, plus five names no class has.
#[test]
fn reserved_kind_names_track_the_ontology() {
    let mut expected: BTreeSet<String> = Class::ALL
        .iter()
        .map(|class| local_name(*class).to_ascii_lowercase())
        .collect();
    assert_eq!(expected.len(), Class::ALL.len());
    expected.extend(
        ["intent", "kind", "kinds", "urn", "did"]
            .into_iter()
            .map(String::from),
    );
    let listed: BTreeSet<String> = RESERVED_KIND_NAMES.iter().map(|s| s.to_string()).collect();
    assert_eq!(listed, expected);

    for class in Class::ALL {
        let local = local_name(*class);
        for spelling in [
            local.to_ascii_lowercase(),
            separated(local, '-'),
            separated(local, '_'),
        ] {
            assert!(is_valid_kind_name(&spelling), "{spelling}");
            assert!(is_reserved_kind_name(&spelling), "{spelling}");
        }
    }
}

/// A SplitMix64 stream, so a failure reproduces with the same cases.
struct Cases(u64);

impl Cases {
    fn next(&mut self, below: usize) -> usize {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) % below as u64) as usize
    }

    fn pick(&mut self, alphabet: &[u8]) -> char {
        alphabet[self.next(alphabet.len())] as char
    }

    fn kind_name(&mut self) -> String {
        const REST: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_-";
        let mut name = String::from(self.pick(&REST[..26]));
        for _ in 0..self.next(63) {
            name.push(self.pick(REST));
        }
        name
    }

    fn ulid(&mut self) -> String {
        const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        let mut ulid = String::from(self.pick(&CROCKFORD[..8]));
        for _ in 0..25 {
            ulid.push(self.pick(CROCKFORD));
        }
        ulid
    }
}

/// The name of every resource parser other than `parse_concept` that accepts
/// `iri_text`.
fn accepted_by_other_parsers(iri_text: &str) -> Vec<&'static str> {
    let checks = [
        ("project", iri::parse_project(iri_text).is_ok()),
        ("change", iri::parse_change(iri_text).is_ok()),
        ("provenance", iri::parse_provenance(iri_text).is_ok()),
        ("attestation", iri::parse_attestation(iri_text).is_ok()),
        (
            "project_identity",
            iri::parse_project_identity(iri_text).is_ok(),
        ),
        ("goal", iri::parse_goal(iri_text).is_ok()),
        ("memory", iri::parse_memory(iri_text).is_ok()),
        ("skill", iri::parse_skill(iri_text).is_ok()),
        ("tool_result", iri::parse_tool_result(iri_text).is_ok()),
        ("view", iri::parse_view(iri_text).is_ok()),
        ("file", iri::parse_file(iri_text).is_ok()),
        ("session", iri::parse_session(iri_text).is_ok()),
        (
            "named_identity",
            iri::parse_named_identity(iri_text).is_ok(),
        ),
        ("identity", iri::parse_identity(iri_text).is_ok()),
        ("intent", iri::parse_intent(iri_text).is_ok()),
        ("child", iri::parse_child(iri_text).is_ok()),
        ("turn", iri::parse_turn(iri_text).is_ok()),
        ("todo", iri::parse_todo(iri_text).is_ok()),
        ("finding", iri::parse_finding(iri_text).is_ok()),
    ];
    checks
        .into_iter()
        .filter_map(|(name, accepted)| accepted.then_some(name))
        .collect()
}

/// Instance and hub names survive the concept IRI, and no other resource
/// parser claims the result. Replaces the retired `<k>:<ULID>` identifier test.
#[test]
fn kind_identity_is_a_concept_iri_and_nothing_else() {
    let project = ProjectId::generate();
    let mut cases = Cases(0x4b49_4e44);
    let mut checked = 0;
    while checked < 2000 {
        let kind = cases.kind_name();
        let uid = cases.ulid();
        assert!(is_valid_kind_name(&kind), "{kind}");
        assert!(is_valid_ulid(&uid), "{uid}");
        if is_reserved_kind_name(&kind) {
            continue;
        }
        checked += 1;

        for name in [format!("{kind}.{uid}"), format!("kind.{kind}")] {
            let minted = iri::mint_concept(&project, &name);
            assert_eq!(
                iri::parse_concept(&minted).unwrap(),
                (project, name.clone()),
                "{minted}"
            );
            let accepted = accepted_by_other_parsers(&minted);
            assert!(accepted.is_empty(), "{minted}: {accepted:?}");
            let (prefix, suffix) = name.split_once('.').unwrap();
            assert!(!suffix.contains('.'), "{name}");
            assert!(prefix == "kind" || prefix == kind, "{name}");
        }
    }
}

// ── Projection ──────────────────────────────────────────────────

const K1: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TR";
const K2: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TS";
const K3: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TT";
const INTENT_UID: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TV";

const ADR_SPEC: &str = "\
---
name: adr
specVersion: 1
title: Architecture Decision Record
fields:
  status:
    type: enum
    values: [proposed, accepted, superseded]
    required: true
  tags:
    type: \"string[]\"
  priority:
    type: integer
  supersedes:
    type: \"ref[]\"
    target: [adr]
    edge: supersedes
  intent:
    type: ref
    target: [intent]
    edge: implements
embed: true
context: true
---
Say what was decided and why.
";

const RFC_SPEC: &str = "\
---
name: rfc
specVersion: 1
fields:
  decidedBy:
    type: ref
    target: [adr]
    edge: decidedBy
  intent:
    type: ref
    target: [intent]
    edge: implements
---
";

fn kind_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.init_vault().unwrap();
    (dir, repo)
}

/// A fresh repository's KG has no VCS facts until its first rebuild, and a vault
/// pass never adds them. Start from that rebuild, so what a later comparison sees
/// is the vault's projection and not the missing project and view nodes.
fn projected_repo() -> (tempfile::TempDir, Repository) {
    let (dir, repo) = kind_repo();
    assert!(repo.rebuild_kg(true).unwrap());
    (dir, repo)
}

fn write_vault(repo: &Repository, rel: &str, content: impl AsRef<[u8]>) {
    let path = repo.vault_dir().join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn remove_vault(repo: &Repository, rel: &str) {
    std::fs::remove_file(repo.vault_dir().join(rel)).unwrap();
}

fn sync(repo: &Repository) -> Vec<String> {
    repo.vault_record_working_copy().unwrap()
}

fn adr(uid: &str, extra: &str) -> String {
    format!(
        "---\nuid: {uid}\ntitle: Use redb storage\nstatus: accepted\n{extra}---\n\
         We chose redb; see src/main.rs.\n"
    )
}

fn rfc(uid: &str, extra: &str) -> String {
    format!("---\nuid: {uid}\ntitle: Storage RFC\n{extra}---\nProposal.\n")
}

fn instance_path(kind: &str, uid: &str) -> String {
    format!("kinds/{kind}/{uid}.md")
}

fn hub_path(kind: &str) -> String {
    format!("kinds/{kind}/_kind.md")
}

/// An intent file labelled with `labels`, written straight to disk so its id
/// is fixed and only `sync` deflates it.
fn labelled_intent(labels: &str) -> String {
    format!(
        "---\nuid: {INTENT_UID}\nid: T::a::1\ntitle: Labelled intent\nkind: feature\n\
         status: backlog\nlabels: [{labels}]\n---\nBody.\n"
    )
}

fn labelled_memory(labels: &str) -> String {
    format!("---\nname: notes\ndescription: Notes\nlabels: [{labels}]\n---\nBody.\n")
}

const INTENT_PATH: &str = "intents/t-a-1/intent.md";
const MEMORY_PATH: &str = "memory/notes.md";

fn concept(repo: &Repository, name: &str) -> String {
    iri::mint_concept(&repo.project_id().unwrap(), name)
}

fn intent_iri(repo: &Repository) -> String {
    iri::mint_intent(&repo.project_id().unwrap(), INTENT_UID).unwrap()
}

fn node(repo: &Repository, id: &str) -> Option<KgNode> {
    repo.vault_kg_node(id).unwrap()
}

fn meta<'a>(node: &'a KgNode, key: &str) -> &'a serde_json::Value {
    &node.metadata.as_ref().unwrap()[key]
}

fn edges_between(repo: &Repository, from: &str, to: &str) -> Vec<KgEdge> {
    repo.workflow_snapshot()
        .unwrap()
        .edges
        .into_iter()
        .filter(|edge| edge.from_id == from && edge.to_id == to)
        .collect()
}

fn references(repo: &Repository, from: &str, to: &str) -> Option<KgEdge> {
    edges_between(repo, from, to)
        .into_iter()
        .find(|edge| edge.kind == P::References)
}

/// The `KindViolation` finding about `subject`, if any.
fn violation(repo: &Repository, subject: &str) -> Option<KgNode> {
    node(
        repo,
        &iri::mint_finding(&repo.project_id().unwrap(), "KindViolation", subject),
    )
}

fn violation_codes(repo: &Repository, subject: &str) -> serde_json::Value {
    meta(
        &violation(repo, subject).expect("a KindViolation finding"),
        "violation_codes",
    )
    .clone()
}

/// What `rebuild_kg` would produce differs from what incremental projection
/// produced: name the rows, since a hash alone says nothing.
fn describe_difference(incremental: &WorkflowSnapshot, rebuilt: &WorkflowSnapshot) -> String {
    let mut out = String::new();
    let nodes = |s: &WorkflowSnapshot| -> BTreeMap<String, String> {
        s.nodes
            .iter()
            .map(|n| (n.id.clone(), serde_json::to_string(n).unwrap()))
            .collect()
    };
    let edges = |s: &WorkflowSnapshot| -> BTreeMap<String, String> {
        s.edges
            .iter()
            .map(|e| {
                (
                    format!("{} {} {}", e.from_id, e.kind, e.to_id),
                    serde_json::to_string(e).unwrap(),
                )
            })
            .collect()
    };
    for (what, left, right) in [
        ("node", nodes(incremental), nodes(rebuilt)),
        ("edge", edges(incremental), edges(rebuilt)),
    ] {
        for key in left.keys().chain(right.keys()).collect::<BTreeSet<_>>() {
            match (left.get(key), right.get(key)) {
                (Some(a), Some(b)) if a != b => {
                    out.push_str(&format!("{what} differs:\n  inc {a}\n  reb {b}\n"))
                }
                (Some(a), None) => out.push_str(&format!("{what} only incremental: {a}\n")),
                (None, Some(b)) => out.push_str(&format!("{what} only rebuilt: {b}\n")),
                _ => {}
            }
        }
    }
    out
}

/// Every edge of a snapshot has both endpoints, the check `rebuild_kg` makes
/// under `?` during writable open.
fn assert_closed(snapshot: &WorkflowSnapshot, stage: &str) {
    let ids: BTreeSet<&str> = snapshot.nodes.iter().map(|n| n.id.as_str()).collect();
    for edge in &snapshot.edges {
        assert!(ids.contains(edge.from_id.as_str()), "{stage}: {edge:?}");
        assert!(
            ids.contains(edge.to_id.as_str()) || edge.to_id.starts_with("https://atomic.dev/ns/"),
            "{stage}: {edge:?}"
        );
    }
}

type Step = (&'static str, fn(&Repository));

/// For every prefix of `steps`, on a fresh repository: incremental projection
/// and a full rebuild agree exactly, and neither leaves an edge dangling.
fn assert_rebuild_agrees_after_every_step(steps: &[Step]) {
    for upto in 1..=steps.len() {
        let (_dir, repo) = projected_repo();
        for (_, step) in &steps[..upto] {
            step(&repo);
        }
        let stage = steps[..upto]
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(" -> ");
        let incremental = repo.workflow_snapshot().unwrap();
        assert_closed(&incremental, &stage);
        assert!(repo.rebuild_kg(true).unwrap(), "{stage}");
        let rebuilt = repo.workflow_snapshot().unwrap();
        assert_eq!(
            incremental.identity,
            rebuilt.identity,
            "after {stage}:\n{}",
            describe_difference(&incremental, &rebuilt)
        );
    }
}

// ── The concept namespace is shared with intent labels ──────────

fn label_hub_on_intent(repo: &Repository) {
    write_vault(repo, INTENT_PATH, labelled_intent("kind.adr"));
    sync(repo);
}
fn label_hub_on_memory(repo: &Repository) {
    write_vault(repo, MEMORY_PATH, labelled_memory("kind.adr"));
    sync(repo);
}
fn label_instance_on_intent(repo: &Repository) {
    write_vault(repo, INTENT_PATH, labelled_intent(&format!("adr.{K1}")));
    sync(repo);
}
fn label_instance_on_memory(repo: &Repository) {
    write_vault(repo, MEMORY_PATH, labelled_memory(&format!("adr.{K1}")));
    sync(repo);
}
fn drop_intent(repo: &Repository) {
    remove_vault(repo, INTENT_PATH);
    sync(repo);
}
fn drop_memory(repo: &Repository) {
    remove_vault(repo, MEMORY_PATH);
    sync(repo);
}
fn add_spec(repo: &Repository) {
    write_vault(repo, &hub_path("adr"), ADR_SPEC);
    sync(repo);
}
fn break_spec(repo: &Repository) {
    write_vault(
        repo,
        &hub_path("adr"),
        ADR_SPEC.replace("specVersion: 1", "specVersion: 2"),
    );
    sync(repo);
}
fn drop_spec(repo: &Repository) {
    remove_vault(repo, &hub_path("adr"));
    sync(repo);
}
fn add_instance(repo: &Repository) {
    write_vault(repo, &instance_path("adr", K1), adr(K1, ""));
    sync(repo);
}
fn corrupt_instance(repo: &Repository) {
    write_vault(
        repo,
        &instance_path("adr", K1),
        format!("---\nuid: {K1}\nstatus: [unclosed\n---\nBody.\n"),
    );
    sync(repo);
}
fn drop_instance(repo: &Repository) {
    remove_vault(repo, &instance_path("adr", K1));
    sync(repo);
}

/// The owned node wins whichever was written first, and a label's owner cannot
/// take the node with it.
#[test]
fn a_label_naming_a_hub_never_displaces_it() {
    for (label, labeller, path) in [
        (
            label_hub_on_intent as fn(&Repository),
            "intent",
            INTENT_PATH,
        ),
        (label_hub_on_memory, "memory", MEMORY_PATH),
    ] {
        for label_first in [true, false] {
            let (_dir, repo) = kind_repo();
            if label_first {
                label(&repo);
                add_spec(&repo);
            } else {
                add_spec(&repo);
                label(&repo);
            }
            let id = concept(&repo, "kind.adr");
            let hub = node(&repo, &id).expect("the hub");
            let stage = format!("{labeller}, label_first={label_first}");
            assert_eq!(hub.label, "Architecture Decision Record", "{stage}");
            assert_eq!(hub.source, "vault", "{stage}");
            assert_eq!(
                meta(&hub, "derived_from_vault_path"),
                &hub_path("adr"),
                "{stage}"
            );
            assert_eq!(meta(&hub, "definition_status"), "registered", "{stage}");

            // Deleting the label's owner leaves the hub alone.
            remove_vault(&repo, path);
            sync(&repo);
            let hub = node(&repo, &id).expect("the hub outlives the label");
            assert_eq!(meta(&hub, "definition_status"), "registered", "{stage}");
        }
    }
}

#[test]
fn a_label_naming_an_instance_never_displaces_it() {
    for (label, labeller, path) in [
        (
            label_instance_on_intent as fn(&Repository),
            "intent",
            INTENT_PATH,
        ),
        (label_instance_on_memory, "memory", MEMORY_PATH),
    ] {
        for label_first in [true, false] {
            let (_dir, repo) = kind_repo();
            if label_first {
                label(&repo);
                add_spec(&repo);
                add_instance(&repo);
            } else {
                add_spec(&repo);
                add_instance(&repo);
                label(&repo);
            }
            let id = concept(&repo, &format!("adr.{K1}"));
            let instance = node(&repo, &id).expect("the instance");
            let stage = format!("{labeller}, label_first={label_first}");
            assert_eq!(instance.label, "Use redb storage", "{stage}");
            assert_eq!(
                meta(&instance, "derived_from_vault_path"),
                &instance_path("adr", K1),
                "{stage}"
            );
            remove_vault(&repo, path);
            sync(&repo);
            assert!(node(&repo, &id).is_some(), "{stage}");
        }
    }
}

#[test]
fn a_label_naming_a_hub_agrees_with_rebuild_in_every_order() {
    assert_rebuild_agrees_after_every_step(&[
        ("label on intent", label_hub_on_intent),
        ("add spec", add_spec),
        ("break spec", break_spec),
        ("restore spec", add_spec),
        ("drop spec", drop_spec),
        ("restore spec", add_spec),
        ("drop label", drop_intent),
    ]);
    assert_rebuild_agrees_after_every_step(&[
        ("add spec", add_spec),
        ("label on memory", label_hub_on_memory),
        ("break spec", break_spec),
        ("drop spec", drop_spec),
        ("restore spec", add_spec),
        ("drop label", drop_memory),
    ]);
}

#[test]
fn a_label_naming_an_instance_agrees_with_rebuild_in_every_order() {
    for label in [
        (
            "label on intent",
            label_instance_on_intent as fn(&Repository),
        ),
        ("label on memory", label_instance_on_memory),
    ] {
        // The label comes first, so the stub predates the row.
        assert_rebuild_agrees_after_every_step(&[
            label,
            ("add instance", add_instance),
            ("add spec", add_spec),
            ("break spec", break_spec),
            ("restore spec", add_spec),
            ("corrupt instance", corrupt_instance),
            ("restore instance", add_instance),
            ("drop spec", drop_spec),
            ("restore spec", add_spec),
            ("drop instance", drop_instance),
        ]);
        // The node comes first.
        assert_rebuild_agrees_after_every_step(&[
            ("add spec", add_spec),
            ("add instance", add_instance),
            label,
            ("break spec", break_spec),
            ("restore spec", add_spec),
            ("drop instance", drop_instance),
            ("restore instance", add_instance),
        ]);
    }
}

/// While the kind node is gone the label is still a label: its edge and a node
/// to point at both survive, as they would after a rebuild.
#[test]
fn a_label_survives_the_node_it_names_going_inert() {
    for label in [
        label_instance_on_intent as fn(&Repository),
        label_instance_on_memory,
    ] {
        let (_dir, repo) = kind_repo();
        add_spec(&repo);
        add_instance(&repo);
        label(&repo);
        let id = concept(&repo, &format!("adr.{K1}"));
        let label_edges = |repo: &Repository| {
            repo.workflow_snapshot()
                .unwrap()
                .edges
                .into_iter()
                .filter(|edge| edge.to_id == id && edge.kind == P::HasLabel)
                .count()
        };
        assert_eq!(label_edges(&repo), 1);

        break_spec(&repo);
        let stub = node(&repo, &id).expect("the label keeps a node");
        assert_eq!(stub.label, format!("adr.{K1}"));
        assert!(meta(&stub, "kind_name").is_null(), "{stub:?}");
        assert_eq!(label_edges(&repo), 1);

        add_spec(&repo);
        assert_eq!(node(&repo, &id).unwrap().label, "Use redb storage");
        assert_eq!(label_edges(&repo), 1);
    }
}

// ── Instances and definitions ───────────────────────────────────

#[test]
fn a_conforming_instance_projects_its_node_and_edges() {
    let (_dir, repo) = kind_repo();
    write_vault(&repo, INTENT_PATH, labelled_intent("storage"));
    write_vault(&repo, &hub_path("adr"), ADR_SPEC);
    write_vault(&repo, &instance_path("adr", K2), adr(K2, ""));
    write_vault(
        &repo,
        &instance_path("adr", K1),
        adr(
            K1,
            &format!(
                "tags: [storage, db]\npriority: 2\nsupersedes: [{K2}]\nintent: {INTENT_UID}\n"
            ),
        ),
    );
    sync(&repo);

    let project = repo.project_id().unwrap();
    let id = concept(&repo, &format!("adr.{K1}"));
    assert_eq!(
        id,
        format!(
            "urn:atomic:project:{}:concept:adr.{K1}",
            project.to_base32()
        )
    );
    let path = instance_path("adr", K1);

    let instance = node(&repo, &id).expect("the instance");
    assert_eq!(instance.types, [Class::Concept.iri()]);
    assert_eq!(instance.label, "Use redb storage");
    assert_eq!(instance.summary.as_deref(), Some("Use redb storage"));
    assert_eq!(instance.source, "vault");
    assert_eq!(meta(&instance, "kind_name"), "adr");
    assert_eq!(meta(&instance, "uid"), K1);
    assert_eq!(meta(&instance, "vault_path"), &path);
    assert_eq!(meta(&instance, "derived_from_vault_path"), &path);
    assert_eq!(meta(&instance, "field.status"), "accepted");
    assert_eq!(meta(&instance, "field.priority"), 2);
    assert_eq!(
        meta(&instance, "field.tags"),
        &serde_json::json!(["storage", "db"])
    );
    // Refs become edges, and verdicts become findings.
    for gone in [
        "field.supersedes",
        "field.intent",
        "conforms",
        "violation_codes",
        "kind",
        "status",
    ] {
        assert!(meta(&instance, gone).is_null(), "{gone}");
    }
    assert!(violation(&repo, &id).is_none());

    let hub = concept(&repo, "kind.adr");
    let kind_edge = references(&repo, &id, &hub).expect("the @kind edge");
    assert_eq!(
        kind_edge.metadata.as_ref().unwrap()["edges"],
        serde_json::json!(["@kind"])
    );
    let supersedes = references(&repo, &id, &concept(&repo, &format!("adr.{K2}"))).unwrap();
    let metadata = supersedes.metadata.as_ref().unwrap();
    assert_eq!(metadata["fields"], serde_json::json!(["supersedes"]));
    assert_eq!(metadata["edges"], serde_json::json!(["supersedes"]));
    assert_eq!(metadata["derived_from_vault_path"], path.as_str());
    let implements = references(&repo, &id, &intent_iri(&repo)).unwrap();
    assert_eq!(
        implements.metadata.as_ref().unwrap()["edges"],
        serde_json::json!(["implements"])
    );
    assert!(node(&repo, &intent_iri(&repo)).is_some());
    // A File stub has no owner, so a file mention could outlive its row.
    assert!(node(&repo, &iri::mint_file(&project, "src/main.rs")).is_none());

    let hub = node(&repo, &hub).expect("the hub");
    assert_eq!(hub.types, [Class::Concept.iri()]);
    assert_eq!(hub.label, "Architecture Decision Record");
    assert_eq!(meta(&hub, "kind_definition"), "adr");
    assert_eq!(meta(&hub, "definition_status"), "registered");
    assert_eq!(
        meta(&hub, "declared_fields"),
        &serde_json::json!(["intent", "priority", "status", "supersedes", "tags"])
    );
    assert_eq!(
        meta(&hub, "declared_edges"),
        &serde_json::json!(["implements", "supersedes"])
    );
    assert_eq!(meta(&hub, "embed"), true);
    assert_eq!(meta(&hub, "context"), true);
    assert_eq!(meta(&hub, "problems"), &serde_json::json!([]));
    assert_eq!(meta(&hub, "vault_path"), &hub_path("adr"));
    assert!(meta(&hub, "status").is_null(), "the adapter reads `status`");

    let found = repo
        .vault_kg_search_by_kind("redb", 10, Class::Concept)
        .unwrap();
    let found: BTreeSet<&str> = found.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(
        found,
        BTreeSet::from([id.as_str(), concept(&repo, &format!("adr.{K2}")).as_str()])
    );
}

/// Edge identity is `(from, to, predicate)`, so two relations on one target are
/// one edge that names both, and the result does not depend on write order.
#[test]
fn relations_to_one_target_merge_into_one_edge() {
    const PLAN: &str = "\
---
name: plan
specVersion: 1
fields:
  tracks:
    type: ref
    target: [intent]
    edge: tracks
  implementsIntent:
    type: ref
    target: [intent]
    edge: implements
  mentioned:
    type: ref
    target: [intent]
  notes:
    type: ref
    target: [memory]
    edge: cites
---
";
    let (_dir, repo) = kind_repo();
    write_vault(&repo, INTENT_PATH, labelled_intent("x"));
    write_vault(&repo, MEMORY_PATH, labelled_memory("y"));
    write_vault(&repo, &hub_path("plan"), PLAN);
    write_vault(
        &repo,
        &instance_path("plan", K1),
        format!(
            "---\nuid: {K1}\ntracks: {INTENT_UID}\nimplementsIntent: {INTENT_UID}\n\
             mentioned: {INTENT_UID}\nnotes: notes\n---\nSee [[notes]].\n"
        ),
    );
    sync(&repo);
    let id = concept(&repo, &format!("plan.{K1}"));
    assert!(violation(&repo, &id).is_none());

    let intent = edges_between(&repo, &id, &intent_iri(&repo));
    assert_eq!(intent.len(), 1, "{intent:?}");
    let metadata = intent[0].metadata.as_ref().unwrap();
    // `mentioned` declares no edge: it is existence-checked and nothing more.
    assert_eq!(
        metadata["fields"],
        serde_json::json!(["implementsIntent", "tracks"])
    );
    assert_eq!(
        metadata["edges"],
        serde_json::json!(["implements", "tracks"])
    );

    let memory = iri::mint_memory(&repo.project_id().unwrap(), "notes");
    let memory = edges_between(&repo, &id, &memory);
    assert_eq!(memory.len(), 1, "{memory:?}");
    let metadata = memory[0].metadata.as_ref().unwrap();
    assert_eq!(metadata["fields"], serde_json::json!(["notes"]));
    assert_eq!(metadata["edges"], serde_json::json!(["@body", "cites"]));
    assert_eq!(
        metadata["derived_from_vault_path"],
        instance_path("plan", K1).as_str()
    );
}

#[test]
fn a_nonconforming_instance_stays_visible_with_a_finding() {
    let (_dir, repo) = kind_repo();
    write_vault(&repo, &hub_path("adr"), ADR_SPEC);
    write_vault(
        &repo,
        &instance_path("adr", K1),
        format!("---\nuid: {K1}\nstatus: maybe\nowner: me\n---\nbody\n"),
    );
    sync(&repo);

    let id = concept(&repo, &format!("adr.{K1}"));
    let instance = node(&repo, &id).expect("the instance");
    assert_eq!(instance.label, K1, "no title: the ULID labels the node");
    assert_eq!(instance.summary, None);
    assert_eq!(meta(&instance, "field.status"), "maybe");
    assert!(meta(&instance, "field.owner").is_null(), "undeclared");
    assert!(meta(&instance, "conforms").is_null());

    let finding = violation(&repo, &id).expect("the finding");
    assert_eq!(finding.types, [Class::Finding.iri()]);
    assert_eq!(meta(&finding, "finding_code"), "KindViolation");
    assert_eq!(
        meta(&finding, "derived_from_vault_path"),
        &instance_path("adr", K1)
    );
    assert_eq!(
        meta(&finding, "violation_codes"),
        &serde_json::json!(["NOT_IN_ENUM", "UNDECLARED_FIELD"])
    );
    assert!(edges_between(&repo, &finding.id, &id)
        .iter()
        .any(|edge| edge.kind == P::AboutResource));
}

#[test]
fn an_instance_without_a_registered_spec_is_inert() {
    let (_dir, repo) = kind_repo();
    let id = |repo: &Repository| concept(repo, &format!("adr.{K1}"));
    write_vault(&repo, &instance_path("adr", K1), adr(K1, ""));
    sync(&repo);
    assert!(node(&repo, &id(&repo)).is_none(), "no spec: inert");
    assert!(node(&repo, &concept(&repo, "kind.adr")).is_none());

    break_spec(&repo);
    assert!(node(&repo, &id(&repo)).is_none(), "invalid spec: inert");
    let hub = concept(&repo, "kind.adr");
    let definition = node(&repo, &hub).expect("the hub");
    assert_eq!(definition.label, "adr");
    assert_eq!(meta(&definition, "definition_status"), "invalid");
    assert!(!meta(&definition, "problems").as_array().unwrap().is_empty());
    assert!(meta(&definition, "declared_fields").is_null());
    let invalid = iri::mint_finding(&repo.project_id().unwrap(), "KindDefinitionInvalid", &hub);
    assert!(node(&repo, &invalid).is_some());

    // Fixing the spec projects the instance without touching it.
    let updated = {
        write_vault(&repo, &hub_path("adr"), ADR_SPEC);
        sync(&repo)
    };
    assert_eq!(updated, [hub_path("adr")]);
    assert!(node(&repo, &id(&repo)).is_some());
    assert!(node(&repo, &invalid).is_none());
}

#[test]
fn a_malformed_instance_is_inert() {
    let (_dir, repo) = kind_repo();
    write_vault(&repo, &hub_path("adr"), ADR_SPEC);
    let marker = "<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\n";
    for (uid, content) in [
        (
            K1,
            format!("---\nuid: {K1}\nstatus: accepted\n---\n{marker}"),
        ),
        (
            K2,
            format!("---\nuid: {K2}\nstatus: [unclosed\n---\nbody\n"),
        ),
        (
            K3,
            "---\ntitle: No uid\nstatus: accepted\n---\nbody\n".to_string(),
        ),
    ] {
        write_vault(&repo, &instance_path("adr", uid), content);
    }
    sync(&repo);
    for uid in [K1, K2, K3] {
        assert!(
            node(&repo, &concept(&repo, &format!("adr.{uid}"))).is_none(),
            "{uid}"
        );
    }
    // The gate is what reports them.
    let report = repo
        .kind_validate(&KindSource::WorkingCopy, Some("adr"))
        .unwrap();
    let codes: BTreeSet<&str> = report.problems.iter().map(|p| p.code.as_str()).collect();
    assert_eq!(
        codes,
        BTreeSet::from(["CONFLICT_MARKER", "FRONTMATTER_INVALID"])
    );
    assert_eq!(
        report.kinds[0].instances[2].violations[0].code.as_str(),
        "MISSING_UID",
        "{report:?}"
    );
}

#[test]
fn a_spec_edit_alone_flips_the_finding() {
    let (_dir, repo) = kind_repo();
    write_vault(&repo, &hub_path("adr"), ADR_SPEC);
    write_vault(&repo, &instance_path("adr", K1), adr(K1, ""));
    sync(&repo);
    let id = concept(&repo, &format!("adr.{K1}"));
    assert!(violation(&repo, &id).is_none());

    // Drop `accepted` from the enum; only the spec file changes.
    write_vault(
        &repo,
        &hub_path("adr"),
        ADR_SPEC.replace("[proposed, accepted, superseded]", "[proposed, superseded]"),
    );
    assert_eq!(sync(&repo), [hub_path("adr")]);
    assert_eq!(
        violation_codes(&repo, &id),
        serde_json::json!(["NOT_IN_ENUM"])
    );

    write_vault(&repo, &hub_path("adr"), ADR_SPEC);
    assert_eq!(sync(&repo), [hub_path("adr")]);
    assert!(violation(&repo, &id).is_none());
}

#[test]
fn deleting_a_spec_removes_its_instance_nodes_and_keeps_the_rows() {
    let (_dir, repo) = kind_repo();
    write_vault(&repo, &hub_path("adr"), ADR_SPEC);
    write_vault(&repo, &instance_path("adr", K2), adr(K2, ""));
    write_vault(
        &repo,
        &instance_path("adr", K1),
        adr(K1, &format!("supersedes: [{K2}]\n")),
    );
    sync(&repo);
    let id = concept(&repo, &format!("adr.{K1}"));
    assert!(references(&repo, &id, &concept(&repo, "kind.adr")).is_some());

    drop_spec(&repo);
    assert!(node(&repo, &concept(&repo, "kind.adr")).is_none());
    assert!(node(&repo, &id).is_none());
    assert!(node(&repo, &concept(&repo, &format!("adr.{K2}"))).is_none());
    assert!(repo
        .workflow_snapshot()
        .unwrap()
        .edges
        .iter()
        .all(|edge| edge.from_id != id && edge.to_id != id));
    assert!(repo
        .vault_retrieve(&instance_path("adr", K1))
        .unwrap()
        .is_some());
}

#[test]
fn deleting_an_instance_removes_its_node_and_leaves_its_neighbours() {
    let (_dir, repo) = kind_repo();
    write_vault(&repo, &hub_path("adr"), ADR_SPEC);
    write_vault(&repo, &instance_path("adr", K2), adr(K2, ""));
    write_vault(
        &repo,
        &instance_path("adr", K1),
        adr(K1, &format!("supersedes: [{K2}]\n")),
    );
    sync(&repo);
    let id = concept(&repo, &format!("adr.{K1}"));
    assert!(node(&repo, &id).is_some());

    drop_instance(&repo);
    assert!(node(&repo, &id).is_none());
    assert!(repo
        .workflow_snapshot()
        .unwrap()
        .edges
        .iter()
        .all(|edge| edge.from_id != id));
    assert!(node(&repo, &concept(&repo, &format!("adr.{K2}"))).is_some());
    assert!(node(&repo, &concept(&repo, "kind.adr")).is_some());
}

/// Every store re-resolves the whole vault, so a referrer converges with no
/// kind-specific hook when its target's spec goes and comes back.
#[test]
fn a_cross_kind_edge_survives_the_target_spec_round_trip() {
    let (_dir, repo) = kind_repo();
    write_vault(&repo, &hub_path("adr"), ADR_SPEC);
    write_vault(&repo, &instance_path("adr", K1), adr(K1, ""));
    write_vault(&repo, &hub_path("rfc"), RFC_SPEC);
    write_vault(
        &repo,
        &instance_path("rfc", K3),
        rfc(K3, &format!("decidedBy: {K1}\n")),
    );
    sync(&repo);
    let rfc_id = concept(&repo, &format!("rfc.{K3}"));
    let adr_id = concept(&repo, &format!("adr.{K1}"));
    assert!(references(&repo, &rfc_id, &adr_id).is_some());
    assert!(violation(&repo, &rfc_id).is_none());

    write_vault(
        &repo,
        &hub_path("adr"),
        ADR_SPEC.replace("specVersion: 1", "specVersion: 2"),
    );
    assert_eq!(sync(&repo), [hub_path("adr")]);
    assert!(node(&repo, &adr_id).is_none());
    assert!(references(&repo, &rfc_id, &adr_id).is_none());
    assert_eq!(
        violation_codes(&repo, &rfc_id),
        serde_json::json!(["UNRESOLVED_REF"])
    );

    write_vault(&repo, &hub_path("adr"), ADR_SPEC);
    assert_eq!(sync(&repo), [hub_path("adr")]);
    assert!(references(&repo, &rfc_id, &adr_id).is_some());
    assert!(violation(&repo, &rfc_id).is_none());
}

#[test]
fn a_deleted_target_makes_the_referrer_nonconforming_until_restored() {
    let (_dir, repo) = kind_repo();
    write_vault(&repo, &hub_path("adr"), ADR_SPEC);
    write_vault(&repo, &instance_path("adr", K1), adr(K1, ""));
    write_vault(&repo, &hub_path("rfc"), RFC_SPEC);
    write_vault(
        &repo,
        &instance_path("rfc", K3),
        rfc(K3, &format!("decidedBy: {K1}\n")),
    );
    sync(&repo);
    let rfc_id = concept(&repo, &format!("rfc.{K3}"));
    assert!(violation(&repo, &rfc_id).is_none());

    drop_instance(&repo);
    assert_eq!(
        violation_codes(&repo, &rfc_id),
        serde_json::json!(["UNRESOLVED_REF"])
    );
    let report = repo
        .kind_validate(&KindSource::WorkingCopy, Some("rfc"))
        .unwrap();
    assert!(!report.kinds[0].instances[0].conforms, "the gate agrees");

    write_vault(&repo, &instance_path("adr", K1), adr(K1, ""));
    sync(&repo);
    assert!(violation(&repo, &rfc_id).is_none());
    assert!(references(&repo, &rfc_id, &concept(&repo, &format!("adr.{K1}"))).is_some());
}

#[test]
fn an_intent_arriving_later_resolves_the_referrer() {
    let (_dir, repo) = kind_repo();
    write_vault(&repo, &hub_path("rfc"), RFC_SPEC);
    write_vault(
        &repo,
        &instance_path("rfc", K3),
        rfc(K3, &format!("intent: {INTENT_UID}\n")),
    );
    sync(&repo);
    let rfc_id = concept(&repo, &format!("rfc.{K3}"));
    assert_eq!(
        violation_codes(&repo, &rfc_id),
        serde_json::json!(["UNRESOLVED_REF"])
    );

    // The intent arrives in a batch that does not touch the instance.
    write_vault(&repo, INTENT_PATH, labelled_intent("x"));
    assert_eq!(sync(&repo), [INTENT_PATH]);
    assert!(violation(&repo, &rfc_id).is_none());
    assert!(references(&repo, &rfc_id, &intent_iri(&repo)).is_some());

    // And leaves again.
    drop_intent(&repo);
    assert_eq!(
        violation_codes(&repo, &rfc_id),
        serde_json::json!(["UNRESOLVED_REF"])
    );
    assert!(references(&repo, &rfc_id, &intent_iri(&repo)).is_none());
}

/// The gate reads intents strictly, projection through the case-folding aliases
/// every vault link uses, so the gate can reject what the KG accepts and never
/// the reverse.
#[test]
fn the_gate_is_stricter_than_projection_about_intent_refs() {
    let (_dir, repo) = kind_repo();
    write_vault(&repo, INTENT_PATH, labelled_intent("x"));
    write_vault(&repo, &hub_path("rfc"), RFC_SPEC);
    // The stored human key is `T::a::1`.
    write_vault(
        &repo,
        &instance_path("rfc", K3),
        rfc(K3, "intent: \"T::A::1\"\n"),
    );
    sync(&repo);

    let report = repo
        .kind_validate(&KindSource::WorkingCopy, Some("rfc"))
        .unwrap();
    assert_eq!(
        report.kinds[0].instances[0].violations[0].code.as_str(),
        "UNRESOLVED_REF",
        "{report:?}"
    );
    let rfc_id = concept(&repo, &format!("rfc.{K3}"));
    assert!(violation(&repo, &rfc_id).is_none());
    assert!(references(&repo, &rfc_id, &intent_iri(&repo)).is_some());
}

#[test]
fn projection_reads_the_stored_definition() {
    let (_dir, repo) = kind_repo();
    write_vault(&repo, &hub_path("adr"), ADR_SPEC);
    let path = instance_path("adr", K1);
    write_vault(&repo, &path, adr(K1, ""));
    sync(&repo);
    let id = concept(&repo, &format!("adr.{K1}"));
    assert!(violation(&repo, &id).is_none());

    // An unrecorded edit to the working-copy definition is not what the stored
    // instance was deflated against: projection ignores it, the gate sees it.
    write_vault(
        &repo,
        &hub_path("adr"),
        ADR_SPEC.replace("[proposed, accepted, superseded]", "[proposed, superseded]"),
    );
    repo.vault_index_kg(&path).unwrap();
    repo.vault_reindex_kg().unwrap();
    assert!(violation(&repo, &id).is_none());
    assert_eq!(
        meta(
            &node(&repo, &concept(&repo, "kind.adr")).unwrap(),
            "declared_fields"
        ),
        &serde_json::json!(["intent", "priority", "status", "supersedes", "tags"])
    );
    let report = repo.kind_validate(&KindSource::WorkingCopy, None).unwrap();
    assert!(!report.kinds[0].instances[0].conforms);

    // A case variant of `_kind.md` is a stray file, and never a spec.
    drop_spec(&repo);
    assert!(node(&repo, &id).is_none());
    repo.vault_store(
        "kinds/adr/_KIND.md",
        VaultEntryType::Scratch,
        Vec::new(),
        serde_json::json!({"name": "adr", "specVersion": 1}).to_string(),
    )
    .unwrap();
    repo.vault_index_kg(&path).unwrap();
    assert!(node(&repo, &id).is_none());
    assert!(node(&repo, &concept(&repo, "kind.adr")).is_none());
}

/// A sandbox submit deflates a view's files, which need not be on disk at all.
#[test]
fn files_not_on_disk_project_like_any_others() {
    let (_dir, repo) = kind_repo();
    let inst = instance_path("rfc", K3);
    repo.vault_record_files(&[
        (hub_path("adr"), Some(ADR_SPEC.into())),
        (instance_path("adr", K1), Some(adr(K1, ""))),
        (hub_path("rfc"), Some(RFC_SPEC.into())),
        (inst.clone(), Some(rfc(K3, &format!("decidedBy: {K1}\n")))),
    ])
    .unwrap();
    assert!(!repo.vault_dir().join(&inst).exists());
    let rfc_id = concept(&repo, &format!("rfc.{K3}"));
    assert!(violation(&repo, &rfc_id).is_none());
    assert!(references(&repo, &rfc_id, &concept(&repo, &format!("adr.{K1}"))).is_some());
}

#[test]
fn a_stored_definition_fails_closed() {
    let entry = |fm: serde_json::Value, body: &str| {
        VaultEntry::new(
            VaultEntryType::Scratch,
            body.as_bytes().to_vec(),
            fm.to_string(),
            String::new(),
        )
    };
    let ok = serde_json::json!({"name": "adr", "specVersion": 1});
    assert!(
        KindDefinition::from_stored("adr", &entry(ok.clone(), "guide\n"))
            .spec()
            .is_some()
    );
    let marker = "<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\n";
    for (fm, body) in [
        (serde_json::json!({}), "guide\n"),
        (ok.clone(), marker),
        (
            serde_json::json!({"name": "adr", "specVersion": 1, "title": marker}),
            "",
        ),
    ] {
        let definition = KindDefinition::from_stored("adr", &entry(fm.clone(), body));
        assert!(definition.spec().is_none(), "{fm} / {body:?}");
    }
}

// ── Totality ────────────────────────────────────────────────────

/// A distinct valid ULID per `n`.
fn uid(n: u32) -> String {
    format!("01JD8ZK3M6W2Q4V7X9B5N1C{n:03}")
}

/// A vault holding everything a recorded file can throw at projection. All of
/// it reaches the repository through ordinary deflate, so any of it can arrive
/// in a pull. None of it may stop a later writable open.
fn write_hostile_kinds(repo: &Repository) {
    let marker = "<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\n";
    write_vault(repo, &hub_path("adr"), ADR_SPEC);
    write_vault(repo, &hub_path("rfc"), RFC_SPEC);
    write_vault(
        repo,
        INTENT_PATH,
        labelled_intent(&format!("kind.adr, adr.{}, ghost.{}", uid(1), uid(1))),
    );
    write_vault(
        repo,
        MEMORY_PATH,
        labelled_memory(&format!("kind.broken, adr.{K1}")),
    );

    // Definitions that do not register.
    for (kind, content) in [
        (
            "broken",
            "---\nname: broken\nspecVersion: 2\n---\n".to_string(),
        ),
        ("noyaml", "no frontmatter at all\n".to_string()),
        ("badyaml", "---\nname: [unclosed\n---\n".to_string()),
        (
            "merged",
            format!("---\nname: merged\nspecVersion: 1\n---\n{marker}"),
        ),
        (
            "wrongname",
            "---\nname: other\nspecVersion: 1\n---\n".to_string(),
        ),
        ("listfm", "---\n- a\n- b\n---\n".to_string()),
        (
            "badfield",
            "---\nname: badfield\nspecVersion: 1\nfields:\n  a.b:\n    type: nope\n---\n"
                .to_string(),
        ),
        (
            "intent",
            "---\nname: intent\nspecVersion: 1\n---\n".to_string(),
        ),
        ("kind", "---\nname: kind\nspecVersion: 1\n---\n".to_string()),
    ] {
        write_vault(repo, &hub_path(kind), content);
    }

    // Instances of kinds that are not registered, and of a name with no spec.
    for kind in ["broken", "intent", "kind", "ghost"] {
        write_vault(repo, &instance_path(kind, &uid(1)), adr(&uid(1), ""));
    }

    // Malformed rows of a registered kind.
    for (n, content) in [
        (
            1,
            format!("---\nuid: {}\nstatus: [unclosed\n---\nbody\n", uid(1)),
        ),
        (2, "no frontmatter\n".to_string()),
        (3, "---\n- a\n- b\n---\nbody\n".to_string()),
        (4, "---\njust text\n---\nbody\n".to_string()),
        (5, "---\n---\nbody\n".to_string()),
        (
            6,
            format!("---\nuid: {}\nstatus: accepted\n---\n{marker}", uid(6)),
        ),
        (
            7,
            format!(
                "---\nuid: {}\ntags:\n  - |\n    <<<<<<< ours\n---\nbody\n",
                uid(7)
            ),
        ),
    ] {
        write_vault(repo, &instance_path("adr", &uid(n)), content);
    }

    // Rows that project but violate their spec in every way a value can.
    for (n, extra) in [
        (20, "uid: 7\n".to_string()),
        (21, format!("uid: {}\n", uid(99))),
        (22, "title: [a, b]\n".to_string()),
        (23, "title: {a: b}\n".to_string()),
        (24, "title: \"   \"\n".to_string()),
        (
            25,
            "title: \"quote\\\" back\\\\ line\\n bell\\a nul\\0 end\"\n".to_string(),
        ),
        (26, "status: {a: 1}\n".to_string()),
        (27, "tags: [{a: 1}, [2], null]\n".to_string()),
        (28, "priority: 99999999999999999999999\n".to_string()),
        (29, "priority: 1.5\n".to_string()),
        (30, "status: null\npriority: null\ntags: null\n".to_string()),
        (
            31,
            "owner: x\nkind: 3\nstatus_: 4\nderived_from_vault_path: y\nvault_path: z\n"
                .to_string(),
        ),
        // Refs that cannot resolve.
        (40, format!("supersedes: [{}]\n", uid(98))),
        (
            41,
            "supersedes: [not-a-ulid, \"\", 7, null, {a: 1}]\n".to_string(),
        ),
        (42, "intent: nope\n".to_string()),
        (43, "supersedes: 3\nintent: [a, b]\n".to_string()),
        // Refs to rows that exist and are inert.
        (44, format!("supersedes: [{}, {}]\n", uid(1), uid(6))),
        (
            45,
            format!(
                "supersedes: [\"adr:{}\", \"ghost:{}\", \"broken:{}\"]\n",
                uid(1),
                uid(1),
                uid(1)
            ),
        ),
        (
            46,
            format!(
                "supersedes: [\"intent:{INTENT_UID}\", \"memory:notes\", \"urn:atomic:adr:{}\"]\n",
                uid(1)
            ),
        ),
        // Refs that do resolve, in a cycle and to itself.
        (47, format!("supersedes: [{}, {}]\n", uid(47), uid(48))),
        (
            48,
            format!("supersedes: [{}]\nintent: {INTENT_UID}\n", uid(47)),
        ),
    ] {
        let mut content = format!("---\nuid: {}\nstatus: accepted\n{extra}---\nbody\n", uid(n));
        // A key given twice would make the whole block fail strict parsing.
        for key in ["uid", "status", "title"] {
            if extra.starts_with(&format!("{key}:")) {
                content = content.replacen(&format!("{key}: "), &format!("{key}_kept: "), 1);
            }
        }
        write_vault(repo, &instance_path("adr", &uid(n)), content);
    }

    // Bodies.
    let long = "x".repeat(300);
    for (n, body) in [
        (60, "[[".to_string()),
        (61, "[[]] [[a]] [[a]] [[../../x]] [[ ]] ]]".to_string()),
        (
            62,
            format!("src/{long}.rs ./a/../b.rs /etc/passwd.md `x/y.json`, <a/b.go>"),
        ),
        (63, format!("[[{long}]] \u{1F980}/\u{0000}.rs")),
    ] {
        write_vault(
            repo,
            &instance_path("adr", &uid(n)),
            format!("---\nuid: {}\nstatus: accepted\n---\n{body}\n", uid(n)),
        );
    }

    // Files that are neither a definition nor an instance.
    for rel in [
        "kinds/README.md".to_string(),
        "kinds/adr/notes.md".to_string(),
        "kinds/adr/sub/x.md".to_string(),
        "kinds/BAD NAME/_kind.md".to_string(),
        format!("kinds/adr/{}.md", uid(70).to_lowercase()),
    ] {
        write_vault(repo, &rel, "---\nname: x\n---\nstray\n");
    }
}

/// Rows deflate cannot produce: non-UTF-8 bytes (a vault scan refuses the file)
/// and frontmatter JSON that is not a map or not JSON. Stored last, because a
/// scan deletes a row with no file behind it.
fn store_hostile_rows(repo: &Repository) {
    let scratch = VaultEntryType::Scratch;
    for (n, frontmatter) in [
        (80, "{not json"),
        (81, "[1,2]"),
        (82, "null"),
        (83, "\"text\""),
        (84, "3"),
        (85, ""),
    ] {
        repo.vault_store(
            &instance_path("adr", &uid(n)),
            scratch,
            b"body".to_vec(),
            frontmatter.to_string(),
        )
        .unwrap();
        repo.vault_store(
            &hub_path(&format!("row{n}")),
            scratch,
            b"body".to_vec(),
            frontmatter.to_string(),
        )
        .unwrap();
    }
    repo.vault_store(
        &instance_path("adr", &uid(86)),
        scratch,
        vec![0xff, 0xfe, 0x00, b'\n'],
        serde_json::json!({"uid": uid(86), "status": "accepted"}).to_string(),
    )
    .unwrap();
    repo.vault_store(
        &hub_path("rowbinary"),
        scratch,
        vec![0xff, 0xfe, 0x00, b'\n'],
        serde_json::json!({"name": "rowbinary", "specVersion": 1}).to_string(),
    )
    .unwrap();
    // A row under `kinds/` that was not stored as Scratch keeps its own arm.
    repo.vault_store(
        &hub_path("typed"),
        VaultEntryType::Memory,
        b"body".to_vec(),
        "{}".to_string(),
    )
    .unwrap();
}

/// Force the writable open to run the rebuild, as a generation bump would.
fn expire_kg_generation(root: &std::path::Path) {
    use atomic_core::pristine::tables::KG_INDEX_META;
    let db = redb::Database::open(root.join(".atomic/atomic.redb")).unwrap();
    let txn = db.begin_write().unwrap();
    txn.open_table(KG_INDEX_META)
        .unwrap()
        .insert("kg_generation", 1)
        .unwrap();
    txn.commit().unwrap();
}

/// Invariant I2, and the most serious failure this design could have:
/// `rebuild_kg` runs under `?` in writable open, so one bad file would lock a
/// user out of their repository.
#[test]
fn no_repository_content_can_fail_projection_or_writable_open() {
    let (dir, repo) = projected_repo();
    write_hostile_kinds(&repo);
    sync(&repo);
    store_hostile_rows(&repo);

    let incremental = repo.workflow_snapshot().unwrap();
    assert_closed(&incremental, "incremental");
    repo.vault_reindex_kg().unwrap();
    for meta in repo.vault_list("kinds/", None).unwrap() {
        repo.vault_index_kg(&meta.path).unwrap();
        let entry = repo.vault_retrieve(&meta.path).unwrap().unwrap();
        repo.vault_extract_kg(&meta.path, &entry).unwrap();
    }
    assert!(repo.rebuild_kg(true).unwrap());
    let rebuilt = repo.workflow_snapshot().unwrap();
    assert_closed(&rebuilt, "rebuilt");
    assert_eq!(
        incremental.identity,
        rebuilt.identity,
        "{}",
        describe_difference(&incremental, &rebuilt)
    );

    let concept_named = |name: &str| concept(&repo, name);
    let id_of = |n: u32| concept_named(&format!("adr.{}", uid(n)));
    // Inert rows have no kind node, and a ref to one is a violation, not an edge.
    for n in [1, 2, 3, 4, 5, 6, 80, 81, 82, 83, 84, 85, 86] {
        let row = rebuilt.nodes.iter().find(|n2| n2.id == id_of(n));
        assert!(
            row.is_none_or(|row| row
                .metadata
                .as_ref()
                .is_none_or(|m| m["kind_name"].is_null())),
            "{n}: {row:?}"
        );
    }
    assert!(violation(&repo, &id_of(44)).is_some());
    assert!(references(&repo, &id_of(44), &id_of(6)).is_none());
    for n in [
        20, 21, 22, 23, 25, 26, 27, 28, 29, 31, 40, 41, 42, 43, 44, 45, 46,
    ] {
        assert!(violation(&repo, &id_of(n)).is_some(), "{n}");
    }
    // A resolving cycle and a self-reference are ordinary edges.
    assert!(references(&repo, &id_of(47), &id_of(48)).is_some());
    assert!(references(&repo, &id_of(48), &id_of(47)).is_some());
    assert!(references(&repo, &id_of(47), &id_of(47)).is_some());
    // Reserved and invalid definitions still get a hub, and instances of them
    // get nothing that could pass for a built-in node.
    for kind in [
        "broken",
        "noyaml",
        "badyaml",
        "merged",
        "wrongname",
        "listfm",
        "badfield",
        "intent",
        "kind",
    ] {
        let hub = node(&repo, &concept_named(&format!("kind.{kind}"))).expect(kind);
        assert_eq!(meta(&hub, "definition_status"), "invalid", "{kind}");
    }
    assert!(
        meta(
            &node(&repo, &concept_named("kind.rowbinary")).unwrap(),
            "definition_status"
        ) == "invalid"
    );
    assert!(node(
        &repo,
        &iri::mint_intent(&repo.project_id().unwrap(), &uid(1)).unwrap()
    )
    .is_none());
    drop(repo);

    expire_kg_generation(dir.path());
    let reopened = Repository::open(dir.path()).expect("writable open survives hostile content");
    assert_eq!(
        reopened
            .pristine()
            .read_txn()
            .unwrap()
            .kg_generation()
            .unwrap(),
        atomic_core::kg::KG_GENERATION
    );
    assert_eq!(
        reopened.workflow_snapshot().unwrap().identity,
        rebuilt.identity
    );
}

// ── Determinism ─────────────────────────────────────────────────

fn add_rfc(repo: &Repository) {
    write_vault(repo, &hub_path("rfc"), RFC_SPEC);
    write_vault(
        repo,
        &instance_path("rfc", K3),
        rfc(K3, &format!("decidedBy: {K1}\nintent: {INTENT_UID}\n")),
    );
    sync(repo);
}
fn add_intent(repo: &Repository) {
    write_vault(repo, INTENT_PATH, labelled_intent(""));
    sync(repo);
}
fn nonconforming_instance(repo: &Repository) {
    write_vault(
        repo,
        &instance_path("adr", K1),
        format!(
            "---\nuid: {K1}\nstatus: maybe\nowner: me\n---\nSee [[missing]] and src/main.rs.\n"
        ),
    );
    sync(repo);
}
fn drop_rfc_spec(repo: &Repository) {
    remove_vault(repo, &hub_path("rfc"));
    sync(repo);
}

/// Stored in any order and through any transition, the incremental projection
/// is what a rebuild would produce.
#[test]
fn incremental_projection_and_rebuild_agree() {
    assert_rebuild_agrees_after_every_step(&[
        ("rfc refers to nothing yet", add_rfc),
        ("add instance before its spec", add_instance),
        ("add spec", add_spec),
        ("add intent", add_intent),
        ("instance stops conforming", nonconforming_instance),
        ("instance conforms again", add_instance),
        ("break spec", break_spec),
        ("restore spec", add_spec),
        ("drop rfc spec", drop_rfc_spec),
        ("drop target", drop_instance),
        ("restore target", add_instance),
        ("drop intent", drop_intent),
        ("drop spec", drop_spec),
    ]);
}

#[test]
fn hostile_content_agrees_with_rebuild_after_deflating_in_any_batch() {
    // One batch against the same files stored one at a time.
    let (_dir, batched) = projected_repo();
    write_hostile_kinds(&batched);
    sync(&batched);
    let (_dir2, single) = projected_repo();
    write_hostile_kinds(&single);
    let mut files: Vec<_> = walk_vault(&single);
    files.sort();
    files.reverse();
    for (rel, content) in files {
        single.vault_record_files(&[(rel, Some(content))]).unwrap();
    }
    for repo in [&batched, &single] {
        let incremental = repo.workflow_snapshot().unwrap();
        repo.rebuild_kg(true).unwrap();
        let rebuilt = repo.workflow_snapshot().unwrap();
        assert_eq!(
            incremental.identity,
            rebuilt.identity,
            "{}",
            describe_difference(&incremental, &rebuilt)
        );
    }
}

fn walk_vault(repo: &Repository) -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|ext| ext == "md") {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned();
                if rel.starts_with("kinds/") || rel == INTENT_PATH || rel == MEMORY_PATH {
                    out.push((rel, std::fs::read_to_string(&path).unwrap()));
                }
            }
        }
    }
    let root = repo.vault_dir();
    let mut out = Vec::new();
    walk(&root, &root, &mut out);
    out
}

// ── Field values and RDF ────────────────────────────────────────

/// No declared field can write a key the SHACL adapter or vault GC reads, and
/// neither hubs nor instances carry one of their own apart from the timestamps
/// every vault node has. The CLI test runs this through `query validate`.
#[test]
fn field_values_never_reach_the_keys_the_rdf_adapter_reads() {
    const SPEC: &str = "\
---
name: tricky
specVersion: 1
fields:
  status:
    type: integer
  priority:
    type: \"string[]\"
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
  path:
    type: string
  derived_from_vault_path:
    type: string
---
";
    let (_dir, repo) = kind_repo();
    write_vault(&repo, &hub_path("tricky"), SPEC);
    write_vault(
        &repo,
        &instance_path("tricky", K1),
        format!(
            "---\nuid: {K1}\nstatus: 3\npriority: [high]\nfinding_code: true\n\
             object_availability: 1\nprojection_completeness: 2\nproject_id: 4\n\
             turn_number: x\nschema_version: y\npath: elsewhere\n\
             derived_from_vault_path: memory/notes.md\n---\nBody.\n"
        ),
    );
    sync(&repo);
    let id = concept(&repo, &format!("tricky.{K1}"));
    assert!(violation(&repo, &id).is_none());
    let instance = node(&repo, &id).unwrap();
    for key in [
        "status",
        "priority",
        "finding_code",
        "object_availability",
        "projection_completeness",
        "project_id",
        "turn_number",
        "schema_version",
        "path",
    ] {
        assert!(meta(&instance, key).is_null(), "{key}");
        assert!(!meta(&instance, &format!("field.{key}")).is_null(), "{key}");
    }
    // Ownership stays with the row, whatever a field of that name says.
    assert_eq!(
        meta(&instance, "derived_from_vault_path"),
        &instance_path("tricky", K1)
    );
    assert_eq!(
        meta(&instance, "field.derived_from_vault_path"),
        "memory/notes.md"
    );
    assert_eq!(instance.path(), None);
    // A row claiming to be owned by another path cannot be deleted with it.
    write_vault(&repo, MEMORY_PATH, labelled_memory("x"));
    sync(&repo);
    drop_memory(&repo);
    assert!(node(&repo, &id).is_some());
}

// ── Search ──────────────────────────────────────────────────────

/// `tokenize_for_fts` keeps words of three or more characters that are not stop
/// words, so a kind name can be unfindable while its instances are not.
#[test]
fn instances_are_searchable_by_title_uid_and_kind_name() {
    let (_dir, repo) = kind_repo();
    for kind in ["adr", "ab", "test"] {
        write_vault(
            &repo,
            &hub_path(kind),
            format!("---\nname: {kind}\nspecVersion: 1\n---\n"),
        );
        write_vault(
            &repo,
            &instance_path(kind, K1),
            format!("---\nuid: {K1}\ntitle: Title of {kind} zebra\n---\nBody.\n"),
        );
    }
    sync(&repo);
    let search = |query: &str| -> BTreeSet<String> {
        repo.vault_kg_search_by_kind(query, 20, Class::Concept)
            .unwrap()
            .into_iter()
            .map(|node| node.id)
            .collect()
    };
    let instance = |kind: &str| concept(&repo, &format!("{kind}.{K1}"));
    let all = BTreeSet::from([instance("adr"), instance("ab"), instance("test")]);
    assert_eq!(search("zebra"), all, "by title");
    assert_eq!(search(K1), all, "by uid, whatever the kind");
    assert!(
        search("adr").contains(&instance("adr")),
        "a three-letter kind"
    );
    assert!(search("ab").is_empty(), "shorter than the tokenizer keeps");
    assert!(search("test").is_empty(), "a stop word");
}

// ── Plain repositories ──────────────────────────────────────────

/// A deterministic digest of every vault-derived node and edge, with the random
/// project id and the fixture's timestamps taken out.
fn vault_projection_digest(repo: &Repository, stamp: &str) -> (String, String) {
    let snapshot = repo.workflow_snapshot().unwrap();
    let project = repo.project_id().unwrap().to_base32();
    let nodes: Vec<&KgNode> = snapshot
        .nodes
        .iter()
        .filter(|node| matches!(node.source.as_str(), "vault" | "vault_reference"))
        .collect();
    let edges: Vec<&KgEdge> = snapshot
        .edges
        .iter()
        .filter(|edge| {
            edge.metadata
                .as_ref()
                .is_some_and(|m| m.get("derived_from_vault_path").is_some())
        })
        .collect();
    let mut text = serde_json::to_string_pretty(&(nodes, edges)).unwrap();
    // A finding's IRI holds the hash of its subject's project-qualified IRI.
    for node in &snapshot.nodes {
        text = text.replace(&Hash::of(node.id.as_bytes()).to_base32(), "SUBJECT");
    }
    let text = text.replace(&project, "PROJECT").replace(stamp, "STAMP");
    (Hash::of(text.as_bytes()).to_base32(), text)
}

/// Invariant I4. The projection of a repository with no `kinds/` rows is pinned
/// to what dev at 5c31fae produces for the same rows: the digest below came from
/// running this exact test there. Re-pin it deliberately if built-in projection
/// changes; the printed dump shows what moved.
#[test]
fn a_repository_with_no_kinds_projects_as_it_did_before_kinds() {
    let stamp = "2026-11-05T12:00:00+00:00";
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let store = |path: &str, kind, frontmatter: serde_json::Value, body: &str| {
        let mut entry = VaultEntry::new(
            kind,
            body.as_bytes().to_vec(),
            frontmatter.to_string(),
            stamp.to_string(),
        );
        entry.updated_at = stamp.to_string();
        repo.vault_store_entry(path, &entry).unwrap();
    };
    store(
        INTENT_PATH,
        VaultEntryType::Intent,
        serde_json::json!({
            "uid": INTENT_UID, "id": "T::a::1", "title": "Plain intent", "kind": "bug",
            "status": "in_progress", "priority": "high", "assignee": "alice",
            "labels": ["storage", "kind.adr", format!("adr.{K1}")],
            "informedBy": ["urn:atomic:memory:notes", "urn:atomic:memory:missing"],
        }),
        "Plain body, see src/lib.rs and [[notes]] and [[missing]].\n",
    );
    store(
        MEMORY_PATH,
        VaultEntryType::Memory,
        serde_json::json!({"name": "notes", "description": "Notes", "labels": "a, b", "author": "bob"}),
        "Memory body, see docs/x.md.\n",
    );
    store(
        "skills/s.md",
        VaultEntryType::Skill,
        serde_json::json!({"name": "S", "description": "A skill"}),
        "Skill body.\n",
    );
    store(
        "goals/g/_goal.md",
        VaultEntryType::Session,
        serde_json::json!({"title": "G", "intent": INTENT_UID}),
        "Goal body.\n",
    );
    store(
        "goals/g/tool-results/r.md",
        VaultEntryType::ToolResult,
        serde_json::json!({"title": "R"}),
        "Ran a/b.rs.\n",
    );
    // Scratch rows stay inert, here as on dev.
    store(
        "notes/scratch.md",
        VaultEntryType::Scratch,
        serde_json::json!({"uid": K1, "title": "Scratch", "labels": ["x"]}),
        "See src/never.rs and [[notes]].\n",
    );
    repo.vault_reindex_kg().unwrap();
    let (digest, text) = vault_projection_digest(&repo, stamp);
    println!("{text}");
    assert_eq!(
        digest, "HWYZLBKIIJPAUZMGRQCCWRFSPGS6LYXBAKDBAGQEYSI4Z7R3KLAA",
        "{text}"
    );
}

// ── Scale ───────────────────────────────────────────────────────

/// `SCALE_PROBE_SIZES=100,1000` narrows a probe to those row counts.
fn probe_sizes() -> Vec<usize> {
    std::env::var("SCALE_PROBE_SIZES")
        .map(|sizes| sizes.split(',').map(|n| n.parse().unwrap()).collect())
        .unwrap_or_else(|_| vec![100, 1_000, 5_000])
}

/// `SCALE_PROBE_ROWS=adr` narrows a probe to one sort of row: `adr` is a custom
/// kind, `memory` a built-in dev already projects the same way.
fn probe_rows() -> Vec<String> {
    std::env::var("SCALE_PROBE_ROWS")
        .map(|rows| rows.split(',').map(String::from).collect())
        .unwrap_or_else(|_| vec!["adr".to_string(), "memory".to_string()])
}

fn probe_timed(count: usize, what: &str, operation: &str, f: impl FnOnce()) {
    let start = std::time::Instant::now();
    f();
    println!(
        "{count:>6} {what:<7} {operation:<22} {:>10.1?}",
        start.elapsed()
    );
}

/// One vault store re-projects the whole vault, and each row of that pass scans
/// the whole KG, so the cost is at least quadratic in rows. This puts numbers on
/// it for custom kinds, beside the same count of memory rows. Rows go straight
/// into redb, since storing them one at a time is `scale_probe_deflate`'s cost:
///
/// `cargo test -p atomic-repository --test it kg_kinds::scale_probe -- --ignored --nocapture --exact`
#[test]
#[ignore = "timing probe, not a test"]
fn scale_probe() {
    for count in probe_sizes() {
        for what in probe_rows() {
            let what = what.as_str();
            let (_dir, repo) = projected_repo();
            write_vault(&repo, &hub_path("adr"), ADR_SPEC);
            sync(&repo);
            let mut txn = repo.pristine().write_txn().unwrap();
            let mut previous: Option<String> = None;
            for _ in 0..count {
                let uid = ulid::Ulid::new().to_string();
                let (path, frontmatter) = if what == "adr" {
                    let frontmatter = serde_json::json!({
                        "uid": uid, "title": "Use redb storage", "status": "accepted",
                        "supersedes": previous.iter().collect::<Vec<_>>(),
                    });
                    (instance_path("adr", &uid), frontmatter)
                } else {
                    let frontmatter = serde_json::json!({"name": uid, "description": "Notes"});
                    (format!("memory/{uid}.md"), frontmatter)
                };
                let entry = VaultEntry::new(
                    if what == "adr" {
                        VaultEntryType::Scratch
                    } else {
                        VaultEntryType::Memory
                    },
                    b"We chose redb.\n".to_vec(),
                    frontmatter.to_string(),
                    "2026-11-05T12:00:00+00:00".to_string(),
                );
                txn.put_vault_entry(&path, &entry).unwrap();
                previous = Some(uid);
            }
            txn.commit().unwrap();

            probe_timed(count, what, "whole-vault pass", || {
                repo.vault_reindex_kg().unwrap();
            });
            probe_timed(count, what, "rebuild_kg", || {
                repo.rebuild_kg(true).unwrap();
            });
            probe_timed(count, what, "workflow_snapshot", || {
                repo.workflow_snapshot().unwrap();
            });
            if what == "adr" {
                let hub = concept(&repo, "kind.adr");
                probe_timed(count, what, "neighbors of the hub", || {
                    repo.vault_kg_neighbors(&hub, 1).unwrap();
                });
            }
            for attempt in 1..=2 {
                let uid = ulid::Ulid::new().to_string();
                let (rel, content) = if what == "adr" {
                    (instance_path("adr", &uid), adr(&uid, ""))
                } else {
                    (format!("memory/{uid}.md"), probe_memory(&uid))
                };
                probe_timed(count, what, &format!("store one row #{attempt}"), || {
                    repo.vault_record_files(&[(rel, Some(content))]).unwrap();
                });
            }
        }
    }
}

fn probe_memory(uid: &str) -> String {
    format!("---\nname: {uid}\ndescription: Notes\n---\nWe chose redb.\n")
}

/// A deflate stores its rows one at a time, and each store is a whole-vault pass,
/// so a first deflate of N rows runs N passes over a growing vault. Every row
/// here is a new file on disk, as after a clone. `SCALE_PROBE_CHUNK=250` stores
/// through `vault_record_files` in chunks and prints running totals, so a run
/// stopped early still says how far it got:
///
/// `cargo test -p atomic-repository --test it kg_kinds::scale_probe_deflate -- --ignored --nocapture --exact`
#[test]
#[ignore = "timing probe, not a test"]
fn scale_probe_deflate() {
    let chunk: Option<usize> = std::env::var("SCALE_PROBE_CHUNK")
        .ok()
        .map(|chunk| chunk.parse().unwrap());
    for count in probe_sizes() {
        for what in probe_rows() {
            let what = what.as_str();
            let (_dir, repo) = projected_repo();
            write_vault(&repo, &hub_path("adr"), ADR_SPEC);
            sync(&repo);
            let mut previous: Option<String> = None;
            let mut files = Vec::with_capacity(count);
            for _ in 0..count {
                let uid = ulid::Ulid::new().to_string();
                files.push(if what == "adr" {
                    let extra = previous
                        .as_ref()
                        .map_or(String::new(), |id| format!("supersedes: [{id}]\n"));
                    (instance_path("adr", &uid), adr(&uid, &extra))
                } else {
                    (format!("memory/{uid}.md"), probe_memory(&uid))
                });
                previous = Some(uid);
            }
            if let Some(chunk) = chunk {
                let start = std::time::Instant::now();
                let mut done = 0;
                for rows in files.chunks(chunk) {
                    let rows: Vec<(String, Option<String>)> = rows
                        .iter()
                        .map(|(rel, content)| (rel.clone(), Some(content.clone())))
                        .collect();
                    repo.vault_record_files(&rows).unwrap();
                    done += rows.len();
                    println!(
                        "{count:>6} {what:<7} stored {done:>6} rows    {:>10.1?}",
                        start.elapsed()
                    );
                }
            } else {
                for (rel, content) in &files {
                    write_vault(&repo, rel, content);
                }
                probe_timed(count, what, "full deflate batch", || {
                    let stored = sync(&repo);
                    assert_eq!(stored.len(), count);
                });
            }
        }
    }
}

// ── What deflate may read under kinds/ ──────────────────────────

/// Deflate holds the rule `KindFiles` states: a link under `kinds/` is never
/// followed, so what it names reaches neither redb nor the KG.
#[cfg(unix)]
#[test]
fn deflate_never_follows_a_link_under_kinds() {
    let (dir, repo) = projected_repo();
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(outside.join("adr")).unwrap();
    std::fs::write(outside.join("instance.md"), adr(K1, "")).unwrap();
    std::fs::write(outside.join("_kind.md"), RFC_SPEC).unwrap();
    std::fs::write(outside.join("adr/_kind.md"), ADR_SPEC).unwrap();
    std::fs::write(outside.join("adr").join(format!("{K2}.md")), adr(K2, "")).unwrap();
    let link = |target: &std::path::Path, rel: &str| {
        let path = repo.vault_dir().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(target, path).unwrap();
    };
    let instance = instance_path("adr", K1);
    let instance_id = concept(&repo, &format!("adr.{K1}"));

    write_vault(&repo, &hub_path("adr"), ADR_SPEC);
    link(&outside.join("instance.md"), &instance);
    link(&outside.join("_kind.md"), &hub_path("rfc"));
    sync(&repo);
    assert!(repo.vault_retrieve(&instance).unwrap().is_none());
    assert!(repo.vault_retrieve(&hub_path("rfc")).unwrap().is_none());
    assert!(node(&repo, &instance_id).is_none());
    assert!(node(&repo, &concept(&repo, "kind.rfc")).is_none());

    // A stored file swapped for a link takes its row and node with it: the row
    // no longer says what the path holds.
    remove_vault(&repo, &instance);
    write_vault(&repo, &instance, adr(K1, ""));
    sync(&repo);
    assert!(node(&repo, &instance_id).is_some());
    remove_vault(&repo, &instance);
    link(&outside.join("instance.md"), &instance);
    sync(&repo);
    assert!(repo.vault_retrieve(&instance).unwrap().is_none());
    assert!(node(&repo, &instance_id).is_none());

    // So does a kind directory swapped for a link, whatever lies behind it.
    remove_vault(&repo, &instance);
    write_vault(&repo, &instance, adr(K1, ""));
    sync(&repo);
    assert!(repo.vault_retrieve(&hub_path("adr")).unwrap().is_some());
    std::fs::remove_dir_all(repo.vault_dir().join("kinds/adr")).unwrap();
    link(&outside.join("adr"), "kinds/adr");
    sync(&repo);
    assert!(repo.vault_retrieve(&hub_path("adr")).unwrap().is_none());
    assert!(repo.vault_retrieve(&instance).unwrap().is_none());
    assert!(repo
        .vault_retrieve(&instance_path("adr", K2))
        .unwrap()
        .is_none());
}

/// Reading a link to a device or a named pipe never returns, and a deflate runs
/// inside `record`.
#[cfg(unix)]
#[test]
fn deflate_does_not_block_on_what_is_not_a_file_under_kinds() {
    let (_dir, repo) = projected_repo();
    write_vault(&repo, &hub_path("adr"), ADR_SPEC);
    write_vault(&repo, MEMORY_PATH, labelled_memory("x"));
    std::os::unix::fs::symlink("/dev/zero", repo.vault_dir().join(instance_path("adr", K1)))
        .unwrap();
    let fifo = repo.vault_dir().join(instance_path("adr", K2));
    assert!(std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success());

    let updated = sync(&repo);
    assert!(updated.contains(&MEMORY_PATH.to_string()), "{updated:?}");
    assert!(repo
        .vault_retrieve(&instance_path("adr", K1))
        .unwrap()
        .is_none());
    assert!(repo
        .vault_retrieve(&instance_path("adr", K2))
        .unwrap()
        .is_none());
}

/// One bad file is `kind validate`'s to report: every other row still deflates,
/// and a file that is there is not taken for deleted.
#[test]
fn a_kind_file_that_cannot_be_read_does_not_stop_the_deflate() {
    let (_dir, repo) = projected_repo();
    let stored = instance_path("adr", K1);
    write_vault(&repo, &hub_path("adr"), ADR_SPEC);
    write_vault(&repo, &stored, adr(K1, ""));
    sync(&repo);
    let before = repo.vault_retrieve(&stored).unwrap().unwrap();

    let not_utf8 = |uid: &str| {
        let mut bytes = format!("---\nuid: {uid}\nstatus: accepted\ntitle: caf").into_bytes();
        bytes.push(0xE9);
        bytes.extend_from_slice(b"\n---\nBody.\n");
        bytes
    };
    write_vault(&repo, &stored, not_utf8(K1));
    write_vault(&repo, &instance_path("adr", K2), not_utf8(K2));
    write_vault(&repo, MEMORY_PATH, labelled_memory("x"));
    let updated = sync(&repo);
    assert!(updated.contains(&MEMORY_PATH.to_string()), "{updated:?}");

    let after = repo.vault_retrieve(&stored).unwrap().unwrap();
    assert_eq!(after.content_hash, before.content_hash);
    assert!(node(&repo, &concept(&repo, &format!("adr.{K1}"))).is_some());
    assert!(repo
        .vault_retrieve(&instance_path("adr", K2))
        .unwrap()
        .is_none());
    let report = repo
        .kind_validate(&KindSource::WorkingCopy, Some("adr"))
        .unwrap();
    assert!(!report.ok, "{report:?}");
    assert_eq!(report.problems.len(), 2, "{report:?}");
}

/// The same containment for a file with no read permission and a dangling link.
#[cfg(unix)]
#[test]
fn a_kind_file_with_no_permission_or_target_does_not_stop_the_deflate() {
    use std::os::unix::fs::PermissionsExt;

    let (_dir, repo) = projected_repo();
    let stored = instance_path("adr", K1);
    write_vault(&repo, &hub_path("adr"), ADR_SPEC);
    write_vault(&repo, &stored, adr(K1, ""));
    sync(&repo);

    let file = repo.vault_dir().join(&stored);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::os::unix::fs::symlink(
        "/nonexistent/target.md",
        repo.vault_dir().join(instance_path("adr", K2)),
    )
    .unwrap();
    write_vault(&repo, MEMORY_PATH, labelled_memory("x"));
    let updated = repo.vault_record_working_copy();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();

    let updated = updated.unwrap();
    assert!(updated.contains(&MEMORY_PATH.to_string()), "{updated:?}");
    assert!(repo.vault_retrieve(&stored).unwrap().is_some());
    assert!(repo
        .vault_retrieve(&instance_path("adr", K2))
        .unwrap()
        .is_none());
}

/// A row's nodes are stored one upsert at a time, against every other row's, on
/// every vault write: a thousand copies of one link must cost one. The merge
/// keeps what storing every copy would have left.
#[test]
fn a_repeated_link_projects_one_copy_of_each_node() {
    let (_dir, repo) = projected_repo();
    write_vault(&repo, &hub_path("adr"), ADR_SPEC);
    write_vault(&repo, MEMORY_PATH, labelled_memory("x"));
    let path = instance_path("adr", K1);
    write_vault(
        &repo,
        &path,
        format!(
            "---\nuid: {K1}\ntitle: Links\nstatus: accepted\nsupersedes: [{K1}]\n---\n{}",
            "[[nowhere]] [[notes]] [[elsewhere]] ".repeat(500)
        ),
    );
    sync(&repo);

    let entry = repo.vault_retrieve(&path).unwrap().unwrap();
    let (nodes, _) = repo.vault_extract_kg(&path, &entry).unwrap();
    let ids: Vec<&str> = nodes.iter().map(|node| node.id.as_str()).collect();
    let distinct: BTreeSet<&str> = ids.iter().copied().collect();
    assert_eq!(ids.len(), distinct.len(), "{ids:#?}");

    // Its own node survives its own ref to itself, and the unresolved links keep
    // their one finding.
    let subject = concept(&repo, &format!("adr.{K1}"));
    assert_eq!(meta(&node(&repo, &subject).unwrap(), "kind_name"), "adr");
    assert!(references(&repo, &subject, &subject).is_some());
    assert!(node(
        &repo,
        &iri::mint_finding(&repo.project_id().unwrap(), "UnresolvedReference", &subject)
    )
    .is_some());

    let incremental = repo.workflow_snapshot().unwrap();
    assert_closed(&incremental, "repeated links");
    assert!(repo.rebuild_kg(true).unwrap());
    let rebuilt = repo.workflow_snapshot().unwrap();
    assert_eq!(
        incremental.identity,
        rebuilt.identity,
        "{}",
        describe_difference(&incremental, &rebuilt)
    );
}

/// A ref to a file that cannot be a node is unresolved on both sides. The gate
/// must not pass what the KG marks: it stays the stricter of the two.
#[test]
fn the_gate_rejects_a_ref_to_an_instance_that_cannot_project() {
    for (name, target) in [
        ("no uid", "---\ntitle: No uid\nstatus: accepted\n---\nBody.\n".to_string()),
        ("no frontmatter", "Body only.\n".to_string()),
        ("bad frontmatter", "---\nuid: [unclosed\n---\nBody.\n".to_string()),
        (
            "conflict markers",
            format!("---\nuid: {K2}\nstatus: accepted\n---\n<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\n"),
        ),
    ] {
        let (_dir, repo) = projected_repo();
        write_vault(&repo, &hub_path("adr"), ADR_SPEC);
        write_vault(&repo, &instance_path("adr", K2), target);
        write_vault(
            &repo,
            &instance_path("adr", K1),
            adr(K1, &format!("supersedes: [\"adr:{K2}\"]\n")),
        );
        sync(&repo);

        let report = repo
            .kind_validate(&KindSource::WorkingCopy, Some("adr"))
            .unwrap();
        let referrer = report.kinds[0]
            .instances
            .iter()
            .find(|instance| instance.uid == K1)
            .unwrap();
        assert!(!referrer.conforms, "{name}: {report:?}");
        assert_eq!(
            referrer.violations[0].code.as_str(),
            "UNRESOLVED_REF",
            "{name}: {report:?}"
        );

        let subject = concept(&repo, &format!("adr.{K1}"));
        assert_eq!(
            violation_codes(&repo, &subject),
            serde_json::json!(["UNRESOLVED_REF"]),
            "{name}"
        );
        assert!(
            references(&repo, &subject, &concept(&repo, &format!("adr.{K2}"))).is_none(),
            "{name}"
        );
    }
}
