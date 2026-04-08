//! Tests for the tag management module.

#[cfg(test)]
mod tests {
    use crate::tags::{
        count_all_tags, count_tags, delete_tag, list_all_tags, list_tag_views, list_tags,
        list_tags_filtered, load_tag, load_tag_any_view, matches_pattern, save_tag, save_tag_force,
        tag_file_path, validate_tag_name, view_tags_dir, Tag, TagError, TagFilter, TagOptions,
        TagSort,
    };
    use atomic_core::change::Author;
    use atomic_core::types::Merkle;
    use chrono::DateTime;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    // Tag Tests

    #[test]
    fn test_tag_new() {
        let state = Merkle::of(b"test state");
        let tag = Tag::new("v1.0.0", "main", 42, state);

        assert_eq!(tag.name, "v1.0.0");
        assert_eq!(tag.view, "main");
        assert_eq!(tag.sequence, 42);
        assert_eq!(tag.state, state);
        assert!(!tag.is_annotated());
        assert!(tag.is_lightweight());
    }

    #[test]
    fn test_tag_annotated() {
        let state = Merkle::of(b"test state");
        let author = Author::new("Alice", Some("alice@example.com"));
        let tag = Tag::annotated("v1.0.0", "main", 42, state, "Release 1.0", author);

        assert!(tag.is_annotated());
        assert!(!tag.is_lightweight());
        assert_eq!(tag.message(), Some("Release 1.0"));
        assert!(tag.author().is_some());
    }

    #[test]
    fn test_tag_builder_pattern() {
        let state = Merkle::of(b"test state");
        let tag = Tag::new("v1.0.0", "main", 42, state)
            .with_message("Release notes")
            .with_author(Author::new("Bob", None::<String>));

        assert!(tag.is_annotated());
        assert_eq!(tag.message(), Some("Release notes"));
    }

    #[test]
    fn test_tag_display() {
        let state = Merkle::of(b"test state");
        let tag = Tag::new("v1.0.0", "main", 42, state);

        let display = format!("{}", tag);
        assert!(display.contains("v1.0.0"));
        assert!(display.contains("42"));
        assert!(display.contains("main"));
    }

    #[test]
    fn test_tag_equality() {
        let state = Merkle::of(b"test state");
        let tag1 = Tag::new("v1.0.0", "main", 42, state)
            .with_timestamp(DateTime::from_timestamp(1000, 0).unwrap());
        let tag2 = Tag::new("v1.0.0", "main", 42, state)
            .with_timestamp(DateTime::from_timestamp(1000, 0).unwrap());

        assert_eq!(tag1, tag2);
    }

    // TagOptions Tests

    #[test]
    fn test_tag_options_default() {
        let options = TagOptions::default();

        assert!(options.message.is_none());
        assert!(options.author.is_none());
        assert!(options.view.is_none());
        assert!(options.sequence.is_none());
        assert!(!options.force);
        assert!(!options.is_annotated());
    }

    #[test]
    fn test_tag_options_builder() {
        let options = TagOptions::new()
            .message("Test message")
            .author("Alice", Some("alice@example.com"))
            .view("feature")
            .sequence(10)
            .force(true);

        assert_eq!(options.message, Some("Test message".to_string()));
        assert!(options.author.is_some());
        assert_eq!(options.view, Some("feature".to_string()));
        assert_eq!(options.sequence, Some(10));
        assert!(options.force);
        assert!(options.is_annotated());
    }

    #[test]
    fn test_tag_options_annotated() {
        let options = TagOptions::annotated("Release notes");

        assert!(options.is_annotated());
        assert_eq!(options.message, Some("Release notes".to_string()));
    }

    // TagFilter Tests

    #[test]
    fn test_tag_filter_default() {
        let filter = TagFilter::default();

        assert!(filter.view.is_none());
        assert!(filter.pattern.is_none());
        assert!(!filter.annotated_only);
        assert!(!filter.lightweight_only);
    }

    #[test]
    fn test_tag_filter_builder() {
        let filter = TagFilter::new()
            .view("main")
            .pattern("v*")
            .annotated_only()
            .sort(TagSort::Timestamp)
            .limit(10);

        assert_eq!(filter.view, Some("main".to_string()));
        assert_eq!(filter.pattern, Some("v*".to_string()));
        assert!(filter.annotated_only);
        assert_eq!(filter.sort, TagSort::Timestamp);
        assert_eq!(filter.limit, Some(10));
    }

    #[test]
    fn test_tag_filter_matches_view() {
        let state = Merkle::of(b"test");
        let tag = Tag::new("v1.0.0", "main", 1, state);

        let filter_main = TagFilter::new().view("main");
        let filter_other = TagFilter::new().view("other");

        assert!(filter_main.matches(&tag));
        assert!(!filter_other.matches(&tag));
    }

    #[test]
    fn test_tag_filter_matches_pattern() {
        let state = Merkle::of(b"test");
        let tag = Tag::new("v1.0.0", "main", 1, state);

        assert!(TagFilter::new().pattern("v*").matches(&tag));
        assert!(TagFilter::new().pattern("*0.0").matches(&tag));
        assert!(TagFilter::new().pattern("*1.0*").matches(&tag));
        assert!(!TagFilter::new().pattern("release*").matches(&tag));
    }

    #[test]
    fn test_tag_filter_matches_annotated() {
        let state = Merkle::of(b"test");
        let lightweight = Tag::new("v1", "main", 1, state);
        let annotated = Tag::new("v2", "main", 2, state).with_message("Test");

        let filter_annotated = TagFilter::new().annotated_only();
        let filter_lightweight = TagFilter::new().lightweight_only();

        assert!(!filter_annotated.matches(&lightweight));
        assert!(filter_annotated.matches(&annotated));
        assert!(filter_lightweight.matches(&lightweight));
        assert!(!filter_lightweight.matches(&annotated));
    }

    // Tag Name Validation Tests

    #[test]
    fn test_validate_tag_name_valid() {
        assert!(validate_tag_name("v1.0.0").is_ok());
        assert!(validate_tag_name("release-2023-01").is_ok());
        assert!(validate_tag_name("my_tag").is_ok());
        assert!(validate_tag_name("123").is_ok());
    }

    #[test]
    fn test_validate_tag_name_empty() {
        let result = validate_tag_name("");
        assert!(matches!(result, Err(TagError::InvalidName { .. })));
    }

    #[test]
    fn test_validate_tag_name_starts_with_dot() {
        let result = validate_tag_name(".hidden");
        assert!(matches!(result, Err(TagError::InvalidName { .. })));
    }

    #[test]
    fn test_validate_tag_name_path_separator() {
        assert!(matches!(
            validate_tag_name("path/to/tag"),
            Err(TagError::InvalidName { .. })
        ));
        assert!(matches!(
            validate_tag_name("path\\to\\tag"),
            Err(TagError::InvalidName { .. })
        ));
    }

    #[test]
    fn test_validate_tag_name_reserved() {
        assert!(matches!(
            validate_tag_name("HEAD"),
            Err(TagError::InvalidName { .. })
        ));
        assert!(matches!(
            validate_tag_name("head"),
            Err(TagError::InvalidName { .. })
        ));
    }

    #[test]
    fn test_validate_tag_name_too_long() {
        let long_name = "a".repeat(300);
        assert!(matches!(
            validate_tag_name(&long_name),
            Err(TagError::InvalidName { .. })
        ));
    }

    // Pattern Matching Tests

    #[test]
    fn test_matches_pattern_exact() {
        assert!(matches_pattern("v1.0.0", "v1.0.0"));
        assert!(!matches_pattern("v1.0.0", "v1.0.1"));
    }

    #[test]
    fn test_matches_pattern_wildcard_all() {
        assert!(matches_pattern("anything", "*"));
        assert!(matches_pattern("", "*"));
    }

    #[test]
    fn test_matches_pattern_prefix() {
        assert!(matches_pattern("v1.0.0", "v*"));
        assert!(matches_pattern("v2.0.0", "v*"));
        assert!(!matches_pattern("release", "v*"));
    }

    #[test]
    fn test_matches_pattern_suffix() {
        assert!(matches_pattern("v1.0.0", "*0.0"));
        assert!(!matches_pattern("v1.0.1", "*0.0"));
    }

    #[test]
    fn test_matches_pattern_contains() {
        assert!(matches_pattern("v1.0.0-beta", "*0.0*"));
        assert!(matches_pattern("pre-1.0.0-post", "*0.0*"));
    }

    // Tag File Operations Tests

    #[test]
    fn test_tag_file_path() {
        let tags_dir = Path::new("/repo/.atomic/tags");
        let path = tag_file_path(tags_dir, "main", "v1.0.0");

        assert_eq!(path, PathBuf::from("/repo/.atomic/tags/main/v1.0.0.tag"));
    }

    #[test]
    fn test_view_tags_dir() {
        let tags_dir = Path::new("/repo/.atomic/tags");
        let path = view_tags_dir(tags_dir, "feature");

        assert_eq!(path, PathBuf::from("/repo/.atomic/tags/feature"));
    }

    #[test]
    fn test_save_and_load_tag() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test state");
        let tag = Tag::new("v1.0.0", "main", 42, state);

        // Save
        save_tag(tags_dir, &tag).unwrap();

        // Load
        let loaded = load_tag(tags_dir, "main", "v1.0.0").unwrap().unwrap();

        assert_eq!(loaded.name, "v1.0.0");
        assert_eq!(loaded.sequence, 42);
        assert_eq!(loaded.state, state);
    }

    #[test]
    fn test_save_tag_already_exists() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        let tag = Tag::new("v1.0.0", "main", 1, state);

        save_tag(tags_dir, &tag).unwrap();
        let result = save_tag(tags_dir, &tag);

        assert!(matches!(result, Err(TagError::AlreadyExists { .. })));
    }

    #[test]
    fn test_save_tag_force() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state1 = Merkle::of(b"state1");
        let state2 = Merkle::of(b"state2");
        let tag1 = Tag::new("v1.0.0", "main", 1, state1);
        let tag2 = Tag::new("v1.0.0", "main", 2, state2);

        save_tag(tags_dir, &tag1).unwrap();
        save_tag_force(tags_dir, &tag2, true).unwrap();

        let loaded = load_tag(tags_dir, "main", "v1.0.0").unwrap().unwrap();
        assert_eq!(loaded.sequence, 2);
    }

    #[test]
    fn test_load_tag_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let result = load_tag(tags_dir, "main", "nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_load_tag_any_view() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        save_tag(tags_dir, &Tag::new("v1.0.0", "main", 1, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v2.0.0", "feature", 2, state)).unwrap();

        // Find tag in main view
        let tag = load_tag_any_view(tags_dir, "v1.0.0").unwrap().unwrap();
        assert_eq!(tag.view, "main");

        // Find tag in feature view
        let tag = load_tag_any_view(tags_dir, "v2.0.0").unwrap().unwrap();
        assert_eq!(tag.view, "feature");

        // Not found in any view
        let result = load_tag_any_view(tags_dir, "nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_delete_tag() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        let tag = Tag::new("v1.0.0", "main", 1, state);

        save_tag(tags_dir, &tag).unwrap();
        assert!(delete_tag(tags_dir, "main", "v1.0.0").unwrap());
        assert!(load_tag(tags_dir, "main", "v1.0.0").unwrap().is_none());
    }

    #[test]
    fn test_delete_tag_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        assert!(!delete_tag(tags_dir, "main", "nonexistent").unwrap());
    }

    #[test]
    fn test_delete_tag_cleans_empty_view_dir() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        save_tag(tags_dir, &Tag::new("v1.0.0", "main", 1, state)).unwrap();

        // View directory should exist
        assert!(view_tags_dir(tags_dir, "main").exists());

        // Delete the only tag
        delete_tag(tags_dir, "main", "v1.0.0").unwrap();

        // View directory should be cleaned up
        assert!(!view_tags_dir(tags_dir, "main").exists());
    }

    #[test]
    fn test_list_tags() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        save_tag(tags_dir, &Tag::new("v1.0.0", "main", 1, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v2.0.0", "main", 2, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v3.0.0", "main", 3, state)).unwrap();

        let tags = list_tags(tags_dir, "main").unwrap();
        assert_eq!(tags.len(), 3);
    }

    #[test]
    fn test_list_tags_empty_dir() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let tags = list_tags(tags_dir, "main").unwrap();
        assert!(tags.is_empty());
    }

    #[test]
    fn test_list_tags_nonexistent_dir() {
        let tags_dir = Path::new("/nonexistent/path");

        let tags = list_tags(tags_dir, "main").unwrap();
        assert!(tags.is_empty());
    }

    #[test]
    fn test_list_all_tags() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        save_tag(tags_dir, &Tag::new("v1.0.0", "main", 1, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v2.0.0", "main", 2, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v1.0.0", "feature", 1, state)).unwrap();

        // list_tags only returns tags from one view
        let main_tags = list_tags(tags_dir, "main").unwrap();
        assert_eq!(main_tags.len(), 2);

        let feature_tags = list_tags(tags_dir, "feature").unwrap();
        assert_eq!(feature_tags.len(), 1);

        // list_all_tags returns tags from all views
        let all_tags = list_all_tags(tags_dir).unwrap();
        assert_eq!(all_tags.len(), 3);
    }

    #[test]
    fn test_list_tag_views() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        save_tag(tags_dir, &Tag::new("v1.0.0", "main", 1, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v1.0.0", "feature", 1, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v1.0.0", "dev", 1, state)).unwrap();

        let views = list_tag_views(tags_dir).unwrap();
        assert_eq!(views.len(), 3);
        assert!(views.contains(&"main".to_string()));
        assert!(views.contains(&"feature".to_string()));
        assert!(views.contains(&"dev".to_string()));
    }

    #[test]
    fn test_same_tag_name_different_views() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state1 = Merkle::of(b"state1");
        let state2 = Merkle::of(b"state2");

        // Same tag name in different views
        save_tag(tags_dir, &Tag::new("release", "main", 10, state1)).unwrap();
        save_tag(tags_dir, &Tag::new("release", "feature", 5, state2)).unwrap();

        // Load from each view
        let main_tag = load_tag(tags_dir, "main", "release").unwrap().unwrap();
        let feature_tag = load_tag(tags_dir, "feature", "release").unwrap().unwrap();

        assert_eq!(main_tag.sequence, 10);
        assert_eq!(main_tag.state, state1);
        assert_eq!(feature_tag.sequence, 5);
        assert_eq!(feature_tag.state, state2);
    }

    #[test]
    fn test_list_tags_filtered() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        save_tag(tags_dir, &Tag::new("v1.0.0", "main", 1, state)).unwrap();
        save_tag(
            tags_dir,
            &Tag::new("v2.0.0", "main", 2, state).with_message("Annotated"),
        )
        .unwrap();
        save_tag(tags_dir, &Tag::new("release-1", "other", 3, state)).unwrap();

        // Filter by pattern
        let filter = TagFilter::new().pattern("v*");
        let tags = list_tags_filtered(tags_dir, &filter).unwrap();
        assert_eq!(tags.len(), 2);

        // Filter by view
        let filter = TagFilter::new().view("main");
        let tags = list_tags_filtered(tags_dir, &filter).unwrap();
        assert_eq!(tags.len(), 2);

        // Filter annotated only
        let filter = TagFilter::new().annotated_only();
        let tags = list_tags_filtered(tags_dir, &filter).unwrap();
        assert_eq!(tags.len(), 1);
    }

    #[test]
    fn test_list_tags_sorted() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        save_tag(tags_dir, &Tag::new("b-tag", "main", 2, state)).unwrap();
        save_tag(tags_dir, &Tag::new("a-tag", "main", 1, state)).unwrap();
        save_tag(tags_dir, &Tag::new("c-tag", "main", 3, state)).unwrap();

        // Sort by name
        let filter = TagFilter::new().sort(TagSort::Name);
        let tags = list_tags_filtered(tags_dir, &filter).unwrap();
        assert_eq!(tags[0].name, "a-tag");
        assert_eq!(tags[1].name, "b-tag");
        assert_eq!(tags[2].name, "c-tag");

        // Sort by sequence
        let filter = TagFilter::new().sort(TagSort::Sequence);
        let tags = list_tags_filtered(tags_dir, &filter).unwrap();
        assert_eq!(tags[0].sequence, 3);
        assert_eq!(tags[1].sequence, 2);
        assert_eq!(tags[2].sequence, 1);
    }

    #[test]
    fn test_list_tags_with_limit() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        for i in 0..10 {
            save_tag(tags_dir, &Tag::new(format!("v{}", i), "main", i, state)).unwrap();
        }

        let filter = TagFilter::new().limit(5);
        let tags = list_tags_filtered(tags_dir, &filter).unwrap();
        assert_eq!(tags.len(), 5);
    }

    #[test]
    fn test_count_tags() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        assert_eq!(count_tags(tags_dir, "main").unwrap(), 0);

        let state = Merkle::of(b"test");
        save_tag(tags_dir, &Tag::new("v1", "main", 1, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v2", "main", 2, state)).unwrap();

        assert_eq!(count_tags(tags_dir, "main").unwrap(), 2);
    }

    #[test]
    fn test_count_all_tags() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        assert_eq!(count_all_tags(tags_dir).unwrap(), 0);

        let state = Merkle::of(b"test");
        save_tag(tags_dir, &Tag::new("v1", "main", 1, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v2", "main", 2, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v1", "feature", 1, state)).unwrap();

        assert_eq!(count_tags(tags_dir, "main").unwrap(), 2);
        assert_eq!(count_tags(tags_dir, "feature").unwrap(), 1);
        assert_eq!(count_all_tags(tags_dir).unwrap(), 3);
    }

    // TagError Tests

    #[test]
    fn test_tag_error_display() {
        let err = TagError::AlreadyExists {
            name: "v1.0.0".to_string(),
        };
        assert!(format!("{}", err).contains("v1.0.0"));

        let err = TagError::NotFound {
            name: "missing".to_string(),
        };
        assert!(format!("{}", err).contains("missing"));

        let err = TagError::InvalidName {
            name: "bad/name".to_string(),
            reason: "contains slash".to_string(),
        };
        assert!(format!("{}", err).contains("bad/name"));
    }

    // TagSort Tests

    #[test]
    fn test_tag_sort_default() {
        let sort = TagSort::default();
        assert_eq!(sort, TagSort::Name);
    }

    #[test]
    fn test_tag_sort_equality() {
        assert_eq!(TagSort::Name, TagSort::Name);
        assert_ne!(TagSort::Name, TagSort::Timestamp);
    }
}
