//! Regression coverage for atomicdotdev/atomic#330: changes recorded by the
//! Claude Code agent hooks ("managed-session" changes) must be publishable
//! through the trusted provenance gate once their session has been attested
//! by the user's own identity.
//!
//! Each test drives the real `atomic agent hooks claude-code <verb>` entry
//! points the way `atomic agent enable` installs them, in a Git repository
//! imported with `atomic git import`, under an isolated `HOME` /
//! `ATOMIC_CONFIG_DIR` so the identity store is private to the test.
//!
//! Before the fix:
//!
//! * After `SessionEnd`, the attestation is signed by the user's default
//!   identity, yet the gate reports that signer as "unknown (not
//!   configured)", so both `atomic git push` and `atomic insert` into a
//!   shared view refuse every hook-recorded change.
//! * A repository that names its identity in `[author] identity` (by
//!   identity name, as `atomic org set` writes it) still never trusts it,
//!   because the name is compared verbatim against the signer DID.
//! * Refusals do not say how to recover: the in-session refusal does not say
//!   that attestations are written at session end, and the untrusted-signer
//!   refusal does not say how to configure `[git.trust]`.

// The hooks drive the Git shim, which is Unix-only for now.
#![cfg(unix)]

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

const USER: &str = "repro";
const AGENT: &str = "repro-claude";

struct Fixture {
    home: TempDir,
    repository: TempDir,
    session: String,
}

impl Fixture {
    /// The issue's setup: a default user identity, a delegated claude-code
    /// agent identity selected for the hooks, and a Git repository imported
    /// into Atomic.
    fn new() -> Self {
        let fixture = Self {
            home: TempDir::new().expect("home tempdir"),
            repository: TempDir::new().expect("repository tempdir"),
            session: format!("issue-330-{}", std::process::id()),
        };
        fixture.atomic_ok(&[
            "identity",
            "new",
            USER,
            "-e",
            "repro@example.invalid",
            "--set-default",
        ]);
        fixture.atomic_ok(&[
            "identity",
            "agent",
            "create",
            AGENT,
            "--agent-type",
            "claude-code",
            "--local",
            "--expires",
            "1d",
        ]);
        fixture.atomic_ok(&["agent", "identity", "set", &format!("{USER}+{AGENT}")]);

        fixture.git_ok(&["init", "-q", "-b", "main"]);
        fixture.git_ok(&["config", "user.name", USER]);
        fixture.git_ok(&["config", "user.email", "repro@example.invalid"]);
        fixture.git_ok(&["config", "commit.gpgsign", "false"]);
        fixture.git_ok(&["config", "maintenance.auto", "false"]);
        fs::write(fixture.root().join("README.md"), b"hello\n").expect("write README");
        fixture.git_ok(&["add", "-A"]);
        fixture.git_ok(&["commit", "-q", "-m", "init"]);
        fixture.atomic_ok(&["git", "import", "--no-vault"]);
        fixture
    }

    fn root(&self) -> &Path {
        self.repository.path()
    }

    fn config_dir(&self) -> std::path::PathBuf {
        self.home.path().join(".atomic")
    }

    fn atomic_with_stdin(&self, args: &[&str], input: Option<&[u8]>) -> Output {
        let mut command = Command::new(ATOMIC_BIN);
        command
            .args(args)
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("ATOMIC_HOME", self.config_dir())
            .env("ATOMIC_CONFIG_DIR", self.config_dir())
            .env("ATOMIC_NONINTERACTIVE", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("TERM", "dumb")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if input.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command.spawn().expect("spawn atomic");
        if let Some(input) = input {
            child
                .stdin
                .take()
                .expect("atomic stdin")
                .write_all(input)
                .expect("write atomic stdin");
        }
        child.wait_with_output().expect("wait for atomic")
    }

    fn atomic(&self, args: &[&str]) -> Output {
        self.atomic_with_stdin(args, None)
    }

    fn atomic_ok(&self, args: &[&str]) -> Output {
        let output = self.atomic(args);
        assert_success(&output, &format!("atomic {}", args.join(" ")));
        output
    }

    fn git_ok(&self, args: &[&str]) -> Output {
        let output = Command::new("git")
            .args(args)
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run git");
        assert_success(&output, &format!("git {}", args.join(" ")));
        output
    }

    /// Pipe one Claude Code hook event into `atomic agent hooks claude-code`.
    fn hook(&self, verb: &str, event: Value) {
        let mut payload = serde_json::json!({
            "session_id": self.session,
            "cwd": self.root(),
            "transcript_path": self.home.path().join("transcript.jsonl"),
        });
        payload
            .as_object_mut()
            .expect("hook payload object")
            .extend(event.as_object().expect("hook event object").clone());
        let output = self.atomic_with_stdin(
            &["agent", "hooks", "claude-code", verb],
            Some(payload.to_string().as_bytes()),
        );
        assert_success(&output, &format!("hook {verb}"));
    }

    /// One agent turn that edits a file; the Stop hook records it into the
    /// session's draft view. Returns the recorded change hash.
    fn agent_turn(&self) -> String {
        fs::write(self.home.path().join("transcript.jsonl"), b"").expect("write transcript");
        self.hook(
            "session-start",
            serde_json::json!({"hook_event_name": "SessionStart", "source": "startup"}),
        );
        self.hook(
            "user-prompt-submit",
            serde_json::json!({
                "hook_event_name": "UserPromptSubmit",
                "prompt": "edit the readme",
            }),
        );
        fs::write(self.root().join("README.md"), b"hello\nworld\n").expect("edit README");
        self.hook(
            "stop",
            serde_json::json!({"hook_event_name": "Stop", "stop_hook_active": false}),
        );
        let change = self.latest_change();
        assert_eq!(
            change["message"].as_str().map(str::trim),
            Some("edit the readme"),
            "the Stop hook must have recorded the agent turn: {change}"
        );
        change["hash"]
            .as_str()
            .expect("recorded change hash")
            .to_string()
    }

    fn end_session(&self) {
        self.hook(
            "session-end",
            serde_json::json!({
                "hook_event_name": "SessionEnd",
                "reason": "prompt_input_exit",
            }),
        );
    }

    fn latest_change(&self) -> Value {
        let log = self.atomic_ok(&["log", "-n", "1", "-f", "json", "--full-hash"]);
        let log: Value = serde_json::from_slice(&log.stdout).expect("log JSON");
        first_entry(&log).clone()
    }

    fn current_view(&self) -> String {
        let status = self.atomic_ok(&["status"]);
        String::from_utf8_lossy(&status.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("On view "))
            .expect("status names the current view")
            .trim()
            .to_string()
    }

    /// The `did:atomic:` DID of a stored identity.
    fn did_of(&self, identity: &str) -> String {
        let shown = self.atomic_ok(&["identity", "show", identity]);
        let id = String::from_utf8_lossy(&shown.stdout)
            .lines()
            .find_map(|line| line.trim().strip_prefix("ID:"))
            .map(|id| id.trim().to_string())
            .expect("identity show prints the identity ID");
        format!("did:atomic:{id}")
    }

    fn append_repo_config(&self, toml: &str) {
        let path = self.root().join(".atomic/config.toml");
        let mut config = fs::read_to_string(&path).expect("read repo config");
        config.push('\n');
        config.push_str(toml);
        fs::write(&path, config).expect("write repo config");
    }

    fn git_push_dry(&self) -> Output {
        self.atomic(&["git", "push", "--no-push", "-b", "feature"])
    }
}

fn first_entry(log: &Value) -> &Value {
    match log {
        Value::Array(entries) => entries.first().expect("at least one log entry"),
        Value::Object(object) => object
            .get("changes")
            .or_else(|| object.get("entries"))
            .and_then(Value::as_array)
            .and_then(|entries| entries.first())
            .expect("at least one log entry"),
        other => panic!("unexpected log JSON shape: {other}"),
    }
}

fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// #330: after `SessionEnd`, the attestation is signed by the user's own
/// default identity — the identity that delegated the hook's agent identity.
/// That is this machine's own signing key, so the change is publishable
/// through `atomic git push`.
#[test]
fn hook_recorded_change_is_git_pushable_after_session_end() {
    let fixture = Fixture::new();
    fixture.agent_turn();
    fixture.end_session();

    let push = fixture.git_push_dry();
    assert_success(
        &push,
        "atomic git push --no-push -b feature after session end",
    );
}

/// #330: the same refusal blocked promoting the hook-recorded change into the
/// shared `main` view with `atomic insert`.
#[test]
fn hook_recorded_change_inserts_into_shared_view_after_session_end() {
    let fixture = Fixture::new();
    let change = fixture.agent_turn();
    fixture.end_session();

    let draft = fixture.current_view();
    assert_ne!(draft, "main", "the Stop hook records into a draft view");
    fixture.atomic_ok(&["view", "switch", "main"]);
    let insert = fixture.atomic(&["insert", "view", &draft]);
    assert_success(&insert, &format!("atomic insert view {draft} into main"));

    let main_tip = fixture.latest_change();
    assert_eq!(
        main_tip["hash"].as_str(),
        Some(change.as_str()),
        "main must now carry the hook-recorded change"
    );
}

/// #330: a repository identity named in `[author] identity` by identity
/// *name* (the form `atomic org set` writes) is the repository's stated
/// trust root and must be resolved to its DID — even when this machine's
/// default identity has since changed.
#[test]
fn author_identity_name_is_resolved_as_the_repository_trust_root() {
    let fixture = Fixture::new();
    fixture.agent_turn();
    fixture.end_session();

    // Move this machine's default to a different identity, so only the
    // repository's configured identity can vouch for the attestation.
    fixture.atomic_ok(&[
        "identity",
        "new",
        "someone-else",
        "-e",
        "else@example.invalid",
        "--set-default",
    ]);
    fixture.append_repo_config(&format!("[author]\nidentity = \"{USER}\"\n"));

    let push = fixture.git_push_dry();
    assert_success(
        &push,
        "atomic git push with the signer named as [author] identity",
    );
}

/// #330: while the session is still open there is no attestation yet — they
/// are written at session end. The refusal must say so instead of leaving the
/// user to guess.
#[test]
fn in_session_refusal_says_attestations_are_written_at_session_end() {
    let fixture = Fixture::new();
    fixture.agent_turn();

    let push = fixture.git_push_dry();
    assert!(
        !push.status.success(),
        "an unattested in-session change must still be refused"
    );
    let text = output_text(&push);
    assert!(
        text.contains("no same-session attestation"),
        "refusal names the missing attestation:\n{text}"
    );
    assert!(
        text.contains("still active") && text.contains("session ends"),
        "refusal must explain that the session is still active and attestations \
         are written when the session ends:\n{text}"
    );
}

/// #330: an attestation signed by an identity that is neither this machine's
/// default nor the repository identity stays untrusted, and the refusal must
/// say how to trust it. Following that advice makes the push succeed.
#[test]
fn untrusted_signer_refusal_explains_how_to_configure_trust() {
    let fixture = Fixture::new();
    fixture.agent_turn();
    fixture.end_session();
    let signer = fixture.did_of(USER);

    // A different default identity: the attestation's signer is now foreign.
    fixture.atomic_ok(&[
        "identity",
        "new",
        "someone-else",
        "-e",
        "else@example.invalid",
        "--set-default",
    ]);

    let refused = fixture.git_push_dry();
    assert!(
        !refused.status.success(),
        "a foreign signer must not satisfy the gate"
    );
    let text = output_text(&refused);
    assert!(
        text.contains(&format!("signer '{signer}'")),
        "refusal names the untrusted signer {signer}:\n{text}"
    );
    assert!(
        text.contains("[git.trust]") && text.contains(".atomic/config.toml"),
        "refusal must say how to configure trust ([git.trust] in .atomic/config.toml):\n{text}"
    );

    fixture.append_repo_config(&format!("[git.trust]\nsigners = [\"{signer}\"]\n"));
    let push = fixture.git_push_dry();
    assert_success(&push, "atomic git push after trusting the signer");
}
