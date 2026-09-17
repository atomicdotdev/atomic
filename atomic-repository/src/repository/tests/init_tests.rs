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
    assert_eq!(opened.root(), root);
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
    repo.provision_sandbox(&sandbox, repo.current_view())
        .unwrap();

    assert_eq!(
        Repository::canonical_change_store_path(&sandbox).unwrap(),
        repo.redb_change_store_path()
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

    let abs_path = temp_dir.path().join("src").join("main.rs");
    let rel_path = repo.to_relative(&abs_path).unwrap();

    assert_eq!(rel_path, PathBuf::from("src/main.rs"));
}

#[test]
fn test_to_absolute() {
    let (temp_dir, repo) = create_temp_repo();

    let rel_path = PathBuf::from("src/main.rs");
    let abs_path = repo.to_absolute(&rel_path);

    assert_eq!(abs_path, temp_dir.path().join("src/main.rs"));
}

#[test]
fn test_is_internal_path() {
    let (_temp_dir, repo) = create_temp_repo();

    assert!(repo.is_internal_path(repo.dot_dir()));
    assert!(repo.is_internal_path(repo.pristine_path()));
    assert!(repo.is_internal_path(repo.changes_dir()));
    assert!(!repo.is_internal_path(repo.root().join("src")));
}
