//! Span-derived blame must use the original Git changes, not a later editor.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run(root: &Path, home: &Path, program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .env("ATOMIC_SERVICE", "local")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap()
}

#[test]
fn incremental_span_import_refuses_alternate_git_index_before_writing() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (root, home) = (root.path(), home.path());
    let atomic = env!("CARGO_BIN_EXE_atomic");
    ok(root, home, "git", &["init", "-q"]);
    ok(root, home, "git", &["config", "user.name", "Original"]);
    ok(
        root,
        home,
        "git",
        &["config", "user.email", "original@example.invalid"],
    );
    fs::write(root.join("source.txt"), b"before\n").unwrap();
    ok(root, home, "git", &["add", "source.txt"]);
    ok(root, home, "git", &["commit", "-qm", "base"]);
    ok(
        root,
        home,
        atomic,
        &["git", "import", "--span", "--no-vault"],
    );
    fs::write(root.join("source.txt"), b"after\n").unwrap();
    ok(root, home, "git", &["add", "source.txt"]);
    ok(root, home, "git", &["commit", "-qm", "next"]);
    let index = fs::read(root.join(".git/index")).unwrap();
    let alternate = home.join("alternate-index");
    fs::write(&alternate, &index).unwrap();
    let database = fs::read(root.join(".atomic/atomic.redb")).unwrap();
    let head = fs::read(root.join(".git/HEAD")).unwrap();
    let output = Command::new(atomic)
        .args(["git", "import", "--span", "--incremental", "--no-vault"])
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .env("ATOMIC_SERVICE", "local")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_INDEX_FILE", &alternate)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(message.contains("GIT_INDEX_FILE"), "{message}");
    assert_eq!(
        fs::read(root.join(".atomic/atomic.redb")).unwrap(),
        database
    );
    assert_eq!(fs::read(root.join(".git/HEAD")).unwrap(), head);
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
    assert_eq!(fs::read(alternate).unwrap(), index);
    assert_eq!(fs::read(root.join("source.txt")).unwrap(), b"after\n");
}

#[test]
fn incremental_span_import_refuses_hidden_tracked_edits() {
    for flag in ["--assume-unchanged", "--skip-worktree"] {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let (root, home) = (root.path(), home.path());
        let atomic = env!("CARGO_BIN_EXE_atomic");
        ok(root, home, "git", &["init", "-q"]);
        ok(root, home, "git", &["config", "user.name", "Original"]);
        ok(
            root,
            home,
            "git",
            &["config", "user.email", "original@example.invalid"],
        );
        fs::write(root.join("source.txt"), b"before\n").unwrap();
        ok(root, home, "git", &["add", "source.txt"]);
        ok(root, home, "git", &["commit", "-qm", "base"]);
        ok(
            root,
            home,
            atomic,
            &["git", "import", "--span", "--no-vault"],
        );
        fs::write(root.join("source.txt"), b"committed\n").unwrap();
        ok(root, home, "git", &["add", "source.txt"]);
        ok(root, home, "git", &["commit", "-qm", "next"]);
        ok(root, home, "git", &["update-index", flag, "source.txt"]);
        fs::write(root.join("source.txt"), b"hidden user edit\n").unwrap();
        let status = ok(
            root,
            home,
            "git",
            &["status", "--porcelain", "--untracked-files=no"],
        );
        assert!(
            status.stdout.is_empty(),
            "fixture did not hide the edit for {flag}"
        );
        let index = fs::read(root.join(".git/index")).unwrap();
        let database = fs::read(root.join(".atomic/atomic.redb")).unwrap();
        let head = fs::read(root.join(".git/HEAD")).unwrap();
        let output = run(
            root,
            home,
            atomic,
            &["git", "import", "--span", "--incremental", "--no-vault"],
        );
        assert!(!output.status.success(), "{flag} unexpectedly accepted");
        let message = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            message.contains("assume-unchanged or skip-worktree"),
            "{flag}: {message}"
        );
        assert_eq!(
            fs::read(root.join(".atomic/atomic.redb")).unwrap(),
            database
        );
        assert_eq!(fs::read(root.join(".git/HEAD")).unwrap(), head);
        assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
        assert_eq!(
            fs::read(root.join("source.txt")).unwrap(),
            b"hidden user edit\n"
        );
    }
}

fn ok(root: &Path, home: &Path, program: &str, args: &[&str]) -> Output {
    let output = run(root, home, program, args);
    assert!(
        output.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn git_span_blame_attributes_each_line_to_original_change_after_incremental_import() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (root, home) = (root.path(), home.path());
    let atomic = env!("CARGO_BIN_EXE_atomic");
    ok(root, home, "git", &["init", "-q"]);
    ok(root, home, "git", &["config", "user.name", "Original"]);
    ok(
        root,
        home,
        "git",
        &["config", "user.email", "original@example.invalid"],
    );
    fs::write(root.join("source.txt"), b"alpha\nbeta\n").unwrap();
    ok(root, home, "git", &["add", "source.txt"]);
    ok(root, home, "git", &["commit", "-qm", "base"]);
    ok(
        root,
        home,
        atomic,
        &["git", "import", "--span", "--no-vault"],
    );
    ok(root, home, "git", &["config", "user.name", "Editor"]);
    ok(
        root,
        home,
        "git",
        &["config", "user.email", "editor@example.invalid"],
    );
    fs::write(root.join("source.txt"), b"alpha\nchanged\n").unwrap();
    fs::write(root.join("blob.bin"), [0, 1, 255]).unwrap();
    ok(root, home, "git", &["add", "source.txt", "blob.bin"]);
    ok(root, home, "git", &["commit", "-qm", "next"]);
    ok(
        root,
        home,
        atomic,
        &["git", "import", "--span", "--incremental", "--no-vault"],
    );
    let log = ok(root, home, atomic, &["log", "--format", "json"]);
    let history: serde_json::Value = serde_json::from_slice(&log.stdout).unwrap();
    let hash = |message: &str| {
        history
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["message"] == message)
            .unwrap()["hash"]
            .as_str()
            .unwrap()
    };
    let blame = ok(root, home, atomic, &["blame", "--short", "source.txt"]);
    let text = std::str::from_utf8(&blame.stdout).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(
        lines[0].starts_with(&hash("base")[..12])
            && lines[0].contains("Original")
            && lines[0].ends_with("alpha"),
        "{text}"
    );
    assert!(
        lines[1].starts_with(&hash("next")[..12])
            && lines[1].contains("Editor")
            && lines[1].ends_with("changed"),
        "{text}"
    );
    assert_eq!(
        fs::read(root.join("source.txt")).unwrap(),
        b"alpha\nchanged\n"
    );
    let binary_blame = run(root, home, atomic, &["blame", "blob.bin"]);
    assert!(
        !binary_blame.status.success(),
        "binary content must not be fabricated as text blame"
    );
    assert_eq!(fs::read(root.join("blob.bin")).unwrap(), [0, 1, 255]);
    fs::write(
        root.join("source.txt"),
        b"temporary before first native record\n",
    )
    .unwrap();
    fs::write(root.join("initial-untracked.txt"), b"preserve me\n").unwrap();
    ok(root, home, atomic, &["restore", "--force"]);
    assert_eq!(
        fs::read(root.join("source.txt")).unwrap(),
        b"alpha\nchanged\n"
    );
    assert_eq!(
        fs::read(root.join("initial-untracked.txt")).unwrap(),
        b"preserve me\n"
    );
    let head = fs::read(root.join(".git/HEAD")).unwrap();
    fs::write(root.join("source.txt"), b"alpha\nnative edit\n").unwrap();
    ok(root, home, atomic, &["record", "-m", "native edit"]);
    let log = ok(root, home, atomic, &["log", "--format", "json"]);
    let history: serde_json::Value = serde_json::from_slice(&log.stdout).unwrap();
    let native = history
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["message"] == "native edit")
        .unwrap()["hash"]
        .as_str()
        .unwrap();
    let blame = ok(root, home, atomic, &["blame", "--short", "source.txt"]);
    let text = std::str::from_utf8(&blame.stdout).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[0].starts_with(&hash("base")[..12]), "{text}");
    assert!(
        lines[1].starts_with(&native[..12])
            && lines[1].contains(&hash("next")[..12])
            && lines[1].contains("multiple")
            && lines[1].ends_with("native edit"),
        "{text}"
    );
    let diff = ok(root, home, atomic, &["diff", "--no-color", "-c", native]);
    let diff = String::from_utf8_lossy(&diff.stdout);
    assert!(
        diff.contains("-changed") && diff.contains("+native edit"),
        "{diff}"
    );
    assert_eq!(fs::read(root.join(".git/HEAD")).unwrap(), head);
    assert_eq!(
        ok(root, home, "git", &["show", ":source.txt"]).stdout,
        b"alpha\nnative edit\n"
    );
    assert_eq!(
        ok(root, home, "git", &["show", ":blob.bin"]).stdout,
        [0, 1, 255]
    );
    assert_eq!(
        fs::read(root.join("source.txt")).unwrap(),
        b"alpha\nnative edit\n"
    );
    fs::write(root.join("source.txt"), b"discarded draft\n").unwrap();
    fs::write(root.join("user-untracked.txt"), b"keep me\n").unwrap();
    ok(root, home, atomic, &["restore", "--force"]);
    assert_eq!(
        fs::read(root.join("source.txt")).unwrap(),
        b"alpha\nnative edit\n"
    );
    assert_eq!(
        fs::read(root.join("user-untracked.txt")).unwrap(),
        b"keep me\n"
    );
    assert_eq!(fs::read(root.join(".git/HEAD")).unwrap(), head);
    fs::write(root.join("source.txt"), b"alpha\nnative edit extra\n").unwrap();
    ok(root, home, atomic, &["record", "-m", "append one token"]);
    let latest = ok(root, home, atomic, &["log", "--format", "json"]);
    let latest_history: serde_json::Value = serde_json::from_slice(&latest.stdout).unwrap();
    let appended = latest_history
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["message"] == "append one token")
        .unwrap()["hash"]
        .as_str()
        .unwrap();
    let blame = ok(root, home, atomic, &["blame", "--short", "source.txt"]);
    let blame = std::str::from_utf8(&blame.stdout).unwrap();
    let second = blame.lines().nth(1).unwrap();
    assert!(
        second.contains(&native[..12])
            && second.contains(&appended[..12])
            && second.contains(&hash("next")[..12]),
        "{blame}"
    );
    assert!(second.ends_with("native edit extra"), "{blame}");
    fs::write(root.join("blob.bin"), [0, 2, 3, 255]).unwrap();
    ok(root, home, atomic, &["record", "-m", "native binary edit"]);
    let log = ok(root, home, atomic, &["log", "--format", "json"]);
    let history: serde_json::Value = serde_json::from_slice(&log.stdout).unwrap();
    let binary_change = history
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["message"] == "native binary edit")
        .unwrap()["hash"]
        .as_str()
        .unwrap();
    let diff = ok(
        root,
        home,
        atomic,
        &["diff", "--no-color", "-c", binary_change],
    );
    let diff = String::from_utf8_lossy(&diff.stdout);
    assert!(diff.to_ascii_lowercase().contains("binary"), "{diff}");
    assert_eq!(fs::read(root.join("blob.bin")).unwrap(), [0, 2, 3, 255]);
    fs::write(root.join("blob.bin"), b"now text\n").unwrap();
    ok(root, home, atomic, &["record", "-m", "binary to text"]);
    let blame = ok(root, home, atomic, &["blame", "--short", "blob.bin"]);
    assert!(String::from_utf8_lossy(&blame.stdout).contains("now text"));
    assert_eq!(fs::read(root.join("blob.bin")).unwrap(), b"now text\n");
    use std::os::unix::fs::PermissionsExt;
    fs::write(
        root.join("source.txt"),
        b"alpha\nnative edit extra executable\n",
    )
    .unwrap();
    fs::set_permissions(root.join("source.txt"), fs::Permissions::from_mode(0o755)).unwrap();
    ok(root, home, atomic, &["record", "-m", "edit and chmod"]);
    assert_eq!(
        fs::read(root.join("source.txt")).unwrap(),
        b"alpha\nnative edit extra executable\n"
    );
    assert_ne!(
        fs::metadata(root.join("source.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert!(String::from_utf8_lossy(
        &ok(root, home, "git", &["ls-files", "--stage", "source.txt"]).stdout
    )
    .starts_with("100755 "));
    let compact = ok(root, home, atomic, &["compact", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&compact.stdout).unwrap();
    assert!(report["after_bytes"].as_u64().unwrap() > 0);
    let blame = ok(root, home, atomic, &["blame", "--short", "source.txt"]);
    assert!(String::from_utf8_lossy(&blame.stdout).contains("native edit extra executable"));
    assert_eq!(
        fs::read(root.join("source.txt")).unwrap(),
        b"alpha\nnative edit extra executable\n"
    );
}
