//! Routing-parity coverage for the flag-forms wired through the service
//! layer: each test drives the `atomic` binary in local service mode (the
//! default — the same libatomic handlers the atomicd transport serves,
//! in-process) and asserts the wire-carried renders: the attestation-aware
//! listing columns, the canonical JSON-LD projections, the whole-vault
//! listing, the own/inherited view classification, the KG node fields,
//! the composed intent update, the deps opt-out, the revise surgery, the
//! record dry-run, and the provenance summary.
#![cfg(not(windows))]

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(repo_dir: &Path, home_dir: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(repo_dir)
        .env("HOME", home_dir)
        .output()
        .expect("run atomic")
}

fn run(repo_dir: &Path, home_dir: &Path, args: &[&str]) -> String {
    let output = atomic(repo_dir, home_dir, args);
    assert!(
        output.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A fresh repo + vault with an isolated HOME carrying a default identity
/// (the attestation flow needs one).
fn fixture(label: &str) -> (TempDir, TempDir) {
    let repo_tmp = TempDir::new().unwrap();
    let home_tmp = TempDir::new().unwrap();
    let init = atomic(repo_tmp.path(), home_tmp.path(), &["init", "--vault"]);
    assert!(
        init.status.success(),
        "{label}: init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let _ = label;
    (repo_tmp, home_tmp)
}

fn ensure_default_identity(repo_dir: &Path, home_dir: &Path) {
    run(
        repo_dir,
        home_dir,
        &["identity", "new", "tester", "--email", "t@example.com"],
    );
    run(repo_dir, home_dir, &["identity", "default", "tester"]);
}

#[test]
fn intent_list_json_carries_attestation_columns_over_the_wire() {
    let (repo, home) = fixture("intent-list");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    ensure_default_identity(repo_dir, home_dir);
    let created = run(repo_dir, home_dir, &["intent", "new", "Parity intent"]);
    let id = created
        .lines()
        .find_map(|line| line.strip_prefix("Created intent: "))
        .expect("intent id")
        .to_string();

    // Unattested: none/na, the manifest kind tag, the title.
    let json = run(repo_dir, home_dir, &["intent", "list", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], serde_json::json!(id));
    assert_eq!(rows[0]["kind"], "feature");
    assert_eq!(rows[0]["attested"], "none");
    assert_eq!(rows[0]["verifies"], "na");
    assert_eq!(rows[0]["title"], "Parity intent");

    // The kind filter validates client-side and narrows over the wire's
    // classification.
    let empty = run(
        repo_dir,
        home_dir,
        &["intent", "list", "--json", "--kind", "chore"],
    );
    assert_eq!(empty.trim(), "[]");

    // After attesting: fresh + yes — the handler computed the fresh
    // tracked attestation and the DID-match-then-verify rule.
    run(repo_dir, home_dir, &["intent", "attest", &id]);
    let json = run(repo_dir, home_dir, &["intent", "list", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert_eq!(rows[0]["attested"], "fresh");
    assert_eq!(rows[0]["verifies"], "yes");

    // The table render carries the same columns.
    let table = run(repo_dir, home_dir, &["intent", "list"]);
    assert!(table.contains("fresh"));
    assert!(table.contains(&id));
}

#[test]
fn memory_list_json_carries_kind_status_about_columns() {
    let (repo, home) = fixture("memory-list");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    run(
        repo_dir,
        home_dir,
        &[
            "memory",
            "new",
            "--kind",
            "lesson",
            "--text",
            ":::memory\nLessons route over the wire.\n:::",
        ],
    );

    let json = run(repo_dir, home_dir, &["memory", "list", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert_eq!(rows.len(), 1, "the index scaffold never lists");
    assert_eq!(rows[0]["kind"], "lesson");
    assert_eq!(rows[0]["status"], "active");
    assert_eq!(rows[0]["about"], 0);
    assert_eq!(rows[0]["attested"], "none");
    assert_eq!(rows[0]["verifies"], "na");

    // --limit truncates the recency-ordered listing.
    run(
        repo_dir,
        home_dir,
        &[
            "memory",
            "new",
            "--kind",
            "context",
            "--text",
            ":::memory\nA second memory.\n:::",
        ],
    );
    let json = run(repo_dir, home_dir, &["memory", "list", "--json", "-n", "1"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn vault_list_json_lists_every_entry_with_type_size_and_date() {
    let (repo, home) = fixture("vault-list");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    run(repo_dir, home_dir, &["intent", "new", "Listed intent"]);

    let json = run(repo_dir, home_dir, &["vault", "list", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    let paths: Vec<&str> = rows
        .iter()
        .map(|row| row["path"].as_str().unwrap())
        .collect();
    assert!(
        paths.iter().any(|p| p.ends_with("/intent.md")),
        "intents list: {paths:?}"
    );
    assert!(
        paths.contains(&"memory/MEMORY.md"),
        "the index scaffold lists in the whole-vault form: {paths:?}"
    );
    for row in &rows {
        assert!(row["type"].is_string());
        assert!(row["size"].is_u64());
        assert!(row["updated_at"].is_string());
    }

    // The type and prefix filters ride the request.
    let json = run(
        repo_dir,
        home_dir,
        &["vault", "list", "--json", "-t", "memory"],
    );
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|row| row["type"] == "memory"));

    let json = run(
        repo_dir,
        home_dir,
        &["vault", "list", "--json", "-p", "intents/"],
    );
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert!(!rows.is_empty());
    assert!(rows
        .iter()
        .all(|row| row["path"].as_str().unwrap().starts_with("intents/")));
}

#[test]
fn vault_show_json_carries_revision_and_entry_metadata() {
    let (repo, home) = fixture("vault-show");
    let (repo_dir, home_dir) = (repo.path(), home.path());

    let json = run(
        repo_dir,
        home_dir,
        &["vault", "show", "memory/MEMORY.md", "--json"],
    );
    let document: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["mode"], "vault_entry_body");
    assert_eq!(document["entry_type"], "memory");
    assert!(document["revision_hash"].is_string());
    let revision = document["revision_hash"].as_str().unwrap().to_string();

    // The --revision guard: the exact revision allows the pull; a stale
    // one refuses.
    run(
        repo_dir,
        home_dir,
        &[
            "vault",
            "show",
            "memory/MEMORY.md",
            "--revision",
            &revision,
            "--json",
        ],
    );
    let stale = atomic(
        repo_dir,
        home_dir,
        &[
            "vault",
            "show",
            "memory/MEMORY.md",
            "--revision",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "--json",
        ],
    );
    assert!(!stale.status.success());
}

#[test]
fn intent_show_json_projects_the_attested_node() {
    let (repo, home) = fixture("intent-show");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    ensure_default_identity(repo_dir, home_dir);
    let created = run(repo_dir, home_dir, &["intent", "new", "Projected intent"]);
    let id = created
        .lines()
        .find_map(|line| line.strip_prefix("Created intent: "))
        .expect("intent id")
        .to_string();

    // The un-attested projection lifts the stored node.
    let json = run(repo_dir, home_dir, &["intent", "show", &id, "--json"]);
    let node: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(node["@type"].is_string());
    assert!(
        node.get("attributedTo")
            .map(|v| v.is_null())
            .unwrap_or(true),
        "the raw lift carries no attribution yet"
    );

    // After attesting, the projection is the ATTESTED node (the signed
    // contentHash + the author DID) — carried over the wire as raw
    // sources and classified client-side.
    run(repo_dir, home_dir, &["intent", "attest", &id]);
    let json = run(repo_dir, home_dir, &["intent", "show", &id, "--json"]);
    let node: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(node["attributedTo"].is_string(), "the attested author DID");
    assert!(node["contentHash"].is_string());
    assert!(
        node.get("proof").is_some(),
        "the signed proof rides the node"
    );
}

#[test]
fn view_list_json_splits_own_and_inherited_counts() {
    let (repo, home) = fixture("view-list");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    std::fs::write(repo_dir.join("base.txt"), "base\n").unwrap();
    run(repo_dir, home_dir, &["add", "base.txt"]);
    run(repo_dir, home_dir, &["record", "-m", "base change"]);

    let json = run(repo_dir, home_dir, &["view", "list", "--json"]);
    let document: serde_json::Value = serde_json::from_str(&json).unwrap();
    let views = document["views"].as_array().unwrap();
    let dev = views
        .iter()
        .find(|view| view["name"] == "dev")
        .expect("dev view");
    let dev_own = dev["own_change_count"].as_u64().expect("dev own count");
    assert!(dev_own >= 1, "the bootstrap + base changes");
    assert_eq!(dev["inherited_change_count"], serde_json::json!(0));
    assert!(dev["state"].is_string());
    assert_eq!(dev["scope"], "shared");

    // A draft with its own change: the inherited half counts the parent's.
    run(repo_dir, home_dir, &["view", "create", "parity-draft"]);
    run(repo_dir, home_dir, &["view", "switch", "parity-draft"]);
    std::fs::write(repo_dir.join("draft.txt"), "draft\n").unwrap();
    run(repo_dir, home_dir, &["add", "draft.txt"]);
    run(repo_dir, home_dir, &["record", "-m", "draft change"]);

    let json = run(repo_dir, home_dir, &["view", "list", "--json"]);
    let document: serde_json::Value = serde_json::from_str(&json).unwrap();
    let views = document["views"].as_array().unwrap();
    let draft = views
        .iter()
        .find(|view| view["name"] == "parity-draft")
        .expect("draft view");
    assert_eq!(draft["own_change_count"], serde_json::json!(1));
    assert_eq!(
        draft["inherited_change_count"],
        serde_json::json!(dev_own),
        "the draft inherits dev's own log"
    );
    assert_eq!(
        draft["change_count"],
        serde_json::json!(dev_own + 1),
        "change_count reads as the own+inherited total"
    );
    assert_eq!(draft["scope"], "draft");
}

#[test]
fn query_neighbors_json_serializes_the_full_domain_subgraph() {
    let (repo, home) = fixture("kg-nodes");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    std::fs::write(repo_dir.join("kg.txt"), "kg fixture\n").unwrap();
    run(repo_dir, home_dir, &["add", "kg.txt"]);
    run(repo_dir, home_dir, &["record", "-m", "kg fixture change"]);

    let json = run(
        repo_dir,
        home_dir,
        &["query", "neighbors", "file:kg.txt", "--json"],
    );
    let subgraph: serde_json::Value = serde_json::from_str(&json).unwrap();
    let nodes = subgraph["nodes"].as_array().unwrap();
    let file_node = nodes
        .iter()
        .find(|node| node["id"] == "file:kg.txt")
        .expect("the file node");
    // The full domain fields the local JSON serializes: kind, label,
    // summary, source, metadata.
    assert_eq!(file_node["kind"], "file");
    assert_eq!(file_node["label"], "kg.txt");
    assert!(file_node["source"].is_string());
    assert!(subgraph["edges"].is_array());

    // The search JSON serializes the same node shape.
    let json = run(repo_dir, home_dir, &["query", "search", "kg.txt", "--json"]);
    let nodes: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert!(
        nodes.iter().any(|node| node["id"] == "file:kg.txt"),
        "the search surfaces the file node"
    );

    // The --kind filter narrows to the kind's nodes.
    let table = run(
        repo_dir,
        home_dir,
        &["query", "search", "kg.txt", "--kind", "file"],
    );
    assert!(table.contains("file:kg.txt"));
}

#[test]
fn mixed_intent_update_applies_every_field_in_one_request() {
    let (repo, home) = fixture("intent-update");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    let created = run(repo_dir, home_dir, &["intent", "new", "Composed update"]);
    let id = created
        .lines()
        .find_map(|line| line.strip_prefix("Created intent: "))
        .expect("intent id")
        .to_string();

    let report = run(
        repo_dir,
        home_dir,
        &[
            "intent",
            "update",
            &id,
            "--status",
            "in_progress",
            "--priority",
            "high",
            "--assignee",
            "alice",
            "--title",
            "Composed and renamed",
        ],
    );
    assert!(report.contains("Updated intent:"));
    assert!(report.contains("status: in_progress"));
    assert!(report.contains("priority: high"));
    assert!(
        "assignee: alice"
            == report
                .lines()
                .find_map(|line| line.strip_prefix("  assignee: "))
                .map(|_| "assignee: alice")
                .unwrap_or("")
    );

    // The composed update persisted atomically: every frontmatter field
    // reads back through the stored entry.
    let json = run(repo_dir, home_dir, &["intent", "show", &id, "--json"]);
    let node: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(node["title"], "Composed and renamed");

    let list = run(repo_dir, home_dir, &["intent", "list"]);
    assert!(list.contains("in_progress"));
}

#[test]
fn insert_deps_opt_out_routes_over_the_wire() {
    let (repo, home) = fixture("insert-deps");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    std::fs::write(repo_dir.join("base.txt"), "base\n").unwrap();
    run(repo_dir, home_dir, &["add", "base.txt"]);
    run(repo_dir, home_dir, &["record", "-m", "base change"]);
    run(repo_dir, home_dir, &["view", "create", "deps-draft"]);
    run(repo_dir, home_dir, &["view", "switch", "deps-draft"]);
    std::fs::write(repo_dir.join("draft.txt"), "draft\n").unwrap();
    run(repo_dir, home_dir, &["add", "draft.txt"]);
    run(repo_dir, home_dir, &["record", "-m", "draft change"]);

    // Both --deps forms route: the default closure and the explicit
    // opt-out ride apply_dependencies.
    let promoted = run(repo_dir, home_dir, &["insert", "--deps=false"]);
    assert!(promoted.contains("Inserted 1 change(s)"), "{promoted}");
}

#[test]
fn revise_content_mode_recaptures_the_working_copy() {
    let (repo, home) = fixture("revise-content");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    std::fs::write(repo_dir.join("first.txt"), "one\n").unwrap();
    run(repo_dir, home_dir, &["add", "first.txt"]);
    run(repo_dir, home_dir, &["record", "-m", "first change"]);
    std::fs::write(repo_dir.join("second.txt"), "two\n").unwrap();
    run(repo_dir, home_dir, &["add", "second.txt"]);
    run(repo_dir, home_dir, &["record", "-m", "second change"]);

    // The dry-run preview renders over the wire's log entries.
    let preview = run(repo_dir, home_dir, &["revise", "@~1", "--dry-run"]);
    assert!(preview.contains("Would revise change"));
    assert!(preview.contains("This will temporarily unrecord"));

    // The content mode re-captures the edited working copy into @~1 and
    // re-applies the pending change above it.
    std::fs::write(repo_dir.join("first.txt"), "one edited\n").unwrap();
    let report = run(
        repo_dir,
        home_dir,
        &["revise", "@~1", "-m", "first change (revised)"],
    );
    assert!(report.contains("Revised "), "{report}");
    assert_eq!(
        std::fs::read_to_string(repo_dir.join("first.txt")).unwrap(),
        "one edited\n",
        "the re-captured content materialized"
    );
    let log = run(repo_dir, home_dir, &["log"]);
    assert!(log.contains("first change (revised)"));
    assert!(
        log.contains("second change"),
        "the pending change re-applied"
    );
}

#[test]
fn record_dry_run_previews_over_the_status_read() {
    let (repo, home) = fixture("record-dry-run");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    std::fs::write(repo_dir.join("dry.txt"), "pending\n").unwrap();
    run(repo_dir, home_dir, &["add", "dry.txt"]);
    std::fs::write(repo_dir.join("dry.txt"), "pending edited\n").unwrap();

    let preview = run(repo_dir, home_dir, &["record", "--dry-run"]);
    assert!(preview.contains("Would record:"));
    assert!(preview.contains("dry.txt"));

    // The dry run never recorded anything.
    let status = run(repo_dir, home_dir, &["status"]);
    assert!(status.contains("dry.txt"));
}

#[test]
fn agent_attest_summary_and_hash_detail_route() {
    let (repo, home) = fixture("agent-attest");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    ensure_default_identity(repo_dir, home_dir);
    std::fs::write(repo_dir.join("sum.txt"), "summary fixture\n").unwrap();
    run(repo_dir, home_dir, &["add", "sum.txt"]);
    run(
        repo_dir,
        home_dir,
        &["record", "-m", "summary fixture change"],
    );

    // The provenance summary renders from the wire-carried domain data.
    let summary = run(
        repo_dir,
        home_dir,
        &["agent", "attest", "--summary", "--view", "dev"],
    );
    assert!(summary.contains("Project: dev"));
    assert!(summary.contains("AI-authored"));

    // The --pending form requires --view (the local body's exact error).
    let missing_view = atomic(
        repo_dir,
        home_dir,
        &["agent", "attest", "--summary", "--pending", "dev"],
    );
    assert!(!missing_view.status.success());
    assert!(String::from_utf8_lossy(&missing_view.stderr)
        .contains("--pending <parent_view> requires --view"));

    // The --hash resolution failure carries the local error text.
    let missing = atomic(
        repo_dir,
        home_dir,
        &["agent", "attest", "--hash", "NOSUCHPREFIX"],
    );
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr)
        .contains("No attestation found matching 'NOSUCHPREFIX'"));
}

#[test]
fn goal_list_routes_with_status_filter_and_json_columns() {
    let (repo, home) = fixture("goal-list");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    run(
        repo_dir,
        home_dir,
        &["vault", "goal", "start", "--developer", "alice"],
    );

    // The plain listing renders the goal rows (the status filter rides
    // the request verbatim).
    let table = run(repo_dir, home_dir, &["vault", "goal", "list"]);
    assert!(table.contains("alice"), "{table}");
    let active = run(
        repo_dir,
        home_dir,
        &["vault", "goal", "list", "--status", "active"],
    );
    assert!(active.contains("alice"));
    let completed = run(
        repo_dir,
        home_dir,
        &["vault", "goal", "list", "--status", "completed"],
    );
    assert!(completed.contains("No goals found."), "{completed}");

    // The JSON carries the name/developer/status/started_at/turns rows.
    let json = run(repo_dir, home_dir, &["vault", "goal", "list", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["developer"], "alice");
    assert_eq!(rows[0]["status"], "active");
    assert!(rows[0]["started_at"].is_string());
    assert!(rows[0]["turns"].is_u64());
}

#[test]
fn diff_change_renders_legacy_content_over_the_wire() {
    use atomic_core::change::Change;
    use atomic_core::types::Base32;
    use atomic_repository::Repository;

    let (repo, home) = fixture("diff-legacy");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    std::fs::write(repo_dir.join("legacy.txt"), "line1\nline2\n").unwrap();
    run(repo_dir, home_dir, &["add", "legacy.txt"]);
    run(repo_dir, home_dir, &["record", "-m", "legacy seed change"]);
    let original = {
        let repo = Repository::open_readonly(repo_dir).unwrap();
        let history = repo
            .log(atomic_repository::HistoryOptions::default())
            .unwrap();
        history
            .iter()
            .max_by_key(|entry| entry.sequence)
            .expect("the recorded change")
            .hash
    };

    // Unrecord it, then re-apply a LEGACY copy: the same hunks/contents
    // with NO file_ops — the pre-file_ops change shape whose diff
    // reconstructs from the graph's before/after content (fetched
    // server-side over the wire).
    run(repo_dir, home_dir, &["unrecord"]);
    let legacy_hash = {
        let repo = Repository::open_readonly(repo_dir).unwrap();
        let change = repo.load_change(&original).unwrap();
        let legacy = Change::with_file_ops(
            change.hashed.header.clone(),
            change.hashed.hunks.clone(),
            Vec::new(),
            change.contents.clone(),
            change.hashed.dependencies.clone(),
        );
        assert!(!legacy.has_file_ops(), "the fixture is a legacy change");
        drop(repo);
        let repo = Repository::open(repo_dir).unwrap();
        let hash = repo.save_change(&legacy).unwrap();
        repo.insert_change(&hash, Default::default()).unwrap();
        hash
    };

    // The routed diff -c renders the reconstructed before/after content:
    // the file was absent before the change, present after.
    let output = atomic(
        repo_dir,
        home_dir,
        &["diff", "-c", &legacy_hash.to_base32()],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("legacy.txt"), "{stdout}");
    assert!(
        stdout.contains("+line1"),
        "the reconstructed after content: {stdout}"
    );
    assert!(
        stdout.contains("legacy seed change"),
        "the change header: {stdout}"
    );
}

#[test]
fn diff_change_file_ops_hunks_pad_context_from_wire_before_content() {
    let (repo, home) = fixture("diff-context");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    std::fs::write(repo_dir.join("ctx.txt"), "one\ntwo\nthree\nfour\n").unwrap();
    run(repo_dir, home_dir, &["add", "ctx.txt"]);
    run(repo_dir, home_dir, &["record", "-m", "context base"]);
    std::fs::write(repo_dir.join("ctx.txt"), "one\ntwo edited\nthree\nfour\n").unwrap();
    run(repo_dir, home_dir, &["record", "-m", "context edit"]);

    // The FileOps hunks pad their context from the wire-carried
    // before-content: a 3-line context hunk (not the zero-context
    // degradation) matches the local render.
    let output = atomic(repo_dir, home_dir, &["diff", "-c", "@", "--no-color"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("-1,5 +1,5"),
        "the full-file context hunk (before-content padded, not zero-context): {stdout}"
    );
    assert!(stdout.contains("-two"), "{stdout}");
    assert!(stdout.contains("+two edited"), "{stdout}");
    assert!(stdout.contains(" three"), "the context line below the edit");
}

/// Record one change touching `name` with the given content (a fixture
/// shorthand).
fn record_file(repo_dir: &Path, home_dir: &Path, name: &str, body: &str, message: &str) {
    std::fs::write(repo_dir.join(name), body).unwrap();
    run(repo_dir, home_dir, &["add", name]);
    run(repo_dir, home_dir, &["record", "-m", message]);
}

#[test]
fn restore_dry_run_single_file_dumps_pristine_bytes() {
    let (repo, home) = fixture("restore-single");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    record_file(repo_dir, home_dir, "file.txt", "recorded\n", "seed");

    // A dirty tracked file: the dump is the PRISTINE bytes, byte-for-byte
    // (the local body writes the graph content to stdout and nothing
    // else).
    std::fs::write(repo_dir.join("file.txt"), "local edit\n").unwrap();
    let dump = run(repo_dir, home_dir, &["restore", "--dry-run", "file.txt"]);
    assert_eq!(dump, "recorded\n", "the pristine bytes alone");

    // A clean tracked file dumps its (identical) pristine content.
    let clean = run(repo_dir, home_dir, &["restore", "--dry-run", "file.txt"]);
    assert_eq!(clean, "recorded\n");

    // A never-added file has no pristine content: the local body's exact
    // FileNotFound refusal (message and exit code).
    std::fs::write(repo_dir.join("untracked.txt"), "never added\n").unwrap();
    let missing = atomic(
        repo_dir,
        home_dir,
        &["restore", "--dry-run", "untracked.txt"],
    );
    assert!(!missing.status.success(), "no pristine content errors");
    assert_eq!(missing.status.code(), Some(3), "the not-found exit code");
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("File not found: 'untracked.txt'"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
}

#[test]
fn restore_dry_run_listing_renders_the_local_lines() {
    let (repo, home) = fixture("restore-listing");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    record_file(repo_dir, home_dir, "base.txt", "base\n", "base");

    // The whole-copy form on a clean tree keeps the whole-tree message.
    let whole = run(repo_dir, home_dir, &["restore", "--dry-run"]);
    assert!(
        whole.contains("Nothing to restore - working copy is clean"),
        "{whole}"
    );

    // An added file previews the untrack (never a content read) with the
    // local hint's singular form.
    std::fs::write(repo_dir.join("new.txt"), "brand new\n").unwrap();
    run(repo_dir, home_dir, &["add", "new.txt"]);
    let preview = run(repo_dir, home_dir, &["restore", "--dry-run", "new.txt"]);
    assert!(
        preview.contains("Would untrack: new.txt (kept on disk)"),
        "{preview}"
    );
    assert!(
        preview.contains("(dry run - 1 file would be restored)"),
        "the local count wording: {preview}"
    );

    // A partial filter with nothing to restore keeps the local body's
    // partial message (it must not claim the whole copy is clean).
    let partial = run(repo_dir, home_dir, &["restore", "--dry-run", "nosuch/"]);
    assert!(
        partial.contains("Nothing to restore for the specified path(s)"),
        "{partial}"
    );
}

#[test]
fn bare_insert_dry_run_previews_and_real_promotion_routes() {
    let (repo, home) = fixture("insert-bare");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    record_file(repo_dir, home_dir, "base.txt", "base\n", "base change");
    run(
        repo_dir,
        home_dir,
        &["view", "create", "feature", "--draft", "--switch"],
    );
    record_file(
        repo_dir,
        home_dir,
        "feature.txt",
        "feature\n",
        "feature change",
    );

    // The dry-run preview: the resolved direction, the numbered missing
    // list, and the local dry-run notice — nothing inserted.
    let preview = run(repo_dir, home_dir, &["insert", "--dry-run"]);
    assert!(
        preview.contains("Inserting 1 change(s): feature → dev"),
        "the resolved source → target: {preview}"
    );
    assert!(
        preview.contains("Dry run: no changes inserted. Re-run without --dry-run to insert."),
        "{preview}"
    );
    assert!(
        preview.contains("1. ") && preview.contains("feature change"),
        "the numbered listing with its message: {preview}"
    );

    // The promotion did not happen yet: the change is still missing on
    // dev (the next insert still has work to do).
    let real = run(repo_dir, home_dir, &["insert"]);
    assert!(
        real.contains("Inserted 1 change(s)"),
        "the local cross-view report: {real}"
    );
    assert!(real.contains("New state:"), "{real}");

    // Running again is the local friendly no-op.
    let again = run(repo_dir, home_dir, &["insert"]);
    assert!(
        again.contains("Already even with 'dev' — nothing to insert."),
        "{again}"
    );
}

#[test]
fn insert_view_and_tag_dry_run_previews_render_the_local_lines() {
    let (repo, home) = fixture("insert-previews");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    record_file(repo_dir, home_dir, "base.txt", "base\n", "base change");
    run(
        repo_dir,
        home_dir,
        &["view", "create", "feature", "--draft", "--switch"],
    );
    record_file(repo_dir, home_dir, "feature.txt", "one\n", "feature change");
    run(repo_dir, home_dir, &["tag", "create", "v1"]);
    record_file(repo_dir, home_dir, "more.txt", "two\n", "beyond the tag");
    run(repo_dir, home_dir, &["view", "switch", "dev"]);

    // The view dry-run lists what would move.
    let view = run(
        repo_dir,
        home_dir,
        &["insert", "view", "feature", "--dry-run"],
    );
    assert!(
        view.contains("Inserting changes from 'feature' to 'dev'..."),
        "{view}"
    );
    assert!(
        view.contains("Dry run: 2 change(s) would be inserted"),
        "{view}"
    );
    assert!(view.contains("Changes:"), "{view}");

    // The tag dry-run with the --from-view override stops at the tag.
    let tag = run(
        repo_dir,
        home_dir,
        &["insert", "tag", "v1", "--from-view", "feature", "--dry-run"],
    );
    assert!(
        tag.contains("Inserting changes up to tag 'v1' from 'feature' to 'dev'..."),
        "{tag}"
    );
    assert!(
        tag.contains("Dry run: 1 change(s) would be inserted"),
        "only the pre-tag change: {tag}"
    );
}

#[test]
fn insert_tag_from_view_routes_and_inserts_up_to_the_tag() {
    let (repo, home) = fixture("insert-tag");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    record_file(repo_dir, home_dir, "base.txt", "base\n", "base change");
    run(
        repo_dir,
        home_dir,
        &["view", "create", "feature", "--draft", "--switch"],
    );
    record_file(
        repo_dir,
        home_dir,
        "at-tag.txt",
        "at\n",
        "change at the tag",
    );
    run(repo_dir, home_dir, &["tag", "create", "v1"]);
    record_file(
        repo_dir,
        home_dir,
        "past-tag.txt",
        "past\n",
        "change past the tag",
    );
    run(repo_dir, home_dir, &["view", "switch", "dev"]);

    // The real form: only the pre-tag change crosses (the FromView
    // override rides the wire's tag source).
    let report = run(
        repo_dir,
        home_dir,
        &["insert", "tag", "v1", "--from-view", "feature"],
    );
    assert!(
        report.contains("Inserted 1 change(s)"),
        "the tag cutoff applied: {report}"
    );
    assert!(
        repo_dir.join("at-tag.txt").exists(),
        "the tagged change landed"
    );
    assert!(
        !repo_dir.join("past-tag.txt").exists(),
        "the past-the-tag change did NOT land"
    );
}

#[test]
fn insert_change_multi_pick_routes_with_handler_side_resolution() {
    use atomic_core::types::Base32;
    use atomic_repository::Repository;

    let (repo, home) = fixture("insert-multi");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    record_file(repo_dir, home_dir, "base.txt", "base\n", "base change");
    run(
        repo_dir,
        home_dir,
        &["view", "create", "feature", "--draft", "--switch"],
    );
    record_file(repo_dir, home_dir, "first.txt", "one\n", "multi first");
    record_file(repo_dir, home_dir, "second.txt", "two\n", "multi second");
    run(repo_dir, home_dir, &["view", "switch", "dev"]);

    // The raw references (full base32) come off the draft's own log —
    // the CLI ships them raw; the handler resolves each.
    let hashes = {
        let repo = Repository::open_readonly(repo_dir).unwrap();
        let history = repo
            .log(atomic_repository::HistoryOptions::default().view("feature".to_string()))
            .unwrap();
        history
            .iter()
            .map(|entry| entry.hash.to_base32())
            .collect::<Vec<_>>()
    };
    assert_eq!(hashes.len(), 2, "the two draft-only changes");

    let report = run(
        repo_dir,
        home_dir,
        &[
            "insert",
            "change",
            &hashes[0][..6].to_lowercase(),
            &hashes[1][..4],
        ],
    );
    assert!(
        report.contains("Cherry-picking 2 change(s) to 'dev'..."),
        "the local multi-insert leading line: {report}"
    );
    assert!(report.contains("Inserted 2 change(s)"), "{report}");
    assert!(repo_dir.join("first.txt").exists(), "the first landed");
    assert!(repo_dir.join("second.txt").exists(), "the second landed");

    // A reference that matches nothing refuses with the local
    // resolution message.
    let missing = atomic(repo_dir, home_dir, &["insert", "change", "NOSUCHPREFIX"]);
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("Change not found: NOSUCHPREFIX"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
}

#[test]
fn insert_single_resolves_raw_prefixes_case_insensitively() {
    let (repo, home) = fixture("insert-single-ref");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    record_file(repo_dir, home_dir, "base.txt", "base\n", "base change");
    run(
        repo_dir,
        home_dir,
        &["view", "create", "feature", "--draft", "--switch"],
    );
    record_file(
        repo_dir,
        home_dir,
        "feature.txt",
        "feature\n",
        "feature change",
    );
    let full = {
        use atomic_core::types::Base32;
        let repo = atomic_repository::Repository::open_readonly(repo_dir).unwrap();
        let history = repo
            .log(atomic_repository::HistoryOptions::default().view("feature".to_string()))
            .unwrap();
        history[0].hash.to_base32()
    };
    run(repo_dir, home_dir, &["view", "switch", "dev"]);

    // A lowercase prefix resolves (the handler-side resolver matches the
    // local parse semantics: full-parse, then the case-insensitive store
    // prefix), and the leading line prints the resolved FULL hash.
    let report = run(repo_dir, home_dir, &["insert", &full[..6].to_lowercase()]);
    assert!(
        report.contains(&format!("Inserting change {full}...")),
        "the resolved full hash: {report}"
    );
    assert!(report.contains("Inserted 1 change(s)"), "{report}");
    assert!(repo_dir.join("feature.txt").exists(), "materialized on dev");
}

// ---------------------------------------------------------------------------
// the newly wired surfaces (REAC::aaron::27): tags, remote registry,
// split, session rebuild, vault materialize/summaries, the goal lifecycle,
// intent delete/link, memory write, triage candidates, and the KG extras
// ---------------------------------------------------------------------------

#[test]
fn tags_route_full_lifecycle_with_render_parity() {
    let (repo, home) = fixture("tags");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    let created = run(repo_dir, home_dir, &["tag", "create", "v1.0.0"]);
    assert_eq!(created.trim(), "✓ Created tag: v1.0.0");
    let annotated = run(
        repo_dir,
        home_dir,
        &["tag", "create", "v2.0.0", "-m", "Release v2"],
    );
    assert_eq!(annotated.trim(), "✓ Created annotated tag: v2.0.0");

    // The listing: the annotated marker, both names, view filter parity.
    let list = run(repo_dir, home_dir, &["tag", "list"]);
    assert!(list.contains(" v1.0.0"), "{list}");
    assert!(list.contains("*v2.0.0"), "{list}");
    let verbose = run(repo_dir, home_dir, &["tag", "list", "--verbose"]);
    assert!(verbose.contains("(seq:"), "{verbose}");
    assert!(verbose.contains("state: "), "{verbose}");

    // The detail report carries the full record fields.
    let show = run(repo_dir, home_dir, &["tag", "show", "v2.0.0"]);
    assert!(show.contains("Tag: v2.0.0"), "{show}");
    assert!(show.contains("View: dev"), "{show}");
    assert!(show.contains("Kind: release"), "{show}");
    assert!(show.contains("Type: annotated"), "{show}");
    assert!(show.contains("Message: Release v2"), "{show}");

    // Duplicate refusal + force + delete + the not-exist hint.
    let duplicate = atomic(repo_dir, home_dir, &["tag", "create", "v2.0.0"]);
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr)
        .contains("Tag 'v2.0.0' already exists. Use --force to overwrite."));
    run(repo_dir, home_dir, &["tag", "create", "v2.0.0", "-f"]);
    let deleted = run(repo_dir, home_dir, &["tag", "delete", "v2.0.0"]);
    assert_eq!(deleted.trim(), "✓ Deleted tag: v2.0.0");
    let missing = run(repo_dir, home_dir, &["tag", "delete", "v2.0.0"]);
    assert!(missing.contains("Tag 'v2.0.0' does not exist"), "{missing}");
    let show_missing = run(repo_dir, home_dir, &["tag", "show", "nope"]);
    assert!(
        show_missing.contains("Tag 'nope' not found"),
        "{show_missing}"
    );
}

#[test]
fn remote_registry_routes_every_action() {
    let (repo, home) = fixture("remote");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    let empty = run(repo_dir, home_dir, &["remote"]);
    assert!(
        empty.contains("No remotes configured. Use 'atomic remote add <name> <url>' to add one."),
        "{empty}"
    );
    run(
        repo_dir,
        home_dir,
        &["remote", "add", "origin", "https://example.com/repo"],
    );
    run(
        repo_dir,
        home_dir,
        &[
            "remote",
            "add",
            "backup",
            "https://backup.example.com/r",
            "--default",
        ],
    );
    let verbose = run(repo_dir, home_dir, &["remote", "--verbose"]);
    assert!(
        verbose.contains("backup\thttps://backup.example.com/r (default)"),
        "{verbose}"
    );
    run(
        repo_dir,
        home_dir,
        &["remote", "rename", "origin", "upstream"],
    );
    run(
        repo_dir,
        home_dir,
        &["remote", "set-url", "upstream", "https://new.example.com/r"],
    );
    run(repo_dir, home_dir, &["remote", "default", "upstream"]);
    let list = run(repo_dir, home_dir, &["remote"]);
    assert!(
        list.contains("upstream") && list.contains("backup"),
        "{list}"
    );
    // An invalid URL refuses client-side with the local message.
    let invalid = atomic(repo_dir, home_dir, &["remote", "add", "bad", "no-scheme"]);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr)
        .contains("Invalid URL 'no-scheme': URL must include a scheme"));
    // push/pull's remote-name resolution rides the same registry read.
    let missing = atomic(repo_dir, home_dir, &["push", "nosuchremote"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("Remote 'nosuchremote' not found"));
}

#[test]
fn top_level_split_routes_with_the_split_report() {
    let (repo, home) = fixture("split");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    record_file(repo_dir, home_dir, "split.txt", "split\n", "split change");
    let out = run(repo_dir, home_dir, &["split", "experimental"]);
    assert!(
        out.contains("Created view: experimental (split from dev with "),
        "{out}"
    );
    assert!(
        out.contains("Use 'atomic view switch experimental' to switch to the new view"),
        "{out}"
    );
    let switched = run(repo_dir, home_dir, &["split", "fresh", "--switch"]);
    assert!(
        switched.contains("✓ Switched to view: fresh ("),
        "{switched}"
    );
}

#[test]
fn session_rebuild_routes_over_the_wire() {
    let (repo, home) = fixture("session-rebuild");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    let out = run(repo_dir, home_dir, &["session", "rebuild"]);
    assert!(out.contains("Session index rebuild complete"), "{out}");
    assert!(out.contains("Indexed:"), "{out}");
    assert!(out.contains("Already present:"), "{out}");
    assert!(out.contains("Corrupt (skipped):"), "{out}");
}

#[test]
fn vault_materialize_and_summaries_route() {
    let (repo, home) = fixture("vault-mat");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    use std::process::Stdio;
    let write = Command::new(ATOMIC_BIN)
        .args(["memory", "write", "mat"])
        .current_dir(repo_dir)
        .env("HOME", home_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn atomic")
        .wait_with_output()
        .expect("write memory");
    assert!(write.status.success());
    std::fs::remove_file(repo_dir.join(".vault/memory/mat.md")).unwrap();
    // The single-entry form prints the path (and rewrites the file).
    let one = run(
        repo_dir,
        home_dir,
        &["vault", "materialize", "-p", "memory/mat.md"],
    );
    assert!(one.contains("Materialized: memory/mat.md"), "{one}");
    assert!(repo_dir.join(".vault/memory/mat.md").exists());
    // The whole-vault form counts every entry.
    let all = run(repo_dir, home_dir, &["vault", "materialize"]);
    assert!(all.contains("Materialized"), "{all}");
    assert!(all.contains("vault entries."), "{all}");
    // Summaries: the empty case and a tool-result preview.
    let empty = run(repo_dir, home_dir, &["vault", "summaries"]);
    assert_eq!(empty.trim(), "{}");
}

#[test]
fn goal_lifecycle_routes_start_stop_resume_show() {
    let (repo, home) = fixture("goal-lifecycle");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    let started = run(
        repo_dir,
        home_dir,
        &["vault", "goal", "start", "--developer", "alice"],
    );
    let goal = started
        .lines()
        .find_map(|line| line.strip_prefix("Started goal: "))
        .expect("goal name")
        .to_string();
    assert!(started.contains("  directory: .vault/goals/"), "{started}");

    // Link an intent to the goal, then stop/resume/show.
    let intent = run(repo_dir, home_dir, &["intent", "new", "Goal work"])
        .lines()
        .find_map(|line| line.strip_prefix("Created intent: "))
        .expect("intent id")
        .to_string();
    let linked = run(
        repo_dir,
        home_dir,
        &["intent", "link", &intent, "--goal", &goal],
    );
    assert!(
        linked.contains(&format!("Linked goal '{goal}' to intent '{intent}'")),
        "{linked}"
    );
    let stopped = run(repo_dir, home_dir, &["vault", "goal", "stop"]);
    assert!(
        stopped.contains(&format!("Suspended goal: {goal} (status: suspended)")),
        "{stopped}"
    );
    let resumed = run(repo_dir, home_dir, &["vault", "goal", "resume", &goal]);
    assert!(
        resumed.contains(&format!("Resumed goal: {goal}")),
        "{resumed}"
    );
    assert!(resumed.contains("developer: alice"), "{resumed}");
    // Resume is a suspended→active transition: stop again, then the JSON
    // form resumes once more.
    run(repo_dir, home_dir, &["vault", "goal", "stop"]);
    let resumed_json = run(
        repo_dir,
        home_dir,
        &["vault", "goal", "resume", &goal, "--json"],
    );
    let json: serde_json::Value = serde_json::from_str(&resumed_json).unwrap();
    assert_eq!(json["name"], serde_json::json!(goal));
    assert_eq!(json["developer"], serde_json::json!("alice"));
    let shown = run(
        repo_dir,
        home_dir,
        &["vault", "goal", "show", &goal, "--json"],
    );
    let shown_json: serde_json::Value = serde_json::from_str(&shown).unwrap();
    assert_eq!(shown_json["name"], serde_json::json!(goal));
    let shown_plain = run(repo_dir, home_dir, &["vault", "goal", "show", &goal]);
    assert_eq!(shown_plain, shown_plain); // both forms succeed over the wire
}

#[test]
fn intent_delete_routes_with_the_backlog_guard() {
    let (repo, home) = fixture("intent-delete");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    let id = run(repo_dir, home_dir, &["intent", "new", "Delete me"])
        .lines()
        .find_map(|line| line.strip_prefix("Created intent: "))
        .expect("intent id")
        .to_string();
    // --force skips the prompt; the report prints the id + file.
    let deleted = run(repo_dir, home_dir, &["intent", "delete", &id, "--force"]);
    assert!(
        deleted.contains(&format!("Deleted intent: {id}")),
        "{deleted}"
    );
    assert!(deleted.contains("  file: .vault/"), "{deleted}");
    // The deleted intent is gone from the listing.
    let list = run(repo_dir, home_dir, &["intent", "list"]);
    assert!(!list.contains(&id), "{list}");
}

#[test]
fn memory_write_routes_over_the_wire() {
    let (repo, home) = fixture("memory-write");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    use std::process::Stdio;
    let output = Command::new(ATOMIC_BIN)
        .args(["memory", "write", "design", "--type", "reference"])
        .current_dir(repo_dir)
        .env("HOME", home_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn atomic")
        .wait_with_output()
        .expect("write memory");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("Wrote memory: memory/design.md ("),
        "{stdout}"
    );
    // The file materialized in the working copy (the handler's vault
    // materialize ran) and the memory lists.
    assert!(repo_dir.join(".vault/memory/design.md").exists());
    let listed = run(repo_dir, home_dir, &["memory", "list"]);
    assert!(listed.contains("design"), "{listed}");
}

#[test]
fn triage_candidates_route_with_json_parity() {
    let (repo, home) = fixture("triage-candidates");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    record_file(
        repo_dir,
        home_dir,
        "feature.txt",
        "feature\n",
        "feature change",
    );
    run(repo_dir, home_dir, &["view", "create", "feature-x"]);
    run(repo_dir, home_dir, &["view", "switch", "feature-x"]);
    record_file(repo_dir, home_dir, "draft.txt", "draft\n", "draft change");
    run(repo_dir, home_dir, &["view", "switch", "dev"]);

    let json = run(
        repo_dir,
        home_dir,
        &[
            "triage",
            "candidates",
            "feature-x",
            "--into",
            "dev",
            "--json",
        ],
    );
    let set: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(set["feature"], serde_json::json!("feature-x"));
    assert_eq!(set["target"], serde_json::json!("dev"));
    assert_eq!(
        set["only_in_feature"].as_array().unwrap().len(),
        1,
        "the draft change is the candidate"
    );
    let plain = run(
        repo_dir,
        home_dir,
        &["triage", "candidates", "feature-x", "--into", "dev"],
    );
    assert!(
        plain.contains("Triage candidates: feature-x → dev"),
        "{plain}"
    );
    // The root-view refusal matches the local semantics.
    let root = atomic(repo_dir, home_dir, &["triage", "candidates", "dev"]);
    assert!(!root.status.success());
    assert!(String::from_utf8_lossy(&root.stderr).contains("is a root view"));
}

#[test]
fn kg_extras_reindex_and_graph_route() {
    let (repo, home) = fixture("kg-extras");
    let (repo_dir, home_dir) = (repo.path(), home.path());
    record_file(
        repo_dir,
        home_dir,
        "greet.rs",
        "pub fn greet() {}\n",
        "add greet",
    );
    let reindexed = run(repo_dir, home_dir, &["query", "reindex"]);
    assert!(reindexed.contains("Indexed "), "{reindexed}");
    assert!(reindexed.contains(" nodes + edges."), "{reindexed}");
    let graph = run(repo_dir, home_dir, &["query", "graph", "greet", "--json"]);
    let value: serde_json::Value = serde_json::from_str(&graph).unwrap();
    let nodes = value["nodes"].as_array().unwrap();
    assert!(!nodes.is_empty(), "the seeded graph carries nodes");
    assert!(
        nodes
            .iter()
            .any(|node| node["id"] == serde_json::json!("file:greet.rs")),
        "the seed file is present: {graph}"
    );
}
