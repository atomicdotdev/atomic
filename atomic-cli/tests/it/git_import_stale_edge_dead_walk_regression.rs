// Temporary Unix-only Git shim support to unblock Windows native CI.
// Revisit after Windows support is defined; see the RFC temporary platform policy.
#![cfg(unix)]

//! Regression: the alive-walk's "alternate alive parent" check for a live
//! successor found behind a dead chain must not depend on traversal order.
//!
//! Original failure (ripgrep, Git commit 11a8f0ea "args: treat --count
//! --only-matching as --count-matches", first-parent ordinal 841): an
//! earlier commit inserted a block between `        })` and `    }`, which
//! in the additive edge model leaves the original `        })` → `    }`
//! edge in place as a stale shortcut. The failing commit then replaced one
//! block line with several new lines and deleted the block's LAST line (the
//! one whose successor is `    }`). The alive-walk reached the kept line
//! `        }` twice: once through the dead replaced line (which pushed it
//! on the DFS stack) and once at the end of the replacement chain. Because
//! the replacement chain's edge sorts first, `        }` was processed
//! before the chain's children were recorded, so when its dead-walk found
//! `    }` behind the deleted line, the stale parent `        })` looked
//! like an alternate alive parent that did NOT reach `        }` in the
//! partially built alive graph. `    }` was therefore not attached under
//! `        }`, surfaced only as a sibling of the block under `        })`,
//! and the fork resolution's change-DAG supersession dropped it: exactly
//! `    }\n` went missing (`renders 41554 byte(s) but the commit tree
//! expects 41560 byte(s)`), failing the fail-closed staged verification.
//!
//! The three states below reproduce the same mechanism with unambiguous
//! line content (pre-fix signature: `renders 29 byte(s) but the commit tree
//! expects 34 byte(s)` — the `quux` line was dropped).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

const PATH: &str = "src/f.rs";

/// Base file: `alpha` is the line a block will be inserted after, `quux`
/// the line it is inserted before.
const STATE_1: &[u8] = b"alpha\nquux\ntail\n";
/// A five-line block inserted between `alpha` and `quux`. The additive
/// edge model keeps the original `alpha -> quux` edge alongside the new
/// `alpha -> i1` and `i5 -> quux` edges.
const STATE_2: &[u8] = b"alpha\ni1\ni2\ni3\ni4\ni5\nquux\ntail\n";
/// Replace `i3` with three new lines and delete `i5`, keeping `i4`: `i4`
/// is now reachable both through the dead `i3` and at the end of the
/// replacement chain, and `quux` sits behind the dead `i5`.
const STATE_3: &[u8] = b"alpha\ni1\ni2\nm1\nm2\nm3\ni4\nquux\ntail\n";

fn atomic(root: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .output()
        .expect("run atomic")
}

fn atomic_ok(root: &Path, home: &Path, args: &[&str]) -> String {
    let output = atomic(root, home, args);
    assert!(
        output.status.success(),
        "atomic {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Stale Edge Tests")
        .env("GIT_AUTHOR_EMAIL", "stale-edge@example.com")
        .env("GIT_COMMITTER_NAME", "Stale Edge Tests")
        .env("GIT_COMMITTER_EMAIL", "stale-edge@example.com")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf8 git output")
        .trim()
        .to_string()
}

/// Build the three-state Git repository: base file, insert a block between
/// two existing lines, then replace a block line and delete the block's
/// last line.
fn fixture() -> (PathBuf, PathBuf) {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join("src")).unwrap();
    for (i, state) in [STATE_1, STATE_2, STATE_3].iter().enumerate() {
        fs::write(root.join(PATH), state).unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", &format!("state {}", i + 1)]);
    }
    (root, home)
}

/// The graph-rendered bytes for `PATH` on `main`, read in-process exactly
/// as another reader would.
fn rendered_on_main(root: &Path) -> Vec<u8> {
    let repo = atomic_repository::Repository::open(root).unwrap();
    let content = repo
        .get_file_content_on_view(PATH, "main")
        .expect("content retrieval must not fail");
    content.unwrap_or_else(|| panic!("'{PATH}' must be present on view main"))
}

#[test]
fn import_of_replace_and_delete_behind_stale_insert_edge_renders_byte_exact() {
    let (root, home) = fixture();

    // The importer's per-commit staged verification is fail-closed: a
    // successful import proves EVERY intermediate state rendered
    // byte-exactly against its commit tree. Pre-fix, this failed at the
    // last commit with "renders 29 byte(s) but the commit tree expects 34".
    let summary = atomic_ok(&root, &home, &["git", "import", "--no-vault"]);
    assert!(
        summary.contains("3 changes"),
        "expected 3 imported changes, got:\n{summary}"
    );

    // The final state's graph render is byte-exact (not only the worktree).
    let rendered = rendered_on_main(&root);
    assert_eq!(
        rendered, STATE_3,
        "graph render must equal the final Git tree byte-for-byte"
    );

    // `quux` — the line behind the deleted `i5`, still reachable through
    // the stale `alpha -> quux` edge — is the exact line the bug dropped.
    assert!(
        rendered.windows(6).any(|w| w == b"\nquux\n"),
        "the line behind the deleted block tail must survive"
    );

    // The workspace is clean and the shim layer verifies.
    let status = atomic_ok(&root, &home, &["status"]);
    assert!(
        status.contains("nothing to record"),
        "status must report a clean workspace, got:\n{status}"
    );
    atomic_ok(&root, &home, &["git", "shim", "verify"]);
}

#[test]
fn native_record_of_replace_and_delete_behind_stale_insert_edge_renders_byte_exact() {
    // The same walk serves the native record path (status / materialization
    // read through the identical retrieval), so the fix must hold there
    // too: record the three states with `atomic record` and prove the final
    // graph render and a clean status. Pre-fix, `atomic status` reported
    // the file as modified right after the third record because the graph
    // render had dropped the `quux` line.
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    fs::create_dir_all(root.join("src")).unwrap();

    atomic_ok(&root, &home, &["init", "--view", "main", "--no-vault"]);
    for (i, state) in [STATE_1, STATE_2, STATE_3].iter().enumerate() {
        fs::write(root.join(PATH), state).unwrap();
        atomic_ok(&root, &home, &["add", PATH]);
        let message = format!("state {}", i + 1);
        atomic_ok(&root, &home, &["record", "-m", &message]);
    }

    let rendered = rendered_on_main(&root);
    assert_eq!(
        rendered, STATE_3,
        "graph render must equal the last recorded state byte-for-byte"
    );
    let status = atomic_ok(&root, &home, &["status"]);
    assert!(
        status.contains("nothing to record"),
        "status must report a clean workspace after record, got:\n{status}"
    );
}
