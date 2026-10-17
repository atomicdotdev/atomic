//! Exercise the real build script through Cargo, including cached rebuilds.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    source: PathBuf,
    target: PathBuf,
}

fn stdout(output: Output) -> String {
    assert!(
        output.status.success(),
        "command failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn git(dir: &Path, args: &[&str]) -> String {
    stdout(
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=Atomic Build Test",
                "-c",
                "user.email=build-test@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap(),
    )
}

impl Fixture {
    fn new(repository: bool) -> Self {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::create_dir_all(source.join("src")).unwrap();
        fs::write(
            source.join("Cargo.toml"),
            "[package]\nname = \"version-fixture\"\nversion = \"0.19.1\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(source.join("build.rs"), include_str!("../build.rs")).unwrap();
        fs::write(
            source.join("src/main.rs"),
            "fn main() { println!(\"{}\", env!(\"ATOMIC_CLI_VERSION\")); }\n",
        )
        .unwrap();
        fs::write(source.join("tracked.txt"), "original\n").unwrap();
        if repository {
            git(&source, &["init", "--initial-branch=dev"]);
            git(&source, &["add", "."]);
            git(&source, &["commit", "-m", "Initial source"]);
        }
        Self {
            dir,
            source,
            target,
        }
    }

    fn build(&self, source: &Path, channel: Option<&str>, commit: Option<&str>) -> String {
        let mut command = Command::new(env!("CARGO"));
        command
            .args(["run", "--quiet", "--offline", "--manifest-path"])
            .arg(source.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(&self.target)
            .env_remove("ATOMIC_BUILD_CHANNEL")
            .env_remove("ATOMIC_BUILD_COMMIT");
        if let Some(channel) = channel {
            command.env("ATOMIC_BUILD_CHANNEL", channel);
        }
        if let Some(commit) = commit {
            command.env("ATOMIC_BUILD_COMMIT", commit);
        }
        stdout(command.output().unwrap())
    }

    fn expected(source: &Path) -> String {
        let commit = git(source, &["rev-parse", "HEAD"]);
        format!("0.19.1 (dev {})", &commit[..12])
    }
}

#[test]
fn cached_build_tracks_edits_and_commits_including_packed_refs() {
    let fixture = Fixture::new(true);
    let source = &fixture.source;
    let initial = Fixture::expected(source);
    assert_eq!(fixture.build(source, None, None), initial);

    fs::write(source.join("untracked.txt"), "not part of the build\n").unwrap();
    assert_eq!(fixture.build(source, None, None), initial);

    fs::write(source.join("tracked.txt"), "modified\n").unwrap();
    let dirty = initial.replace(')', "-dirty)");
    assert_eq!(fixture.build(source, None, None), dirty);
    fs::write(source.join("tracked.txt"), "original\n").unwrap();
    assert_eq!(fixture.build(source, None, None), initial);
    fs::write(source.join("tracked.txt"), "modified again\n").unwrap();
    assert_eq!(fixture.build(source, None, None), dirty);
    git(source, &["add", "tracked.txt"]);
    assert_eq!(fixture.build(source, None, None), dirty);

    git(source, &["commit", "-m", "Modify tracked source"]);
    assert_eq!(fixture.build(source, None, None), Fixture::expected(source));

    git(source, &["pack-refs", "--all", "--prune"]);
    assert_eq!(fixture.build(source, None, None), Fixture::expected(source));
    git(
        source,
        &[
            "commit",
            "--allow-empty",
            "-m",
            "Move HEAD without source edits",
        ],
    );
    let expected = Fixture::expected(source);
    assert_ne!(expected, initial);
    assert_eq!(fixture.build(source, None, None), expected);

    // The resulting artifact keeps its identity away from the checkout, even
    // when the run-time environment asks for a different channel.
    let binary = fixture
        .target
        .join("debug")
        .join(format!("version-fixture{}", std::env::consts::EXE_SUFFIX));
    assert_eq!(
        stdout(
            Command::new(binary)
                .current_dir(fixture.dir.path())
                .env("ATOMIC_BUILD_CHANNEL", "release")
                .output()
                .unwrap()
        ),
        expected
    );
}

#[test]
fn detached_worktree_refreshes_its_own_head() {
    let fixture = Fixture::new(true);
    let worktree = fixture.dir.path().join("worktree");
    git(
        &fixture.source,
        &[
            "worktree",
            "add",
            "--detach",
            worktree.to_str().unwrap(),
            "HEAD",
        ],
    );
    let initial = Fixture::expected(&worktree);
    assert_eq!(fixture.build(&worktree, None, None), initial);

    git(
        &fixture.source,
        &["commit", "--allow-empty", "-m", "Next source commit"],
    );
    let next = git(&fixture.source, &["rev-parse", "HEAD"]);
    git(&worktree, &["checkout", "--detach", &next]);
    let expected = Fixture::expected(&worktree);
    assert_ne!(expected, initial);
    assert_eq!(fixture.build(&worktree, None, None), expected);
}

#[test]
fn archive_builds_and_channel_changes_refresh_the_version() {
    let fixture = Fixture::new(false);
    let source = &fixture.source;
    assert_eq!(fixture.build(source, None, None), "0.19.1 (dev unknown)");
    assert_eq!(
        fixture.build(
            source,
            Some("dev"),
            Some("0123456789abcdef0123456789abcdef01234567")
        ),
        "0.19.1 (dev 0123456789ab)"
    );
    assert_eq!(fixture.build(source, Some("release"), None), "0.19.1");
    assert_eq!(fixture.build(source, None, None), "0.19.1 (dev unknown)");
}
