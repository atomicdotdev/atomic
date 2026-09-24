use super::*;

#[test]
fn test_init_creates_structure() {
    let temp_dir = TempDir::new().unwrap();
    let repo = Repository::init(temp_dir.path()).unwrap();

    assert!(repo.dot_dir().exists());
    assert!(repo.pristine_path().exists());
    assert!(repo.changes_dir().exists());
    assert_eq!(
        repo.redb_change_store_path(),
        repo.dot_dir().join(REDB_CHANGE_STORE_FILE)
    );
    assert!(!repo.redb_change_store_path().exists());
    assert!(repo.config_path().exists());
}

#[test]
fn test_init_fails_if_exists() {
    let (temp_dir, _repo) = create_temp_repo();

    let result = Repository::init(temp_dir.path());
    assert!(matches!(result, Err(RepositoryError::AlreadyExists { .. })));
}

#[test]
fn test_open_existing() {
    let (temp_dir, repo) = create_temp_repo();
    let root = repo.root().to_path_buf();

    // Drop the original repository to release the database lock
    drop(repo);

    let opened = Repository::open(temp_dir.path()).unwrap();
    // The repository canonicalizes its root (macOS tempdirs live behind
    // /var → /private/var); compare canonicalized on both sides.
    assert_eq!(
        std::fs::canonicalize(opened.root()).unwrap(),
        std::fs::canonicalize(&root).unwrap()
    );
    assert_eq!(opened.current_view(), DEFAULT_STACK);
}

#[test]
fn test_open_legacy_repository_defers_redb_store_without_touching_changes() {
    let (temp_dir, repo) = create_temp_repo();
    let store_path = repo.redb_change_store_path();
    let legacy_change = repo.changes_dir().join("legacy-change");
    std::fs::write(&legacy_change, b"existing filesystem authority").unwrap();
    drop(repo);

    assert!(!store_path.exists());

    let reopened = Repository::open(temp_dir.path()).unwrap();
    assert_eq!(reopened.redb_change_store_path(), store_path);
    assert!(!store_path.exists());
    assert_eq!(
        std::fs::read(&legacy_change).unwrap(),
        b"existing filesystem authority"
    );
}

#[test]
fn test_canonical_change_store_path_follows_sandbox_pointer() {
    let temp_dir = TempDir::new().unwrap();
    let repo_root = temp_dir.path().join("repo");
    let sandbox = temp_dir.path().join("agent-sandbox");
    let repo = Repository::init(&repo_root).unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    repo.provision_sandbox(working_copy, &sandbox, repo.current_view())
        .unwrap();

    // Compare canonical dot_dirs (macOS tempdirs live behind
    // /var → /private/var); the change-store file itself is created lazily,
    // so canonicalize the directory rather than the file.
    assert_eq!(
        std::fs::canonicalize(Repository::canonical_dot_dir(&sandbox).unwrap()).unwrap(),
        std::fs::canonicalize(repo.dot_dir()).unwrap()
    );
    assert_eq!(
        Repository::canonical_change_store_path(&sandbox).unwrap(),
        Repository::canonical_dot_dir(&sandbox)
            .unwrap()
            .join("changes.redb")
    );
}

#[test]
fn test_open_from_subdirectory() {
    let (temp_dir, repo) = create_temp_repo();
    let root = repo.root().to_path_buf();

    // Drop the original repository to release the database lock
    drop(repo);

    // Create a subdirectory
    let subdir = temp_dir.path().join("src").join("lib");
    std::fs::create_dir_all(&subdir).unwrap();

    // Open from subdirectory should find the root
    let opened = Repository::open(&subdir).unwrap();
    assert_eq!(opened.root(), root);
}

#[test]
fn test_open_not_found() {
    let temp_dir = TempDir::new().unwrap();
    let result = Repository::open(temp_dir.path());
    assert!(matches!(result, Err(RepositoryError::NotFound { .. })));
}

#[test]
fn test_is_repository() {
    let (temp_dir, _repo) = create_temp_repo();

    assert!(Repository::is_repository(temp_dir.path()));

    let non_repo = TempDir::new().unwrap();
    assert!(!Repository::is_repository(non_repo.path()));
}

#[test]
fn test_change_path() {
    let (_temp_dir, repo) = create_temp_repo();

    let hash = "ABCDEF123456";
    let path = repo.change_path(hash);

    assert!(path.to_string_lossy().contains("AB"));
    assert!(path.to_string_lossy().contains(hash));
}

#[test]
fn test_to_relative() {
    let (temp_dir, repo) = create_temp_repo();

    // repo.root() is canonical (e.g. macOS /var -> /private/var); derive
    // the input from the canonical root so the comparison is platform-fair.
    let root = std::fs::canonicalize(temp_dir.path()).unwrap();
    let abs_path = root.join("src").join("main.rs");
    let rel_path = repo.to_relative(&abs_path).unwrap();

    assert_eq!(rel_path, PathBuf::from("src/main.rs"));
}

#[test]
fn test_to_absolute() {
    let (temp_dir, repo) = create_temp_repo();

    let rel_path = PathBuf::from("src/main.rs");
    let abs_path = repo.to_absolute(&rel_path);

    // repo.root() is canonical (e.g. macOS /var -> /private/var).
    let root = std::fs::canonicalize(temp_dir.path()).unwrap();
    assert_eq!(abs_path, root.join("src/main.rs"));
}

#[test]
fn test_is_internal_path() {
    let (_temp_dir, repo) = create_temp_repo();

    assert!(repo.is_internal_path(repo.dot_dir()));
    assert!(repo.is_internal_path(repo.pristine_path()));
    assert!(repo.is_internal_path(repo.changes_dir()));
    assert!(!repo.is_internal_path(repo.root().join("src")));
}

/// Dropping a repository that performed a record must release the redb lock
/// so the bridge checkpoint verifier can reopen (CB-8A record projection).
#[test]
fn test_drop_after_record_releases_the_database_lock() {
    let (_temp_dir, repo) = create_temp_repo();
    let root = repo.root().to_path_buf();
    std::fs::write(root.join("file.txt"), b"content\n").unwrap();
    repo.add(root.join("file.txt"), Default::default()).unwrap();
    let outcome = repo
        .record_with_message("record then reopen", Default::default())
        .unwrap();
    assert!(outcome.was_applied());
    drop(repo);
    assert!(
        Repository::open(&root).is_ok(),
        "sequential reopen after record must succeed"
    );
}

/// The CB-8A record projection sequence: workspace transaction + record +
/// drop, then a fresh writable open (the checkpoint verifier's) must succeed.
#[test]
fn test_workspace_record_then_reopen_releases_the_database_lock() {
    let (_temp_dir, repo) = create_temp_repo();
    let root = repo.root().to_path_buf();
    std::fs::write(root.join("file.txt"), b"content\n").unwrap();
    repo.add(root.join("file.txt"), Default::default()).unwrap();

    // Mirror the record command: open_for_workspace_transaction, enter the
    // workspace, record, then drop everything.
    drop(repo);
    let mut repo = Repository::open_for_workspace_transaction(&root).unwrap();
    let start = repo
        .begin_workspace_txn(WorkspaceTxnMode::Reconcile)
        .unwrap();
    let workspace = match start {
        crate::WorkspaceTxnStart::Ready(workspace) => workspace,
        crate::WorkspaceTxnStart::Remediation(_) => panic!("expected ready workspace"),
    };
    let working_copy = workspace.working_copy();
    let outcome = repo
        .record_with_lifecycle(
            working_copy,
            atomic_core::change::ChangeHeader::builder()
                .message("workspace record")
                .author(atomic_core::change::Author::new("T", Some("t@t")))
                .build(),
            Default::default(),
            crate::repository::snapshot::RecordLifecycle::Durable,
        )
        .unwrap();
    assert!(outcome.was_applied());
    drop(workspace);
    drop(repo);
    assert!(
        Repository::open(&root).is_ok(),
        "reopen after workspace record must succeed"
    );
}
