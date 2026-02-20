use super::*;

// TESTS

#[cfg(test)]
mod tests {
    use super::*;

    // GlobalizeOptions Tests

    #[test]
    fn test_options_new_returns_defaults() {
        let opts = GlobalizeOptions::new();
        assert!(!opts.include_empty_files());
        assert!(opts.validate_positions());
        assert_eq!(opts.max_hunk_size(), 0);
        assert_eq!(opts.default_encoding(), Encoding::Utf8);
    }

    #[test]
    fn test_options_default() {
        let opts = GlobalizeOptions::default();
        assert!(!opts.include_empty_files());
        assert!(opts.validate_positions());
    }

    #[test]
    fn test_options_include_empty_files() {
        let opts = GlobalizeOptions::new().with_include_empty_files(true);
        assert!(opts.include_empty_files());
    }

    #[test]
    fn test_options_validate_positions() {
        let opts = GlobalizeOptions::new().with_validate_positions(false);
        assert!(!opts.validate_positions());
    }

    #[test]
    fn test_options_max_hunk_size() {
        let opts = GlobalizeOptions::new().with_max_hunk_size(1024);
        assert_eq!(opts.max_hunk_size(), 1024);
    }

    #[test]
    fn test_options_default_encoding() {
        let opts = GlobalizeOptions::new().with_default_encoding(Encoding::Binary);
        assert_eq!(opts.default_encoding(), Encoding::Binary);
    }

    #[test]
    fn test_options_builder_chain() {
        let opts = GlobalizeOptions::new()
            .with_include_empty_files(true)
            .with_validate_positions(false)
            .with_max_hunk_size(2048)
            .with_default_encoding(Encoding::Latin1);

        assert!(opts.include_empty_files());
        assert!(!opts.validate_positions());
        assert_eq!(opts.max_hunk_size(), 2048);
        assert_eq!(opts.default_encoding(), Encoding::Latin1);
    }

    #[test]
    fn test_options_clone() {
        let opts1 = GlobalizeOptions::new().with_include_empty_files(true);
        let opts2 = opts1.clone();
        assert!(opts2.include_empty_files());
    }

    #[test]
    fn test_options_debug() {
        let opts = GlobalizeOptions::new();
        let debug = format!("{:?}", opts);
        assert!(debug.contains("GlobalizeOptions"));
    }

    // GlobalizeError Tests

    #[test]
    fn test_error_path_not_found() {
        let err = GlobalizeError::PathNotFound {
            path: "test.rs".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("test.rs"));
        assert!(msg.contains("not found"));
    }

    #[test]
    fn test_error_inode_not_found() {
        let err = GlobalizeError::InodeNotFound {
            inode: Inode::new(42),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("42"));
    }

    #[test]
    fn test_error_parent_not_found() {
        let err = GlobalizeError::ParentNotFound {
            path: "src/test.rs".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("src/test.rs"));
    }

    #[test]
    fn test_error_missing_context() {
        let err = GlobalizeError::MissingContext {
            path: "test.rs".to_string(),
            line: 42,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("test.rs"));
        assert!(msg.contains("42"));
    }

    #[test]
    fn test_error_missing_field() {
        let err = GlobalizeError::MissingField {
            path: "test.rs".to_string(),
            field: "inode",
        };
        let msg = format!("{}", err);
        assert!(msg.contains("test.rs"));
        assert!(msg.contains("inode"));
    }

    #[test]
    fn test_error_invalid_line() {
        let err = GlobalizeError::InvalidLine {
            path: "test.rs".to_string(),
            line: 100,
            max_line: 50,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("100"));
        assert!(msg.contains("50"));
    }

    // CacheStats Tests

    #[test]
    fn test_cache_stats_display() {
        let stats = CacheStats {
            inode_cache_size: 10,
            position_cache_size: 20,
        };
        let display = format!("{}", stats);
        assert!(display.contains("10"));
        assert!(display.contains("20"));
    }

    #[test]
    fn test_cache_stats_equality() {
        let stats1 = CacheStats {
            inode_cache_size: 5,
            position_cache_size: 10,
        };
        let stats2 = CacheStats {
            inode_cache_size: 5,
            position_cache_size: 10,
        };
        let stats3 = CacheStats {
            inode_cache_size: 5,
            position_cache_size: 15,
        };
        assert_eq!(stats1, stats2);
        assert_ne!(stats1, stats3);
    }

    // Helper Function Tests

    #[test]
    fn test_extract_filename_with_path() {
        assert_eq!(extract_filename("src/lib/mod.rs"), "mod.rs");
    }

    #[test]
    fn test_extract_filename_root_level() {
        assert_eq!(extract_filename("Cargo.toml"), "Cargo.toml");
    }

    #[test]
    fn test_extract_filename_deep_path() {
        assert_eq!(extract_filename("a/b/c/d/e.txt"), "e.txt");
    }

    #[test]
    fn test_extract_filename_empty() {
        assert_eq!(extract_filename(""), "");
    }

    #[test]
    fn test_extract_parent_with_path() {
        assert_eq!(extract_parent("src/lib/mod.rs"), "src/lib");
    }

    #[test]
    fn test_extract_parent_root_level() {
        assert_eq!(extract_parent("Cargo.toml"), "");
    }

    #[test]
    fn test_extract_parent_deep_path() {
        assert_eq!(extract_parent("a/b/c/d/e.txt"), "a/b/c/d");
    }

    #[test]
    fn test_extract_parent_empty() {
        assert_eq!(extract_parent(""), "");
    }

    // Position Conversion Tests

    #[test]
    fn test_position_to_option_hash_root() {
        let pos = Position::new(NodeId::ROOT, ChangePosition::new(0));
        let converted = position_to_option_hash(pos);
        // ROOT positions use Some(Hash::NONE) to indicate the virtual root span
        assert!(converted.change.is_some());
        assert_eq!(converted.change.unwrap(), Hash::NONE);
        assert_eq!(converted.pos, ChangePosition::new(0));
    }

    #[test]
    fn test_position_to_option_hash_non_root() {
        let pos = Position::new(NodeId::new(42), ChangePosition::new(100));
        let converted = position_to_option_hash(pos);
        // Currently returns None for self-reference
        assert!(converted.change.is_none());
        assert_eq!(converted.pos, ChangePosition::new(100));
    }

    #[test]
    fn test_vertex_to_option_hash() {
        let node = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        let converted = vertex_to_option_hash(node);
        assert!(converted.change.is_none());
        assert_eq!(converted.start, ChangePosition::new(0));
        assert_eq!(converted.end, ChangePosition::new(10));
    }

    #[test]
    fn test_node_id_to_option_hash_root() {
        let result = node_id_to_option_hash(NodeId::ROOT);
        // ROOT node uses Some(Hash::NONE) to indicate the virtual root
        assert!(result.is_some());
        assert_eq!(result.unwrap(), Hash::NONE);
    }

    #[test]
    fn test_node_id_to_option_hash_non_root() {
        let result = node_id_to_option_hash(NodeId::new(42));
        // Currently returns None for self-reference
        assert!(result.is_none());
    }

    // GlobalizedFile Tests

    #[test]
    fn test_globalized_file_new() {
        let gf = GlobalizedFile::new("test.rs");
        assert_eq!(gf.path(), "test.rs");
        assert!(gf.is_empty());
        assert_eq!(gf.hunk_count(), 0);
    }

    #[test]
    fn test_globalized_file_set_bytes() {
        let mut gf = GlobalizedFile::new("test.rs");
        gf.set_bytes_added(100);
        assert_eq!(gf.bytes_added(), 100);
    }

    #[test]
    fn test_globalized_file_set_deps() {
        let mut gf = GlobalizedFile::new("test.rs");
        gf.set_dependency_count(5);
        assert_eq!(gf.dependency_count(), 5);
    }

    #[test]
    fn test_globalized_file_into_hunks() {
        let gf = GlobalizedFile::new("test.rs");
        let hunks = gf.into_hunks();
        assert!(hunks.is_empty());
    }
}
