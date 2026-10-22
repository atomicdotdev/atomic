//! User-defined kinds across views, driven through the binary.
//!
//! The kinds proposal (atomicdotdev/atomic#276) leans on existing patch-theory
//! machinery rather than adding its own: an instance depends on its
//! definition because the definition's change created the kind directory,
//! `insert` pulls that dependency along, `unrecord` refuses to orphan it, and
//! the registry a view sees is built from that view's files only. These tests
//! pin what that machinery actually does.
#![cfg(not(windows))]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

const DEF_PATH: &str = ".vault/kinds/adr/_kind.md";

const DEF_C: &str = "---
name: adr
specVersion: 1
fields:
  status:
    type: enum
    values: [proposed, accepted]
---
Version C.
";

const DEF_D: &str = "---
name: adr
specVersion: 1
fields:
  owner:
    type: string
---
Version D.
";

struct Fixture {
    repo: TempDir,
    home: TempDir,
}

impl Fixture {
    /// A repository whose `dev` view holds one recorded file.
    fn new() -> Self {
        let fixture = Fixture {
            repo: TempDir::new().unwrap(),
            home: TempDir::new().unwrap(),
        };
        fixture.ok(&["init"]);
        fs::write(fixture.root().join("base.txt"), "base\n").unwrap();
        fixture.ok(&["add", "base.txt"]);
        fixture.ok(&["record", "-m", "base"]);
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

    /// Run, assert success, return stdout + stderr.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        let text = text(&out);
        assert!(
            out.status.success(),
            "{args:?} failed ({:?}):\n{text}",
            out.status.code()
        );
        text
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(out.status.success(), "{args:?}:\n{}", text(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root().join(rel)
    }

    /// Create a draft view under `dev` and switch to it.
    fn draft(&self, name: &str) {
        self.ok(&[
            "view", "create", name, "--draft", "--parent", "dev", "--switch",
        ]);
    }

    /// `add` and `record` exactly `path`, returning the new change's full hash.
    fn record_path(&self, message: &str, path: &str) -> String {
        self.ok(&["add", path]);
        self.ok(&["record", "-m", message, path]);
        let log = self.json(&["log", "--format", "json", "-n", "1"]);
        assert_eq!(log[0]["message"], message, "{log}");
        log[0]["hash"].as_str().unwrap().to_string()
    }

    fn dependencies(&self, hash: &str) -> Vec<String> {
        let change = self.json(&["change", hash, "--format", "json", "--full-hash"]);
        change["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d.as_str().unwrap().to_string())
            .collect()
    }

    fn log_hashes(&self, view: &str) -> Vec<String> {
        self.json(&["log", "--format", "json", "--view", view])
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["hash"].as_str().unwrap().to_string())
            .collect()
    }

    fn write_definition(&self, content: &str) {
        fs::create_dir_all(self.path(".vault/kinds/adr")).unwrap();
        fs::write(self.path(DEF_PATH), content).unwrap();
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// On draft view `a`: the starter `adr` definition recorded alone, then one
/// instance recorded as its own change.
struct KindThenInstance {
    f: Fixture,
    definition: String,
    instance: String,
    instance_path: String,
}

fn kind_then_instance() -> KindThenInstance {
    let f = Fixture::new();
    f.draft("a");
    f.ok(&["kind", "init", "adr"]);
    let definition = f.record_path("feat: add adr kind", DEF_PATH);

    let created = f.json(&["kind", "new", "adr", "--title", "First", "--json"]);
    let instance_path = created["file"].as_str().unwrap().to_string();
    let instance = f.record_path("docs: first adr", &instance_path);
    KindThenInstance {
        f,
        definition,
        instance,
        instance_path,
    }
}

/// Scenario 1. The instance change depends on the definition change, because
/// the definition's change created `.vault/kinds/adr/` and globalize adds a
/// dependency on the change that created a new file's parent directory.
#[test]
fn instance_change_depends_on_definition_change() {
    let s = kind_then_instance();
    assert_ne!(s.definition, s.instance);
    assert_eq!(s.f.dependencies(&s.instance), vec![s.definition.clone()]);

    let shown = s.f.ok(&["change", &s.instance, "--show-deps"]);
    assert!(shown.contains("Dependencies: 1"), "{shown}");
    assert!(shown.contains("feat: add adr kind"), "{shown}");
}

/// Scenario 2. Inserting only the instance change into a sibling view brings
/// the definition along through the dependency closure.
#[test]
fn insert_of_instance_brings_definition() {
    let s = kind_then_instance();
    let f = &s.f;
    f.draft("b");
    assert!(!f.path(DEF_PATH).exists());
    assert!(f.log_hashes("b").is_empty());

    let inserted = f.ok(&["insert", &s.instance]);
    assert!(inserted.contains("Inserted"), "{inserted}");

    let log = f.log_hashes("b");
    assert!(log.contains(&s.definition), "{log:?}");
    assert!(log.contains(&s.instance), "{log:?}");
    assert!(f.path(DEF_PATH).is_file());
    assert!(f.path(&s.instance_path).is_file());

    let report = f.json(&["kind", "validate", "--view", "b", "--json"]);
    assert_eq!(report["ok"], true, "{report}");
    assert_eq!(report["kinds"][0]["name"], "adr");
    assert_eq!(report["kinds"][0]["status"], "registered");
    assert_eq!(report["kinds"][0]["instance_count"], 1);
    assert_eq!(report["kinds"][0]["instances"][0]["conforms"], true);
    f.ok(&["kind", "validate", "--view", "b"]);
}

/// Scenario 3. Unrecording the definition while an instance depends on it is
/// refused, and nothing is removed.
///
/// The proposal names `UnrecordError::HasDependents`; the CLI actually goes
/// through `Repository::unrecord`'s view-closure check, which returns
/// `RepositoryError::Unrecord(..)`. The CLI then renders that as an internal
/// error ("appears to be a bug", exit 128), not as a usage error. The refusal
/// is what the proposal needs; the exit code and wording are pre-existing.
#[test]
fn unrecord_of_definition_is_refused_while_instance_depends_on_it() {
    let s = kind_then_instance();
    let f = &s.f;

    let out = f.run(&["unrecord", &s.definition]);
    let msg = text(&out);
    assert!(!out.status.success(), "{msg}");
    assert_eq!(out.status.code(), Some(128), "{msg}");
    assert!(
        msg.contains(&format!("Cannot unrecord {}", s.definition)),
        "{msg}"
    );
    assert!(msg.contains(&s.instance), "{msg}");
    assert!(msg.contains("depends on it"), "{msg}");

    assert!(f.path(DEF_PATH).is_file());
    assert!(f.path(&s.instance_path).is_file());
    let log = f.log_hashes("a");
    assert!(log.contains(&s.definition) && log.contains(&s.instance));
    f.ok(&["kind", "validate", "--view", "a"]);
}

/// Scenario 5. The registry is per view: a definition recorded only on `a`
/// is not a kind of `dev`, which `a` inherits from but which never saw it.
#[test]
fn registry_is_per_view() {
    let s = kind_then_instance();
    let f = &s.f;

    let a = f.json(&["kind", "validate", "--view", "a", "--json"]);
    assert_eq!(a["source"], serde_json::json!({"view": "a"}));
    assert_eq!(a["kinds"][0]["name"], "adr", "{a}");
    assert_eq!(a["kinds"][0]["instance_count"], 1, "{a}");

    let dev = f.json(&["kind", "validate", "--view", "dev", "--json"]);
    assert_eq!(dev["ok"], true, "{dev}");
    assert_eq!(dev["kinds"], serde_json::json!([]), "{dev}");
    assert_eq!(dev["problems"], serde_json::json!([]), "{dev}");
    let human = f.ok(&["kind", "validate", "--view", "dev"]);
    assert!(human.contains("No kinds in the view dev"), "{human}");
}

/// Views `c` and `d` each record a different `.vault/kinds/adr/_kind.md`;
/// we are left on `d`. Returns the hash of `c`'s definition change.
fn divergent_definitions() -> (Fixture, String) {
    let f = Fixture::new();
    f.draft("c");
    f.write_definition(DEF_C);
    let on_c = f.record_path("feat: adr kind (c)", DEF_PATH);

    f.ok(&["view", "switch", "dev"]);
    f.draft("d");
    f.write_definition(DEF_D);
    f.record_path("feat: adr kind (d)", DEF_PATH);
    (f, on_c)
}

/// Scenario 4 (flagged "not yet runtime-tested" by the proposal). Two views
/// independently add the same `_kind.md` path, then one view's definition
/// change is inserted into the other.
///
/// What happens: the insert succeeds (exit 0) and both changes stay in `d`,
/// but each view created its own inodes for `.vault/kinds`, `.vault/kinds/adr`
/// and `_kind.md`, so all three paths become *name conflicts* (two claimants
/// for one path), not a content merge. The materialized `_kind.md` holds both
/// versions between name-conflict markers, and `kind validate` on the working
/// copy fails closed: the kind stays unregistered because of the markers.
///
/// `kind validate --view d` does not report the conflict; see
/// `view_validate_reports_name_conflicted_definition`.
#[test]
fn same_definition_path_on_two_views_becomes_a_name_conflict() {
    let (f, on_c) = divergent_definitions();
    assert_eq!(fs::read_to_string(f.path(DEF_PATH)).unwrap(), DEF_D);

    let inserted = f.ok(&["insert", &on_c]);
    assert!(inserted.contains("Inserted"), "{inserted}");
    assert!(inserted.contains("conflicted files"), "{inserted}");
    assert!(
        inserted.contains(".vault/kinds/adr/_kind.md (line 1)"),
        "{inserted}"
    );
    assert!(f.log_hashes("d").contains(&on_c));

    // Both versions, between name-conflict markers (atomic writes `>>>>>>>`
    // first and `<<<<<<<` last for name conflicts).
    let file = fs::read_to_string(f.path(DEF_PATH)).unwrap();
    assert_eq!(
        file,
        format!(
            ">>>>>>> {DEF_PATH} (name conflict)\n{DEF_C}=======\n{DEF_D}\
             <<<<<<< {DEF_PATH} (name conflict)\n"
        )
    );

    let status = f.ok(&["status"]);
    for path in [".vault/kinds", ".vault/kinds/adr", DEF_PATH] {
        assert!(
            status
                .lines()
                .any(|l| l.contains("name conflict") && l.trim_end().ends_with(path)),
            "{path}: {status}"
        );
    }

    let out = f.run(&["kind", "validate"]);
    let msg = text(&out);
    assert_eq!(out.status.code(), Some(2), "{msg}");
    assert!(
        msg.contains(
            "✗ .vault/kinds/adr/_kind.md: the file contains unresolved merge-conflict markers"
        ),
        "{msg}"
    );
    assert!(msg.contains("adr stays unregistered"), "{msg}");

    let out = f.run(&["kind", "validate", "--json"]);
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["ok"], false, "{report}");
    assert_eq!(report["kinds"][0]["name"], "adr", "{report}");
    assert_ne!(report["kinds"][0]["status"], "registered", "{report}");

    // `c` itself is untouched.
    let c = f.json(&["kind", "validate", "--view", "c", "--json"]);
    assert_eq!(c["ok"], true, "{c}");
    assert_eq!(c["kinds"][0]["status"], "registered", "{c}");
}

/// After scenario 4, `kind validate --view d` should fail closed, as the
/// working-copy check does. Today it exits 0 with "No kinds in the view d":
/// `Repository::visible_file_paths` omits name-conflicted paths (and
/// `get_file_content_on_view` refuses them), so the view source of the
/// registry never sees `_kind.md` and the kind silently disappears.
#[test]
#[ignore = "kinds view source drops name-conflicted definitions (fails open)"]
fn view_validate_reports_name_conflicted_definition() {
    let (f, on_c) = divergent_definitions();
    f.ok(&["insert", &on_c]);

    let out = f.run(&["kind", "validate", "--view", "d"]);
    let msg = text(&out);
    assert_eq!(out.status.code(), Some(2), "{msg}");
    assert!(msg.contains("conflict"), "{msg}");
}
