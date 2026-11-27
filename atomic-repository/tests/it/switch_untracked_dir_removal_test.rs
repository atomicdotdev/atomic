//! Switching to a view that no longer projects a recorded directory must
//! also sweep the directory's untracked, data-free scaffolds — not fail
//! closed on `DirectoryNotEmpty` — while untracked files and symlinks
//! still block the removal.
//!
//! `atomic init` leaves exactly this shape: the recorded vault defaults
//! plus the untracked `goals/`, `intents/`, and `scratch/` scaffolds
//! `init_vault` creates on disk, so the plain quick-start flow
//! (`init` → `view create` → `view switch`) failed on every platform
//! with "untracked children are never removed recursively".

use atomic_repository::{RecordOptions, Repository};

fn record(repo: &Repository, message: &str) {
    let working_copy = repo.require_working_copy_id().unwrap();
    let outcome = repo
        .record_with_message(working_copy, message, RecordOptions::default())
        .unwrap();
    assert!(!outcome.has_errors(), "{:?}", outcome.errors());
}

/// The `atomic init --view <name>` shape: a draft anchored on the shared
/// root holds the recorded changes, so a later default overlay (anchored
/// on the same shared root, empty own log) projects nothing and the
/// switch must remove the draft's materialized paths.
fn seed_draft_view(repo: &mut Repository) {
    let working_copy = repo.require_working_copy_id().unwrap();
    repo.create_overlay_view("work", None, None).unwrap();
    repo.switch_view(working_copy, "work").unwrap();
}

#[test]
fn switch_to_empty_view_sweeps_untracked_scaffold_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    seed_draft_view(&mut repo);

    // A recorded directory...
    std::fs::create_dir_all(dir.path().join("app")).unwrap();
    std::fs::write(dir.path().join("app/a.txt"), "a\n").unwrap();
    repo.add(working_copy, "app/a.txt", Default::default())
        .unwrap();
    record(&repo, "record app");

    // ...with untracked, data-free scaffolds beneath it (the shape
    // `atomic init` leaves under `.vault/{goals,intents,scratch}`).
    std::fs::create_dir_all(dir.path().join("app/empty")).unwrap();
    std::fs::create_dir_all(dir.path().join("app/nested/deeper")).unwrap();

    repo.create_overlay_view("empty-view", None, None).unwrap();
    repo.switch_view(working_copy, "empty-view").unwrap();

    assert!(!dir.path().join("app").exists());
    assert!(!dir.path().join("app/empty").exists());
    assert!(!dir.path().join("app/nested").exists());
}

#[test]
fn switch_to_empty_view_still_fails_closed_on_untracked_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    seed_draft_view(&mut repo);

    std::fs::create_dir_all(dir.path().join("app")).unwrap();
    std::fs::write(dir.path().join("app/a.txt"), "a\n").unwrap();
    repo.add(working_copy, "app/a.txt", Default::default())
        .unwrap();
    record(&repo, "record app");

    // Untracked user bytes beneath the removed directory keep the removal
    // failing closed: the sweep never touches files.
    std::fs::create_dir_all(dir.path().join("app/keep")).unwrap();
    std::fs::write(dir.path().join("app/keep/user.txt"), "user\n").unwrap();

    repo.create_overlay_view("empty-view", None, None).unwrap();
    let error = match repo.switch_view(working_copy, "empty-view") {
        Err(error) => error,
        Ok(_) => panic!("an untracked file beneath a removed directory must block the switch"),
    };
    assert!(
        error
            .to_string()
            .contains("untracked children are never removed recursively"),
        "unexpected error: {error}"
    );
    // The untracked bytes survive the failed switch.
    assert_eq!(
        std::fs::read(dir.path().join("app/keep/user.txt")).unwrap(),
        b"user\n"
    );
}

#[test]
fn vault_scaffold_dirs_do_not_block_switching_away() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    seed_draft_view(&mut repo);

    // The `atomic init` shape: recorded vault defaults plus the untracked
    // goals/intents/scratch scaffolds `init_vault` leaves on disk.
    repo.init_vault().unwrap();
    for path in [
        ".vault/prompts/system_prompt.md",
        ".vault/skills/atomic-vault.md",
        ".vault/skills/code-intelligence.md",
        ".vault/memory/MEMORY.md",
    ] {
        repo.add(working_copy, path, Default::default()).unwrap();
    }
    record(&repo, "Initialize vault");
    assert!(dir.path().join(".vault/goals").exists());

    repo.create_overlay_view("empty-view", None, None).unwrap();
    repo.switch_view(working_copy, "empty-view").unwrap();
    assert!(!dir.path().join(".vault").exists());

    // Switching back re-materializes the recorded vault content.
    repo.switch_view(working_copy, "work").unwrap();
    assert!(dir.path().join(".vault/memory/MEMORY.md").exists());
}
