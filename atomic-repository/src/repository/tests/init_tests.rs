use super::*;

#[test]
fn test_init_creates_structure() {
    let temp_dir = TempDir::new().unwrap();
    let repo = Repository::init(temp_dir.path()).unwrap();

    assert!(repo.dot_dir().exists());
    assert!(repo.pristine_path().exists());
    assert!(repo.changes_dir().exists());
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
