//! `atomic diagnostics` end to end: a hermetic home, the real binary.
#![cfg(not(windows))]

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

struct Fixture {
    work: TempDir,
    home: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            work: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_atomic"));
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("ATOMIC_") {
                command.env_remove(name);
            }
        }
        command
            .args(args)
            .current_dir(self.work.path())
            .env("HOME", self.home.path())
            .env("ATOMIC_HOME", self.home.path().join(".atomic"))
            .env("ATOMIC_CONFIG_DIR", self.home.path().join(".atomic"))
            .env("ATOMIC_NONINTERACTIVE", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("NO_COLOR", "1")
            .output()
            .expect("run atomic")
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn json(&self) -> Value {
        let text = self.ok(&["diagnostics", "--json"]);
        serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("one JSON document expected: {error}: {text}"))
    }

    /// An installed integration whose recorded file is still on disk, and
    /// whose settings file carries its hook command.
    fn install_integration(&self, agent: &str, version: &str) {
        let home = self.home.path();
        let file = home.join(".claude/skills/atomic-vault/SKILL.md");
        let body = "---\nname: atomic-vault\nversion: 2.0.0\n---\n";
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, body).unwrap();
        let settings = home.join(".claude/settings.json");
        std::fs::write(
            &settings,
            r#"{"hooks":{"Stop":[{"hooks":[{"command":"atomic agent hooks claude-code stop"}]}]}}"#,
        )
        .unwrap();
        let receipt = serde_json::json!({
            "schema": 1,
            "agent": agent,
            "version": version,
            "cli_version": "0.19.0",
            "installed_at": "2026-10-01T12:00:00+00:00",
            "source": "https://token:secret@example.com/pkg",
            "files": [{
                "dst": file,
                "blake3": blake3::hash(body.as_bytes()).to_hex().to_string(),
            }],
            "settings": [{
                "target": settings,
                "hooks_key": "hooks",
                "command_prefix": "atomic agent hooks claude-code",
            }],
        });
        let dir = home.join(".atomic/integrations").join(agent);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("receipt.json"), receipt.to_string()).unwrap();
    }
}

fn find<'v>(list: &'v Value, key: &str, name: &str) -> &'v Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|entry| entry[key] == name)
        .unwrap_or_else(|| panic!("{name} listed in {list}"))
}

#[test]
fn outside_a_workspace_the_report_still_succeeds_with_what_it_can_read() {
    let fixture = Fixture::new();
    let report = fixture.json();

    assert_eq!(report["schema"], 1);
    assert!(report["atomic"]["version"]
        .as_str()
        .unwrap()
        .starts_with(env!("CARGO_PKG_VERSION")));
    assert_eq!(report["workspace"]["status"], "not_found");
    assert_eq!(report["git_shim"]["status"], "not_applicable");
    assert_eq!(report["skills"]["package"]["status"], "not_found");
    assert_eq!(
        find(&report["integrations"]["packages"], "agent", "claude-code")["status"],
        "not_installed"
    );
    assert_eq!(
        find(&report["agents"], "name", "claude-code")["enabled"],
        "no"
    );

    let text = fixture.ok(&["diagnostics"]);
    assert!(text.starts_with("Atomic diagnostics\n"));
    assert!(text.contains("  Workspace:       none (not inside an Atomic workspace)\n"));
}

#[test]
fn installed_integration_and_enabled_agent_are_reported_separately_and_redacted() {
    let fixture = Fixture::new();
    fixture.install_integration("claude-code", "1.4.0");

    let report = fixture.json();
    let claude = find(&report["integrations"]["packages"], "agent", "claude-code");
    assert_eq!(claude["status"], "installed");
    assert_eq!(claude["version"], "1.4.0");
    assert_eq!(claude["files"]["present"], 1);
    assert_eq!(claude["source"], "https://***@example.com/pkg");
    assert_eq!(
        find(&report["agents"], "name", "claude-code")["enabled"],
        "yes"
    );

    let skill = &report["skills"]["installed"][0];
    assert_eq!(skill["name"], "atomic-vault");
    assert_eq!(skill["version"], "2.0.0");
    assert_eq!(skill["path"], "~/.claude/skills/atomic-vault/SKILL.md");

    let everything = format!("{report}{}", fixture.ok(&["diagnostics"]));
    assert!(
        !everything.contains("secret"),
        "credentials never reach the report"
    );
    let home = fixture.home.path().display().to_string();
    assert!(!everything.contains(&home), "home is shown as ~");
}

#[test]
fn inside_a_git_backed_workspace_the_shim_is_reported_and_nothing_changes() {
    let fixture = Fixture::new();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(fixture.work.path())
        .status()
        .unwrap()
        .success());
    fixture.ok(&["init", "--view", "main"]);
    let before = snapshot(fixture.work.path());

    let report = fixture.json();
    assert_eq!(report["workspace"]["status"], "found");
    assert_eq!(report["git_shim"]["status"], "disabled");
    let dispatchers = report["git_shim"]["hooks"]["dispatchers"]
        .as_array()
        .unwrap();
    assert_eq!(dispatchers.len(), 5);
    assert!(dispatchers.iter().all(|d| d["state"] == "not_installed"));
    fixture.ok(&["diagnostics"]);

    assert_eq!(
        before,
        snapshot(fixture.work.path()),
        "diagnostics is read-only"
    );
}

/// Every file under `root` with its length and modification time.
fn snapshot(root: &Path) -> Vec<(String, u64, std::time::SystemTime)> {
    let mut entries = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                entries.push((
                    entry.path().display().to_string(),
                    meta.len(),
                    meta.modified().unwrap(),
                ));
            }
        }
    }
    entries.sort();
    entries
}
