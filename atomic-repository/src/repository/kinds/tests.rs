use std::sync::Arc;

use atomic_canonical::kind_spec::{KindViolationCode, RefResolver};
use tempfile::tempdir;

use super::*;
use crate::repository::tests::create_temp_repo;
use crate::tracking::TrackingOptions;

const UID: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TR";
const UID2: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TS";
const UID3: &str = "01JD8ZK3M6W2Q4V7X9B5N1C3TT";

const ADR_DEF: &str = "\
---
name: adr
specVersion: 1
title: Architecture Decision Record
fields:
  status:
    type: enum
    values: [proposed, accepted, superseded]
    required: true
  supersedes:
    type: ref[]
    target: [adr]
    edge: SUPERSEDES
  intent:
    type: ref
    target: [intent]
    edge: IMPLEMENTS
  memo:
    type: ref
    target: [memory]
body:
  required: true
embed: true
---
# How to write an ADR

Say what was decided and why.
";

fn write(root: &Path, vault_rel: &str, content: &str) {
    let path = root.join(".vault").join(vault_rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn adr_instance(uid: &str, extra: &str) -> String {
    format!("---\nuid: {uid}\ntitle: Use redb\nstatus: accepted\n{extra}---\nWe chose redb.\n")
}

fn vault_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.init_vault().unwrap();
    (dir, repo)
}

fn codes(report: &KindValidationReport) -> Vec<(KindProblemCode, String)> {
    report
        .problems
        .iter()
        .map(|p| (p.code, p.path.clone()))
        .collect()
}

// ── paths and helpers ───────────────────────────────────────────────────

#[test]
fn kinds_classify_paths() {
    use KindPath::*;
    assert_eq!(
        KindPath::classify("kinds/adr/_kind.md"),
        Some(Definition { kind: "adr".into() })
    );
    assert_eq!(
        KindPath::classify(&format!("kinds/adr/{UID}.md")),
        Some(Instance {
            kind: "adr".into(),
            uid: UID.into()
        })
    );
    let lower = format!("kinds/adr/{}.md", UID.to_lowercase());
    let txt = format!("kinds/adr/{UID}.txt");
    for stray in [
        "kinds/readme.md",
        "kinds/adr/notes.md",
        "kinds/adr/sub/_kind.md",
        lower.as_str(),
        txt.as_str(),
        "kinds/ADR/_kind.md",
    ] {
        assert_eq!(KindPath::classify(stray), Some(Stray), "{stray}");
    }
    assert_eq!(KindPath::classify("kindsx/adr/_kind.md"), None);
    assert_eq!(KindPath::classify("memory/a.md"), None);
}

#[test]
fn kinds_helpers() {
    let (dir, repo) = vault_repo();
    assert_eq!(repo.kinds_dir(), dir.path().join(".vault/kinds"));
    assert_eq!(kind_definition_path("adr"), "kinds/adr/_kind.md");
    assert_eq!(
        kind_instance_path("adr", UID),
        format!("kinds/adr/{UID}.md")
    );
    let uid = mint_kind_uid();
    assert!(atomic_canonical::kind_spec::is_valid_ulid(&uid), "{uid}");
    assert_ne!(uid, mint_kind_uid());
}

// ── working-copy registry ───────────────────────────────────────────────

#[test]
fn kinds_working_copy_registry() {
    let (dir, repo) = vault_repo();
    let root = dir.path();
    write(root, "kinds/adr/_kind.md", ADR_DEF);
    write(root, &format!("kinds/adr/{UID}.md"), &adr_instance(UID, ""));
    write(
        root,
        "kinds/rfc/_kind.md",
        "---\nname: rfc\nspecVersion: 1\nfields:\n  summary:\n    type: string\n    pattern: \"^[A-Z]\"\n---\nguide\n",
    );
    write(
        root,
        "kinds/note/_kind.md",
        "---\nname: note\nspecVersion: 1\nfields: {a: 1\n---\n",
    );
    write(
        root,
        "kinds/memo/_kind.md",
        "---\nname: memo\nspecVersion: 1\n---\n<<<<<<< ours\nA\n=======\nB\n>>>>>>> theirs\n",
    );
    write(root, "kinds/bare/_kind.md", "name: bare\nspecVersion: 1\n");

    let registry = repo.kind_registry(&KindSource::WorkingCopy).unwrap();
    let names: Vec<_> = registry.kinds().map(KindEntry::name).collect();
    assert_eq!(names, ["adr", "bare", "memo", "note", "rfc"]);

    let adr = registry.get("adr").unwrap();
    let spec = adr.spec().expect("adr registers");
    assert!(spec.embed);
    assert_eq!(spec.edges(), ["adr.IMPLEMENTS", "adr.SUPERSEDES"]);
    assert_eq!(adr.definition.def_path, "kinds/adr/_kind.md");
    assert!(adr.definition.guidance.starts_with("# How to write an ADR"));
    assert_eq!(adr.instances, [format!("kinds/adr/{UID}.md")]);

    let problems = |name: &str| match &registry.get(name).unwrap().definition.status {
        KindStatus::Invalid(p) => p.join("\n"),
        KindStatus::Registered(_) => panic!("{name} must not register"),
    };
    assert!(problems("rfc").contains("pattern"), "{}", problems("rfc"));
    assert!(
        problems("note").contains("frontmatter"),
        "{}",
        problems("note")
    );
    assert!(
        problems("memo").contains("conflict"),
        "{}",
        problems("memo")
    );
    assert!(
        problems("bare").contains("no frontmatter"),
        "{}",
        problems("bare")
    );
    assert!(registry.spec("rfc").is_none());

    // Working-copy registries are never cached.
    let again = repo.kind_registry(&KindSource::WorkingCopy).unwrap();
    assert!(!Arc::ptr_eq(&registry, &again));
}

#[test]
fn kinds_load_kind_reads_one_definition() {
    let (dir, repo) = vault_repo();
    write(dir.path(), "kinds/adr/_kind.md", ADR_DEF);
    // A broken sibling must not matter to a single-kind lookup.
    write(dir.path(), "kinds/rfc/_kind.md", "---\n: :\n---\n");
    let files = repo.kind_files(&KindSource::WorkingCopy).unwrap();
    let adr = KindRegistry::load_kind(&files, "adr").unwrap().unwrap();
    assert!(adr.spec().is_some());
    assert!(KindRegistry::load_kind(&files, "missing")
        .unwrap()
        .is_none());
    assert!(KindRegistry::load_kind(&files, "../x").unwrap().is_none());
}

#[test]
fn kinds_missing_dir_is_empty() {
    let (_dir, repo) = vault_repo();
    let registry = repo.kind_registry(&KindSource::WorkingCopy).unwrap();
    assert!(registry.is_empty());
    let report = repo.kind_validate(&KindSource::WorkingCopy, None).unwrap();
    assert!(report.ok);
    assert!(report.kinds.is_empty());
}

// ── validation report ───────────────────────────────────────────────────

#[test]
fn kinds_validate_conforming_and_violations() {
    let (dir, repo) = vault_repo();
    let root = dir.path();
    write(root, "kinds/adr/_kind.md", ADR_DEF);
    write(root, &format!("kinds/adr/{UID}.md"), &adr_instance(UID, ""));
    write(
        root,
        &format!("kinds/adr/{UID2}.md"),
        &format!("---\nuid: {UID2}\nstatus: maybe\nextra: 1\n---\n\n"),
    );

    let report = repo.kind_validate(&KindSource::WorkingCopy, None).unwrap();
    assert!(!report.ok);
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    let adr = &report.kinds[0];
    assert_eq!(adr.status, KindReportStatus::Registered);
    assert_eq!(adr.instance_count, 2);
    let good = &adr.instances[0];
    assert!(good.conforms, "{:?}", good.violations);
    assert_eq!(good.uid, UID);
    assert_eq!(good.title.as_deref(), Some("Use redb"));
    let bad = &adr.instances[1];
    assert!(!bad.conforms);
    let vcodes: Vec<_> = bad.violations.iter().map(|v| v.code).collect();
    for code in [
        KindViolationCode::NotInEnum,
        KindViolationCode::UndeclaredField,
        KindViolationCode::BodyRequired,
    ] {
        assert!(vcodes.contains(&code), "{code}: {vcodes:?}");
    }

    // Filtering to the kind gives the same answer; an unknown kind errors.
    let only = repo
        .kind_validate(&KindSource::WorkingCopy, Some("adr"))
        .unwrap();
    assert_eq!(only.kinds, report.kinds);
    assert!(repo
        .kind_validate(&KindSource::WorkingCopy, Some("nope"))
        .is_err());

    // Only the conforming instance left.
    std::fs::remove_file(root.join(format!(".vault/kinds/adr/{UID2}.md"))).unwrap();
    assert!(
        repo.kind_validate(&KindSource::WorkingCopy, None)
            .unwrap()
            .ok
    );
}

#[test]
fn kinds_validate_repository_problem_codes() {
    let (dir, repo) = vault_repo();
    let root = dir.path();
    write(root, "kinds/adr/_kind.md", ADR_DEF);
    // FRONTMATTER_INVALID: a duplicate key, and no frontmatter at all.
    write(
        root,
        &format!("kinds/adr/{UID}.md"),
        &format!("---\nuid: {UID}\nuid: {UID}\n---\nbody\n"),
    );
    write(root, &format!("kinds/adr/{UID2}.md"), "just a body\n");
    // CONFLICT_MARKER: inside the frontmatter, not the body.
    write(
        root,
        &format!("kinds/adr/{UID3}.md"),
        &format!(
            "---\nuid: {UID3}\n<<<<<<< ours\nstatus: accepted\n=======\nstatus: proposed\n>>>>>>> theirs\n---\nbody\n"
        ),
    );
    // KIND_INVALID: the definition and its instance.
    write(root, "kinds/rfc/_kind.md", "---\nname: rfc\n---\n");
    write(root, &format!("kinds/rfc/{UID}.md"), &adr_instance(UID, ""));
    // UNREGISTERED_KIND.
    write(
        root,
        &format!("kinds/orphan/{UID}.md"),
        &adr_instance(UID, ""),
    );
    // STRAY_FILE in every shape.
    let lower = format!("kinds/adr/{}.md", UID.to_lowercase());
    for stray in [
        "kinds/README.md",
        "kinds/adr/notes.txt",
        "kinds/adr/nested/_kind.md",
        lower.as_str(),
        "kinds/Bad_Dir/_kind.md",
    ] {
        write(root, stray, "x\n");
    }

    let report = repo.kind_validate(&KindSource::WorkingCopy, None).unwrap();
    assert!(!report.ok);
    let got = codes(&report);
    use KindProblemCode::*;
    let expect = [
        (FrontmatterInvalid, format!("kinds/adr/{UID}.md")),
        (FrontmatterInvalid, format!("kinds/adr/{UID2}.md")),
        (ConflictMarker, format!("kinds/adr/{UID3}.md")),
        (KindInvalid, "kinds/rfc/_kind.md".to_string()),
        (KindInvalid, format!("kinds/rfc/{UID}.md")),
        (UnregisteredKind, format!("kinds/orphan/{UID}.md")),
        (StrayFile, "kinds/README.md".to_string()),
        (StrayFile, "kinds/adr/notes.txt".to_string()),
        (StrayFile, "kinds/adr/nested/_kind.md".to_string()),
        (StrayFile, lower.clone()),
        (StrayFile, "kinds/Bad_Dir/_kind.md".to_string()),
    ];
    for e in &expect {
        assert!(got.contains(e), "missing {e:?} in {got:?}");
    }
    assert_eq!(got.len(), expect.len(), "{got:?}");

    let rfc = report.kinds.iter().find(|k| k.name == "rfc").unwrap();
    assert_eq!(rfc.status, KindReportStatus::Invalid);
    assert!(!rfc.spec_problems.is_empty());
    assert_eq!(rfc.instances[0].problem, Some(KindInvalid));
    assert!(rfc.instances[0].violations.is_empty());

    let adr = report.kinds.iter().find(|k| k.name == "adr").unwrap();
    assert_eq!(adr.instance_count, 3);
    assert!(adr.instances.iter().all(|i| !i.conforms));

    // The JSON shape the CLI emits.
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["source"], "working_copy");
    assert_eq!(json["kinds"][1]["status"], "invalid");
    assert!(json["problems"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["code"] == "STRAY_FILE"));

    // A filter keeps only that kind's problems.
    let only = repo
        .kind_validate(&KindSource::WorkingCopy, Some("orphan"))
        .unwrap();
    assert!(only.kinds.is_empty());
    assert_eq!(
        codes(&only),
        [(UnregisteredKind, format!("kinds/orphan/{UID}.md"))]
    );

    // Validation never writes.
    assert!(!root.join(".vault/kinds/orphan/_kind.md").exists());
}

// ── reference resolution ────────────────────────────────────────────────

#[test]
fn kinds_resolve_refs_to_instances_memory_and_intents() {
    let (dir, repo) = vault_repo();
    let root = dir.path();
    let created = repo
        .vault_intent_create(crate::IntentCreateOptions {
            title: "Adopt kinds".to_string(),
            priority: None,
            assignee: None,
            labels: Vec::new(),
            session_id: None,
            turn_id: None,
            kind: None,
        })
        .unwrap();
    write(
        root,
        "memory/storage.md",
        "---\nname: storage\n---\nnotes\n",
    );
    write(root, "kinds/adr/_kind.md", ADR_DEF);
    write(
        root,
        &format!("kinds/adr/{UID2}.md"),
        &adr_instance(UID2, ""),
    );
    write(
        root,
        &format!("kinds/adr/{UID}.md"),
        &adr_instance(
            UID,
            &format!(
                "supersedes: [{UID2}, \"adr:{UID2}\"]\nintent: {}\nmemo: storage\n",
                created.uid
            ),
        ),
    );
    write(
        root,
        &format!("kinds/adr/{UID3}.md"),
        &adr_instance(
            UID3,
            "supersedes: [01JD8ZK3M6W2Q4V7X9B5N1C3ZZ]\nintent: NOPE-999\nmemo: missing\n",
        ),
    );

    let files = repo.kind_files(&KindSource::WorkingCopy).unwrap();
    let resolver = KindRefResolver::new(&files);
    let intent_subject = crate::repository::vault_triples::entry_subject(
        &created.intent_file,
        atomic_core::pristine::VaultEntryType::Intent,
    );
    assert_eq!(
        resolver.resolve("intent", &created.uid),
        Some(intent_subject.clone())
    );
    assert_eq!(
        resolver.resolve("intent", &created.id),
        Some(intent_subject.clone())
    );
    assert_eq!(
        resolver.resolve("memory", "storage").as_deref(),
        Some("memory:storage")
    );
    assert_eq!(resolver.resolve("memory", "../memory/storage"), None);
    assert_eq!(resolver.resolve("adr", UID2), Some(format!("adr:{UID2}")));
    assert_eq!(resolver.resolve("adr", &UID2.to_lowercase()), None);
    assert_eq!(resolver.resolve("rfc", UID2), None);

    let report = repo.kind_validate(&KindSource::WorkingCopy, None).unwrap();
    let adr = &report.kinds[0];
    let linked = adr.instances.iter().find(|i| i.uid == UID).unwrap();
    assert!(linked.conforms, "{:?}", linked.violations);
    let broken = adr.instances.iter().find(|i| i.uid == UID3).unwrap();
    let unresolved: Vec<_> = broken
        .violations
        .iter()
        .filter(|v| v.code == KindViolationCode::UnresolvedRef)
        .filter_map(|v| v.field.as_deref())
        .collect();
    assert_eq!(unresolved, ["intent", "memo", "supersedes"]);

    // The resolved refs are what projection turns into edges.
    let registry = repo.kind_registry(&KindSource::WorkingCopy).unwrap();
    let spec = registry.spec("adr").unwrap();
    let bytes = files.read(&format!("kinds/adr/{UID}.md")).unwrap().unwrap();
    let InstanceCheck::Checked { report, .. } = check_instance(spec, UID, &bytes, &resolver) else {
        panic!("instance must parse");
    };
    let adr2 = format!("adr:{UID2}");
    let edges: Vec<_> = report
        .resolved_refs
        .iter()
        .map(|r| (r.edge.as_deref(), r.target_id.as_str()))
        .collect();
    assert_eq!(
        edges,
        [
            (Some("adr.IMPLEMENTS"), intent_subject.as_str()),
            (None, "memory:storage"),
            (Some("adr.SUPERSEDES"), adr2.as_str()),
        ]
    );
}

// ── view source ─────────────────────────────────────────────────────────

#[test]
fn kinds_view_source_and_merkle_cache() {
    let (dir, mut repo) = create_temp_repo();
    let root = dir.path();
    std::fs::write(root.join("seed.txt"), "seed\n").unwrap();
    repo.add("seed.txt", TrackingOptions::default()).unwrap();
    repo.record_all("seed").unwrap();

    // The kind is recorded on a draft view; its parent `dev` never sees it.
    repo.create_view_from("feature", "dev").unwrap();
    repo.switch_view("feature").unwrap();
    write(root, "kinds/adr/_kind.md", ADR_DEF);
    let instance = format!("kinds/adr/{UID}.md");
    write(root, &instance, &adr_instance(UID, ""));
    for path in ["kinds/adr/_kind.md", instance.as_str()] {
        repo.add(format!(".vault/{path}"), TrackingOptions::default())
            .unwrap();
    }
    repo.record_all("add adr kind").unwrap();

    let feature = KindSource::View("feature".into());
    let files = repo.kind_files(&feature).unwrap();
    assert!(files.exists("kinds/adr/_kind.md"));
    assert_eq!(
        files.list_kind_files().unwrap(),
        [instance.clone(), "kinds/adr/_kind.md".to_string()]
    );

    let registry = repo.kind_registry(&feature).unwrap();
    let adr = registry.get("adr").expect("feature declares adr");
    assert!(adr.spec().is_some());
    assert_eq!(adr.instances, std::slice::from_ref(&instance));
    let report = repo.kind_validate(&feature, None).unwrap();
    assert!(report.ok, "{report:?}");
    assert_eq!(report.source, feature);
    assert_eq!(
        serde_json::to_value(&report.source).unwrap(),
        serde_json::json!({"view": "feature"})
    );

    // The view without that change sees no kind, though the files are in
    // the working copy.
    let dev = KindSource::View("dev".into());
    assert!(repo.kind_registry(&dev).unwrap().is_empty());
    assert!(repo.kind_validate(&dev, Some("adr")).is_err());
    assert!(repo
        .kind_registry(&KindSource::WorkingCopy)
        .unwrap()
        .get("adr")
        .is_some());
    assert!(repo.kind_files(&KindSource::View("nope".into())).is_err());

    // The cache returns the same registry until the view's state changes.
    assert!(Arc::ptr_eq(
        &registry,
        &repo.kind_registry(&feature).unwrap()
    ));
    let second = format!("kinds/adr/{UID2}.md");
    write(root, &second, &adr_instance(UID2, ""));
    repo.add(format!(".vault/{second}"), TrackingOptions::default())
        .unwrap();
    assert!(Arc::ptr_eq(
        &registry,
        &repo.kind_registry(&feature).unwrap()
    ));
    repo.record_all("second adr").unwrap();
    let fresh = repo.kind_registry(&feature).unwrap();
    assert!(!Arc::ptr_eq(&registry, &fresh));
    assert_eq!(fresh.get("adr").unwrap().instances.len(), 2);

    // A child view inherits its parent's kinds.
    repo.create_view_from("child", "feature").unwrap();
    let child = repo
        .kind_registry(&KindSource::View("child".into()))
        .unwrap();
    assert_eq!(child.get("adr").unwrap().instances.len(), 2);

    // A record on the parent changes what the child sees, so it must also
    // change the child's cache key.
    let third = format!("kinds/adr/{UID3}.md");
    write(root, &third, &adr_instance(UID3, ""));
    repo.add(format!(".vault/{third}"), TrackingOptions::default())
        .unwrap();
    repo.record_all("third adr").unwrap();
    let child_after = repo
        .kind_registry(&KindSource::View("child".into()))
        .unwrap();
    assert_eq!(child_after.get("adr").unwrap().instances.len(), 3);
}
