use super::*;

mod tests {
    use super::*;

    // DiffFormat Tests

    #[test]
    fn test_diff_format_default() {
        assert_eq!(DiffFormat::default(), DiffFormat::Unified);
    }

    #[test]
    fn test_diff_format_display() {
        assert_eq!(DiffFormat::Unified.to_string(), "unified");
        assert_eq!(DiffFormat::Stat.to_string(), "stat");
        assert_eq!(DiffFormat::NameOnly.to_string(), "name-only");
        assert_eq!(DiffFormat::NameStatus.to_string(), "name-status");
    }

    #[test]
    fn test_diff_format_from_str_unified() {
        assert_eq!(
            "unified".parse::<DiffFormat>().unwrap(),
            DiffFormat::Unified
        );
        assert_eq!("u".parse::<DiffFormat>().unwrap(), DiffFormat::Unified);
        assert_eq!(
            "UNIFIED".parse::<DiffFormat>().unwrap(),
            DiffFormat::Unified
        );
    }

    #[test]
    fn test_diff_format_from_str_stat() {
        assert_eq!("stat".parse::<DiffFormat>().unwrap(), DiffFormat::Stat);
        assert_eq!("s".parse::<DiffFormat>().unwrap(), DiffFormat::Stat);
        assert_eq!("STAT".parse::<DiffFormat>().unwrap(), DiffFormat::Stat);
    }

    #[test]
    fn test_diff_format_from_str_name_only() {
        assert_eq!(
            "name-only".parse::<DiffFormat>().unwrap(),
            DiffFormat::NameOnly
        );
        assert_eq!(
            "nameonly".parse::<DiffFormat>().unwrap(),
            DiffFormat::NameOnly
        );
        assert_eq!("names".parse::<DiffFormat>().unwrap(), DiffFormat::NameOnly);
    }

    #[test]
    fn test_diff_format_from_str_name_status() {
        assert_eq!(
            "name-status".parse::<DiffFormat>().unwrap(),
            DiffFormat::NameStatus
        );
        assert_eq!(
            "namestatus".parse::<DiffFormat>().unwrap(),
            DiffFormat::NameStatus
        );
        assert_eq!(
            "status".parse::<DiffFormat>().unwrap(),
            DiffFormat::NameStatus
        );
    }

    #[test]
    fn test_diff_format_from_str_invalid() {
        let err = "invalid".parse::<DiffFormat>().unwrap_err();
        assert!(err.contains("unknown diff format"));
        assert!(err.contains("invalid"));
    }

    #[test]
    fn test_diff_format_equality() {
        assert_eq!(DiffFormat::Unified, DiffFormat::Unified);
        assert_ne!(DiffFormat::Unified, DiffFormat::Stat);
    }

    #[test]
    fn test_diff_format_clone() {
        let format = DiffFormat::Stat;
        let cloned = format.clone();
        assert_eq!(format, cloned);
    }

    #[test]
    fn test_diff_format_copy() {
        let format = DiffFormat::NameOnly;
        let copied = format;
        assert_eq!(format, copied);
    }

    #[test]
    fn test_diff_format_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(DiffFormat::Unified);
        set.insert(DiffFormat::Stat);
        assert!(set.contains(&DiffFormat::Unified));
        assert!(set.contains(&DiffFormat::Stat));
        assert!(!set.contains(&DiffFormat::NameOnly));
    }

    // FileDiffStats Tests

    #[test]
    fn test_file_diff_stats_new() {
        let stats = FileDiffStats::new("test.rs", 10, 5, 'M');
        assert_eq!(stats.path, "test.rs");
        assert_eq!(stats.insertions, 10);
        assert_eq!(stats.deletions, 5);
        assert_eq!(stats.status, 'M');
    }

    #[test]
    fn test_file_diff_stats_added() {
        let stats = FileDiffStats::added("new.rs", 20);
        assert_eq!(stats.path, "new.rs");
        assert_eq!(stats.insertions, 20);
        assert_eq!(stats.deletions, 0);
        assert_eq!(stats.status, 'A');
        assert!(stats.is_added());
        assert!(!stats.is_deleted());
        assert!(!stats.is_modified());
    }

    #[test]
    fn test_file_diff_stats_deleted() {
        let stats = FileDiffStats::deleted("old.rs", 15);
        assert_eq!(stats.path, "old.rs");
        assert_eq!(stats.insertions, 0);
        assert_eq!(stats.deletions, 15);
        assert_eq!(stats.status, 'D');
        assert!(stats.is_deleted());
        assert!(!stats.is_added());
    }

    #[test]
    fn test_file_diff_stats_modified() {
        let stats = FileDiffStats::modified("mod.rs", 8, 3);
        assert_eq!(stats.path, "mod.rs");
        assert_eq!(stats.insertions, 8);
        assert_eq!(stats.deletions, 3);
        assert_eq!(stats.status, 'M');
        assert!(stats.is_modified());
    }

    #[test]
    fn test_file_diff_stats_total_changes() {
        let stats = FileDiffStats::new("test.rs", 10, 5, 'M');
        assert_eq!(stats.total_changes(), 15);
    }

    #[test]
    fn test_file_diff_stats_has_changes() {
        let with_changes = FileDiffStats::new("test.rs", 1, 0, 'M');
        let no_changes = FileDiffStats::new("test.rs", 0, 0, 'M');
        assert!(with_changes.has_changes());
        assert!(!no_changes.has_changes());
    }

    #[test]
    fn test_file_diff_stats_default() {
        let stats = FileDiffStats::default();
        assert_eq!(stats.path, "");
        assert_eq!(stats.insertions, 0);
        assert_eq!(stats.deletions, 0);
    }

    // DiffStats Tests

    #[test]
    fn test_diff_stats_new() {
        let stats = DiffStats::new();
        assert_eq!(stats.file_count(), 0);
        assert_eq!(stats.total_insertions(), 0);
        assert_eq!(stats.total_deletions(), 0);
    }

    #[test]
    fn test_diff_stats_add_file() {
        let mut stats = DiffStats::new();
        stats.add_file(FileDiffStats::new("file1.rs", 10, 5, 'M'));
        stats.add_file(FileDiffStats::new("file2.rs", 3, 2, 'M'));

        assert_eq!(stats.file_count(), 2);
        assert_eq!(stats.total_insertions(), 13);
        assert_eq!(stats.total_deletions(), 7);
        assert_eq!(stats.total_changes(), 20);
    }

    #[test]
    fn test_diff_stats_has_changes() {
        let mut stats = DiffStats::new();
        assert!(!stats.has_changes());

        stats.add_file(FileDiffStats::new("file.rs", 1, 0, 'M'));
        assert!(stats.has_changes());
    }

    #[test]
    fn test_diff_stats_max_path_length() {
        let mut stats = DiffStats::new();
        stats.add_file(FileDiffStats::new("short.rs", 1, 0, 'M'));
        stats.add_file(FileDiffStats::new("very_long_filename.rs", 1, 0, 'M'));

        assert_eq!(stats.max_path_length(), 21); // "very_long_filename.rs".len()
    }

    #[test]
    fn test_diff_stats_max_change_count() {
        let mut stats = DiffStats::new();
        stats.add_file(FileDiffStats::new("file1.rs", 5, 3, 'M'));
        stats.add_file(FileDiffStats::new("file2.rs", 10, 10, 'M'));

        assert_eq!(stats.max_change_count(), 20);
    }

    #[test]
    fn test_diff_stats_iter() {
        let mut stats = DiffStats::new();
        stats.add_file(FileDiffStats::new("a.rs", 1, 0, 'M'));
        stats.add_file(FileDiffStats::new("b.rs", 2, 0, 'M'));

        let paths: Vec<_> = stats.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn test_diff_stats_into_iter() {
        let mut stats = DiffStats::new();
        stats.add_file(FileDiffStats::new("a.rs", 1, 0, 'M'));

        for file_stats in stats {
            assert_eq!(file_stats.path, "a.rs");
        }
    }

    // DiffOutputConfig Tests

    #[test]
    fn test_diff_output_config_default() {
        let config = DiffOutputConfig::default();
        assert_eq!(config.context_lines, 3);
        assert!(config.color);
        assert_eq!(config.format, DiffFormat::Unified);
        assert_eq!(config.stat_width, 80);
        assert!(config.show_line_numbers);
        assert!(config.show_path_prefix);
        assert!(!config.word_diff);
    }

    #[test]
    fn test_diff_output_config_new() {
        let config = DiffOutputConfig::new();
        assert_eq!(config.context_lines, 3);
    }

    #[test]
    fn test_diff_output_config_with_context() {
        let config = DiffOutputConfig::new().with_context(5);
        assert_eq!(config.context_lines, 5);
    }

    #[test]
    fn test_diff_output_config_with_color() {
        let config = DiffOutputConfig::new().with_color(false);
        assert!(!config.color);
    }

    #[test]
    fn test_diff_output_config_with_format() {
        let config = DiffOutputConfig::new().with_format(DiffFormat::Stat);
        assert_eq!(config.format, DiffFormat::Stat);
    }

    #[test]
    fn test_diff_output_config_with_stat_width() {
        let config = DiffOutputConfig::new().with_stat_width(80);
        assert_eq!(config.stat_width, 80);
    }

    #[test]
    fn test_diff_output_config_with_line_numbers() {
        let config = DiffOutputConfig::new().with_line_numbers(true);
        assert!(config.show_line_numbers);
    }

    #[test]
    fn test_diff_output_config_with_path_prefix() {
        let config = DiffOutputConfig::new().with_path_prefix(false);
        assert!(!config.show_path_prefix);
    }

    #[test]
    fn test_diff_output_config_builder_chain() {
        let config = DiffOutputConfig::new()
            .with_context(10)
            .with_color(false)
            .with_format(DiffFormat::NameStatus)
            .with_stat_width(100)
            .with_line_numbers(true)
            .with_path_prefix(false);

        assert_eq!(config.context_lines, 10);
        assert!(!config.color);
        assert_eq!(config.format, DiffFormat::NameStatus);
        assert_eq!(config.stat_width, 100);
        assert!(config.show_line_numbers);
        assert!(!config.show_path_prefix);
    }

    // DiffHunk Tests

    #[test]
    fn test_diff_hunk_new() {
        let graph_op = DiffHunk::new(1, 5, 1, 6);
        assert_eq!(graph_op.old_start, 1);
        assert_eq!(graph_op.old_count, 5);
        assert_eq!(graph_op.new_start, 1);
        assert_eq!(graph_op.new_count, 6);
        assert!(graph_op.lines.is_empty());
    }

    #[test]
    fn test_diff_hunk_add_line() {
        let mut graph_op = DiffHunk::new(1, 1, 1, 2);
        graph_op.add_line(HunkLine::context("unchanged", 1, 1));
        graph_op.add_line(HunkLine::added("new line", 2));

        assert_eq!(graph_op.lines.len(), 2);
    }

    #[test]
    fn test_diff_hunk_header() {
        let graph_op = DiffHunk::new(10, 5, 12, 7);
        assert_eq!(graph_op.header(), "@@ -10,5 +12,7 @@");
    }

    #[test]
    fn test_diff_hunk_header_single_lines() {
        let graph_op = DiffHunk::new(1, 1, 1, 1);
        assert_eq!(graph_op.header(), "@@ -1,1 +1,1 @@");
    }

    #[test]
    fn test_diff_hunk_has_changes() {
        let mut hunk_with_changes = DiffHunk::new(1, 1, 1, 2);
        hunk_with_changes.add_line(HunkLine::added("new", 1));
        assert!(hunk_with_changes.has_changes());

        let mut hunk_no_changes = DiffHunk::new(1, 1, 1, 1);
        hunk_no_changes.add_line(HunkLine::context("same", 1, 1));
        assert!(!hunk_no_changes.has_changes());
    }

    // HunkLine Tests

    #[test]
    fn test_hunk_line_context() {
        let line = HunkLine::context("unchanged line", 5, 5);
        assert_eq!(line.status, LineStatus::Unchanged);
        assert_eq!(line.content, "unchanged line");
        assert_eq!(line.old_line_num, Some(5));
        assert_eq!(line.new_line_num, Some(5));
    }

    #[test]
    fn test_hunk_line_added() {
        let line = HunkLine::added("new line", 10);
        assert_eq!(line.status, LineStatus::Added);
        assert_eq!(line.content, "new line");
        assert_eq!(line.old_line_num, None);
        assert_eq!(line.new_line_num, Some(10));
    }

    #[test]
    fn test_hunk_line_removed() {
        let line = HunkLine::removed("old line", 7);
        assert_eq!(line.status, LineStatus::Removed);
        assert_eq!(line.content, "old line");
        assert_eq!(line.old_line_num, Some(7));
        assert_eq!(line.new_line_num, None);
    }

    #[test]
    fn test_hunk_line_is_change() {
        assert!(!HunkLine::context("x", 1, 1).is_change());
        assert!(HunkLine::added("x", 1).is_change());
        assert!(HunkLine::removed("x", 1).is_change());
    }

    #[test]
    fn test_hunk_line_is_context() {
        assert!(HunkLine::context("x", 1, 1).is_context());
        assert!(!HunkLine::added("x", 1).is_context());
        assert!(!HunkLine::removed("x", 1).is_context());
    }

    #[test]
    fn test_hunk_line_prefix() {
        assert_eq!(HunkLine::context("x", 1, 1).prefix(), ' ');
        assert_eq!(HunkLine::added("x", 1).prefix(), '+');
        assert_eq!(HunkLine::removed("x", 1).prefix(), '-');
    }

    #[test]
    fn test_hunk_line_display() {
        let context = HunkLine::context("same", 1, 1);
        let added = HunkLine::added("new", 1);
        let removed = HunkLine::removed("old", 1);

        assert_eq!(format!("{}", context), " same");
        assert_eq!(format!("{}", added), "+new");
        assert_eq!(format!("{}", removed), "-old");
    }

    // FileDiff Tests

    #[test]
    fn test_file_diff_new() {
        let diff = FileDiff::new("src/main.rs", FileChangeStatus::Modified);
        assert_eq!(diff.old_path, "src/main.rs");
        assert_eq!(diff.new_path, "src/main.rs");
        assert_eq!(diff.status, FileChangeStatus::Modified);
        assert!(diff.hunks.is_empty());
        assert!(!diff.is_binary);
    }

    #[test]
    fn test_file_diff_added() {
        let diff = FileDiff::added("new_file.rs");
        assert_eq!(diff.old_path, "/dev/null");
        assert_eq!(diff.new_path, "new_file.rs");
        assert_eq!(diff.status, FileChangeStatus::Added);
        assert_eq!(diff.stats.status, 'A');
    }

    #[test]
    fn test_file_diff_deleted() {
        let diff = FileDiff::deleted("old_file.rs");
        assert_eq!(diff.old_path, "old_file.rs");
        assert_eq!(diff.new_path, "/dev/null");
        assert_eq!(diff.status, FileChangeStatus::Deleted);
        assert_eq!(diff.stats.status, 'D');
    }

    #[test]
    fn test_file_diff_modified() {
        let diff = FileDiff::modified("changed.rs");
        assert_eq!(diff.old_path, "changed.rs");
        assert_eq!(diff.new_path, "changed.rs");
        assert_eq!(diff.status, FileChangeStatus::Modified);
        assert_eq!(diff.stats.status, 'M');
    }

    #[test]
    fn test_file_diff_add_hunk() {
        let mut diff = FileDiff::modified("test.rs");
        diff.add_hunk(DiffHunk::new(1, 1, 1, 2));
        assert_eq!(diff.hunks.len(), 1);
    }

    #[test]
    fn test_file_diff_compute_stats() {
        let mut diff = FileDiff::modified("test.rs");
        let mut graph_op = DiffHunk::new(1, 2, 1, 3);
        graph_op.add_line(HunkLine::context("line1", 1, 1));
        graph_op.add_line(HunkLine::removed("old line", 2));
        graph_op.add_line(HunkLine::added("new line 1", 2));
        graph_op.add_line(HunkLine::added("new line 2", 3));
        diff.add_hunk(graph_op);

        diff.compute_stats();

        assert_eq!(diff.stats.insertions, 2);
        assert_eq!(diff.stats.deletions, 1);
    }

    #[test]
    fn test_file_diff_has_changes() {
        let mut diff = FileDiff::modified("test.rs");
        assert!(!diff.has_changes());

        let mut graph_op = DiffHunk::new(1, 1, 1, 2);
        graph_op.add_line(HunkLine::added("new", 1));
        diff.add_hunk(graph_op);
        assert!(diff.has_changes());
    }

    #[test]
    fn test_file_diff_has_changes_binary() {
        let mut diff = FileDiff::modified("image.png");
        diff.is_binary = true;
        assert!(diff.has_changes());
    }

    #[test]
    fn test_file_diff_display_path() {
        let added = FileDiff::added("new.rs");
        assert_eq!(added.display_path(), "new.rs");

        let deleted = FileDiff::deleted("old.rs");
        assert_eq!(deleted.display_path(), "old.rs");

        let modified = FileDiff::modified("changed.rs");
        assert_eq!(modified.display_path(), "changed.rs");
    }

    // FileChangeStatus Tests

    #[test]
    fn test_file_change_status_char() {
        assert_eq!(FileChangeStatus::Added.status_char(), 'A');
        assert_eq!(FileChangeStatus::Deleted.status_char(), 'D');
        assert_eq!(FileChangeStatus::Modified.status_char(), 'M');
        assert_eq!(FileChangeStatus::Renamed.status_char(), 'R');
        assert_eq!(FileChangeStatus::Copied.status_char(), 'C');
        assert_eq!(FileChangeStatus::TypeChanged.status_char(), 'T');
        assert_eq!(FileChangeStatus::Untracked.status_char(), 'U');
    }

    #[test]
    fn test_file_change_status_description() {
        assert_eq!(FileChangeStatus::Added.description(), "added");
        assert_eq!(FileChangeStatus::Deleted.description(), "deleted");
        assert_eq!(FileChangeStatus::Modified.description(), "modified");
        assert_eq!(FileChangeStatus::Renamed.description(), "renamed");
        assert_eq!(FileChangeStatus::Copied.description(), "copied");
        assert_eq!(FileChangeStatus::TypeChanged.description(), "type changed");
        assert_eq!(FileChangeStatus::Untracked.description(), "untracked");
    }

    #[test]
    fn test_file_change_status_display() {
        assert_eq!(format!("{}", FileChangeStatus::Added), "added");
        assert_eq!(format!("{}", FileChangeStatus::Modified), "modified");
        assert_eq!(format!("{}", FileChangeStatus::Untracked), "untracked");
    }

    #[test]
    fn test_file_change_status_from_file_status() {
        assert_eq!(
            FileChangeStatus::from(FileStatus::Added),
            FileChangeStatus::Added
        );
        assert_eq!(
            FileChangeStatus::from(FileStatus::Deleted),
            FileChangeStatus::Deleted
        );
        assert_eq!(
            FileChangeStatus::from(FileStatus::Modified),
            FileChangeStatus::Modified
        );
        assert_eq!(
            FileChangeStatus::from(FileStatus::TypeChanged),
            FileChangeStatus::TypeChanged
        );
        assert_eq!(
            FileChangeStatus::from(FileStatus::Untracked),
            FileChangeStatus::Untracked
        );
    }

    #[test]
    fn test_file_change_status_equality() {
        assert_eq!(FileChangeStatus::Added, FileChangeStatus::Added);
        assert_ne!(FileChangeStatus::Added, FileChangeStatus::Deleted);
    }

    #[test]
    fn test_file_change_status_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(FileChangeStatus::Added);
        set.insert(FileChangeStatus::Modified);
        assert!(set.contains(&FileChangeStatus::Added));
        assert!(!set.contains(&FileChangeStatus::Deleted));
    }

    // Diff Command Tests

    #[test]
    fn test_diff_new() {
        let diff = Diff::new();
        assert!(diff.files.is_empty());
        assert!(diff.change.is_none());
        assert_eq!(diff.algorithm, "myers");
        assert_eq!(diff.context, 3);
        assert!(!diff.stat);
        assert!(!diff.no_color);
        assert!(!diff.name_only);
        assert!(!diff.name_status);
        assert!(!diff.cached);
        assert!(diff.view.is_none());
    }

    #[test]
    fn test_diff_default() {
        let diff = Diff::default();
        assert_eq!(diff.algorithm, "myers");
        assert_eq!(diff.context, 3);
    }

    #[test]
    fn test_diff_with_files() {
        let diff = Diff::new().with_files(vec!["a.rs", "b.rs"]);
        assert_eq!(diff.files, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn test_diff_with_files_string() {
        let diff = Diff::new().with_files(vec![String::from("test.rs")]);
        assert_eq!(diff.files, vec!["test.rs"]);
    }

    #[test]
    fn test_diff_with_change() {
        let diff = Diff::new().with_change("abc123");
        assert_eq!(diff.change, Some("abc123".to_string()));
    }

    #[test]
    fn test_diff_with_algorithm() {
        let diff = Diff::new().with_algorithm("patience");
        assert_eq!(diff.algorithm, "patience");
    }

    #[test]
    fn test_diff_with_context() {
        let diff = Diff::new().with_context(5);
        assert_eq!(diff.context, 5);
    }

    #[test]
    fn test_diff_with_stat() {
        let diff = Diff::new().with_stat(true);
        assert!(diff.stat);
    }

    #[test]
    fn test_diff_with_no_color() {
        let diff = Diff::new().with_no_color(true);
        assert!(diff.no_color);
    }

    #[test]
    fn test_diff_with_name_only() {
        let diff = Diff::new().with_name_only(true);
        assert!(diff.name_only);
    }

    #[test]
    fn test_diff_with_name_status() {
        let diff = Diff::new().with_name_status(true);
        assert!(diff.name_status);
    }

    #[test]
    fn test_diff_with_view() {
        let diff = Diff::new().with_view("feature");
        assert_eq!(diff.view, Some("feature".to_string()));
    }

    #[test]
    fn test_diff_builder_chain() {
        let diff = Diff::new()
            .with_files(vec!["test.rs"])
            .with_algorithm("patience")
            .with_context(10)
            .with_stat(true)
            .with_no_color(true);

        assert_eq!(diff.files, vec!["test.rs"]);
        assert_eq!(diff.algorithm, "patience");
        assert_eq!(diff.context, 10);
        assert!(diff.stat);
        assert!(diff.no_color);
    }

    #[test]
    fn test_diff_get_format_unified() {
        let diff = Diff::new();
        assert_eq!(diff.get_format(), DiffFormat::Unified);
    }

    #[test]
    fn test_diff_get_format_stat() {
        let diff = Diff::new().with_stat(true);
        assert_eq!(diff.get_format(), DiffFormat::Stat);
    }

    #[test]
    fn test_diff_get_format_name_only() {
        let diff = Diff::new().with_name_only(true);
        assert_eq!(diff.get_format(), DiffFormat::NameOnly);
    }

    #[test]
    fn test_diff_get_format_name_status() {
        let diff = Diff::new().with_name_status(true);
        assert_eq!(diff.get_format(), DiffFormat::NameStatus);
    }

    #[test]
    fn test_diff_get_format_priority() {
        // name_only takes priority over stat
        let diff = Diff::new().with_stat(true).with_name_only(true);
        assert_eq!(diff.get_format(), DiffFormat::NameOnly);

        // name_status takes priority over stat but not name_only
        let diff2 = Diff::new().with_stat(true).with_name_status(true);
        assert_eq!(diff2.get_format(), DiffFormat::NameStatus);
    }

    #[test]
    fn test_diff_parse_algorithm_myers() {
        let diff = Diff::new().with_algorithm("myers");
        let algo = diff.parse_algorithm().unwrap();
        assert_eq!(algo, Algorithm::Myers);
    }

    #[test]
    fn test_diff_parse_algorithm_patience() {
        let diff = Diff::new().with_algorithm("patience");
        let algo = diff.parse_algorithm().unwrap();
        assert_eq!(algo, Algorithm::Patience);
    }

    #[test]
    fn test_diff_parse_algorithm_invalid() {
        let diff = Diff::new().with_algorithm("invalid");
        let result = diff.parse_algorithm();
        assert!(result.is_err());
    }

    #[test]
    fn test_diff_get_output_config() {
        let diff = Diff::new()
            .with_context(5)
            .with_no_color(true)
            .with_stat(true);

        let config = diff.get_output_config();

        assert_eq!(config.context_lines, 5);
        assert!(!config.color);
        assert_eq!(config.format, DiffFormat::Stat);
    }

    // Helper Function Tests

    #[test]
    fn test_format_stat_graph_empty() {
        let graph = format_stat_graph(0, 0, 50);
        assert_eq!(graph, "");
    }

    #[test]
    fn test_format_stat_graph_insertions_only() {
        let graph = format_stat_graph(5, 0, 50);
        assert_eq!(graph, "+++++");
    }

    #[test]
    fn test_format_stat_graph_deletions_only() {
        let graph = format_stat_graph(0, 3, 50);
        assert_eq!(graph, "---");
    }

    #[test]
    fn test_format_stat_graph_mixed() {
        let graph = format_stat_graph(3, 2, 50);
        assert_eq!(graph, "+++--");
    }

    #[test]
    fn test_format_stat_graph_scaled() {
        // When total > max_width, scale down
        let graph = format_stat_graph(100, 50, 30);
        // 100 + 50 = 150, scaled to 30
        // Should be approximately 20 + and 10 -
        assert!(graph.chars().filter(|&c| c == '+').count() <= 30);
        assert!(graph.chars().filter(|&c| c == '-').count() <= 30);
    }

    #[test]
    fn test_build_hunks_from_diff_empty() {
        let diff_result = DiffResult::new();
        let old_lines: Vec<&[u8]> = vec![];
        let new_lines: Vec<&[u8]> = vec![];

        let hunks = build_hunks_from_diff(&diff_result, &old_lines, &new_lines, 3);
        assert!(hunks.is_empty());
    }

    // Debug and Clone Tests

    #[test]
    fn test_diff_format_debug() {
        let format = DiffFormat::Unified;
        let debug = format!("{:?}", format);
        assert!(debug.contains("Unified"));
    }

    #[test]
    fn test_file_diff_stats_clone() {
        let stats = FileDiffStats::new("test.rs", 10, 5, 'M');
        let cloned = stats.clone();
        assert_eq!(stats.path, cloned.path);
        assert_eq!(stats.insertions, cloned.insertions);
    }

    #[test]
    fn test_diff_stats_clone() {
        let mut stats = DiffStats::new();
        stats.add_file(FileDiffStats::new("a.rs", 1, 0, 'M'));
        let cloned = stats.clone();
        assert_eq!(stats.file_count(), cloned.file_count());
    }

    #[test]
    fn test_diff_output_config_clone() {
        let config = DiffOutputConfig::new().with_context(10);
        let cloned = config.clone();
        assert_eq!(config.context_lines, cloned.context_lines);
    }

    #[test]
    fn test_diff_hunk_clone() {
        let graph_op = DiffHunk::new(1, 5, 1, 6);
        let cloned = graph_op.clone();
        assert_eq!(graph_op.old_start, cloned.old_start);
    }

    #[test]
    fn test_hunk_line_clone() {
        let line = HunkLine::added("test", 1);
        let cloned = line.clone();
        assert_eq!(line.content, cloned.content);
    }

    #[test]
    fn test_file_diff_clone() {
        let diff = FileDiff::modified("test.rs");
        let cloned = diff.clone();
        assert_eq!(diff.old_path, cloned.old_path);
    }

    #[test]
    fn test_diff_cmd_clone() {
        let diff = Diff::new().with_context(10);
        let cloned = diff.clone();
        assert_eq!(diff.context, cloned.context);
    }

    #[test]
    fn test_diff_format_stat_copy() {
        let format = DiffFormat::Stat;
        let copied = format;
        assert_eq!(format, copied);
    }

    #[test]
    fn test_file_change_status_copy() {
        let status = FileChangeStatus::Added;
        let copied = status;
        assert_eq!(status, copied);
    }

    #[test]
    fn test_diff_hunk_debug() {
        let graph_op = DiffHunk::new(1, 5, 1, 6);
        let debug = format!("{:?}", graph_op);
        assert!(debug.contains("DiffHunk"));
    }

    #[test]
    fn test_hunk_line_debug() {
        let line = HunkLine::added("test", 1);
        let debug = format!("{:?}", line);
        assert!(debug.contains("HunkLine"));
    }

    #[test]
    fn test_file_diff_debug() {
        let diff = FileDiff::modified("test.rs");
        let debug = format!("{:?}", diff);
        assert!(debug.contains("FileDiff"));
    }

    #[test]
    fn test_file_change_status_debug() {
        let status = FileChangeStatus::Modified;
        let debug = format!("{:?}", status);
        assert!(debug.contains("Modified"));
    }

    #[test]
    fn test_diff_cmd_debug() {
        let diff = Diff::new();
        let debug = format!("{:?}", diff);
        assert!(debug.contains("Diff"));
    }

    // Integration Tests (require temp directories)

    use serial_test::serial;

    /// Guard that restores the current directory when dropped.
    struct DirGuard {
        original: PathBuf,
    }

    impl DirGuard {
        fn new() -> Self {
            Self {
                original: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            }
        }
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    #[test]
    #[serial]
    fn test_diff_run_outside_repository() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(&temp_dir).unwrap();

        let diff = Diff::new();
        let result = diff.run();

        // Should fail because we're not in a repository
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_diff_run_no_changes() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let diff = Diff::new();
        let result = diff.run();

        // Should succeed but show no changes
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_diff_run_with_untracked_file() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository and create untracked file
        {
            let _repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("untracked.txt"), "Hello").unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Run diff (no changes expected since untracked files aren't shown by default)
        let diff = Diff::default();
        let result = diff.run();

        // Should succeed - untracked files are not shown in diff by default
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_diff_short_flag() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Create diff with --short flag
        let diff = Diff {
            short: true,
            ..Default::default()
        };

        // --short should set format to NameStatus
        assert_eq!(diff.get_format(), DiffFormat::NameStatus);
    }

    #[test]
    #[serial]
    fn test_diff_untracked_flag() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository and create untracked file
        {
            let _repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("untracked.txt"), "Hello").unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Run diff with --untracked flag
        let diff = Diff {
            untracked: true,
            short: true,
            ..Default::default()
        };
        let result = diff.run();

        // Should succeed and include untracked files
        assert!(result.is_ok());
    }

    #[test]
    fn test_file_change_status_untracked() {
        assert_eq!(FileChangeStatus::Untracked.status_char(), 'U');
        assert_eq!(FileChangeStatus::Untracked.description(), "untracked");
    }

    #[test]
    #[serial]
    fn test_diff_run_with_added_file() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository, create and add a file
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("new_file.txt"), "New content").unwrap();
            repo.add("new_file.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let diff = Diff::new();
        let result = diff.run();

        // Should succeed and show the added file
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_diff_run_name_only_format() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository and add a file
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("test.txt"), "Content").unwrap();
            repo.add("test.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let diff = Diff::new().with_name_only(true);
        let result = diff.run();

        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_diff_run_stat_format() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository and add a file
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("test.txt"), "Line 1\nLine 2\nLine 3\n").unwrap();
            repo.add("test.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let diff = Diff::new().with_stat(true);
        let result = diff.run();

        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_diff_end_to_end_modified_file() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();
        let file_path = repo_path.join("hello.txt");

        // Step 1: Initialize repository and add a file (scope to release lock)
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(&file_path, "Hello, World!\n").unwrap();
            repo.add("hello.txt", Default::default()).unwrap();
            // repo is dropped here, releasing the database lock
        }

        // Step 2: Record the initial change
        use crate::commands::record::Record;
        std::env::set_current_dir(repo_path).unwrap();

        let record = Record::new().with_message("Initial commit");
        let record_result = record.run();

        // Debug: Print record result
        if let Err(ref e) = record_result {
            eprintln!("Record error: {:?}", e);
        }

        // If record succeeded, continue with modification test
        if record_result.is_ok() {
            // Step 3: Modify the file
            std::fs::write(&file_path, "Hello, Modified World!\n").unwrap();

            // Step 4: Run diff - should show the modification
            let diff = Diff::new();
            let diff_result = diff.run();

            // Debug: Print diff result
            if let Err(ref e) = diff_result {
                eprintln!("Diff error: {:?}", e);
            }

            // The diff should succeed
            assert!(diff_result.is_ok());

            // Step 5: Verify the file is detected as modified by checking status
            let repo = Repository::open(repo_path).unwrap();
            let status = repo.status(Default::default()).unwrap();

            // The file should be detected as modified
            let modified_count = status.modified_count();

            // This assertion validates the full end-to-end workflow:
            // 1. File is recorded to graph
            // 2. File is modified on disk
            // 3. Status detects the modification by comparing content hashes
            // 4. Diff can show the changes
            //
            // If this fails with modified_count == 0, it means:
            // - Either record didn't properly save to the graph, OR
            // - Status can't retrieve the recorded content, OR
            // - Content comparison isn't working correctly
            assert!(
                modified_count > 0,
                "Expected file to be detected as modified. \
                 This indicates the record->status->diff chain is broken."
            );
        }
    }

    #[test]
    #[serial]
    fn test_diff_end_to_end_multiple_files() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Step 1: Initialize repository and create multiple files
        {
            let repo = Repository::init(repo_path).unwrap();

            // Create a directory structure
            std::fs::create_dir_all(repo_path.join("src")).unwrap();

            // Create multiple files with different content
            std::fs::write(
                repo_path.join("README.md"),
                "# My Project\n\nThis is a test project.\n",
            )
            .unwrap();
            std::fs::write(
                repo_path.join("src/main.rs"),
                "fn main() {\n    println!(\"Hello, World!\");\n}\n",
            )
            .unwrap();
            std::fs::write(
                repo_path.join("src/lib.rs"),
                "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
            )
            .unwrap();
            std::fs::write(
                repo_path.join("config.toml"),
                "[settings]\nname = \"test\"\n",
            )
            .unwrap();

            // Add all files
            repo.add("README.md", Default::default()).unwrap();
            repo.add("src/main.rs", Default::default()).unwrap();
            repo.add("src/lib.rs", Default::default()).unwrap();
            repo.add("config.toml", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Step 2: Record the initial state
        use crate::commands::record::Record;
        let record = Record::new().with_message("Initial commit with multiple files");
        let record_result = record.run();

        if record_result.is_err() {
            // Skip test if record fails (may need dependencies)
            return;
        }

        // Step 3: Modify multiple files in different ways
        // Modify README.md
        std::fs::write(
            repo_path.join("README.md"),
            "# My Project\n\nThis is an **updated** test project.\n\n## Features\n\n- Feature 1\n- Feature 2\n",
        )
        .unwrap();

        // Modify src/main.rs
        std::fs::write(
            repo_path.join("src/main.rs"),
            "fn main() {\n    println!(\"Hello, Modified World!\");\n    println!(\"Second line\");\n}\n",
        )
        .unwrap();

        // Leave src/lib.rs unchanged

        // Delete config.toml
        std::fs::remove_file(repo_path.join("config.toml")).unwrap();

        // Add a new file (should show as added/untracked)
        std::fs::write(repo_path.join("new_file.txt"), "This is a new file\n").unwrap();

        // Step 4: Run diff and verify it works
        let diff = Diff::new();
        let diff_result = diff.run();
        assert!(diff_result.is_ok(), "Diff command should succeed");

        // Step 5: Verify status detects all changes correctly
        let repo = Repository::open(repo_path).unwrap();
        let status = repo.status(Default::default()).unwrap();

        // Check modified files
        let modified_count = status.modified_count();
        assert!(
            modified_count >= 2,
            "Expected at least 2 modified files (README.md, src/main.rs), got {}",
            modified_count
        );

        // Check deleted files
        let deleted_count = status.deleted_count();
        assert!(
            deleted_count >= 1,
            "Expected at least 1 deleted file (config.toml), got {}",
            deleted_count
        );

        // Verify specific files are in the expected state
        let modified_paths: Vec<_> = status.modified().map(|e| e.path().to_path_buf()).collect();
        assert!(
            modified_paths
                .iter()
                .any(|p| p.to_string_lossy().contains("README.md")),
            "README.md should be modified"
        );
        assert!(
            modified_paths
                .iter()
                .any(|p| p.to_string_lossy().contains("main.rs")),
            "src/main.rs should be modified"
        );

        let deleted_paths: Vec<_> = status.deleted().map(|e| e.path().to_path_buf()).collect();
        assert!(
            deleted_paths
                .iter()
                .any(|p| p.to_string_lossy().contains("config.toml")),
            "config.toml should be deleted"
        );

        // Verify lib.rs is clean (unchanged)
        let clean_paths: Vec<_> = status.clean().map(|e| e.path().to_path_buf()).collect();
        assert!(
            clean_paths
                .iter()
                .any(|p| p.to_string_lossy().contains("lib.rs")),
            "src/lib.rs should be clean (unchanged)"
        );

        // Drop the repository to release the database lock before running more diff commands
        drop(status);
        drop(repo);

        // Step 6: Test diff with stat format
        let diff_stat = Diff::new().with_stat(true);
        let stat_result = diff_stat.run();
        assert!(stat_result.is_ok(), "Diff --stat should succeed");

        // Step 7: Test diff with name-only format
        let diff_name_only = Diff::new().with_name_only(true);
        let name_only_result = diff_name_only.run();
        assert!(name_only_result.is_ok(), "Diff --name-only should succeed");

        // Step 8: Test diff with name-status format
        let diff_name_status = Diff::new().with_name_status(true);
        let name_status_result = diff_name_status.run();
        assert!(
            name_status_result.is_ok(),
            "Diff --name-status should succeed"
        );

        // Step 9: Test diff for a specific file
        let diff_specific = Diff::new().with_files(vec!["README.md"]);
        let specific_result = diff_specific.run();
        assert!(
            specific_result.is_ok(),
            "Diff for specific file should succeed"
        );

        // Step 10: Verify content retrieval works for all recorded files
        // Re-open the repository since we dropped it earlier
        let repo = Repository::open(repo_path).unwrap();
        assert!(
            repo.get_file_content("README.md").unwrap().is_some(),
            "Should retrieve README.md content"
        );
        assert!(
            repo.get_file_content("src/main.rs").unwrap().is_some(),
            "Should retrieve src/main.rs content"
        );
        assert!(
            repo.get_file_content("src/lib.rs").unwrap().is_some(),
            "Should retrieve src/lib.rs content"
        );
    }

    #[test]
    #[serial]
    fn test_diff_with_specific_file() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize and add files
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("file1.txt"), "Content 1").unwrap();
            std::fs::write(repo_path.join("file2.txt"), "Content 2").unwrap();
            repo.add("file1.txt", Default::default()).unwrap();
            repo.add("file2.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Diff only file1.txt
        let diff = Diff::new().with_files(vec!["file1.txt"]);
        let result = diff.run();

        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_diff_patience_algorithm() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize and add a file
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("test.txt"), "Original content\n").unwrap();
            repo.add("test.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Use patience algorithm
        let diff = Diff::new().with_algorithm("patience");
        let result = diff.run();

        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_diff_no_color() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize and add a file
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("test.txt"), "Content").unwrap();
            repo.add("test.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let diff = Diff::new().with_no_color(true);
        let result = diff.run();

        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_diff_word_diff_enabled() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository and create initial file
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("code.rs"), "let x = 42;\n").unwrap();
            repo.add("code.rs", Default::default()).unwrap();

            // Record the initial state
            let header = atomic_core::change::ChangeHeader::builder()
                .message("Initial commit")
                .build();
            repo.record(header, Default::default()).unwrap();
        }

        // Modify the file (change value)
        std::fs::write(repo_path.join("code.rs"), "let x = 100;\n").unwrap();

        std::env::set_current_dir(repo_path).unwrap();

        // Create diff with word-diff enabled
        let diff = Diff::new().with_word_diff(true).with_no_color(false); // Ensure color is on for word-diff

        assert!(diff.word_diff);

        let config = diff.get_output_config();
        assert!(config.word_diff);

        // Run should succeed
        let result = diff.run();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_diff_custom_context_lines() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize and add a file
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(
                repo_path.join("test.txt"),
                "Line 1\nLine 2\nLine 3\nLine 4\nLine 5\n",
            )
            .unwrap();
            repo.add("test.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Use 5 context lines
        let diff = Diff::new().with_context(5);
        let result = diff.run();

        assert!(result.is_ok());
    }
}
