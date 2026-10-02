#[path = "../src/commands/git/wip.rs"]
mod wip;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;
use wip::{
    capture_tracked_wip, is_publishable_git_ref, is_wip_ref, WipCaptureError, WipCaptureRequest,
    WIP_REF_PREFIX,
};

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn git(root: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("run git")
}

fn git_ok(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = git(root, args);
    assert!(
        output.status.success(),
        "git {:?} failed:\nstdout:\n{}\nstderr:\n{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn git_text(root: &Path, args: &[&str]) -> String {
    String::from_utf8(git_ok(root, args))
        .expect("UTF-8 Git output")
        .trim()
        .to_string()
}

fn init_git(root: &Path) {
    git_ok(root, &["init", "-q"]);
    git_ok(root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git_ok(root, &["config", "user.name", "Atomic Test"]);
    git_ok(root, &["config", "user.email", "atomic@example.com"]);
    git_ok(root, &["config", "core.autocrlf", "false"]);
}

fn commit_all(root: &Path, message: &str) -> String {
    git_ok(root, &["add", "--all"]);
    git_ok(root, &["commit", "-q", "-m", message]);
    git_text(root, &["rev-parse", "HEAD"])
}

fn git_object(root: &Path, spec: &str) -> Option<Vec<u8>> {
    let output = git(root, &["show", spec]);
    output.status.success().then_some(output.stdout)
}

fn parse_forensic_digest(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let line = text
        .lines()
        .find(|line| line.trim_start().starts_with("refs:") && line.contains("digest "))
        .expect("forensic refs digest line");
    line.split("digest ")
        .nth(1)
        .expect("digest suffix")
        .trim_end_matches(')')
        .to_string()
}

fn atomic(root: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .output()
        .expect("run atomic")
}

#[test]
fn capture_preserves_tracked_repository_bytes_and_live_state() {
    let repository = TempDir::new().expect("repository tempdir");
    let root = repository.path();
    init_git(root);

    fs::write(root.join(".gitattributes"), "filtered.txt text eol=lf\n").unwrap();
    fs::write(root.join("filtered.txt"), b"before\n").unwrap();
    fs::write(root.join("zero.txt"), b"not empty\n").unwrap();
    fs::write(root.join("delete.txt"), b"delete me\n").unwrap();
    fs::write(root.join("executable.sh"), b"#!/bin/sh\necho before\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{symlink, PermissionsExt};
        fs::set_permissions(
            root.join("executable.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        symlink("filtered.txt", root.join("link")).unwrap();
    }
    let head_before = commit_all(root, "initial");

    fs::write(root.join("filtered.txt"), b"after\r\n").unwrap();
    fs::write(root.join("zero.txt"), b"").unwrap();
    fs::remove_file(root.join("delete.txt")).unwrap();
    fs::write(root.join("executable.sh"), b"#!/bin/sh\necho after\n").unwrap();
    fs::write(root.join("staged.txt"), b"staged addition\n").unwrap();
    git_ok(root, &["add", "staged.txt"]);
    fs::write(root.join("untracked-secret.txt"), b"do not capture\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{symlink, PermissionsExt};
        fs::set_permissions(
            root.join("executable.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::remove_file(root.join("link")).unwrap();
        symlink("zero.txt", root.join("link")).unwrap();
    }

    fs::create_dir_all(root.join(".atomic/bridge")).unwrap();
    fs::write(root.join(".atomic/current_view"), b"agent-view\n").unwrap();
    fs::write(
        root.join(".atomic/bridge/workspace.json"),
        b"{\"sentinel\":true}\n",
    )
    .unwrap();
    let current_view_before = fs::read(root.join(".atomic/current_view")).unwrap();
    let checkpoint_before = fs::read(root.join(".atomic/bridge/workspace.json")).unwrap();
    let index_before = fs::read(root.join(".git/index")).unwrap();

    let request = WipCaptureRequest::new(root, "workspace-one", "operation-one");
    let capture = capture_tracked_wip(request).expect("capture tracked WIP");

    assert!(capture.ref_name.starts_with(WIP_REF_PREFIX));
    assert!(is_wip_ref(capture.ref_name.as_bytes()));
    assert!(!is_publishable_git_ref(capture.ref_name.as_bytes()));
    assert_eq!(capture.parent_oid.as_deref(), Some(head_before.as_str()));
    assert_eq!(
        git_text(root, &["rev-parse", &capture.ref_name]),
        capture.commit_oid
    );

    let parents = git_text(
        root,
        &["rev-list", "--parents", "-n", "1", &capture.commit_oid],
    );
    let parent_fields: Vec<_> = parents.split_whitespace().collect();
    assert_eq!(
        parent_fields,
        vec![capture.commit_oid.as_str(), head_before.as_str()]
    );

    let message = git_text(root, &["log", "-1", "--format=%B", &capture.ref_name]);
    assert!(message.contains("does not assert authorship or pre-checkout provenance"));
    for forbidden in [
        "Atomic-View:",
        "Atomic-State:",
        "Atomic-Changes:",
        "agent",
        "workspace-one",
        "operation-one",
    ] {
        assert!(
            !message.contains(forbidden),
            "recovery message must not contain {forbidden:?}: {message}"
        );
    }
    assert_eq!(
        git_text(
            root,
            &["log", "-1", "--format=%an <%ae>", &capture.ref_name]
        ),
        "Atomic Recovery <recovery@atomic.invalid>"
    );

    assert_eq!(
        git_object(root, &format!("{}:filtered.txt", capture.ref_name)),
        Some(b"after\n".to_vec()),
        "Git clean filters must define repository bytes"
    );
    assert_eq!(
        git_object(root, &format!("{}:zero.txt", capture.ref_name)),
        Some(Vec::new())
    );
    assert_eq!(
        git_object(root, &format!("{}:staged.txt", capture.ref_name)),
        Some(b"staged addition\n".to_vec())
    );
    assert!(git_object(root, &format!("{}:delete.txt", capture.ref_name)).is_none());
    assert!(
        git_object(root, &format!("{}:untracked-secret.txt", capture.ref_name)).is_none(),
        "untracked paths must not enter the recovery tree"
    );

    #[cfg(unix)]
    {
        assert_eq!(
            git_object(root, &format!("{}:link", capture.ref_name)),
            Some(b"zero.txt".to_vec())
        );
        let executable = git_text(root, &["ls-tree", &capture.ref_name, "executable.sh"]);
        assert!(executable.starts_with("100755 blob "), "{executable}");
        let link = git_text(root, &["ls-tree", &capture.ref_name, "link"]);
        assert!(link.starts_with("120000 blob "), "{link}");
    }

    let reflog = git_text(
        root,
        &["reflog", "show", "--format=%H %gs", &capture.ref_name],
    );
    assert!(reflog.contains(&capture.commit_oid));
    assert!(reflog.contains("atomic wip recovery"));

    let expected_paths = [
        b"delete.txt".as_slice(),
        b"executable.sh".as_slice(),
        b"filtered.txt".as_slice(),
        b"staged.txt".as_slice(),
        b"zero.txt".as_slice(),
    ]
    .into_iter()
    .map(<[u8]>::to_vec)
    .collect::<BTreeSet<_>>();
    let captured_paths = capture.paths.iter().cloned().collect::<BTreeSet<_>>();
    assert!(expected_paths.is_subset(&captured_paths));
    assert!(!captured_paths.contains(b"untracked-secret.txt".as_slice()));

    assert_eq!(git_text(root, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index_before);
    assert_eq!(
        fs::read(root.join(".atomic/current_view")).unwrap(),
        current_view_before
    );
    assert_eq!(
        fs::read(root.join(".atomic/bridge/workspace.json")).unwrap(),
        checkpoint_before
    );

    fs::write(root.join("filtered.txt"), b"newer\n").unwrap();
    let second = capture_tracked_wip(request).expect_err("create-only ref must reject overwrite");
    assert!(matches!(
        second,
        WipCaptureError::RefAlreadyExists { ref ref_name } if ref_name == &capture.ref_name
    ));
    assert_eq!(
        git_text(root, &["rev-parse", &capture.ref_name]),
        capture.commit_oid,
        "failed recapture must not move the original recovery ref"
    );
}

#[test]
fn wip_ref_is_forensic_but_does_not_change_canonical_refs_digest() {
    let repository = TempDir::new().expect("repository tempdir");
    let home = TempDir::new().expect("home tempdir");
    let root = repository.path();
    init_git(root);
    fs::write(root.join("tracked.txt"), b"one\n").unwrap();
    commit_all(root, "initial");

    let init = atomic(root, home.path(), &["init", "--view", "main"]);
    assert!(
        init.status.success(),
        "atomic init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let before = atomic(root, home.path(), &["status", "--no-reconcile"]);
    assert!(before.status.success());
    let digest_before = parse_forensic_digest(&before.stdout);

    fs::write(root.join("tracked.txt"), b"two\n").unwrap();
    let capture = capture_tracked_wip(WipCaptureRequest::new(
        root,
        "digest-workspace",
        "digest-operation",
    ))
    .expect("capture WIP");

    let after = atomic(root, home.path(), &["status", "--no-reconcile"]);
    assert!(
        after.status.success(),
        "forensic status failed: {}",
        String::from_utf8_lossy(&after.stderr)
    );
    let digest_after = parse_forensic_digest(&after.stdout);
    assert_eq!(digest_after, digest_before);
    assert!(
        String::from_utf8_lossy(&after.stdout).contains(&capture.ref_name),
        "WIP ref must remain visible in forensic observation"
    );
}

#[test]
fn atomic_publication_predicate_and_branch_refspec_exclude_wip_namespace() {
    assert!(is_publishable_git_ref(b"refs/heads/main"));
    assert!(is_publishable_git_ref(b"refs/atomic/bindings/example"));
    assert!(!is_publishable_git_ref(b"refs/atomic/wip/workspace/op"));

    let repository = TempDir::new().expect("repository tempdir");
    let remote = TempDir::new().expect("remote tempdir");
    let root = repository.path();
    init_git(root);
    fs::write(root.join("tracked.txt"), b"one\n").unwrap();
    commit_all(root, "initial");
    fs::write(root.join("tracked.txt"), b"two\n").unwrap();
    let capture = capture_tracked_wip(WipCaptureRequest::new(
        root,
        "publish-workspace",
        "publish-operation",
    ))
    .expect("capture WIP");

    git_ok(remote.path(), &["init", "--bare", "-q"]);
    let remote_path = remote.path().to_string_lossy().to_string();
    git_ok(root, &["remote", "add", "origin", &remote_path]);
    git_ok(root, &["push", "-q", "origin", "HEAD:refs/heads/main"]);

    let remote_wip = git_text(
        remote.path(),
        &["for-each-ref", "--format=%(refname)", WIP_REF_PREFIX],
    );
    assert!(
        remote_wip.is_empty(),
        "unexpected remote WIP refs: {remote_wip}"
    );
    assert_eq!(
        git_text(root, &["rev-parse", &capture.ref_name]),
        capture.commit_oid
    );
}
