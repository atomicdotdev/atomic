//! `atomic diagnostics` end to end: a hermetic home, the real binary.
#![cfg(not(windows))]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

use serde_json::Value;
use tempfile::TempDir;

struct Fixture {
    work: TempDir,
    home: TempDir,
    binary: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir_in(home.path()).unwrap();
        // Cargo can release its target lock while tests run, allowing another
        // worktree to replace the shared binary between fixtures or commands.
        static BINARY: OnceLock<(Vec<u8>, std::fs::Permissions)> = OnceLock::new();
        let (bytes, permissions) = BINARY.get_or_init(|| {
            let path = Path::new(env!("CARGO_BIN_EXE_atomic"));
            (
                std::fs::read(path).expect("read test binary"),
                std::fs::metadata(path)
                    .expect("stat test binary")
                    .permissions(),
            )
        });
        let binary = home.path().join("atomic-under-test");
        std::fs::write(&binary, bytes).expect("pin test binary");
        std::fs::set_permissions(&binary, permissions.clone())
            .expect("make test binary executable");
        Self { work, home, binary }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_at(self.work.path(), args)
    }

    fn run_at(&self, work: &Path, args: &[&str]) -> Output {
        let mut command = Command::new(&self.binary);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("ATOMIC_")
                || name.to_string_lossy().starts_with("GIT_")
            {
                command.env_remove(name);
            }
        }
        command
            .args(args)
            .current_dir(work)
            .env("HOME", self.home.path())
            .env("ATOMIC_HOME", self.home.path().join(".atomic"))
            .env("ATOMIC_CONFIG_DIR", self.home.path().join(".atomic"))
            .env("ATOMIC_NONINTERACTIVE", "1")
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("XDG_DATA_HOME", self.home.path().join(".local/share"))
            .env("XDG_CACHE_HOME", self.home.path().join(".cache"))
            .env("XDG_STATE_HOME", self.home.path().join(".local/state"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("NO_COLOR", "1")
            .output()
            .expect("run atomic")
    }

    fn ok_at(&self, work: &Path, args: &[&str]) -> String {
        let output = self.run_at(work, args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("SYNTHETIC_"));
        String::from_utf8(output.stdout).unwrap()
    }

    fn git(&self, work: &Path, args: &[&str]) {
        let output = Command::new("git")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", self.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .current_dir(work)
            .args(args)
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?}: {output:?}");
    }

    fn linked_git_worktree(&self, atomic: bool) -> std::path::PathBuf {
        let source = self.home.path().join("source");
        std::fs::create_dir(&source).unwrap();
        self.git(&source, &["init", "-q"]);
        self.git(
            &source,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "initial",
            ],
        );
        if atomic {
            self.ok_at(&source, &["init", "--view", "main"]);
        }
        self.git(
            &source,
            &[
                "worktree",
                "add",
                "--detach",
                self.work.path().to_str().unwrap(),
                "HEAD",
            ],
        );
        let pointer = std::fs::read_to_string(self.work.path().join(".git")).unwrap();
        let git_dir = pointer.trim().strip_prefix("gitdir: ").unwrap();
        let path = Path::new(git_dir);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.work.path().join(path)
        }
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("SYNTHETIC_"),
            "synthetic source values must not reach stderr"
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn assert_home_redacted(&self, output: &str) {
        for home in [
            self.home.path().to_path_buf(),
            self.home.path().canonicalize().unwrap(),
        ] {
            assert!(
                !output.contains(&home.display().to_string()),
                "home directory must be normalized"
            );
        }
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
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"atomic agent hooks claude-code stop"}]}]}}"#,
        )
        .unwrap();
        let receipt = serde_json::json!({
            "schema": 1,
            "agent": agent,
            "version": version,
            "cli_version": "0.19.0",
            "installed_at": "2026-10-01T12:00:00+00:00",
            "source": "https://token:secret@example.com/pkg?token=SYNTHETIC_QUERY#SYNTHETIC_FRAGMENT",
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

#[test]
fn claude_enabled_requires_an_active_hook_not_unrelated_text() {
    let fixture = Fixture::new();
    fixture.install_integration("claude-code", "1.4.0");
    let settings = fixture.home.path().join(".claude/settings.json");
    for body in [
        r#"{"notes":"atomic agent hooks claude-code is not installed","hooks":{}}"#,
        r#"{"disableAllHooks":true,"hooks":{"Stop":[{"hooks":[{"type":"command","command":"atomic agent hooks claude-code stop"}]}]}}"#,
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"prompt","prompt":"atomic agent hooks claude-code stop"}]}]}}"#,
    ] {
        std::fs::write(&settings, body).unwrap();
        let report = fixture.json();
        assert_eq!(
            find(&report["integrations"]["packages"], "agent", "claude-code")["status"],
            "installed"
        );
        assert_ne!(
            find(&report["agents"], "name", "claude-code")["enabled"],
            "yes",
            "{body}"
        );
        let text = fixture.ok(&["diagnostics"]);
        let line = text
            .lines()
            .find(|line| line.contains("Claude Code (claude-code):"))
            .expect("Claude listed");
        assert!(!line.contains("yes"), "{line}");
    }
}

#[test]
fn built_in_claude_hooks_work_without_a_receipt() {
    let fixture = Fixture::new();
    let path = fixture.home.path().join(".claude/settings.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"test -d .atomic || test -f .atomic-sandbox && atomic agent hooks claude-code stop || true"}]}]}}"#).unwrap();
    let report = fixture.json();
    assert_eq!(
        find(&report["integrations"]["packages"], "agent", "claude-code")["status"],
        "not_installed"
    );
    assert_eq!(
        find(&report["agents"], "name", "claude-code")["enabled"],
        "yes"
    );
    assert!(fixture
        .ok(&["diagnostics"])
        .contains("Claude Code (claude-code): yes"));
}

#[test]
fn receipt_hook_key_cannot_promote_an_unrelated_settings_field() {
    let fixture = Fixture::new();
    fixture.install_integration("claude-code", "1.4.0");
    let settings = fixture.home.path().join(".claude/settings.json");
    let hooks = serde_json::json!({"Stop":[{"hooks":[{"type":"command","command":"atomic agent hooks claude-code stop"}]}]});
    std::fs::write(
        &settings,
        serde_json::json!({"hooks":{},"otherHooks":hooks}).to_string(),
    )
    .unwrap();
    let receipt = fixture
        .home
        .path()
        .join(".atomic/integrations/claude-code/receipt.json");
    let mut data: Value = serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
    data["settings"][0]["hooks_key"] = "otherHooks".into();
    std::fs::write(receipt, data.to_string()).unwrap();
    assert_eq!(
        find(&fixture.json()["agents"], "name", "claude-code")["enabled"],
        "unknown"
    );
}

#[test]
fn built_in_codex_and_gemini_hooks_require_active_native_entries() {
    for (agent, path, event, verb) in [
        ("codex", ".codex/hooks.json", "Stop", "stop"),
        (
            "gemini-cli",
            ".gemini/settings.json",
            "AfterAgent",
            "after-agent",
        ),
    ] {
        let fixture = Fixture::new();
        let path = fixture.home.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut settings = serde_json::json!({
            "hooks": {event: [{"hooks": [{"type":"command", "name":"atomic-test", "command":format!("atomic agent hooks {agent} {verb}")}]}]}
        });
        std::fs::write(&path, settings.to_string()).unwrap();
        assert_eq!(
            find(&fixture.json()["agents"], "name", agent)["enabled"],
            "yes",
            "{agent}"
        );
        settings["hooks"][event][0]["hooks"][0]["disabled"] = true.into();
        std::fs::write(&path, settings.to_string()).unwrap();
        assert_ne!(
            find(&fixture.json()["agents"], "name", agent)["enabled"],
            "yes",
            "{agent}"
        );
    }
}

#[test]
fn malformed_repository_config_never_discloses_values_in_either_renderer() {
    let fixture = Fixture::new();
    fixture.ok(&["init", "--view", "main"]);
    let config = fixture.work.path().join(".atomic/config.toml");
    let cases = [
        "[remotes.origin]\nurl = \"https://SYNTHETIC_USER:SYNTHETIC_PASSWORD@example.com/repo?token=SYNTHETIC_QUERY#SYNTHETIC_FRAGMENT\" typo\n",
        "[git.shim]\nenabled = \"SYNTHETIC_TYPE_VALUE\"\n",
    ];
    let mut leaks = Vec::new();
    for input in cases {
        std::fs::write(&config, input).unwrap();
        let text = fixture.ok(&["diagnostics"]);
        let json = fixture.ok(&["diagnostics", "--json"]);
        let report: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(report["git_shim"]["status"], "unknown");
        assert!(report["git_shim"]["reason"]
            .as_str()
            .unwrap()
            .contains("configuration"));
        for (format, output) in [("text", &text), ("json", &json)] {
            if output.contains("SYNTHETIC_") {
                leaks.push(format);
            }
            if output.contains(&fixture.home.path().display().to_string()) {
                leaks.push("home path");
            }
            fixture.assert_home_redacted(output);
        }
    }
    assert!(
        leaks.is_empty(),
        "configuration values leaked into formats: {leaks:?}"
    );
}

#[test]
fn installed_skill_metadata_and_missing_fields_are_reported() {
    let fixture = Fixture::new();
    fixture.install_integration("claude-code", "1.4.0");
    let skill = fixture
        .home
        .path()
        .join(".claude/skills/atomic-vault/SKILL.md");
    std::fs::write(&skill, "---\nname: atomic-vault\nmetadata:\n  version: \"1.0.0\"\n  updatedAt: \"2026-10-28T18:18:12Z\"\n---\nbody\n").unwrap();
    let report = fixture.json();
    let installed = find(&report["skills"]["installed"], "name", "atomic-vault");
    assert_eq!(installed["version"], "1.0.0");
    assert_eq!(installed["updated_at"], "2026-10-28T18:18:12Z");
    assert_eq!(installed["metadata_status"], "complete");
    assert_eq!(installed["missing_metadata"], serde_json::json!([]));
    let text = fixture.ok(&["diagnostics"]);
    assert!(text.contains("v1.0.0, updated 2026-10-28T18:18:12Z"));

    std::fs::write(
        &skill,
        "---\nname: atomic-vault\nmetadata:\n  version: '1.0.0'\n---\n",
    )
    .unwrap();
    let report = fixture.json();
    let installed = find(&report["skills"]["installed"], "name", "atomic-vault");
    assert_eq!(installed["metadata_status"], "incomplete");
    assert_eq!(
        installed["missing_metadata"],
        serde_json::json!(["metadata.updatedAt"])
    );
    assert!(fixture
        .ok(&["diagnostics"])
        .contains("missing metadata.updatedAt"));

    std::fs::write(&skill, "---\nname: atomic-vault\n---\n").unwrap();
    let report = fixture.json();
    let installed = find(&report["skills"]["installed"], "name", "atomic-vault");
    assert_eq!(installed["metadata_status"], "incomplete");
    assert_eq!(
        installed["missing_metadata"],
        serde_json::json!(["metadata.version", "metadata.updatedAt"])
    );
    assert!(fixture
        .ok(&["diagnostics"])
        .contains("older or unversioned copy"));

    std::fs::write(&skill, "---\nmetadata: [SYNTHETIC_PRIVATE_DETAIL\n---\n").unwrap();
    let report = fixture.json();
    let installed = find(&report["skills"]["installed"], "name", "atomic-vault");
    assert_eq!(installed["metadata_status"], "invalid");
    assert!(!report.to_string().contains("SYNTHETIC_PRIVATE_DETAIL"));
    let text = fixture.ok(&["diagnostics"]);
    assert!(text.contains("invalid skill metadata"));
    assert!(!text.contains("SYNTHETIC_PRIVATE_DETAIL"));

    std::fs::remove_file(&skill).unwrap();
    std::fs::create_dir(&skill).unwrap();
    let report = fixture.json();
    let installed = find(&report["skills"]["installed"], "name", "atomic-vault");
    assert_eq!(installed["metadata_status"], "unavailable");
    assert!(fixture
        .ok(&["diagnostics"])
        .contains("skill metadata unavailable"));
}

fn hook_event(agent: &str, at: &str, outcome: &str) -> atomic_agent::hook_log::SessionLogEvent {
    atomic_agent::hook_log::SessionLogEvent {
        event_id: uuid::Uuid::new_v4().to_string(),
        session_id: None,
        parent_session_id: None,
        agent: agent.to_string(),
        display_name: agent.to_string(),
        verb: "session-start".to_string(),
        outcome: outcome.to_string(),
        detail: Some("SYNTHETIC_PRIVATE_DETAIL".to_string()),
        at: at.to_string(),
    }
}

#[test]
fn last_fired_uses_repository_history_and_pending_events() {
    let fixture = Fixture::new();
    fixture.ok(&["init", "--view", "main"]);
    fixture.install_integration("claude-code", "1.4.0");
    {
        let store = atomic_repository::redb_change_store::RedbChangeStore::open_existing(
            fixture.work.path().join(".atomic/atomic.redb"),
        )
        .unwrap();
        store
            .append_session_log(&hook_event("claude-code", "2026-10-01T12:00:00Z", "ok"))
            .unwrap();
        store
            .append_session_log(&hook_event("other-agent", "2026-12-01T12:00:00Z", "ok"))
            .unwrap();
        store
            .append_session_log(&hook_event(
                "claude-code",
                "2026-10-02T12:00:00+03:00",
                "error",
            ))
            .unwrap();
    }
    atomic_agent::hook_log::spool(
        fixture.work.path(),
        &hook_event("claude-code", "2026-10-02T11:00:00Z", "error"),
    )
    .unwrap();
    let report = fixture.json();
    let integration = find(&report["integrations"]["packages"], "agent", "claude-code");
    assert_eq!(integration["last_fired"]["status"], "observed");
    assert_eq!(integration["last_fired"]["at"], "2026-10-02T11:00:00+00:00");
    let text = fixture.ok(&["diagnostics"]);
    assert!(text.contains("Last fired:"));
    assert!(text.contains("2026-10-02T11:00:00+00:00 (current repository)"));
    assert!(!text.contains("SYNTHETIC_PRIVATE_DETAIL"));
    assert!(!report.to_string().contains("SYNTHETIC_PRIVATE_DETAIL"));
}

#[test]
fn last_fired_distinguishes_missing_and_unavailable_history() {
    let fixture = Fixture::new();
    fixture.install_integration("claude-code", "1.4.0");
    let report = fixture.json();
    let integration = find(&report["integrations"]["packages"], "agent", "claude-code");
    assert_eq!(integration["last_fired"]["status"], "unavailable");
    fixture.ok(&["init", "--view", "main"]);
    let report = fixture.json();
    let integration = find(&report["integrations"]["packages"], "agent", "claude-code");
    assert_eq!(integration["last_fired"]["status"], "not_observed");
    let pending = fixture.work.path().join(".atomic/hook-log-pending");
    std::fs::create_dir_all(&pending).unwrap();
    std::fs::write(pending.join("bad.json"), "SYNTHETIC_PRIVATE_DETAIL").unwrap();
    let report = fixture.json();
    let integration = find(&report["integrations"]["packages"], "agent", "claude-code");
    assert_eq!(integration["last_fired"]["status"], "unavailable");
    assert!(!report.to_string().contains("SYNTHETIC_PRIVATE_DETAIL"));
    let text = fixture.ok(&["diagnostics"]);
    assert!(text.contains("Last fired:    unknown"));
    assert!(!text.contains("SYNTHETIC_PRIVATE_DETAIL"));
}

#[test]
fn last_fired_is_populated_by_a_real_hook_dispatch() {
    let fixture = Fixture::new();
    fixture.ok(&["init", "--view", "main"]);
    fixture.install_integration("claude-code", "1.4.0");
    let mut command = Command::new(&fixture.binary);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("ATOMIC_")
            || name.to_string_lossy().starts_with("GIT_")
        {
            command.env_remove(name);
        }
    }
    let mut hook = command
        .args([
            "agent",
            "hooks",
            "claude-code",
            "session-start",
            "--foreground",
        ])
        .current_dir(fixture.work.path())
        .env("HOME", fixture.home.path())
        .env("ATOMIC_HOME", fixture.home.path().join(".atomic"))
        .env("ATOMIC_CONFIG_DIR", fixture.home.path().join(".atomic"))
        .env("ATOMIC_SERVICE", "local")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    hook.stdin.take().unwrap().write_all(b"{").unwrap();
    assert!(!hook.wait_with_output().unwrap().status.success());
    let report = fixture.json();
    let integration = find(&report["integrations"]["packages"], "agent", "claude-code");
    assert_eq!(integration["last_fired"]["status"], "observed");
    chrono::DateTime::parse_from_rfc3339(integration["last_fired"]["at"].as_str().unwrap())
        .expect("hook dispatch timestamp");
}

#[test]
fn last_fired_reports_unknown_when_database_is_in_use() {
    let fixture = Fixture::new();
    fixture.ok(&["init", "--view", "main"]);
    fixture.install_integration("claude-code", "1.4.0");
    let _held = atomic_repository::redb_change_store::RedbChangeStore::open_existing(
        fixture.work.path().join(".atomic/atomic.redb"),
    )
    .unwrap();
    let report = fixture.json();
    let integration = find(&report["integrations"]["packages"], "agent", "claude-code");
    assert_eq!(integration["last_fired"]["status"], "unavailable");
    assert!(integration["last_fired"]["reason"]
        .as_str()
        .unwrap()
        .contains("in use"));
}

#[test]
fn malformed_receipt_and_manifest_never_disclose_values_in_either_renderer() {
    let fixture = Fixture::new();
    let integration = fixture.home.path().join(".atomic/integrations/claude-code");
    std::fs::create_dir_all(&integration).unwrap();
    std::fs::write(
        integration.join("receipt.json"),
        r#"{"schema":"SYNTHETIC_RECEIPT_SECRET"}"#,
    )
    .unwrap();
    let cache = fixture
        .home
        .path()
        .join(".atomic/integrations/atomic-skills/repo");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(
        cache.join("atomic-integration.toml"),
        "schema = \"SYNTHETIC_MANIFEST_SECRET\"\n",
    )
    .unwrap();
    let text = fixture.ok(&["diagnostics"]);
    let json = fixture.ok(&["diagnostics", "--json"]);
    let report: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        find(&report["integrations"]["packages"], "agent", "claude-code")["status"],
        "unreadable"
    );
    assert_eq!(
        find(&report["agents"], "name", "claude-code")["enabled"],
        "unknown"
    );
    assert_eq!(report["skills"]["package"]["status"], "unreadable");
    for output in [&text, &json] {
        assert!(
            !output.contains("SYNTHETIC_"),
            "source values must not reach diagnostics"
        );
        fixture.assert_home_redacted(output);
    }
}

#[test]
fn unreadable_configuration_and_settings_remain_unknown_without_home_paths() {
    let fixture = Fixture::new();
    fixture.ok(&["init", "--view", "main"]);
    let config = fixture.work.path().join(".atomic/config.toml");
    std::fs::remove_file(&config).unwrap();
    std::fs::create_dir(&config).unwrap();
    fixture.install_integration("claude-code", "1.4.0");
    let settings = fixture.home.path().join(".claude/settings.json");
    std::fs::remove_file(&settings).unwrap();
    std::fs::create_dir(&settings).unwrap();

    let text = fixture.ok(&["diagnostics"]);
    let json = fixture.ok(&["diagnostics", "--json"]);
    let report: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(report["git_shim"]["status"], "unknown");
    assert_eq!(
        find(&report["agents"], "name", "claude-code")["enabled"],
        "unknown"
    );
    assert!(report["git_shim"]["reason"]
        .as_str()
        .unwrap()
        .contains("configuration"));
    for output in [&text, &json] {
        fixture.assert_home_redacted(output);
    }
}

#[test]
fn git_inspection_errors_do_not_echo_configuration_or_home_paths() {
    let fixture = Fixture::new();
    assert!(Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .args(["init", "-q"])
        .current_dir(fixture.work.path())
        .env("HOME", fixture.home.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .unwrap()
        .success());
    fixture.ok(&["init", "--view", "main"]);
    std::fs::write(
        fixture.work.path().join(".git/config"),
        "[core]\n  bare = SYNTHETIC_GIT_SECRET\n",
    )
    .unwrap();
    let text = fixture.ok(&["diagnostics"]);
    let json = fixture.ok(&["diagnostics", "--json"]);
    let report: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(report["git_shim"]["hooks"]["status"], "unreadable");
    assert!(report["git_shim"]["hooks"]["reason"]
        .as_str()
        .unwrap()
        .contains("Git"));
    for output in [&text, &json] {
        assert!(
            !output.contains("SYNTHETIC_GIT_SECRET"),
            "Git configuration values must not reach diagnostics"
        );
        fixture.assert_home_redacted(output);
    }
}

#[test]
fn plugin_file_presence_alone_does_not_prove_enablement() {
    let fixture = Fixture::new();
    let path = fixture
        .home
        .path()
        .join(".config/opencode/plugins/atomic-hooks.ts");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "// not an active Atomic hook\n").unwrap();
    let report = fixture.json();
    assert_eq!(
        find(&report["agents"], "name", "opencode")["enabled"],
        "unknown"
    );
}

#[test]
fn unreadable_receipt_and_manifest_are_unknown_with_safe_reasons() {
    let fixture = Fixture::new();
    let receipt = fixture
        .home
        .path()
        .join(".atomic/integrations/claude-code/receipt.json");
    let manifest = fixture
        .home
        .path()
        .join(".atomic/integrations/atomic-skills/repo/atomic-integration.toml");
    std::fs::create_dir_all(receipt).unwrap();
    std::fs::create_dir_all(manifest).unwrap();
    let text = fixture.ok(&["diagnostics"]);
    let json = fixture.ok(&["diagnostics", "--json"]);
    let report: Value = serde_json::from_str(&json).unwrap();
    let integration = find(&report["integrations"]["packages"], "agent", "claude-code");
    assert_eq!(integration["status"], "unreadable");
    assert!(integration["reason"].as_str().unwrap().contains("receipt"));
    assert_eq!(
        find(&report["agents"], "name", "claude-code")["enabled"],
        "unknown"
    );
    assert_eq!(report["skills"]["package"]["status"], "unreadable");
    assert!(report["skills"]["package"]["reason"]
        .as_str()
        .unwrap()
        .contains("manifest"));
    fixture.assert_home_redacted(&text);
    fixture.assert_home_redacted(&json);
}

#[test]
fn malformed_claude_settings_are_unknown_with_and_without_a_receipt() {
    for receipt in [false, true] {
        let fixture = Fixture::new();
        if receipt {
            fixture.install_integration("claude-code", "1.0.0");
        }
        let settings = fixture.home.path().join(".claude/settings.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, "{ SYNTHETIC_SETTINGS_SECRET").unwrap();
        let report = fixture.json();
        let claude = find(&report["agents"], "name", "claude-code");
        assert_eq!(claude["enabled"], "unknown", "receipt={receipt}: {claude}");
        assert!(!claude["evidence"].as_array().unwrap().is_empty());
        let text = fixture.ok(&["diagnostics"]);
        assert!(text.contains("Claude Code (claude-code): unknown"));
        assert!(text.contains("settings could not be parsed"));
        assert!(!format!("{report}{text}").contains("SYNTHETIC_SETTINGS_SECRET"));
    }
}

#[test]
fn unreadable_workspace_and_home_settings_remain_unknown() {
    for workspace in [false, true] {
        let fixture = Fixture::new();
        if workspace {
            fixture.ok(&["init", "--view", "main"]);
        }
        let base = if workspace {
            fixture.work.path()
        } else {
            fixture.home.path()
        };
        std::fs::create_dir_all(base.join(".claude/settings.json")).unwrap();
        let report = fixture.json();
        let claude = find(&report["agents"], "name", "claude-code");
        assert_eq!(claude["enabled"], "unknown");
        assert!(claude["evidence"].to_string().contains("could not be read"));
    }
}

#[test]
fn malformed_builtin_hook_formats_without_receipts_are_unknown() {
    let fixture = Fixture::new();
    for (name, relative) in [
        ("gemini-cli", ".gemini/settings.json"),
        ("codex", ".codex/hooks.json"),
    ] {
        let path = fixture.home.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "{ bad JSON SYNTHETIC_SETTINGS_SECRET").unwrap();
        let report = fixture.json();
        let agent = find(&report["agents"], "name", name);
        assert_eq!(agent["enabled"], "unknown", "{name}: {agent}");
        assert!(!agent.to_string().contains("SYNTHETIC_SETTINGS_SECRET"));
    }
}

#[test]
fn conflicting_or_unreadable_scopes_never_prove_enabled() {
    let fixture = Fixture::new();
    fixture.ok(&["init", "--view", "main"]);
    fixture.install_integration("claude-code", "1.0.0");
    let path = fixture.work.path().join(".claude/settings.local.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    for body in [
        r#"{"disableAllHooks":true}"#,
        r#"{"hooks":{"Stop":[{"disabled":true,"hooks":[{"type":"command","command":"atomic agent hooks claude-code stop"}]}]}}"#,
        "{ malformed",
    ] {
        std::fs::write(&path, body).unwrap();
        let report = fixture.json();
        assert_eq!(
            find(&report["agents"], "name", "claude-code")["enabled"],
            "unknown",
            "{body}"
        );
    }
    std::fs::write(&path, r#"{"hooks":{}}"#).unwrap();
    assert_eq!(
        find(&fixture.json()["agents"], "name", "claude-code")["enabled"],
        "yes"
    );
}

#[test]
fn receipt_cannot_make_an_unloaded_settings_file_active() {
    let fixture = Fixture::new();
    fixture.install_integration("claude-code", "1.0.0");
    let actual = fixture.home.path().join(".claude/settings.json");
    let other = fixture.home.path().join("SYNTHETIC_RECEIPT_PATH.json");
    std::fs::copy(&actual, &other).unwrap();
    std::fs::write(&actual, r#"{"hooks":{}}"#).unwrap();
    let receipt = fixture
        .home
        .path()
        .join(".atomic/integrations/claude-code/receipt.json");
    let mut data: Value = serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
    data["settings"][0]["target"] = other.to_string_lossy().into_owned().into();
    std::fs::write(receipt, data.to_string()).unwrap();
    let report = fixture.json();
    assert_eq!(
        find(&report["agents"], "name", "claude-code")["enabled"],
        "unknown"
    );
    assert!(!format!("{report}{}", fixture.ok(&["diagnostics"])).contains("SYNTHETIC_RECEIPT_PATH"));
}

#[cfg(unix)]
#[test]
fn inaccessible_settings_permissions_are_unknown_when_enforced() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let path = fixture.home.path().join(".claude/settings.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, r#"{"hooks":{}}"#).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&path).is_err() {
        assert_eq!(
            find(&fixture.json()["agents"], "name", "claude-code")["enabled"],
            "unknown"
        );
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[cfg(unix)]
#[test]
fn dangling_settings_symlink_is_not_treated_as_missing() {
    let fixture = Fixture::new();
    let path = fixture.home.path().join(".claude/settings.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(fixture.home.path().join("missing"), path).unwrap();
    let report = fixture.json();
    assert_eq!(
        find(&report["agents"], "name", "claude-code")["enabled"],
        "unknown"
    );
}

#[cfg(unix)]
#[test]
fn dangling_workspace_marker_keeps_discovery_unknown() {
    let fixture = Fixture::new();
    std::os::unix::fs::symlink(
        fixture.work.path().join("SYNTHETIC_MISSING_TARGET"),
        fixture.work.path().join(".atomic-sandbox"),
    )
    .unwrap();
    let report = fixture.json();
    assert_eq!(report["workspace"]["status"], "unreadable");
    assert_eq!(
        find(&report["agents"], "name", "claude-code")["enabled"],
        "unknown"
    );
    assert!(
        !format!("{report}{}", fixture.ok(&["diagnostics"])).contains("SYNTHETIC_MISSING_TARGET")
    );
}

#[test]
fn linked_git_without_atomic_is_not_a_workspace() {
    let fixture = Fixture::new();
    fixture.linked_git_worktree(false);
    let report = fixture.json();
    assert_eq!(report["workspace"]["status"], "not_found");
    assert_eq!(
        find(&report["agents"], "name", "claude-code")["enabled"],
        "no"
    );
    let text = fixture.ok(&["diagnostics"]);
    assert!(text
        .lines()
        .any(|line| line.contains("Not enabled") && line.contains("claude-code")));
}

#[test]
fn linked_atomic_workspace_finds_active_workspace_hooks() {
    let fixture = Fixture::new();
    fixture.linked_git_worktree(true);
    let settings = fixture.work.path().join(".claude/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(settings, r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"atomic agent hooks claude-code stop"}]}]}}"#).unwrap();
    let report = fixture.json();
    assert_eq!(report["workspace"]["status"], "found");
    assert_eq!(
        find(&report["agents"], "name", "claude-code")["enabled"],
        "yes"
    );
    assert!(fixture
        .ok(&["diagnostics"])
        .contains("Claude Code (claude-code): yes"));
}

#[test]
fn broken_linked_git_administration_makes_workspace_unknown() {
    let fixture = Fixture::new();
    let git_dir = fixture.linked_git_worktree(false);
    std::fs::write(git_dir.join("commondir"), "SYNTHETIC_INVALID_COMMONDIR\n").unwrap();
    let pointer = std::fs::read(fixture.work.path().join(".git")).unwrap();
    let settings = fixture.work.path().join(".claude/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(settings, r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"atomic agent hooks claude-code stop"}]}]}}"#).unwrap();
    let json = fixture.ok(&["diagnostics", "--json"]);
    let report: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(report["workspace"]["status"], "unreadable");
    assert_eq!(
        find(&report["agents"], "name", "claude-code")["enabled"],
        "unknown"
    );
    let text = fixture.ok(&["diagnostics"]);
    assert!(text.contains("Claude Code (claude-code): unknown"));
    assert!(!format!("{text}{json}").contains("SYNTHETIC_INVALID_COMMONDIR"));
    fixture.assert_home_redacted(&text);
    fixture.assert_home_redacted(&json);
    assert_eq!(
        std::fs::read(fixture.work.path().join(".git")).unwrap(),
        pointer
    );
    assert_eq!(
        std::fs::read(git_dir.join("commondir")).unwrap(),
        b"SYNTHETIC_INVALID_COMMONDIR\n"
    );
}

#[test]
fn valid_atomic_parent_wins_over_broken_nested_git() {
    let fixture = Fixture::new();
    fixture.ok(&["init", "--view", "main"]);
    let nested = fixture.work.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join(".git"), "gitdir: SYNTHETIC_BROKEN_GITDIR\n").unwrap();
    let settings = fixture.work.path().join(".claude/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(settings, r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"atomic agent hooks claude-code stop"}]}]}}"#).unwrap();
    let json = fixture.ok_at(&nested, &["diagnostics", "--json"]);
    let report: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(report["workspace"]["status"], "found");
    assert_eq!(
        find(&report["agents"], "name", "claude-code")["enabled"],
        "yes"
    );
    let text = fixture.ok_at(&nested, &["diagnostics"]);
    assert!(text.contains("Claude Code (claude-code): yes"));
    assert!(!format!("{text}{json}").contains("SYNTHETIC_BROKEN_GITDIR"));

    let nested_link = fixture.work.path().join("nested-link");
    std::fs::create_dir(&nested_link).unwrap();
    std::os::unix::fs::symlink("SYNTHETIC_MISSING_GITDIR", nested_link.join(".git")).unwrap();
    let json = fixture.ok_at(&nested_link, &["diagnostics", "--json"]);
    let report: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(report["workspace"]["status"], "found");
    assert_eq!(
        find(&report["agents"], "name", "claude-code")["enabled"],
        "yes"
    );
    assert!(!json.contains("SYNTHETIC_MISSING_GITDIR"));
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
    assert!(
        !everything.contains("SYNTHETIC_"),
        "query and fragment are removed"
    );
    let home = fixture.home.path().display().to_string();
    assert!(!everything.contains(&home), "home is shown as ~");
}

#[test]
fn inside_a_git_backed_workspace_the_shim_is_reported_and_nothing_changes() {
    let fixture = Fixture::new();
    assert!(Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .args(["init", "-q"])
        .current_dir(fixture.work.path())
        .env("HOME", fixture.home.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
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
