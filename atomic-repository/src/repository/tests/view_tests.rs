use super::*;

#[test]
fn test_set_current_view() {
    let (_temp_dir, mut repo) = create_temp_repo();

    // First create the view
    repo.create_view("feature-view").unwrap();

    // Then switch to it
    repo.set_current_view("feature-view").unwrap();
    assert_eq!(repo.current_view(), "feature-view");

    // Verify it persists - drop repo first to release lock
    let root = repo.root().to_path_buf();
    drop(repo);

    let reopened = Repository::open(&root).unwrap();
    assert_eq!(reopened.current_view(), "feature-view");
}

#[test]
fn test_set_current_view_nonexistent() {
    let (_temp_dir, mut repo) = create_temp_repo();

    // Trying to switch to a nonexistent view should fail
    let result = repo.set_current_view("nonexistent");
    assert!(matches!(result, Err(RepositoryError::ViewNotFound { .. })));
}

#[test]
fn test_create_view() {
    let (_temp_dir, mut repo) = create_temp_repo();

    // Create a new view
    repo.create_view("feature").unwrap();

    // Verify it exists
    assert!(repo.view_exists("feature").unwrap());

    // Creating the same view again should fail
    let result = repo.create_view("feature");
    assert!(matches!(
        result,
        Err(RepositoryError::ViewAlreadyExists { .. })
    ));
}

#[test]
fn test_list_views() {
    let (_temp_dir, mut repo) = create_temp_repo();

    // Should have default "dev" view
    let views = repo.list_views().unwrap();
    assert!(views.contains(&"dev".to_string()));

    // Create additional views
    repo.create_view("feature-a").unwrap();
    repo.create_view("feature-b").unwrap();

    let views = repo.list_views().unwrap();
    assert_eq!(views.len(), 3);
    assert!(views.contains(&"dev".to_string()));
    assert!(views.contains(&"feature-a".to_string()));
    assert!(views.contains(&"feature-b".to_string()));
}

#[test]
fn test_default_view_name() {
    let (_temp_dir, repo) = create_temp_repo();
    assert_eq!(repo.current_view(), "dev");
    assert_eq!(DEFAULT_STACK, "dev");
}

#[test]
fn test_delete_view() {
    use atomic_core::pristine::{MutTxnT, ViewScope, ViewTxnT};
    let (_temp_dir, mut repo) = create_temp_repo();

    // Create a draft view (only draft views can be deleted)
    {
        let mut txn = repo.pristine.write_txn().unwrap();
        let dev = txn.get_view("dev").unwrap().unwrap();
        txn.create_view("to-delete", ViewScope::Draft, Some(dev.id))
            .unwrap();
        txn.commit().unwrap();
    }
    assert!(repo.view_exists("to-delete").unwrap());

    // Delete the view
    repo.delete_view("to-delete").unwrap();

    // Verify it's gone
    assert!(!repo.view_exists("to-delete").unwrap());
}

#[test]
fn test_delete_view_nonexistent() {
    let (_temp_dir, mut repo) = create_temp_repo();

    // Trying to delete a nonexistent view should fail
    let result = repo.delete_view("nonexistent");
    assert!(matches!(result, Err(RepositoryError::ViewNotFound { .. })));
}

#[test]
fn test_delete_current_view_fails() {
    let (_temp_dir, mut repo) = create_temp_repo();

    // Trying to delete the current view should fail
    let result = repo.delete_view("dev");
    assert!(matches!(
        result,
        Err(RepositoryError::CannotDeleteCurrentView { .. })
    ));
}

#[test]
fn test_delete_view_preserves_others() {
    use atomic_core::pristine::{MutTxnT, ViewScope, ViewTxnT};
    let (_temp_dir, mut repo) = create_temp_repo();

    // Create two draft views (only draft views can be deleted)
    {
        let mut txn = repo.pristine.write_txn().unwrap();
        let dev = txn.get_view("dev").unwrap().unwrap();
        txn.create_view("keep-me", ViewScope::Draft, Some(dev.id))
            .unwrap();
        txn.create_view("delete-me", ViewScope::Draft, Some(dev.id))
            .unwrap();
        txn.commit().unwrap();
    }

    // Delete one
    repo.delete_view("delete-me").unwrap();

    // Verify the other still exists
    assert!(repo.view_exists("keep-me").unwrap());
    assert!(!repo.view_exists("delete-me").unwrap());
}

#[test]
fn test_get_view_info() {
    let (_temp_dir, mut repo) = create_temp_repo();

    // Create a view
    repo.create_view("info-test").unwrap();

    // Get info
    let info = repo.get_view_info("info-test").unwrap();
    assert_eq!(info.name, "info-test");
    assert_eq!(info.change_count, 0);
    assert!(info.is_empty());
}

#[test]
fn test_get_view_info_nonexistent() {
    let (_temp_dir, repo) = create_temp_repo();

    // Trying to get info for a nonexistent view should fail
    let result = repo.get_view_info("nonexistent");
    assert!(matches!(result, Err(RepositoryError::ViewNotFound { .. })));
}

#[test]
fn test_view_info_state_methods() {
    let (_temp_dir, mut repo) = create_temp_repo();

    repo.create_view("state-test").unwrap();
    let info = repo.get_view_info("state-test").unwrap();

    // Test state methods
    let base32 = info.state_base32();
    assert!(!base32.is_empty());

    let short = info.state_short();
    assert!(short.len() <= 12);

    // For an empty view
    assert!(info.is_empty());
}
