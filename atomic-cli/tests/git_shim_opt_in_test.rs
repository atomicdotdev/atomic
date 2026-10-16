//! The colocated Git shim is explicit per-repository opt-in (RFC §2,
//! CB-13C `[git.shim] enabled`, default false). A `.git` directory next to
//! `.atomic` is not consent: without the opt-in and without a checkpoint
//! written by an explicit shim command, ordinary Atomic commands keep their
//! native behavior and never refuse for a missing Git anchor, never write the
//! Git index, and never create Git refs. Once the user opts in, an unanchored
//! workspace refuses with the exact commands that resolve it.

// Temporary Unix-only Git shim support to unblock Windows native CI.
// Revisit after Windows support is defined; see the RFC temporary platform policy.
#![cfg(unix)]
use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

struct Fixture {
    repository: TempDir,
    home: TempDir,
}

impl Fixture {
    /// `git init` (unborn HEAD, no commits) + `atomic init`: the reported
    /// "start both from scratch" sequence.
    fn fresh_git_init() -> Self {
        let fixture = Self {
            repository: TempDir::new().expect("repository tempdir"),
            home: TempDir::new().expect("home tempdir"),
        };
        fixture.git_ok(&["init", "-q", "-b", "main"]);
        fixture.git_ok(&["config", "user.name", "Opt-in Tests"]);
        fixture.git_ok(&["config", "user.email", "opt-in@example.com"]);
        fixture.git_ok(&["config", "commit.gpgsign", "false"]);
        fixture.git_ok(&["config", "core.autocrlf", "false"]);
        fixture.atomic_ok(&["init", "--no-vault", "--view", "main"]);
        fixture
    }

    /// An existing Git repository with one commit, then `atomic init`.
    fn existing_git_history() -> Self {
        let fixture = Self {
            repository: TempDir::new().expect("repository tempdir"),
            home: TempDir::new().expect("home tempdir"),
        };
        fixture.git_ok(&["init", "-q", "-b", "main"]);
        fixture.git_ok(&["config", "user.name", "Opt-in Tests"]);
        fixture.git_ok(&["config", "user.email", "opt-in@example.com"]);
        fixture.git_ok(&["config", "commit.gpgsign", "false"]);
        fixture.git_ok(&["config", "core.autocrlf", "false"]);
        fs::write(fixture.root().join("base.txt"), b"base\n").unwrap();
        fixture.git_ok(&["add", "base.txt"]);
        fixture.git_ok(&["commit", "--no-gpg-sign", "-q", "-m", "base"]);
        fixture.atomic_ok(&["init", "--no-vault", "--view", "main"]);
        fixture
    }

    fn root(&self) -> &Path {
        self.repository.path()
    }

    fn atomic(&self, args: &[&str]) -> Output {
        Command::new(ATOMIC_BIN)
            .args(args)
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("ATOMIC_HOME", self.home.path().join(".atomic"))
            .env("ATOMIC_NONINTERACTIVE", "1")
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("TERM", "dumb")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run atomic")
    }

    fn atomic_ok(&self, args: &[&str]) -> Output {
        let output = self.atomic(args);
        assert_success(&output, &format!("atomic {}", args.join(" ")));
        output
    }

    fn git(&self, args: &[&str]) -> Output {
        Command::new("git")
            .args(args)
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run git")
    }

    fn git_ok(&self, args: &[&str]) -> Output {
        let output = self.git(args);
        assert_success(&output, &format!("git {}", args.join(" ")));
        output
    }

    fn git_text(&self, args: &[&str]) -> String {
        String::from_utf8(self.git_ok(args).stdout).expect("UTF-8 Git output")
    }

    fn hooks_installed(&self) -> Vec<String> {
        let mut hooks: Vec<String> = fs::read_dir(self.root().join(".git/hooks"))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| !name.ends_with(".sample"))
                    .collect()
            })
            .unwrap_or_default();
        hooks.sort();
        hooks
    }

    /// Run one OpenCode agent hook dispatch the way the atomic-opencode
    /// plugin does: JSON on stdin, foreground, from the repository root.
    fn agent_hook(&self, verb: &str, session_id: &str) -> Output {
        use std::io::Write;
        let payload = format!(
            "{{\"session_id\":\"{session_id}\",\"cwd\":\"{}\",\"timestamp\":\"2026-10-14T10:30:00Z\"}}",
            self.root().display()
        );
        let mut child = Command::new(ATOMIC_BIN)
            .args(["agent", "hooks", "opencode", verb, "--foreground"])
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("ATOMIC_HOME", self.home.path().join(".atomic"))
            .env("ATOMIC_NONINTERACTIVE", "1")
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("TERM", "dumb")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn atomic agent hooks");
        child
            .stdin
            .as_mut()
            .expect("hook stdin")
            .write_all(payload.as_bytes())
            .expect("write hook stdin");
        child.wait_with_output().expect("wait for agent hook")
    }

    /// A retained shadow exclude marker without shim enrollment: written by
    /// import/push, kept on disk by `git shim disable`, and never removed by
    /// plain `atomic init` inside an existing Git checkout.
    fn retain_shadow_marker(&self) {
        fs::create_dir_all(self.root().join(".git/info")).unwrap();
        fs::write(self.root().join(".git/info/exclude"), b"/.atomic/\n").unwrap();
    }
}

fn assert_success(output: &Output, what: &str) {
    if !output.status.success() {
        panic!(
            "{what} failed with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Every ordinary command used in the reported sequence succeeds natively,
/// and none of them touches the user's Git index, refs, or hooks.
fn assert_native_workflow(fixture: &Fixture, git_index_before: &str) {
    fs::write(fixture.root().join("f.txt"), b"hello\n").unwrap();
    fixture.atomic_ok(&["add", "f.txt"]);
    fixture.atomic_ok(&["status"]);
    let json = fixture.atomic_ok(&["status", "--json"]);
    serde_json::from_slice::<Value>(&json.stdout).expect("status --json emits JSON");
    fixture.atomic_ok(&["record", "-m", "first"]);

    fs::write(fixture.root().join("f.txt"), b"hello again\n").unwrap();
    let diff = fixture.atomic_ok(&["diff", "--name-only", "--no-color"]);
    assert!(output_text(&diff).contains("f.txt"));
    fixture.atomic_ok(&["stash", "push"]);
    fixture.atomic_ok(&["stash", "pop"]);
    assert_eq!(
        fs::read(fixture.root().join("f.txt")).unwrap(),
        b"hello again\n"
    );
    fixture.atomic_ok(&["tag", "create", "v1"]);

    assert_eq!(
        fixture.git_text(&["ls-files", "--stage"]),
        git_index_before,
        "ordinary Atomic commands must not write the Git index without the shim opt-in"
    );
    assert_eq!(
        fixture.git_text(&["tag", "--list"]),
        "",
        "`atomic tag create` must not export Git tags without the shim opt-in"
    );
    assert!(
        fixture.hooks_installed().is_empty(),
        "no Git hooks may be installed without the opt-in: {:?}",
        fixture.hooks_installed()
    );
    assert!(!fixture
        .root()
        .join(".atomic/bridge/workspace.json")
        .exists());
}

#[test]
fn fresh_git_init_without_shim_keeps_native_commands_working() {
    let fixture = Fixture::fresh_git_init();
    assert_native_workflow(&fixture, "");
}

#[test]
fn existing_git_history_without_shim_keeps_native_commands_working() {
    let fixture = Fixture::existing_git_history();
    let index_before = fixture.git_text(&["ls-files", "--stage"]);
    assert_native_workflow(&fixture, &index_before);
}

#[test]
fn status_does_not_mix_git_index_state_without_shim_opt_in() {
    let fixture = Fixture::existing_git_history();
    fs::write(fixture.root().join("git-only.txt"), b"staged in git\n").unwrap();
    fixture.git_ok(&["add", "git-only.txt"]);

    let status = fixture.atomic_ok(&["status", "--short"]);
    let text = output_text(&status);
    assert!(
        text.contains("?? git-only.txt"),
        "a Git-staged file Atomic does not track stays untracked in native status:\n{text}"
    );
}

#[test]
fn opted_in_workspace_without_anchor_refuses_with_actionable_remediation() {
    let fixture = Fixture::existing_git_history();
    fixture.atomic_ok(&["git", "shim", "enable"]);

    let refused = fixture.atomic(&["status"]);
    assert!(
        !refused.status.success(),
        "unanchored shim workspace must refuse"
    );
    let text = output_text(&refused);
    assert!(text.contains("MissingCheckpoint"), "{text}");
    assert!(text.contains("atomic git shim reconcile"), "{text}");
    assert!(text.contains("atomic git shim disable"), "{text}");

    fixture.atomic_ok(&["git", "shim", "reconcile"]);
    fixture.atomic_ok(&["status"]);
}

#[test]
fn opted_in_unborn_head_names_the_first_commit_remediation() {
    let fixture = Fixture::fresh_git_init();
    fixture.atomic_ok(&["git", "shim", "enable"]);
    fs::write(fixture.root().join("f.txt"), b"hello\n").unwrap();

    let refused = fixture.atomic(&["add", "f.txt"]);
    assert!(
        !refused.status.success(),
        "unborn shim workspace must refuse"
    );
    let text = output_text(&refused);
    assert!(text.contains("UnbornHead"), "{text}");
    assert!(text.contains("first Git commit"), "{text}");
    assert!(text.contains("atomic git shim reconcile"), "{text}");
}

#[test]
fn disabling_an_unanchored_shim_restores_native_behavior() {
    let fixture = Fixture::fresh_git_init();
    fixture.atomic_ok(&["git", "shim", "enable"]);
    assert!(!fixture.atomic(&["status"]).status.success());

    fixture.atomic_ok(&["git", "shim", "disable"]);
    fixture.atomic_ok(&["status"]);
}

/// The OpenCode agent bridge dispatches `atomic agent hooks opencode <verb>`
/// on every tool call. In a colocated-but-unenrolled repository with a
/// retained shadow exclude marker, dispatch must stay native: it may never
/// demand a shim checkpoint (the reported every-tool-call exit 128) and
/// never bootstrap shim state.
#[test]
fn agent_hooks_stay_native_with_a_retained_shadow_marker() {
    let fixture = Fixture::existing_git_history();
    fixture.retain_shadow_marker();
    let index_before = fixture.git_text(&["ls-files", "--stage"]);

    for verb in ["session-start", "user-prompt", "before-tool", "after-tool"] {
        let output = fixture.agent_hook(verb, "ses-gate");
        assert_success(&output, &format!("agent hooks opencode {verb}"));
    }

    assert!(
        !fixture
            .root()
            .join(".atomic/bridge/workspace.json")
            .exists(),
        "agent hooks must not bootstrap the shim checkpoint"
    );
    assert_eq!(
        fixture.git_text(&["ls-files", "--stage"]),
        index_before,
        "agent hooks must not write the Git index without the shim opt-in"
    );
    assert_eq!(
        fixture.git_text(&["for-each-ref", "--format=%(refname)", "refs/atomic"]),
        "",
        "agent hooks must not create Atomic Git refs without the opt-in"
    );
}

/// Hook dispatch outside an Atomic repository exits quietly: the plugin's
/// per-dispatch gating normally prevents the call entirely, and a repository
/// that disappears between probe and dispatch must not surface as a hook
/// failure (which the bridge plugins' strict tool propagation would turn
/// into a blocked tool call).
#[test]
fn agent_hooks_exit_quietly_outside_a_repository() {
    for with_git in [false, true] {
        let directory = TempDir::new().expect("tempdir");
        let home = TempDir::new().expect("home tempdir");
        let root = directory.path();
        if with_git {
            let output = Command::new("git")
                .args(["init", "-q", "-b", "main"])
                .current_dir(root)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .expect("git init");
            assert_success(&output, "git init");
        }
        let payload = format!(
            "{{\"session_id\":\"ses-none\",\"cwd\":\"{}\",\"timestamp\":\"2026-10-14T10:30:00Z\"}}",
            root.display()
        );
        use std::io::Write;
        let mut child = Command::new(ATOMIC_BIN)
            .args([
                "agent",
                "hooks",
                "opencode",
                "session-start",
                "--foreground",
            ])
            .current_dir(root)
            .env("HOME", home.path())
            .env("ATOMIC_NONINTERACTIVE", "1")
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn atomic agent hooks");
        child
            .stdin
            .as_mut()
            .expect("hook stdin")
            .write_all(payload.as_bytes())
            .expect("write hook stdin");
        let output = child.wait_with_output().expect("wait for agent hook");
        assert_success(&output, "agent hooks opencode session-start");
        assert!(
            !root.join(".atomic").exists(),
            "hook dispatch must never create a repository"
        );
    }
}

/// An enrolled-but-unanchored workspace keeps its fail-closed refusal with
/// the anchor-or-disable remediation at the agent boundary too.
#[test]
fn agent_hooks_refuse_with_remediation_when_enrolled_unanchored() {
    let fixture = Fixture::existing_git_history();
    fixture.atomic_ok(&["git", "shim", "enable"]);

    let refused = fixture.agent_hook("session-start", "ses-enrolled");
    assert!(
        !refused.status.success(),
        "enrolled but unanchored shim must refuse the agent boundary"
    );
    let text = output_text(&refused);
    assert!(text.contains("MissingCheckpoint"), "{text}");
    assert!(text.contains("atomic git shim reconcile"), "{text}");
    assert!(text.contains("atomic git shim disable"), "{text}");
    assert!(
        !fixture
            .root()
            .join(".atomic/bridge/workspace.json")
            .exists(),
        "the refusal must not anchor the shim"
    );
}
