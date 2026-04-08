//! Tests for the repository output module.

#[cfg(test)]
mod tests {
    use crate::output::repo::conflict::{FileConflict, FileConflictType};
    use crate::output::repo::file::FileOutputResult;
    use crate::output::repo::repository::types::{
        MaterializeError, MaterializeOptions, OutputItem,
    };
    use crate::output::repo::repository::MaterializeResult;
    use crate::types::{Inode, Position};
    use std::time::SystemTime;

    // ========================================================================
    // MaterializeOptions Tests
    // ========================================================================

    #[test]
    fn test_options_new() {
        let opts = MaterializeOptions::new();

        assert!(opts.prefix.is_empty());
        assert!(opts.if_modified_since.is_none());
        assert!(opts.output_name_conflicts);
        assert!(!opts.include_deleted);
        assert!(opts.max_vertices_per_file.is_none());
        assert_eq!(opts.salt, 0);
        assert!(!opts.parallel);
        assert_eq!(opts.num_workers, 1);
    }

    #[test]
    fn test_options_default() {
        let opts = MaterializeOptions::default();

        assert!(opts.prefix.is_empty());
        assert!(!opts.parallel);
    }

    #[test]
    fn test_options_prefix() {
        let opts = MaterializeOptions::new().prefix("src/");

        assert_eq!(opts.prefix, "src/");
    }

    #[test]
    fn test_options_prefix_empty() {
        let opts = MaterializeOptions::new().prefix("");

        assert!(opts.prefix.is_empty());
    }

    #[test]
    fn test_options_if_modified_since() {
        let time = SystemTime::now();
        let opts = MaterializeOptions::new().if_modified_since(time);

        assert!(opts.if_modified_since.is_some());
    }

    #[test]
    fn test_options_output_name_conflicts() {
        let opts = MaterializeOptions::new().output_name_conflicts(false);

        assert!(!opts.output_name_conflicts);
    }

    #[test]
    fn test_options_include_deleted() {
        let opts = MaterializeOptions::new().include_deleted(true);

        assert!(opts.include_deleted);
    }

    #[test]
    fn test_options_max_vertices_per_file() {
        let opts = MaterializeOptions::new().max_vertices_per_file(5000);

        assert_eq!(opts.max_vertices_per_file, Some(5000));
    }

    #[test]
    fn test_options_salt() {
        let opts = MaterializeOptions::new().salt(42);

        assert_eq!(opts.salt, 42);
    }

    #[test]
    fn test_options_parallel() {
        let opts = MaterializeOptions::new().parallel(true);

        assert!(opts.parallel);
    }

    #[test]
    fn test_options_num_workers() {
        let opts = MaterializeOptions::new().num_workers(8);

        assert_eq!(opts.num_workers, 8);
    }

    #[test]
    fn test_options_chaining() {
        let opts = MaterializeOptions::new()
            .prefix("src/")
            .include_deleted(true)
            .output_name_conflicts(false)
            .salt(100)
            .parallel(true)
            .num_workers(4);

        assert_eq!(opts.prefix, "src/");
        assert!(opts.include_deleted);
        assert!(!opts.output_name_conflicts);
        assert_eq!(opts.salt, 100);
        assert!(opts.parallel);
        assert_eq!(opts.num_workers, 4);
    }

    #[test]
    fn test_options_matches_prefix_empty() {
        let opts = MaterializeOptions::new();

        assert!(opts.matches_prefix("anything"));
        assert!(opts.matches_prefix("src/main.rs"));
        assert!(opts.matches_prefix(""));
    }

    #[test]
    fn test_options_matches_prefix_with_prefix() {
        let opts = MaterializeOptions::new().prefix("src/");

        assert!(opts.matches_prefix("src/main.rs"));
        assert!(opts.matches_prefix("src/lib/mod.rs"));
        assert!(!opts.matches_prefix("tests/test.rs"));
        assert!(!opts.matches_prefix("Cargo.toml"));
    }

    #[test]
    fn test_options_to_file_options_default() {
        let opts = MaterializeOptions::new();
        let file_opts = opts.to_file_options();

        assert!(!file_opts.include_deleted);
        assert!(file_opts.max_vertices.is_none());
    }

    #[test]
    fn test_options_to_file_options_with_deleted() {
        let opts = MaterializeOptions::new().include_deleted(true);
        let file_opts = opts.to_file_options();

        assert!(file_opts.include_deleted);
    }

    #[test]
    fn test_options_to_file_options_with_max() {
        let opts = MaterializeOptions::new().max_vertices_per_file(1000);
        let file_opts = opts.to_file_options();

        assert_eq!(file_opts.max_vertices, Some(1000));
    }

    #[test]
    fn test_options_clone() {
        let opts = MaterializeOptions::new().prefix("test/");
        let cloned = opts.clone();

        assert_eq!(opts.prefix, cloned.prefix);
    }

    #[test]
    fn test_options_debug() {
        let opts = MaterializeOptions::new();
        let debug = format!("{:?}", opts);

        assert!(debug.contains("MaterializeOptions"));
    }

    // ========================================================================
    // MaterializeResult Tests
    // ========================================================================

    #[test]
    fn test_result_new() {
        let result = MaterializeResult::new();

        assert_eq!(result.files_written, 0);
        assert_eq!(result.files_skipped, 0);
        assert_eq!(result.directories_created, 0);
        assert_eq!(result.bytes_written, 0);
        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn test_result_default() {
        let result = MaterializeResult::default();

        assert_eq!(result.files_written, 0);
    }

    #[test]
    fn test_result_has_conflicts_empty() {
        let result = MaterializeResult::new();

        assert!(!result.has_conflicts());
    }

    #[test]
    fn test_result_has_conflicts_with_conflict() {
        let mut result = MaterializeResult::new();
        result.add_conflict(FileConflict::new(
            "test.rs".to_string(),
            FileConflictType::Order,
        ));

        assert!(result.has_conflicts());
    }

    #[test]
    fn test_result_conflict_count() {
        let mut result = MaterializeResult::new();

        assert_eq!(result.conflict_count(), 0);

        result.add_conflict(FileConflict::new(
            "a.rs".to_string(),
            FileConflictType::Order,
        ));
        result.add_conflict(FileConflict::new(
            "b.rs".to_string(),
            FileConflictType::Name,
        ));

        assert_eq!(result.conflict_count(), 2);
    }

    #[test]
    fn test_result_add_conflict() {
        let mut result = MaterializeResult::new();
        let conflict = FileConflict::new("test.rs".to_string(), FileConflictType::Cyclic);

        result.add_conflict(conflict);

        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].conflict_type, FileConflictType::Cyclic);
    }

    #[test]
    fn test_result_conflicts_of_type() {
        let mut result = MaterializeResult::new();
        result.add_conflict(FileConflict::new(
            "a.rs".to_string(),
            FileConflictType::Order,
        ));
        result.add_conflict(FileConflict::new(
            "b.rs".to_string(),
            FileConflictType::Name,
        ));
        result.add_conflict(FileConflict::new(
            "c.rs".to_string(),
            FileConflictType::Order,
        ));

        let order_conflicts: Vec<_> = result.conflicts_of_type(FileConflictType::Order).collect();
        assert_eq!(order_conflicts.len(), 2);

        let name_conflicts: Vec<_> = result.conflicts_of_type(FileConflictType::Name).collect();
        assert_eq!(name_conflicts.len(), 1);
    }

    #[test]
    fn test_result_name_conflicts() {
        let mut result = MaterializeResult::new();
        result.add_conflict(FileConflict::new(
            "a.rs".to_string(),
            FileConflictType::Name,
        ));
        result.add_conflict(FileConflict::new(
            "b.rs".to_string(),
            FileConflictType::Order,
        ));

        let name_conflicts: Vec<_> = result.name_conflicts().collect();
        assert_eq!(name_conflicts.len(), 1);
    }

    #[test]
    fn test_result_content_conflicts() {
        let mut result = MaterializeResult::new();
        result.add_conflict(FileConflict::new(
            "a.rs".to_string(),
            FileConflictType::Order,
        ));
        result.add_conflict(FileConflict::new(
            "b.rs".to_string(),
            FileConflictType::Cyclic,
        ));
        result.add_conflict(FileConflict::new(
            "c.rs".to_string(),
            FileConflictType::Zombie,
        ));
        result.add_conflict(FileConflict::new(
            "d.rs".to_string(),
            FileConflictType::Name,
        ));

        let content_conflicts: Vec<_> = result.content_conflicts().collect();
        assert_eq!(content_conflicts.len(), 3);
    }

    #[test]
    fn test_result_merge_file_result() {
        let mut result = MaterializeResult::new();

        let file_result = FileOutputResult::empty("test.rs", Inode::ROOT)
            .with_bytes_written(1024)
            .with_vertices_processed(10)
            .with_edges_traversed(20);

        result.merge_file_result(file_result, false);

        assert_eq!(result.files_written, 1);
        assert_eq!(result.bytes_written, 1024);
        assert_eq!(result.vertices_processed, 10);
        assert_eq!(result.edges_traversed, 20);
    }

    #[test]
    fn test_result_merge_file_result_with_conflicts() {
        let mut result = MaterializeResult::new();

        let mut file_result = FileOutputResult::empty("test.rs", Inode::ROOT);
        file_result.add_conflict(FileConflict::new(
            "test.rs".to_string(),
            FileConflictType::Order,
        ));

        result.merge_file_result(file_result, false);

        assert_eq!(result.conflict_count(), 1);
    }

    #[test]
    fn test_result_merge_file_result_truncated() {
        let mut result = MaterializeResult::new();

        let file_result = FileOutputResult::empty("test.rs", Inode::ROOT).with_truncated(true);

        result.merge_file_result(file_result, false);

        assert_eq!(result.files_truncated, 1);
    }

    #[test]
    fn test_result_merge_file_result_store() {
        let mut result = MaterializeResult::new();

        let file_result = FileOutputResult::empty("test.rs", Inode::ROOT);

        result.merge_file_result(file_result, true);

        assert!(result.file_results.contains_key("test.rs"));
    }

    #[test]
    fn test_result_record_skipped() {
        let mut result = MaterializeResult::new();

        result.record_skipped();
        result.record_skipped();

        assert_eq!(result.files_skipped, 2);
    }

    #[test]
    fn test_result_record_directory() {
        let mut result = MaterializeResult::new();

        result.record_directory();

        assert_eq!(result.directories_created, 1);
    }

    #[test]
    fn test_result_to_outcome() {
        let mut result = MaterializeResult::new();
        result.files_written = 5;
        result.directories_created = 2;
        result.files_skipped = 1;
        result.bytes_written = 10000;

        let outcome = result.to_outcome();

        assert_eq!(outcome.files_written(), 5);
        assert_eq!(outcome.directories_created(), 2);
        assert_eq!(outcome.files_skipped(), 1);
        assert_eq!(outcome.bytes_written, 10000);
    }

    #[test]
    fn test_result_clone() {
        let mut result = MaterializeResult::new();
        result.files_written = 3;

        let cloned = result.clone();

        assert_eq!(result.files_written, cloned.files_written);
    }

    #[test]
    fn test_result_debug() {
        let result = MaterializeResult::new();
        let debug = format!("{:?}", result);

        assert!(debug.contains("MaterializeResult"));
    }

    // ========================================================================
    // MaterializeError Tests
    // ========================================================================

    #[test]
    fn test_error_display_pristine() {
        let err: MaterializeError<std::io::Error> =
            MaterializeError::Pristine(crate::pristine::PristineError::ViewNotFound {
                name: "test".to_string(),
            });
        let display = format!("{}", err);

        assert!(display.contains("Pristine error"));
    }

    #[test]
    fn test_error_display_change_store() {
        let err: MaterializeError<std::io::Error> =
            MaterializeError::ChangeStore("not found".to_string());
        let display = format!("{}", err);

        assert!(display.contains("Change store error"));
    }

    #[test]
    fn test_error_display_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: MaterializeError<std::io::Error> = MaterializeError::Io(io_err);
        let display = format!("{}", err);

        assert!(display.contains("I/O error"));
    }

    #[test]
    fn test_error_display_working_copy() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err: MaterializeError<std::io::Error> = MaterializeError::WorkingCopy(io_err);
        let display = format!("{}", err);

        assert!(display.contains("Working copy error"));
    }

    #[test]
    fn test_error_display_tree() {
        let err: MaterializeError<std::io::Error> =
            MaterializeError::TreeError("invalid tree".to_string());
        let display = format!("{}", err);

        assert!(display.contains("Tree traversal error"));
    }

    #[test]
    fn test_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "test");
        let err: MaterializeError<std::io::Error> = io_err.into();

        match err {
            MaterializeError::Io(_) => (),
            _ => panic!("Expected Io variant"),
        }
    }

    #[test]
    fn test_error_from_pristine() {
        let pristine_err = crate::pristine::PristineError::ViewNotFound {
            name: "test".to_string(),
        };
        let err: MaterializeError<std::io::Error> = pristine_err.into();

        match err {
            MaterializeError::Pristine(_) => (),
            _ => panic!("Expected Pristine variant"),
        }
    }

    #[test]
    fn test_error_debug() {
        let err: MaterializeError<std::io::Error> = MaterializeError::TreeError("test".to_string());
        let debug = format!("{:?}", err);

        assert!(debug.contains("TreeError"));
    }

    #[test]
    fn test_error_source_pristine() {
        use std::error::Error;

        let err: MaterializeError<std::io::Error> =
            MaterializeError::Pristine(crate::pristine::PristineError::ViewNotFound {
                name: "test".to_string(),
            });

        assert!(err.source().is_some());
    }

    #[test]
    fn test_error_source_io() {
        use std::error::Error;

        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "test");
        let err: MaterializeError<std::io::Error> = MaterializeError::Io(io_err);

        assert!(err.source().is_some());
    }

    #[test]
    fn test_error_source_change_store() {
        use std::error::Error;

        let err: MaterializeError<std::io::Error> =
            MaterializeError::ChangeStore("test".to_string());

        assert!(err.source().is_none());
    }

    // ========================================================================
    // OutputItem Tests
    // ========================================================================

    #[test]
    fn test_output_item_file() {
        let item = OutputItem::file("src/main.rs", Inode::ROOT, Position::ROOT);

        assert_eq!(item.path, "src/main.rs");
        assert_eq!(item.inode, Inode::ROOT);
        assert!(!item.is_directory);
    }

    #[test]
    fn test_output_item_directory() {
        let item = OutputItem::directory("src/lib", Inode::ROOT);

        assert_eq!(item.path, "src/lib");
        assert!(item.is_directory);
    }

    #[test]
    fn test_output_item_with_metadata() {
        let item = OutputItem::file("test.rs", Inode::ROOT, Position::ROOT)
            .with_metadata(crate::output::traits::FileMetadata::executable());

        assert!(item.metadata.is_executable());
    }

    #[test]
    fn test_output_item_clone() {
        let item = OutputItem::file("test.rs", Inode::ROOT, Position::ROOT);
        let cloned = item.clone();

        assert_eq!(item.path, cloned.path);
    }

    #[test]
    fn test_output_item_debug() {
        let item = OutputItem::file("test.rs", Inode::ROOT, Position::ROOT);
        let debug = format!("{:?}", item);

        assert!(debug.contains("OutputItem"));
        assert!(debug.contains("test.rs"));
    }
}
