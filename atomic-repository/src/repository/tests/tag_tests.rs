use super::*;

#[test]
fn test_repo_create_tag() {
    let (_temp_dir, repo) = create_temp_repo();

    let tag = repo.create_tag("v1.0.0", TagOptions::default()).unwrap();

    assert_eq!(tag.name, "v1.0.0");
    assert_eq!(tag.view, DEFAULT_STACK);
    assert!(!tag.is_annotated());
}

#[test]
fn test_repo_create_annotated_tag() {
    let (_temp_dir, repo) = create_temp_repo();

    let options = TagOptions::default()
        .message("Release 1.0")
        .author("Alice", Some("alice@example.com"));

    let tag = repo.create_tag("v1.0.0", options).unwrap();

    assert_eq!(tag.name, "v1.0.0");
    assert!(tag.is_annotated());
    assert_eq!(tag.message(), Some("Release 1.0"));
}

#[test]
fn test_repo_get_tag() {
    let (_temp_dir, repo) = create_temp_repo();

    repo.create_tag("v1.0.0", TagOptions::default()).unwrap();

    let tag = repo.get_tag("v1.0.0").unwrap();
    assert!(tag.is_some());
    assert_eq!(tag.unwrap().name, "v1.0.0");
}

#[test]
fn test_repo_get_tag_not_found() {
    let (_temp_dir, repo) = create_temp_repo();

    let tag = repo.get_tag("nonexistent").unwrap();
    assert!(tag.is_none());
}

#[test]
fn test_repo_list_tags() {
    let (_temp_dir, repo) = create_temp_repo();

    repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
    repo.create_tag("v2.0.0", TagOptions::default()).unwrap();

    let tags = repo.list_tags().unwrap();
    assert_eq!(tags.len(), 2);
}

#[test]
fn test_repo_list_tags_empty() {
    let (_temp_dir, repo) = create_temp_repo();

    let tags = repo.list_tags().unwrap();
    assert!(tags.is_empty());
}

#[test]
fn test_repo_list_tags_filtered() {
    let (_temp_dir, repo) = create_temp_repo();

    repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
    repo.create_tag("v2.0.0", TagOptions::default().message("Annotated"))
        .unwrap();
    repo.create_tag("release-1", TagOptions::default()).unwrap();

    // Filter by pattern
    let filter = TagFilter::new().pattern("v*");
    let tags = repo.list_tags_filtered(&filter).unwrap();
    assert_eq!(tags.len(), 2);

    // Filter annotated only
    let filter = TagFilter::new().annotated_only();
    let tags = repo.list_tags_filtered(&filter).unwrap();
    assert_eq!(tags.len(), 1);
}

#[test]
fn test_repo_delete_tag() {
    let (_temp_dir, repo) = create_temp_repo();

    repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
    assert!(repo.delete_tag("v1.0.0").unwrap());
    assert!(repo.get_tag("v1.0.0").unwrap().is_none());
}

#[test]
fn test_repo_delete_tag_not_found() {
    let (_temp_dir, repo) = create_temp_repo();

    assert!(!repo.delete_tag("nonexistent").unwrap());
}

#[test]
fn test_repo_tag_count() {
    let (_temp_dir, repo) = create_temp_repo();

    assert_eq!(repo.tag_count().unwrap(), 0);

    repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
    repo.create_tag("v2.0.0", TagOptions::default()).unwrap();

    assert_eq!(repo.tag_count().unwrap(), 2);
}

#[test]
fn test_repo_create_tag_invalid_name() {
    let (_temp_dir, repo) = create_temp_repo();

    let result = repo.create_tag("", TagOptions::default());
    assert!(matches!(
        result,
        Err(RepositoryError::InvalidTagName { .. })
    ));

    let result = repo.create_tag("bad/name", TagOptions::default());
    assert!(matches!(
        result,
        Err(RepositoryError::InvalidTagName { .. })
    ));
}

#[test]
fn test_repo_create_tag_already_exists() {
    let (_temp_dir, repo) = create_temp_repo();

    repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
    let result = repo.create_tag("v1.0.0", TagOptions::default());

    // Should fail because tag exists
    assert!(result.is_err());
}

#[test]
fn test_repo_create_tag_force_overwrite() {
    let (_temp_dir, repo) = create_temp_repo();

    repo.create_tag("v1.0.0", TagOptions::default()).unwrap();

    // Force overwrite should succeed
    let tag = repo
        .create_tag("v1.0.0", TagOptions::default().force(true))
        .unwrap();
    assert_eq!(tag.name, "v1.0.0");
}

#[test]
fn test_repo_get_tag_from_view() {
    let (_temp_dir, repo) = create_temp_repo();

    // Create tag in current view
    repo.create_tag("v1.0.0", TagOptions::default()).unwrap();

    // Get from current view (default behavior)
    let tag = repo.get_tag("v1.0.0").unwrap();
    assert!(tag.is_some());

    // Get from specific view
    let tag = repo.get_tag_from_view("v1.0.0", DEFAULT_STACK).unwrap();
    assert!(tag.is_some());

    // Get from different view (should not exist)
    let tag = repo.get_tag_from_view("v1.0.0", "other").unwrap();
    assert!(tag.is_none());
}

#[test]
fn test_repo_list_tags_for_view() {
    let (_temp_dir, repo) = create_temp_repo();

    // Create tags in current view
    repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
    repo.create_tag("v2.0.0", TagOptions::default()).unwrap();

    // list_tags returns current view only
    let tags = repo.list_tags().unwrap();
    assert_eq!(tags.len(), 2);

    // list_tags_for_view with current view
    let tags = repo.list_tags_for_view(DEFAULT_STACK).unwrap();
    assert_eq!(tags.len(), 2);

    // list_tags_for_view with other view (empty)
    let tags = repo.list_tags_for_view("other").unwrap();
    assert!(tags.is_empty());
}

#[test]
fn test_repo_list_all_tags() {
    let (_temp_dir, repo) = create_temp_repo();

    // Create tags (all go to current view since we can't easily switch)
    repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
    repo.create_tag("v2.0.0", TagOptions::default()).unwrap();

    // list_all_tags includes all views
    let all_tags = repo.list_all_tags().unwrap();
    assert_eq!(all_tags.len(), 2);
}

#[test]
fn test_repo_tag_count_for_view() {
    let (_temp_dir, repo) = create_temp_repo();

    repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
    repo.create_tag("v2.0.0", TagOptions::default()).unwrap();

    // tag_count returns count for current view
    assert_eq!(repo.tag_count().unwrap(), 2);

    // tag_count_for_view with specific view
    assert_eq!(repo.tag_count_for_view(DEFAULT_STACK).unwrap(), 2);
    assert_eq!(repo.tag_count_for_view("other").unwrap(), 0);

    // tag_count_all returns total across all views
    assert_eq!(repo.tag_count_all().unwrap(), 2);
}

#[test]
fn test_repo_delete_tag_from_view() {
    let (_temp_dir, repo) = create_temp_repo();

    repo.create_tag("v1.0.0", TagOptions::default()).unwrap();

    // Delete from wrong view should return false
    assert!(!repo.delete_tag_from_view("v1.0.0", "other").unwrap());

    // Tag should still exist
    assert!(repo.get_tag("v1.0.0").unwrap().is_some());

    // Delete from correct view should succeed
    assert!(repo.delete_tag_from_view("v1.0.0", DEFAULT_STACK).unwrap());
    assert!(repo.get_tag("v1.0.0").unwrap().is_none());
}

#[test]
fn test_repo_list_tag_views() {
    let (_temp_dir, repo) = create_temp_repo();

    // Initially no views have tags
    let views = repo.list_tag_views().unwrap();
    assert!(views.is_empty());

    // Create a tag
    repo.create_tag("v1.0.0", TagOptions::default()).unwrap();

    // Now current view should be listed
    let views = repo.list_tag_views().unwrap();
    assert_eq!(views.len(), 1);
    assert!(views.contains(&DEFAULT_STACK.to_string()));
}
