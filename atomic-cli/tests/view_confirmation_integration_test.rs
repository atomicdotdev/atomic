//! Verify that non-interactive split/delete cannot silently orphan work.

use std::process::{Command, Stdio};

use atomic_core::change::ChangeHeader;
use atomic_core::types::Base32;
use atomic_repository::{RecordOptions, Repository};

fn run(root: &std::path::Path, args: &[&str], succeeds: bool) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_atomic"))
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(output.status.success(), succeeds, "{args:?}: {text}");
    text
}

#[test]
fn split_and_orphaning_delete_require_explicit_confirmation_without_a_terminal() {
    let temp = tempfile::tempdir().unwrap();
    let hash;
    let source;
    {
        let repo = Repository::init(temp.path()).unwrap();
        source = repo.current_view().to_string();
        std::fs::write(temp.path().join("f.txt"), "keep this work\n").unwrap();
        repo.add("f.txt", Default::default()).unwrap();
        hash = *repo
            .record(
                ChangeHeader::new("important work"),
                RecordOptions::default(),
            )
            .unwrap()
            .hash();
    }

    run(
        temp.path(),
        &["view", "split", "wip", "--last", "1", "--dry-run"],
        true,
    );
    let refusal = run(temp.path(), &["view", "split", "wip", "--last", "1"], false);
    assert!(refusal.contains("--confirm"));
    {
        let repo = Repository::open(temp.path()).unwrap();
        assert!(!repo.view_exists("wip").unwrap());
        assert_eq!(repo.view_own_change_hashes(&source).unwrap(), vec![hash]);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("f.txt")).unwrap(),
            "keep this work\n"
        );
    }

    run(
        temp.path(),
        &["view", "split", "wip", "--last", "1", "--confirm"],
        true,
    );
    let refusal = run(temp.path(), &["view", "delete", "wip"], false);
    assert!(refusal.contains("orphan"));
    assert!(refusal.contains(&hash.to_base32()));
    assert!(refusal.contains("--force"));
    {
        let repo = Repository::open(temp.path()).unwrap();
        assert!(repo.view_exists("wip").unwrap());
        assert_eq!(repo.view_own_change_hashes("wip").unwrap(), vec![hash]);
    }

    run(temp.path(), &["view", "delete", "wip", "--force"], true);
    let repo = Repository::open(temp.path()).unwrap();
    assert!(!repo.view_exists("wip").unwrap());
    assert!(repo.views_containing_change(&hash).unwrap().is_empty());
    assert!(
        repo.load_change(&hash).is_ok(),
        "view deletion does not erase the change object"
    );
}
