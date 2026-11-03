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
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ATOMIC_BUILD_CHANNEL");
    println!("cargo:rerun-if-env-changed=ATOMIC_BUILD_COMMIT");
    let version = env::var("CARGO_PKG_VERSION").expect("Cargo supplies the package version");
    let channel = env::var("ATOMIC_BUILD_CHANNEL").unwrap_or_else(|_| "dev".to_string());
    if channel == "release" {
        println!("cargo:rustc-env=ATOMIC_BUILD_VERSION={version}");
        return;
    }
    assert_eq!(
        channel, "dev",
        "ATOMIC_BUILD_CHANNEL must be 'dev' or 'release'"
    );
    let dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo supplies the manifest directory"),
    );
    let root = git(&dir, &["rev-parse", "--show-toplevel"]).map(|path| PathBuf::from(path.trim()));
    let mut dirty = false;
    if let Some(root) = &root {
        for name in ["HEAD", "index", "packed-refs", "refs/heads"] {
            if let Some(path) = git(root, &["rev-parse", "--git-path", name]) {
                let path = root.join(path.trim());
                if path.exists() {
                    println!("cargo:rerun-if-changed={}", path.display());
                }
            }
        }
        if let Some(files) = git(root, &["ls-files", "-z"]) {
            for file in files.split('\0').filter(|file| !file.is_empty()) {
                println!("cargo:rerun-if-changed={}", root.join(file).display());
            }
        }
        dirty = git(root, &["status", "--porcelain", "--untracked-files=no"])
            .is_some_and(|status| !status.trim().is_empty());
    }
    let supplied_commit = env::var("ATOMIC_BUILD_COMMIT").ok();
    let commit = supplied_commit
        .clone()
        .or_else(|| {
            root.as_deref()
                .and_then(|root| git(root, &["rev-parse", "--verify", "HEAD"]))
        })
        .unwrap_or_else(|| "unknown".to_string());
    let commit = commit.trim();
    let commit = if commit.len() >= 12
        && commit.len() <= 64
        && commit.bytes().all(|b| b.is_ascii_hexdigit())
    {
        &commit[..12]
    } else {
        assert!(
            supplied_commit.is_none(),
            "ATOMIC_BUILD_COMMIT must contain a 12 to 64 character hexadecimal commit"
        );
        "unknown"
    };
    let dirty = if dirty { "-dirty" } else { "" };
    println!("cargo:rustc-env=ATOMIC_BUILD_VERSION={version} (dev {commit}{dirty})");
}
