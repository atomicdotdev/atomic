//! Exercise the executable with and without its optional Git adapter.
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn atomic(root: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atomic"))
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .env("ATOMIC_NONINTERACTIVE", "1")
        .env("NO_COLOR", "1")
        .env_remove("VOYAGE_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .unwrap()
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn ok(root: &Path, home: &Path, args: &[&str]) -> Output {
    let output = atomic(root, home, args);
    assert!(output.status.success(), "{args:?}: {}", text(&output));
    output
}

#[test]
fn git_commands_match_build_feature() {
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let help = text(&ok(root.path(), home.path(), &["--help"]));
    for command in ["git", "stage", "unstage"] {
        assert_eq!(
            help.lines()
                .any(|line| line.split_whitespace().next() == Some(command)),
            cfg!(feature = "git-shim"),
            "{command} availability in root help: {help}"
        );
        let output = atomic(root.path(), home.path(), &[command, "--help"]);
        assert_eq!(
            output.status.success(),
            cfg!(feature = "git-shim"),
            "{command}: {}",
            text(&output)
        );
        if command == "git" && cfg!(feature = "git-shim") {
            assert!(text(&output).contains("import"));
            assert!(text(&output).contains("shim"));
        }
    }
}

#[cfg(not(feature = "git-shim"))]
#[test]
fn disabled_staging_commands_preserve_unenrolled_git_index_and_native_history() {
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let (root, home) = (root.path(), home.path());
    ok(root, home, &["init", "--no-vault", "--view", "main"]);
    fs::write(root.join("tracked.txt"), b"baseline\n").unwrap();
    ok(root, home, &["add", "tracked.txt"]);
    ok(root, home, &["record", "-m", "native baseline"]);
    let native_history = ok(root, home, &["log"]).stdout;

    // An unenrolled colocated repository bypasses the enrollment guard.
    // Keep distinct HEAD, index, and worktree bytes: either staging operation
    // would change the real index if its command were still available.
    let git = git2::Repository::init(root).unwrap();
    let mut index = git.index().unwrap();
    index.add_path(Path::new("tracked.txt")).unwrap();
    let tree_id = index.write_tree().unwrap();
    let signature = git2::Signature::now("Feature test", "feature@example.invalid").unwrap();
    let head = git
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "Git baseline",
            &git.find_tree(tree_id).unwrap(),
            &[],
        )
        .unwrap();
    let entry = index.get_path(Path::new("tracked.txt"), 0).unwrap();
    index.add_frombuffer(&entry, b"staged\n").unwrap();
    index.write().unwrap();
    let index_path = git.path().join("index");
    let index_bytes = fs::read(&index_path).unwrap();
    drop(index);
    drop(git);
    fs::write(root.join("tracked.txt"), b"working\n").unwrap();
    assert!(!root.join(".atomic/bridge/workspace.json").exists());

    for command in ["stage", "unstage"] {
        let output = atomic(root, home, &[command, "tracked.txt"]);
        assert!(!output.status.success(), "{command}: {}", text(&output));
        assert_eq!(fs::read(&index_path).unwrap(), index_bytes, "{command}");
        assert_eq!(fs::read(root.join("tracked.txt")).unwrap(), b"working\n");
        assert_eq!(ok(root, home, &["log"]).stdout, native_history);
        assert!(!root.join(".atomic/bridge/workspace.json").exists());
    }
    let git = git2::Repository::open(root).unwrap();
    assert_eq!(git.head().unwrap().target(), Some(head));
    let repo = atomic_repository::Repository::open_readonly(root).unwrap();
    assert!(repo
        .list_tracked_files()
        .unwrap()
        .iter()
        .any(|file| file.path == Path::new("tracked.txt")));
}

#[test]
fn native_record_and_view_round_trip_work_in_either_build() {
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let (root, home) = (root.path(), home.path());
    ok(root, home, &["init", "--no-vault", "--view", "main"]);
    fs::write(root.join("hello.txt"), "first\n").unwrap();
    ok(root, home, &["add", "hello.txt"]);
    ok(root, home, &["record", "-m", "first"]);
    ok(
        root,
        home,
        &["view", "create", "topic", "--draft", "--parent", "main"],
    );
    ok(root, home, &["view", "switch", "topic"]);
    fs::write(root.join("hello.txt"), "second\n").unwrap();
    ok(root, home, &["record", "-m", "second"]);
    ok(root, home, &["view", "switch", "main"]);
    assert_eq!(
        fs::read_to_string(root.join("hello.txt")).unwrap(),
        "first\n"
    );
    ok(root, home, &["view", "switch", "topic"]);
    assert_eq!(
        fs::read_to_string(root.join("hello.txt")).unwrap(),
        "second\n"
    );
    let status = ok(root, home, &["status", "--json"]);
    serde_json::from_slice::<serde_json::Value>(&status.stdout).unwrap();
    assert!(!root.join(".git").exists());
}

#[cfg(not(feature = "git-shim"))]
#[test]
fn disabled_git_entry_points_refuse_before_creating_repository() {
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    for args in [
        vec![
            "init",
            "--adopt-git",
            "--binding-key-file",
            "unused-key",
            "--no-vault",
        ],
        vec!["clone", "https://example.invalid/repo.git", "destination"],
        vec!["clone", "ssh://example.invalid/repo", "destination"],
        vec![
            "clone",
            "--into-existing",
            "https://example.invalid/repo",
            "destination",
        ],
        vec![
            "clone",
            "--git",
            "https://example.invalid/repo",
            "destination",
        ],
    ] {
        let output = atomic(root.path(), home.path(), &args);
        assert!(!output.status.success());
        assert!(
            text(&output).contains("without Git interoperability"),
            "{}",
            text(&output)
        );
        assert!(!root.path().join(".atomic").exists());
        assert!(!root.path().join("destination").exists());
    }
    ok(root.path(), home.path(), &["init", "--no-vault"]);
    for args in [["status", "--git"], ["diff", "--git"]] {
        let output = atomic(root.path(), home.path(), &args);
        assert!(!output.status.success());
        assert!(
            text(&output).contains("without Git interoperability"),
            "{}",
            text(&output)
        );
    }
}

#[cfg(not(feature = "git-shim"))]
#[test]
fn checkpoint_alone_requires_a_shim_enabled_build() {
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    ok(root.path(), home.path(), &["init", "--no-vault"]);
    let checkpoint = root.path().join(".atomic/bridge/workspace.json");
    fs::create_dir_all(checkpoint.parent().unwrap()).unwrap();
    // Even malformed enrollment evidence must not be treated as native mode.
    fs::write(&checkpoint, b"incomplete checkpoint").unwrap();
    let output = atomic(root.path(), home.path(), &["status"]);
    assert!(!output.status.success());
    assert!(
        text(&output).contains("built without `git-shim`"),
        "{}",
        text(&output)
    );
    assert_eq!(fs::read(checkpoint).unwrap(), b"incomplete checkpoint");
}

#[cfg(not(feature = "git-shim"))]
#[test]
fn disabled_build_refuses_enrolled_workspace_without_recording() {
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    ok(root.path(), home.path(), &["init", "--no-vault"]);
    fs::write(root.path().join("pending.txt"), "unrecorded\n").unwrap();
    ok(root.path(), home.path(), &["add", "pending.txt"]);
    let before = ok(root.path(), home.path(), &["log"]).stdout;
    let config_path = root.path().join(".atomic/config.toml");
    let mut config = atomic_config::RepoConfig::load(&config_path).unwrap();
    config.git.shim.enabled = true;
    config.save(&config_path).unwrap();
    for args in [
        vec!["record", "-m", "must refuse"],
        vec!["status"],
        vec!["view", "create", "must-not-exist"],
    ] {
        let output = atomic(root.path(), home.path(), &args);
        assert!(!output.status.success());
        assert!(
            text(&output).contains("built without `git-shim`"),
            "{}",
            text(&output)
        );
    }
    assert_eq!(
        fs::read_to_string(root.path().join("pending.txt")).unwrap(),
        "unrecorded\n"
    );
    config.git.shim.enabled = false;
    config.save(&config_path).unwrap();
    assert_eq!(before, ok(root.path(), home.path(), &["log"]).stdout);
    let repo = atomic_repository::Repository::open_readonly(root.path()).unwrap();
    assert!(!repo.view_exists("must-not-exist").unwrap());
}
