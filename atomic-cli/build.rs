use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn short_commit(commit: &str) -> Option<&str> {
    let commit = commit.trim();
    (commit.len() >= 12 && commit.len() <= 64 && commit.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| &commit[..12])
}

fn watch_repository(root: &Path) {
    // --git-path resolves shared refs and worktree-local HEAD/index correctly.
    // Watch loose refs as well as packed refs: an empty commit need not change
    // any source file, and updating a packed branch can create a new loose ref.
    for name in ["HEAD", "index", "packed-refs", "refs/heads"] {
        if let Some(path) = git(root, &["rev-parse", "--git-path", name]) {
            let path = root.join(path.trim());
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }

    // Track unstaged edits too, including workspace files outside atomic-cli.
    // Watching only the index would leave a cached dirty marker stale.
    if let Some(files) = git(root, &["ls-files", "-z"]) {
        for file in files.split('\0').filter(|file| !file.is_empty()) {
            println!("cargo:rerun-if-changed={}", root.join(file).display());
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ATOMIC_BUILD_CHANNEL");
    println!("cargo:rerun-if-env-changed=ATOMIC_BUILD_COMMIT");

    let version = env::var("CARGO_PKG_VERSION").expect("Cargo supplies the package version");
    let channel = env::var("ATOMIC_BUILD_CHANNEL").unwrap_or_else(|_| "dev".to_string());
    match channel.as_str() {
        "release" => {
            println!("cargo:rustc-env=ATOMIC_CLI_VERSION={version}");
            return;
        }
        "dev" => {}
        _ => panic!("ATOMIC_BUILD_CHANNEL must be 'dev' or 'release'"),
    }

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let root = git(&manifest_dir, &["rev-parse", "--show-toplevel"])
        .map(|path| PathBuf::from(path.trim()));
    let mut dirty = false;
    if let Some(root) = &root {
        watch_repository(root);
        dirty = git(root, &["status", "--porcelain", "--untracked-files=no"])
            .is_some_and(|status| !status.trim().is_empty());
    }

    let commit = match env::var("ATOMIC_BUILD_COMMIT") {
        Ok(commit) => {
            let short = short_commit(&commit)
                .expect("ATOMIC_BUILD_COMMIT must contain a 12 to 64 character hexadecimal commit");
            short.to_string()
        }
        Err(_) => root
            .as_deref()
            .and_then(|root| git(root, &["rev-parse", "--verify", "HEAD"]))
            .and_then(|commit| short_commit(&commit).map(str::to_owned))
            .unwrap_or_else(|| "unknown".to_string()),
    };
    let dirty = if dirty { "-dirty" } else { "" };
    println!("cargo:rustc-env=ATOMIC_CLI_VERSION={version} (dev {commit}{dirty})");
}
