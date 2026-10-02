//! Process-level coverage for what the git shim writes to the log file:
//! bridge events, the evidence Git's hooks journal, the reconcile and switch
//! spans with their decisions, and the messages each command printed, all
//! inside the command's span.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

/// A Git repository with an isolated home. Git passes the environment on to
/// the hooks it runs, so the dispatched `atomic` processes log to the same
/// file.
struct Sandbox {
    root: TempDir,
    home: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            root: TempDir::new().unwrap(),
            home: TempDir::new().unwrap(),
        }
    }

    fn dir(&self) -> &Path {
        self.root.path()
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(self.dir())
            .env("HOME", self.home.path())
            .env("ATOMIC_CONFIG_DIR", self.home.path().join(".atomic"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Shim Logging")
            .env("GIT_AUTHOR_EMAIL", "shim@example.com")
            .env("GIT_COMMITTER_NAME", "Shim Logging")
            .env("GIT_COMMITTER_EMAIL", "shim@example.com")
            .env_remove("ATOMIC_LOG")
            .env_remove("ATOMIC_LOG_DIR")
            .env_remove("RUST_LOG");
        command
    }

    fn atomic(&self, args: &[&str]) -> Output {
        self.command(ATOMIC_BIN)
            .args(args)
            .output()
            .expect("run atomic")
    }

    fn atomic_ok(&self, args: &[&str]) -> Output {
        let output = self.atomic(args);
        assert!(output.status.success(), "atomic {args:?}: {output:?}");
        output
    }

    fn git(&self, args: &[&str]) {
        let output = self.command("git").args(args).output().expect("run git");
        assert!(output.status.success(), "git {args:?}: {output:?}");
    }

    fn commit(&self, path: &str, content: &str, message: &str) {
        fs::write(self.dir().join(path), content).unwrap();
        self.git(&["add", path]);
        self.git(&["commit", "-q", "-m", message]);
    }

    fn log_dir(&self) -> PathBuf {
        self.home.path().join(".atomic").join("logs")
    }

    fn log(&self) -> String {
        let mut files: Vec<PathBuf> = fs::read_dir(self.log_dir())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        files.sort();
        files
            .iter()
            .map(|path| fs::read_to_string(path).unwrap())
            .collect()
    }
}

impl Sandbox {
    /// Wait until a log line contains every needle.
    fn wait_for_log(&self, needles: &[&str]) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let log = self.log();
            if log
                .lines()
                .any(|line| needles.iter().all(|needle| line.contains(needle)))
            {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("no line with {needles:?} in:\n{}", self.log());
    }
}

/// The first log line containing every needle.
fn line_with<'a>(log: &'a str, needles: &[&str]) -> &'a str {
    log.lines()
        .find(|line| needles.iter().all(|needle| line.contains(needle)))
        .unwrap_or_else(|| panic!("no line with {needles:?} in:\n{log}"))
}

/// A bridge-enabled repository on `main`, then a commit on `feature` made
/// through Git so its hooks run, reconciled into Atomic.
fn bridged_feature_commit() -> Sandbox {
    let sandbox = Sandbox::new();
    sandbox.git(&["init", "-q", "-b", "main"]);
    sandbox.commit("a.txt", "a\n", "one");
    sandbox.atomic_ok(&["init"]);
    sandbox.atomic_ok(&["git", "import"]);
    sandbox.atomic_ok(&["git", "bridge", "enable"]);

    // With -v, whose `atomic=debug` would show an `info` line, so the copy
    // stays off the terminal only because its target is file-only.
    let aligned = sandbox.atomic_ok(&["-v", "git", "bridge", "reconcile"]);
    let stdout = String::from_utf8_lossy(&aligned.stdout);
    let stderr = String::from_utf8_lossy(&aligned.stderr);
    assert_eq!(
        stdout.matches("Git HEAD already matches").count(),
        1,
        "printed once: {stdout}"
    );
    assert!(
        !stderr.contains("Git HEAD already matches"),
        "the file copy never reaches the terminal: {stderr}"
    );

    sandbox.git(&["checkout", "-q", "-b", "feature"]);
    // post-checkout starts a detached observer; let it finish inside the
    // sandbox before going on.
    sandbox.wait_for_log(&["cmd=git bridge observe-deferred", "command finished"]);
    sandbox.commit("b.txt", "b\n", "two");
    sandbox.atomic_ok(&["git", "bridge", "reconcile"]);
    sandbox
}

#[test]
fn reconcile_logs_its_decision_and_outcome_inside_its_span() {
    let sandbox = bridged_feature_commit();
    let log = sandbox.log();

    // The aligned run: the bridge event, and the message it printed.
    line_with(
        &log,
        &[
            "cmd=git bridge reconcile",
            "reconcile{",
            "atomic::bridge::event",
            r#""event":"reconcile""#,
            r#""direction":"neither""#,
            r#""outcome":"no_change""#,
        ],
    );
    line_with(
        &log,
        &[
            "cmd=git bridge reconcile",
            "atomic::printed",
            "Git HEAD already matches the current Atomic view",
        ],
    );

    // The run after the Git commit: the direction is logged before the
    // import runs, and the import happens inside the reconcile span.
    line_with(
        &log,
        &["reconcile{", "reconcile direction", "direction=GitToAtomic"],
    );
    line_with(&log, &["reconcile{", "import_head{", "close time.busy"]);
    line_with(
        &log,
        &[
            "reconcile{",
            r#""direction":"git_to_atomic""#,
            r#""outcome":"applied""#,
        ],
    );
}

#[test]
fn git_hooks_log_the_evidence_they_journal() {
    let sandbox = bridged_feature_commit();
    let log = sandbox.log();

    line_with(
        &log,
        &[
            "cmd=git bridge hook-post-checkout",
            "atomic::git::hook",
            r#""record_type":"post-checkout""#,
            r#""checkout_kind":"branch""#,
        ],
    );
    line_with(
        &log,
        &[
            "cmd=git bridge hook-reference-transaction",
            "atomic::git::hook",
            r#""state":"committed""#,
            "refs/heads/feature",
        ],
    );
    // The observer post-checkout scheduled logs what it saw of Git.
    line_with(
        &log,
        &[
            "cmd=git bridge observe-deferred",
            "atomic::git::hook",
            r#""record_type":"deferred-observation""#,
            r#""head_kind":"attached""#,
        ],
    );
    assert!(
        !log.contains("ref-movement-only"),
        "the fixed interpretation text stays in the journal"
    );
}

#[test]
fn bridge_switch_logs_the_steps_it_reached() {
    let sandbox = bridged_feature_commit();
    // Whatever the outcome, the log shows how far the switch got.
    let switched = sandbox.atomic(&["git", "bridge", "switch", "main"]);
    let log = sandbox.log();

    line_with(
        &log,
        &[
            "cmd=git bridge switch",
            r#"switch{view="main"}"#,
            "checkpoint matches",
        ],
    );
    let outcome = if switched.status.success() {
        "outcome=ok"
    } else {
        "outcome=error"
    };
    line_with(
        &log,
        &["cmd=git bridge switch", "command finished", outcome],
    );
    line_with(&log, &[r#"switch{view="main"}"#, "close time.busy"]);
}
