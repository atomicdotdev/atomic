//! The colocated Git bridge is explicit per-repository opt-in (RFC §2,
//! CB-13C `[git.bridge] enabled`, default false). A `.git` directory next to
//! `.atomic` is not consent: without the opt-in and without a checkpoint
//! written by an explicit bridge command, ordinary Atomic commands keep their
//! native behavior and never refuse for a missing Git anchor, never write the
//! Git index, and never create Git refs. Once the user opts in, an unanchored
//! workspace refuses with the exact commands that resolve it.

#![cfg(not(windows))]
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
        "ordinary Atomic commands must not write the Git index without the bridge opt-in"
    );
    assert_eq!(
        fixture.git_text(&["tag", "--list"]),
        "",
        "`atomic tag create` must not export Git tags without the bridge opt-in"
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
fn fresh_git_init_without_bridge_keeps_native_commands_working() {
    let fixture = Fixture::fresh_git_init();
    assert_native_workflow(&fixture, "");
}

#[test]
fn existing_git_history_without_bridge_keeps_native_commands_working() {
    let fixture = Fixture::existing_git_history();
    let index_before = fixture.git_text(&["ls-files", "--stage"]);
    assert_native_workflow(&fixture, &index_before);
}

#[test]
fn status_does_not_mix_git_index_state_without_bridge_opt_in() {
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
    fixture.atomic_ok(&["git", "bridge", "enable"]);

    let refused = fixture.atomic(&["status"]);
    assert!(
        !refused.status.success(),
        "unanchored bridge workspace must refuse"
    );
    let text = output_text(&refused);
    assert!(text.contains("MissingCheckpoint"), "{text}");
    assert!(text.contains("atomic git bridge reconcile"), "{text}");
    assert!(text.contains("atomic git bridge disable"), "{text}");

    fixture.atomic_ok(&["git", "bridge", "reconcile"]);
    fixture.atomic_ok(&["status"]);
}

#[test]
fn opted_in_unborn_head_names_the_first_commit_remediation() {
    let fixture = Fixture::fresh_git_init();
    fixture.atomic_ok(&["git", "bridge", "enable"]);
    fs::write(fixture.root().join("f.txt"), b"hello\n").unwrap();

    let refused = fixture.atomic(&["add", "f.txt"]);
    assert!(
        !refused.status.success(),
        "unborn bridge workspace must refuse"
    );
    let text = output_text(&refused);
    assert!(text.contains("UnbornHead"), "{text}");
    assert!(text.contains("first Git commit"), "{text}");
    assert!(text.contains("atomic git bridge reconcile"), "{text}");
}

#[test]
fn disabling_an_unanchored_bridge_restores_native_behavior() {
    let fixture = Fixture::fresh_git_init();
    fixture.atomic_ok(&["git", "bridge", "enable"]);
    assert!(!fixture.atomic(&["status"]).status.success());

    fixture.atomic_ok(&["git", "bridge", "disable"]);
    fixture.atomic_ok(&["status"]);
}
