use super::*;


// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // SemanticLine tests

    #[test]
    fn test_semantic_line_new() {
        let line = SemanticLine::new(b"let x = 42;\n", 1);
        assert_eq!(line.line_num(), 1);
        assert!(!line.tokens().is_empty());
    }

    #[test]
    fn test_semantic_line_from_bytes() {
        let content = b"line1\nline2\nline3\n";
        let lines = SemanticLine::from_bytes(content);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].line_num(), 1);
        assert_eq!(lines[1].line_num(), 2);
        assert_eq!(lines[2].line_num(), 3);
    }

    #[test]
    fn test_semantic_line_content_str() {
        let line = SemanticLine::new(b"hello world\n", 1);
        assert_eq!(line.content_str(), "hello world\n");
    }

    #[test]
    fn test_semantic_line_is_blank() {
        let blank = SemanticLine::new(b"   \n", 1);
        assert!(blank.is_blank());

        let not_blank = SemanticLine::new(b"hello\n", 1);
        assert!(!not_blank.is_blank());
    }

    #[test]
    fn test_semantic_line_significant_token_count() {
        let line = SemanticLine::new(b"let x = 42;\n", 1);
        // "let", "x", "=", "42", ";" are significant
        assert!(line.significant_token_count() >= 4);
    }

    // TokenChange tests

    #[test]
    fn test_token_change_unchanged() {
        let token = Token::new(b"hello", TokenKind::Word, 0);
        let change = TokenChange::Unchanged {
            token: token.clone(),
            old_range: 0..5,
            new_range: 0..5,
        };
        assert!(change.is_unchanged());
        assert!(!change.is_change());
        assert!(change.old_token().is_some());
        assert!(change.new_token().is_some());
    }

    #[test]
    fn test_token_change_inserted() {
        let token = Token::new(b"world", TokenKind::Word, 6);
        let change = TokenChange::Inserted {
            token: token.clone(),
            new_range: 6..11,
        };
        assert!(change.is_inserted());
        assert!(change.is_change());
        assert!(change.old_token().is_none());
        assert!(change.new_token().is_some());
        assert!(change.old_range().is_none());
        assert!(change.new_range().is_some());
    }

    #[test]
    fn test_token_change_deleted() {
        let token = Token::new(b"old", TokenKind::Word, 0);
        let change = TokenChange::Deleted {
            token: token.clone(),
            old_range: 0..3,
        };
        assert!(change.is_deleted());
        assert!(change.is_change());
        assert!(change.old_token().is_some());
        assert!(change.new_token().is_none());
    }

    #[test]
    fn test_token_change_replaced() {
        let old_token = Token::new(b"foo", TokenKind::Word, 0);
        let new_token = Token::new(b"bar", TokenKind::Word, 0);
        let change = TokenChange::Replaced {
            old_token,
            new_token,
            old_range: 0..3,
            new_range: 0..3,
        };
        assert!(change.is_replaced());
        assert!(change.is_change());
    }

    #[test]
    fn test_token_change_description() {
        let token = Token::new(b"hello", TokenKind::Word, 0);
        let change = TokenChange::Inserted {
            token,
            new_range: 0..5,
        };
        let desc = change.description();
        assert!(desc.contains("inserted"));
        assert!(desc.contains("hello"));
    }

    // LineChange tests

    #[test]
    fn test_line_change_added() {
        let line = SemanticLine::new(b"new line\n", 5);
        let tokens = create_insertion_tokens(&line);
        let change = LineChange::Added {
            line_num: 5,
            line,
            tokens,
        };
        assert!(change.is_added());
        assert!(!change.is_deleted());
        assert!(!change.is_modified());
        assert!(change.old_line_num().is_none());
        assert_eq!(change.new_line_num(), Some(5));
    }

    #[test]
    fn test_line_change_deleted() {
        let line = SemanticLine::new(b"old line\n", 3);
        let tokens = create_deletion_tokens(&line);
        let change = LineChange::Deleted {
            line_num: 3,
            line,
            tokens,
        };
        assert!(change.is_deleted());
        assert_eq!(change.old_line_num(), Some(3));
        assert!(change.new_line_num().is_none());
    }

    #[test]
    fn test_line_change_modified() {
        let before = SemanticLine::new(b"let x = 1;\n", 1);
        let after = SemanticLine::new(b"let x = 2;\n", 1);
        let token_changes = compute_token_changes(&before, &after, &WordDiffConfig::default());

        let change = LineChange::Modified {
            old_line_num: 1,
            new_line_num: 1,
            before,
            after,
            token_changes,
        };
        assert!(change.is_modified());
        assert_eq!(change.old_line_num(), Some(1));
        assert_eq!(change.new_line_num(), Some(1));
    }

    #[test]
    fn test_line_change_summary() {
        let line = SemanticLine::new(b"added\n", 1);
        let tokens = create_insertion_tokens(&line);
        let change = LineChange::Added {
            line_num: 1,
            line,
            tokens,
        };
        let summary = change.summary();
        assert!(summary.starts_with("+1:"));
    }

    // SemanticDiffStats tests

    #[test]
    fn test_semantic_diff_stats_default() {
        let stats = SemanticDiffStats::default();
        assert!(!stats.has_changes());
        assert_eq!(stats.total_line_changes(), 0);
        assert_eq!(stats.total_token_changes(), 0);
    }

    #[test]
    fn test_semantic_diff_stats_has_changes() {
        let mut stats = SemanticDiffStats::default();
        stats.lines_added = 1;
        assert!(stats.has_changes());
    }

    #[test]
    fn test_semantic_diff_stats_totals() {
        let stats = SemanticDiffStats {
            lines_added: 2,
            lines_deleted: 1,
            lines_modified: 3,
            tokens_inserted: 10,
            tokens_deleted: 5,
            tokens_replaced: 2,
        };
        assert_eq!(stats.total_line_changes(), 6);
        assert_eq!(stats.total_token_changes(), 17);
    }

    // SemanticDiffConfig tests

    #[test]
    fn test_semantic_diff_config_default() {
        let config = SemanticDiffConfig::default();
        assert!(!config.include_context);
        assert!(!config.ignore_whitespace);
        assert!(!config.ignore_blank_lines);
    }

    #[test]
    fn test_semantic_diff_config_builder() {
        let config = SemanticDiffConfig::new()
            .with_algorithm(Algorithm::Patience)
            .with_context(5)
            .ignore_whitespace()
            .ignore_blank_lines();

        assert_eq!(config.algorithm, Algorithm::Patience);
        assert!(config.include_context);
        assert_eq!(config.context_lines, 5);
        assert!(config.ignore_whitespace);
        assert!(config.ignore_blank_lines);
    }

    // semantic_diff tests

    #[test]
    fn test_semantic_diff_identical() {
        let content = b"line1\nline2\nline3\n";
        let diff = semantic_diff(content, content);
        assert!(diff.is_unchanged());
        assert!(!diff.has_changes());
        assert!(diff.changes().is_empty());
    }

    #[test]
    fn test_semantic_diff_empty() {
        let diff = semantic_diff(b"", b"");
        assert!(diff.is_unchanged());
        assert!(diff.is_empty());
    }

    #[test]
    fn test_semantic_diff_add_line() {
        let old = b"line1\nline3\n";
        let new = b"line1\nline2\nline3\n";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());
        assert_eq!(diff.stats().lines_added, 1);
        assert_eq!(diff.stats().lines_deleted, 0);

        let added: Vec<_> = diff.added_lines().collect();
        assert_eq!(added.len(), 1);
    }

    #[test]
    fn test_semantic_diff_delete_line() {
        let old = b"line1\nline2\nline3\n";
        let new = b"line1\nline3\n";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());
        assert_eq!(diff.stats().lines_deleted, 1);
        assert_eq!(diff.stats().lines_added, 0);

        let deleted: Vec<_> = diff.deleted_lines().collect();
        assert_eq!(deleted.len(), 1);
    }

    #[test]
    fn test_semantic_diff_modify_line() {
        let old = b"let x = 1;\n";
        let new = b"let x = 42;\n";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());
        assert_eq!(diff.stats().lines_modified, 1);

        let modified: Vec<_> = diff.modified_lines().collect();
        assert_eq!(modified.len(), 1);

        // Check that we have token-level changes
        let change = &modified[0];
        let token_changes = change.token_changes();
        assert!(!token_changes.is_empty());

        // Should have some replaced tokens (1 -> 42)
        let replaced: Vec<_> = token_changes.iter().filter(|tc| tc.is_replaced()).collect();
        assert!(!replaced.is_empty(), "Expected replaced tokens for 1 -> 42");
    }

    #[test]
    fn test_semantic_diff_token_level_detail() {
        // This is THE test - proving we get token-level granularity
        let old = b"const result = calculateSum(a, b);\n";
        let new = b"const result = calculateSum(a, b, c);\n";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());

        // Should be a modified line, not add+delete
        assert_eq!(diff.stats().lines_modified, 1);
        assert_eq!(diff.stats().lines_added, 0);
        assert_eq!(diff.stats().lines_deleted, 0);

        // Get the modification
        let modified: Vec<_> = diff.modified_lines().collect();
        let change = &modified[0];

        // The token changes should show the ", c" being added
        let token_changes = change.token_changes();
        let insertions: Vec<_> = token_changes.iter().filter(|tc| tc.is_inserted()).collect();

        // We should have insertions for ", c"
        assert!(!insertions.is_empty(), "Expected token insertions for ', c'");

        // Verify we can find the 'c' token
        let has_c = insertions.iter().any(|tc| {
            if let TokenChange::Inserted { token, .. } = tc {
                token.as_str() == "c"
            } else {
                false
            }
        });
        assert!(has_c, "Expected to find inserted 'c' token");
    }

    #[test]
    fn test_semantic_diff_variable_rename() {
        let old = b"let foo = 42;\n";
        let new = b"let bar = 42;\n";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());
        assert_eq!(diff.stats().lines_modified, 1);

        let modified: Vec<_> = diff.modified_lines().collect();
        let token_changes = modified[0].token_changes();

        // Should have a replacement from 'foo' to 'bar'
        let replaced: Vec<_> = token_changes.iter().filter(|tc| tc.is_replaced()).collect();
        assert!(!replaced.is_empty());

        // Check for foo -> bar replacement
        let has_foo_bar = replaced.iter().any(|tc| {
            if let TokenChange::Replaced {
                old_token,
                new_token,
                ..
            } = tc
            {
                old_token.as_str() == "foo" && new_token.as_str() == "bar"
            } else {
                false
            }
        });
        assert!(has_foo_bar, "Expected foo -> bar replacement");
    }

    #[test]
    fn test_semantic_diff_multiline() {
        let old = b"fn main() {\n    let x = 1;\n    println!(x);\n}\n";
        let new = b"fn main() {\n    let x = 42;\n    let y = 2;\n    println!(x);\n}\n";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());

        // Line 2 modified (1 -> 42), line 3 added (let y = 2)
        assert!(diff.stats().lines_modified >= 1);
        assert!(diff.stats().lines_added >= 1);
    }

    #[test]
    fn test_semantic_diff_all_token_changes() {
        let old = b"a b c\n";
        let new = b"a x c\n";

        let diff = semantic_diff(old, new);
        let all_changes: Vec<_> = diff.all_token_changes().collect();

        // Should have multiple token changes (unchanged 'a', replaced 'b'->'x', unchanged 'c')
        assert!(!all_changes.is_empty());
    }

    #[test]
    fn test_semantic_diff_display() {
        let old = b"old\n";
        let new = b"new\n";

        let diff = semantic_diff(old, new);
        let display = format!("{}", diff);

        // Should produce some output
        assert!(!display.is_empty());
    }

    // Helper function tests

    #[test]
    fn test_create_insertion_tokens() {
        let line = SemanticLine::new(b"hello world\n", 1);
        let tokens = create_insertion_tokens(&line);

        // All should be insertions
        for tc in &tokens {
            assert!(tc.is_inserted());
        }
    }

    #[test]
    fn test_create_deletion_tokens() {
        let line = SemanticLine::new(b"goodbye world\n", 1);
        let tokens = create_deletion_tokens(&line);

        // All should be deletions
        for tc in &tokens {
            assert!(tc.is_deleted());
        }
    }

    #[test]
    fn test_token_byte_range() {
        let content = b"hello world";
        let tokens: Vec<Token> = Tokenizer::new(content).collect();

        // First token "hello" should be at 0..5
        let range0 = token_byte_range(&tokens, 0);
        assert_eq!(range0, 0..5);

        // Space should be at 5..6
        let range1 = token_byte_range(&tokens, 1);
        assert_eq!(range1, 5..6);

        // "world" should be at 6..11
        let range2 = token_byte_range(&tokens, 2);
        assert_eq!(range2, 6..11);
    }

    #[test]
    fn test_compute_token_changes_identical() {
        let line1 = SemanticLine::new(b"let x = 42;\n", 1);
        let line2 = SemanticLine::new(b"let x = 42;\n", 1);

        let changes = compute_token_changes(&line1, &line2, &WordDiffConfig::default());

        // All tokens should be unchanged
        for tc in &changes {
            assert!(tc.is_unchanged(), "Expected unchanged, got: {:?}", tc);
        }
    }

    #[test]
    fn test_semantic_diff_hello_world_highlighting() {
        // This is THE canonical test from the spec:
        // "hello" → "hello_world" highlighting
        let old = b"let name = \"hello\";\n";
        let new = b"let name = \"hello_world\";\n";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());
        assert_eq!(diff.stats().lines_modified, 1);

        // Get the modified line
        let modified: Vec<_> = diff.modified_lines().collect();
        assert_eq!(modified.len(), 1);

        let change = &modified[0];
        let token_changes = change.token_changes();

        // Should have a replacement for the string token
        let replaced: Vec<_> = token_changes.iter().filter(|tc| tc.is_replaced()).collect();
        assert!(!replaced.is_empty(), "Expected string replacement");

        // Verify the string changed from "hello" to "hello_world"
        let has_hello_change = replaced.iter().any(|tc| {
            if let TokenChange::Replaced {
                old_token,
                new_token,
                ..
            } = tc
            {
                old_token.as_str().contains("hello") && new_token.as_str().contains("hello_world")
            } else {
                false
            }
        });
        assert!(
            has_hello_change,
            "Expected 'hello' -> 'hello_world' replacement"
        );
    }

    #[test]
    fn test_semantic_diff_function_argument_added() {
        // Test the example from the spec:
        // calculateSum(a, b) → calculateSum(a, b, c)
        let old = b"const result = calculateSum(a, b);\n";
        let new = b"const result = calculateSum(a, b, c);\n";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());

        // Should be a modified line (not delete + add)
        assert_eq!(diff.stats().lines_modified, 1);
        assert_eq!(diff.stats().lines_added, 0);
        assert_eq!(diff.stats().lines_deleted, 0);

        // Get token changes
        let modified: Vec<_> = diff.modified_lines().collect();
        let token_changes = modified[0].token_changes();

        // Should have insertions for ", c"
        let insertions: Vec<_> = token_changes.iter().filter(|tc| tc.is_inserted()).collect();
        assert!(!insertions.is_empty(), "Expected insertions for ', c'");

        // Most tokens should be unchanged
        let unchanged_count = token_changes.iter().filter(|tc| tc.is_unchanged()).count();
        assert!(unchanged_count > insertions.len(), "Most tokens should be unchanged");
    }

    #[test]
    fn test_compute_token_changes_single_token_change() {
        let line1 = SemanticLine::new(b"let x = 1;\n", 1);
        let line2 = SemanticLine::new(b"let x = 2;\n", 1);

        let changes = compute_token_changes(&line1, &line2, &WordDiffConfig::default());

        // Most tokens unchanged, one replaced (1 -> 2)
        let replaced: Vec<_> = changes.iter().filter(|tc| tc.is_replaced()).collect();
        assert!(!replaced.is_empty());

        // Verify it's the number that changed
        let has_num_change = replaced.iter().any(|tc| {
            if let TokenChange::Replaced {
                old_token,
                new_token,
                ..
            } = tc
            {
                old_token.as_str() == "1" && new_token.as_str() == "2"
            } else {
                false
            }
        });
        assert!(has_num_change);
    }
}
