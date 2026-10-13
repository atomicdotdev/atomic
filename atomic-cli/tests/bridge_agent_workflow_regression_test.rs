#![cfg(unix)]
//! Real CLI boundaries: a private rename snapshot must not change another
//! closure's file identity; agent view selection and recording must finish the
//! same Git coordination as ordinary view/record commands.
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

struct Fixture {
    dir: tempfile::TempDir,
    feature: git2::Oid,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = git2::Repository::init(root).unwrap();
        git.set_head("refs/heads/main").unwrap();
        let mut config = git.config().unwrap();
        config.set_str("user.name", "Bridge Test").unwrap();
        config
            .set_str("user.email", "bridge@example.invalid")
            .unwrap();
        config.set_str("core.hooksPath", ".git/hooks").unwrap();
        let sig = git2::Signature::now("Bridge Test", "bridge@example.invalid").unwrap();
        fs::write(root.join("tracked.txt"), b"base\n").unwrap();
        let mut index = git.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        let base = git
            .commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
            .unwrap();
        drop(tree);
        fs::write(root.join("tracked.txt"), b"feature edit\n").unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        let feature = git
            .commit(
                Some("refs/heads/feature"),
                &sig,
                &sig,
                "feature",
                &tree,
                &[&git.find_commit(base).unwrap()],
            )
            .unwrap();
        git.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        drop(tree);
        drop(index);
        drop(config);
        drop(git);
        let f = Self { dir, feature };
        f.atomic(&["git", "import", "--no-vault"]);
        let key = root_key(f.root());
        f.atomic(&[
            "git",
            "bridge",
            "enable",
            "--no-refspecs",
            "--binding-key-file",
            key.to_str().unwrap(),
        ]);
        f.atomic(&["git", "bridge", "reconcile"]);
        f
    }
    fn root(&self) -> &Path {
        self.dir.path()
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_atomic"));
        c.current_dir(self.root())
            .env("ATOMIC_SERVICE", "local")
            .env("ATOMIC_CONFIG_DIR", self.root().join(".git/test-config"))
            .env("ATOMIC_NONINTERACTIVE", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Bridge Test")
            .env("GIT_AUTHOR_EMAIL", "bridge@example.invalid")
            .env("GIT_COMMITTER_NAME", "Bridge Test")
            .env("GIT_COMMITTER_EMAIL", "bridge@example.invalid");
        c
    }
    fn atomic(&self, args: &[&str]) -> String {
        let out = self.command().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "atomic {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
    fn hook(&self, verb: &str) {
        let out = self.hook_output(verb);
        assert!(
            out.status.success(),
            "hook {verb}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    fn hook_output(&self, verb: &str) -> std::process::Output {
        self.hook_output_in(verb, self.root())
    }
    fn hook_output_in(&self, verb: &str, root: &Path) -> std::process::Output {
        let payload = serde_json::json!({"session_id": "bridge-agent", "cwd": root,
            "prompt": "Edit tracked file", "model": "test", "provider": "test",
            "response": "Edited tracked file", "turn_duration_ms": 10});
        let mut child = self
            .command()
            .current_dir(root)
            .args(["agent", "hooks", "opencode", verb, "--foreground", "--json"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}
fn root_key(root: &Path) -> std::path::PathBuf {
    let key = root.join(".atomic/test-binding.key");
    fs::write(&key, "11".repeat(32)).unwrap();
    key
}

#[test]
fn rename_status_restore_does_not_poison_foreign_import() {
    for short in [false, true] {
        let f = Fixture::new();
        fs::rename(f.root().join("tracked.txt"), f.root().join("renamed.txt")).unwrap();
        {
            let git = git2::Repository::open(f.root()).unwrap();
            let mut index = git.index().unwrap();
            index.remove_path(Path::new("tracked.txt")).unwrap();
            index.add_path(Path::new("renamed.txt")).unwrap();
            index.write().unwrap();
        }
        f.atomic(if short {
            &["status", "--short"]
        } else {
            &["status"]
        });
        fs::rename(f.root().join("renamed.txt"), f.root().join("tracked.txt")).unwrap();
        {
            let git = git2::Repository::open(f.root()).unwrap();
            let mut index = git.index().unwrap();
            index
                .read_tree(&git.head().unwrap().peel_to_tree().unwrap())
                .unwrap();
            index.write().unwrap();
            assert_eq!(
                git.find_reference("refs/heads/feature").unwrap().target(),
                Some(f.feature)
            );
            git.set_head("refs/heads/feature").unwrap();
            git.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
                .unwrap();
        }
        f.atomic(&["git", "bridge", "reconcile"]);
        f.atomic(&["git", "bridge", "verify"]);
        assert_eq!(
            fs::read(f.root().join("tracked.txt")).unwrap(),
            b"feature edit\n"
        );
        f.atomic(&["view", "switch", "main"]);
        assert_eq!(fs::read(f.root().join("tracked.txt")).unwrap(), b"base\n");
        f.atomic(&["view", "switch", "feature"]);
        assert_eq!(
            fs::read(f.root().join("tracked.txt")).unwrap(),
            b"feature edit\n"
        );
        assert!(!f.root().join("renamed.txt").exists());
        f.atomic(&["git", "bridge", "verify"]);
    }
}

#[test]
fn actual_agent_start_and_stop_keep_bridge_aligned() {
    let f = Fixture::new();
    f.hook("session-start");
    assert!(f.atomic(&["status", "--short"]).trim().is_empty());
    f.atomic(&["git", "bridge", "verify"]);
    f.hook("user-prompt");
    fs::write(f.root().join("tracked.txt"), b"agent edit\n").unwrap();
    f.hook("stop");
    assert!(f.atomic(&["status", "--short"]).trim().is_empty());
    f.atomic(&["git", "bridge", "verify"]);
    let ledger: serde_json::Value =
        serde_json::from_str(&f.atomic(&["session", "show", "bridge-agent", "--json"])).unwrap();
    assert_eq!(ledger[1].as_array().unwrap().len(), 1);
    assert!(!ledger.to_string().contains("UnattributedGitOperation"));
    let git = git2::Repository::open(f.root()).unwrap();
    let tree = git.head().unwrap().peel_to_tree().unwrap();
    let entry = tree.get_path(Path::new("tracked.txt")).unwrap();
    assert_eq!(
        git.find_blob(entry.id()).unwrap().content(),
        b"agent edit\n"
    );
}

#[test]
fn agent_start_preserves_git_owned_state_and_refuses_before_fork() {
    let f = Fixture::new();
    let before_view = fs::read(f.root().join(".atomic/current_view")).unwrap();
    let before_head = fs::read(f.root().join(".git/HEAD")).unwrap();
    let before_index = fs::read(f.root().join(".git/index")).unwrap();
    fs::write(f.root().join(".git/index.lock"), b"external writer").unwrap();
    fs::write(f.root().join("tracked.txt"), b"private work\n").unwrap();
    let mut child = f
        .command()
        .args([
            "agent",
            "hooks",
            "opencode",
            "session-start",
            "--foreground",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"session_id":"blocked-start"}"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        !out.status.success(),
        "startup must refuse Git's live index lock"
    );
    assert_eq!(
        fs::read(f.root().join(".atomic/current_view")).unwrap(),
        before_view
    );
    assert_eq!(fs::read(f.root().join(".git/HEAD")).unwrap(), before_head);
    assert_eq!(fs::read(f.root().join(".git/index")).unwrap(), before_index);
    assert_eq!(
        fs::read(f.root().join("tracked.txt")).unwrap(),
        b"private work\n"
    );
    assert!(!f
        .root()
        .join(".atomic/sessions/blocked-start.json")
        .exists());
}

#[test]
fn fallback_agent_start_with_pending_work_keeps_bytes_and_bridge_baseline() {
    let f = Fixture::new();
    fs::write(
        f.root().join("tracked.txt"),
        b"pending work before first prompt\n",
    )
    .unwrap();
    f.hook("user-prompt");
    assert_eq!(
        fs::read(f.root().join("tracked.txt")).unwrap(),
        b"pending work before first prompt\n"
    );
    assert!(f.atomic(&["status", "--short"]).contains("tracked.txt"));
    f.hook("stop");
    f.atomic(&["git", "bridge", "verify"]);
}

#[test]
fn atomic_record_inside_agent_turn_is_a_verified_repository_operation() {
    let f = Fixture::new();
    f.hook("session-start");
    f.hook("user-prompt");
    fs::write(f.root().join("tracked.txt"), b"explicit atomic edit\n").unwrap();
    f.atomic(&[
        "record",
        "-am",
        "explicit agent record",
        "--author",
        "Bridge Test <bridge@example.invalid>",
    ]);
    f.hook("stop");
    f.atomic(&["git", "bridge", "verify"]);
    f.hook("user-prompt");
    fs::write(f.root().join("tracked.txt"), b"second explicit edit\n").unwrap();
    f.atomic(&[
        "record",
        "-am",
        "second explicit record",
        "--author",
        "Bridge Test <bridge@example.invalid>",
    ]);
    fs::write(
        f.root().join("tracked.txt"),
        b"second edit with unrecorded remainder\n",
    )
    .unwrap();
    f.hook("stop");
    f.atomic(&["git", "bridge", "verify"]);
}

#[test]
fn agent_start_preserves_partial_staging_and_existing_snapshot() {
    let f = Fixture::new();
    fs::write(f.root().join("tracked.txt"), b"staged edit\n").unwrap();
    {
        let git = git2::Repository::open(f.root()).unwrap();
        let mut index = git.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
    }
    fs::write(f.root().join("tracked.txt"), b"unstaged edit\n").unwrap();
    fs::write(f.root().join("untracked.txt"), b"private file\n").unwrap();
    f.atomic(&["status"]); // Persist a private snapshot before the fork.
    let index = fs::read(f.root().join(".git/index")).unwrap();
    f.hook("session-start");
    f.hook("session-start"); // A fresh process resuming the same session.
    assert_eq!(fs::read(f.root().join(".git/index")).unwrap(), index);
    assert_eq!(
        fs::read(f.root().join("tracked.txt")).unwrap(),
        b"unstaged edit\n"
    );
    assert_eq!(
        fs::read(f.root().join("untracked.txt")).unwrap(),
        b"private file\n"
    );
    f.hook("user-prompt");
    f.atomic(&[
        "record",
        "-am",
        "record staged and unstaged work",
        "--author",
        "Bridge Test <bridge@example.invalid>",
    ]);
    f.hook("stop");
    f.atomic(&["git", "bridge", "verify"]);
}

#[test]
fn prior_projection_does_not_authenticate_a_foreign_commit() {
    let f = Fixture::new();
    f.hook("session-start");
    f.hook("user-prompt");
    fs::write(f.root().join("tracked.txt"), b"atomic change\n").unwrap();
    f.atomic(&[
        "record",
        "-am",
        "legitimate projection",
        "--author",
        "Bridge Test <bridge@example.invalid>",
    ]);
    {
        let git = git2::Repository::open(f.root()).unwrap();
        let parent = git.head().unwrap().peel_to_commit().unwrap();
        let sig = git2::Signature::now("Foreign", "foreign@example.invalid").unwrap();
        git.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "foreign empty commit",
            &parent.tree().unwrap(),
            &[&parent],
        )
        .unwrap();
    }
    let before_head = fs::read(f.root().join(".git/HEAD")).unwrap();
    let out = f.hook_output("stop");
    assert!(
        !out.status.success(),
        "a prior verified projection must not explain an extra foreign transition: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read(f.root().join(".git/HEAD")).unwrap(), before_head);
}

#[test]
fn agent_in_nested_sandbox_does_not_project_to_the_hosts_git() {
    let f = Fixture::new();
    let sandbox = f.root().join(".atomic/agent-sandbox");
    f.atomic(&[
        "sandbox",
        "create",
        "agent-sandbox",
        "--from",
        "main",
        "--dest",
        sandbox.to_str().unwrap(),
    ]);
    let head = fs::read(f.root().join(".git/HEAD")).unwrap();
    let index = fs::read(f.root().join(".git/index")).unwrap();
    let view = fs::read(f.root().join(".atomic/current_view")).unwrap();
    for verb in ["session-start", "user-prompt", "stop"] {
        if verb == "stop" {
            fs::write(sandbox.join("tracked.txt"), b"sandbox edit\n").unwrap();
        }
        let out = f.hook_output_in(verb, &sandbox);
        assert!(
            out.status.success(),
            "sandbox {verb}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(fs::read(f.root().join(".git/HEAD")).unwrap(), head);
    assert_eq!(fs::read(f.root().join(".git/index")).unwrap(), index);
    assert_eq!(
        fs::read(f.root().join(".atomic/current_view")).unwrap(),
        view
    );
    assert_eq!(fs::read(f.root().join("tracked.txt")).unwrap(), b"base\n");
    f.atomic(&["git", "bridge", "verify"]);
}
