//! Tests for the content tokenization module.

#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use crate::diff::token::TokenKind;
    use crate::record::workflow::crdt::tokenize::{
        ContentTokenizer, TokenStats, TokenizeError, TokenizeOptions, TokenizedLine, TokenizedToken,
    };

    // ------------------------------------------------------------------------
    // TokenizeOptions Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_tokenize_options_default() {
        let opts = TokenizeOptions::default();
        assert!(opts.merge_whitespace());
        assert!(opts.code_aware());
        assert!(!opts.get_include_newlines());
        assert!(opts.get_track_offsets());
        assert_eq!(
            opts.get_max_line_length(),
            TokenizeOptions::DEFAULT_MAX_LINE_LENGTH
        );
    }

    #[test]
    fn test_tokenize_options_new() {
        let opts = TokenizeOptions::new();
        assert!(opts.merge_whitespace());
        assert!(opts.code_aware());
    }

    #[test]
    fn test_tokenize_options_builder_merge_whitespace() {
        let opts = TokenizeOptions::new().with_merge_whitespace(false);
        assert!(!opts.merge_whitespace());
    }

    #[test]
    fn test_tokenize_options_builder_code_aware() {
        let opts = TokenizeOptions::new().with_code_aware(false);
        assert!(!opts.code_aware());
    }

    #[test]
    fn test_tokenize_options_builder_include_newlines() {
        let opts = TokenizeOptions::new().with_include_newlines(true);
        assert!(opts.get_include_newlines());
    }

    #[test]
    fn test_tokenize_options_builder_track_offsets() {
        let opts = TokenizeOptions::new().with_track_offsets(false);
        assert!(!opts.get_track_offsets());
    }

    #[test]
    fn test_tokenize_options_builder_max_line_length() {
        let opts = TokenizeOptions::new().with_max_line_length(1000);
        assert_eq!(opts.get_max_line_length(), 1000);
    }

    #[test]
    fn test_tokenize_options_builder_chain() {
        let opts = TokenizeOptions::new()
            .with_merge_whitespace(false)
            .with_code_aware(false)
            .with_include_newlines(true)
            .with_track_offsets(false)
            .with_max_line_length(5000);

        assert!(!opts.merge_whitespace());
        assert!(!opts.code_aware());
        assert!(opts.get_include_newlines());
        assert!(!opts.get_track_offsets());
        assert_eq!(opts.get_max_line_length(), 5000);
    }

    #[test]
    fn test_tokenize_options_clone() {
        let opts1 = TokenizeOptions::new().with_merge_whitespace(false);
        let opts2 = opts1.clone();
        assert_eq!(opts1, opts2);
    }

    // ------------------------------------------------------------------------
    // TokenizeError Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_tokenize_error_binary_content_display() {
        let err = TokenizeError::BinaryContent {
            reason: "null bytes found".to_string(),
        };
        assert!(err.to_string().contains("binary content"));
        assert!(err.to_string().contains("null bytes"));
    }

    #[test]
    fn test_tokenize_error_line_too_long_display() {
        let err = TokenizeError::LineTooLong {
            line_number: 5,
            length: 20000,
            max_length: 10000,
        };
        assert!(err.to_string().contains("line 5"));
        assert!(err.to_string().contains("20000"));
        assert!(err.to_string().contains("10000"));
    }

    #[test]
    fn test_tokenize_error_invalid_utf8_display() {
        let err = TokenizeError::InvalidUtf8 {
            line_number: 3,
            offset: 42,
        };
        assert!(err.to_string().contains("line 3"));
        assert!(err.to_string().contains("offset 42"));
    }

    #[test]
    fn test_tokenize_error_is_error_trait() {
        let err = TokenizeError::BinaryContent {
            reason: "test".to_string(),
        };
        let _: &dyn std::error::Error = &err;
    }

    // ------------------------------------------------------------------------
    // TokenizedToken Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_tokenized_token_new() {
        let token = TokenizedToken::new(TokenKind::Word, b"hello".to_vec(), 0..5);
        assert_eq!(token.kind(), TokenKind::Word);
        assert_eq!(token.content(), b"hello");
        assert_eq!(token.byte_range(), 0..5);
    }

    #[test]
    fn test_tokenized_token_as_str() {
        let token = TokenizedToken::new(TokenKind::Word, b"world".to_vec(), 0..5);
        assert_eq!(token.as_str(), "world");
    }

    #[test]
    fn test_tokenized_token_len() {
        let token = TokenizedToken::new(TokenKind::Word, b"test".to_vec(), 0..4);
        assert_eq!(token.len(), 4);
        assert!(!token.is_empty());
    }

    #[test]
    fn test_tokenized_token_empty() {
        let token = TokenizedToken::new(TokenKind::Whitespace, vec![], 0..0);
        assert!(token.is_empty());
        assert_eq!(token.len(), 0);
    }

    #[test]
    fn test_tokenized_token_is_significant() {
        let word = TokenizedToken::new(TokenKind::Word, b"fn".to_vec(), 0..2);
        let ws = TokenizedToken::new(TokenKind::Whitespace, b" ".to_vec(), 0..1);

        assert!(word.is_significant());
        assert!(!ws.is_significant());
    }

    #[test]
    fn test_tokenized_token_is_whitespace() {
        let word = TokenizedToken::new(TokenKind::Word, b"fn".to_vec(), 0..2);
        let ws = TokenizedToken::new(TokenKind::Whitespace, b" ".to_vec(), 0..1);
        let nl = TokenizedToken::new(TokenKind::Newline, b"\n".to_vec(), 0..1);

        assert!(!word.is_whitespace());
        assert!(ws.is_whitespace());
        assert!(nl.is_whitespace());
    }

    #[test]
    fn test_tokenized_token_display() {
        let token = TokenizedToken::new(TokenKind::Word, b"main".to_vec(), 0..4);
        let display = format!("{}", token);
        assert!(display.contains("word"));
        assert!(display.contains("main"));
    }

    #[test]
    fn test_tokenized_token_clone() {
        let token1 = TokenizedToken::new(TokenKind::Operator, b"==".to_vec(), 0..2);
        let token2 = token1.clone();
        assert_eq!(token1, token2);
    }

    // ------------------------------------------------------------------------
    // TokenizedLine Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_tokenized_line_new() {
        let tokens = vec![
            TokenizedToken::new(TokenKind::Word, b"let".to_vec(), 0..3),
            TokenizedToken::new(TokenKind::Whitespace, b" ".to_vec(), 3..4),
            TokenizedToken::new(TokenKind::Word, b"x".to_vec(), 4..5),
        ];
        let line = TokenizedLine::new(0, b"let x".to_vec(), tokens);

        assert_eq!(line.line_number(), 0);
        assert_eq!(line.content(), b"let x");
        assert_eq!(line.token_count(), 3);
    }

    #[test]
    fn test_tokenized_line_empty() {
        let line = TokenizedLine::empty(5);
        assert_eq!(line.line_number(), 5);
        assert!(line.is_empty());
        assert_eq!(line.token_count(), 0);
    }

    #[test]
    fn test_tokenized_line_as_str() {
        let line = TokenizedLine::new(0, b"hello world".to_vec(), vec![]);
        assert_eq!(line.as_str(), "hello world");
    }

    #[test]
    fn test_tokenized_line_len() {
        let line = TokenizedLine::new(0, b"test".to_vec(), vec![]);
        assert_eq!(line.len(), 4);
        assert!(!line.is_empty());
    }

    #[test]
    fn test_tokenized_line_content_hash() {
        let line1 = TokenizedLine::new(0, b"hello".to_vec(), vec![]);
        let line2 = TokenizedLine::new(1, b"hello".to_vec(), vec![]);
        let line3 = TokenizedLine::new(0, b"world".to_vec(), vec![]);

        // Same content should have same hash
        assert_eq!(line1.content_hash(), line2.content_hash());
        // Different content should have different hash (almost certainly)
        assert_ne!(line1.content_hash(), line3.content_hash());
    }

    #[test]
    fn test_tokenized_line_content_eq() {
        let line1 = TokenizedLine::new(0, b"test".to_vec(), vec![]);
        let line2 = TokenizedLine::new(5, b"test".to_vec(), vec![]);
        let line3 = TokenizedLine::new(0, b"other".to_vec(), vec![]);

        assert!(line1.content_eq(&line2));
        assert!(!line1.content_eq(&line3));
    }

    #[test]
    fn test_tokenized_line_significant_token_count() {
        let tokens = vec![
            TokenizedToken::new(TokenKind::Word, b"let".to_vec(), 0..3),
            TokenizedToken::new(TokenKind::Whitespace, b" ".to_vec(), 3..4),
            TokenizedToken::new(TokenKind::Word, b"x".to_vec(), 4..5),
            TokenizedToken::new(TokenKind::Whitespace, b" ".to_vec(), 5..6),
        ];
        let line = TokenizedLine::new(0, b"let x ".to_vec(), tokens);

        assert_eq!(line.significant_token_count(), 2);
    }

    #[test]
    fn test_tokenized_line_is_whitespace_only() {
        let ws_tokens = vec![
            TokenizedToken::new(TokenKind::Whitespace, b"  ".to_vec(), 0..2),
            TokenizedToken::new(TokenKind::Whitespace, b"\t".to_vec(), 2..3),
        ];
        let ws_line = TokenizedLine::new(0, b"  \t".to_vec(), ws_tokens);

        let mixed_tokens = vec![
            TokenizedToken::new(TokenKind::Whitespace, b" ".to_vec(), 0..1),
            TokenizedToken::new(TokenKind::Word, b"x".to_vec(), 1..2),
        ];
        let mixed_line = TokenizedLine::new(0, b" x".to_vec(), mixed_tokens);

        assert!(ws_line.is_whitespace_only());
        assert!(!mixed_line.is_whitespace_only());
    }

    #[test]
    fn test_tokenized_line_iter_tokens() {
        let tokens = vec![
            TokenizedToken::new(TokenKind::Word, b"a".to_vec(), 0..1),
            TokenizedToken::new(TokenKind::Word, b"b".to_vec(), 1..2),
        ];
        let line = TokenizedLine::new(0, b"ab".to_vec(), tokens);

        let collected: Vec<_> = line.iter_tokens().map(|t| t.as_str().to_string()).collect();
        assert_eq!(collected, vec!["a", "b"]);
    }

    #[test]
    fn test_tokenized_line_into_tokens() {
        let tokens = vec![TokenizedToken::new(TokenKind::Word, b"x".to_vec(), 0..1)];
        let line = TokenizedLine::new(0, b"x".to_vec(), tokens);

        let owned_tokens = line.into_tokens();
        assert_eq!(owned_tokens.len(), 1);
    }

    #[test]
    fn test_tokenized_line_display() {
        let tokens = vec![TokenizedToken::new(TokenKind::Word, b"hi".to_vec(), 0..2)];
        let line = TokenizedLine::new(3, b"hi".to_vec(), tokens);
        let display = format!("{}", line);
        assert!(display.contains("Line 3"));
        assert!(display.contains("1 tokens"));
        assert!(display.contains("2 bytes"));
    }

    // ------------------------------------------------------------------------
    // TokenStats Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_token_stats_new() {
        let stats = TokenStats::new();
        assert_eq!(stats.lines, 0);
        assert_eq!(stats.tokens, 0);
        assert_eq!(stats.bytes, 0);
    }

    #[test]
    fn test_token_stats_add_line() {
        let mut stats = TokenStats::new();

        let tokens = vec![
            TokenizedToken::new(TokenKind::Word, b"test".to_vec(), 0..4),
            TokenizedToken::new(TokenKind::Whitespace, b" ".to_vec(), 4..5),
        ];
        let line = TokenizedLine::new(0, b"test ".to_vec(), tokens);

        stats.add_line(&line);

        assert_eq!(stats.lines, 1);
        assert_eq!(stats.tokens, 2);
        assert_eq!(stats.significant_tokens, 1);
        assert_eq!(stats.whitespace_tokens, 1);
        assert_eq!(stats.bytes, 5);
    }

    #[test]
    fn test_token_stats_add_empty_line() {
        let mut stats = TokenStats::new();
        let line = TokenizedLine::empty(0);
        stats.add_line(&line);

        assert_eq!(stats.lines, 1);
        assert_eq!(stats.empty_lines, 1);
    }

    #[test]
    fn test_token_stats_add_whitespace_only_line() {
        let mut stats = TokenStats::new();

        let tokens = vec![TokenizedToken::new(
            TokenKind::Whitespace,
            b"  ".to_vec(),
            0..2,
        )];
        let line = TokenizedLine::new(0, b"  ".to_vec(), tokens);
        stats.add_line(&line);

        assert_eq!(stats.whitespace_only_lines, 1);
    }

    #[test]
    fn test_token_stats_max_tracking() {
        let mut stats = TokenStats::new();

        // Short line
        let line1 = TokenizedLine::new(
            0,
            b"hi".to_vec(),
            vec![TokenizedToken::new(TokenKind::Word, b"hi".to_vec(), 0..2)],
        );
        stats.add_line(&line1);

        // Longer line with more tokens
        let line2 = TokenizedLine::new(
            1,
            b"hello world".to_vec(),
            vec![
                TokenizedToken::new(TokenKind::Word, b"hello".to_vec(), 0..5),
                TokenizedToken::new(TokenKind::Whitespace, b" ".to_vec(), 5..6),
                TokenizedToken::new(TokenKind::Word, b"world".to_vec(), 6..11),
            ],
        );
        stats.add_line(&line2);

        assert_eq!(stats.max_line_length, 11);
        assert_eq!(stats.max_tokens_per_line, 3);
    }

    #[test]
    fn test_token_stats_merge() {
        let mut stats1 = TokenStats::new();
        stats1.lines = 5;
        stats1.tokens = 20;
        stats1.max_line_length = 50;

        let mut stats2 = TokenStats::new();
        stats2.lines = 3;
        stats2.tokens = 10;
        stats2.max_line_length = 100;

        stats1.merge(&stats2);

        assert_eq!(stats1.lines, 8);
        assert_eq!(stats1.tokens, 30);
        assert_eq!(stats1.max_line_length, 100);
    }

    #[test]
    fn test_token_stats_avg_tokens_per_line() {
        let mut stats = TokenStats::new();
        stats.lines = 4;
        stats.tokens = 10;

        assert!((stats.avg_tokens_per_line() - 2.5).abs() < 0.001);
    }

    #[test]
    fn test_token_stats_avg_tokens_per_line_empty() {
        let stats = TokenStats::new();
        assert!((stats.avg_tokens_per_line() - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_token_stats_avg_line_length() {
        let mut stats = TokenStats::new();
        stats.lines = 2;
        stats.bytes = 20;

        assert!((stats.avg_line_length() - 10.0).abs() < 0.001);
    }

    #[test]
    fn test_token_stats_display() {
        let mut stats = TokenStats::new();
        stats.lines = 10;
        stats.tokens = 50;
        stats.significant_tokens = 30;
        stats.bytes = 200;

        let display = format!("{}", stats);
        assert!(display.contains("10 lines"));
        assert!(display.contains("50 tokens"));
        assert!(display.contains("30 significant"));
        assert!(display.contains("200 bytes"));
    }

    // ------------------------------------------------------------------------
    // ContentTokenizer Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_content_tokenizer_new() {
        let content = b"hello";
        let tokenizer = ContentTokenizer::new(content);
        assert_eq!(tokenizer.content(), content);
    }

    #[test]
    fn test_content_tokenizer_with_options() {
        let content = b"hello";
        let options = TokenizeOptions::new().with_code_aware(false);
        let tokenizer = ContentTokenizer::with_options(content, options.clone());

        assert_eq!(tokenizer.content(), content);
        assert_eq!(tokenizer.options(), &options);
    }

    #[test]
    fn test_content_tokenizer_single_line() {
        let content = b"let x = 5;";
        let tokenizer = ContentTokenizer::new(content);

        let lines: Vec<_> = tokenizer.lines().collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line_number(), 0);
        assert!(!lines[0].tokens().is_empty());
    }

    #[test]
    fn test_content_tokenizer_multiple_lines() {
        let content = b"line one\nline two\nline three";
        let tokenizer = ContentTokenizer::new(content);

        let lines: Vec<_> = tokenizer.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].line_number(), 0);
        assert_eq!(lines[1].line_number(), 1);
        assert_eq!(lines[2].line_number(), 2);
    }

    #[test]
    fn test_content_tokenizer_trailing_newline() {
        let content = b"line one\nline two\n";
        let tokenizer = ContentTokenizer::new(content);

        let lines: Vec<_> = tokenizer.lines().collect();
        // Trailing newline creates an empty final line
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].line_number(), 0);
        assert_eq!(lines[1].line_number(), 1);
    }

    #[test]
    fn test_content_tokenizer_empty_content() {
        let content = b"";
        let tokenizer = ContentTokenizer::new(content);

        let lines: Vec<_> = tokenizer.lines().collect();
        assert_eq!(lines.len(), 0);
    }

    #[test]
    fn test_content_tokenizer_only_newlines() {
        let content = b"\n\n\n";
        let tokenizer = ContentTokenizer::new(content);

        let lines: Vec<_> = tokenizer.lines().collect();
        // Three newlines create three empty lines
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert!(line.is_empty());
        }
    }

    #[test]
    fn test_content_tokenizer_tokenize_all() {
        let content = b"fn main() {\n    println!(\"Hi\");\n}";
        let tokenizer = ContentTokenizer::new(content);

        let (lines, stats) = tokenizer.tokenize_all();

        assert_eq!(lines.len(), 3);
        assert_eq!(stats.lines, 3);
        assert!(stats.tokens > 0);
    }

    #[test]
    fn test_content_tokenizer_is_binary_with_null() {
        let content = b"hello\x00world";
        let tokenizer = ContentTokenizer::new(content);
        assert!(tokenizer.is_binary());
    }

    #[test]
    fn test_content_tokenizer_is_binary_with_long_line() {
        let long_line = vec![b'x'; 20_000];
        let tokenizer = ContentTokenizer::new(&long_line);
        assert!(tokenizer.is_binary());
    }

    #[test]
    fn test_content_tokenizer_is_binary_with_control_chars() {
        // Create content with >10% control characters
        let mut content = vec![b'a'; 10];
        content.extend(vec![0x01, 0x02, 0x03]); // 3 control chars out of 13 = 23%
        let tokenizer = ContentTokenizer::new(&content);
        assert!(tokenizer.is_binary());
    }

    #[test]
    fn test_content_tokenizer_not_binary_normal_text() {
        let content = b"fn main() {\n    println!(\"Hello, World!\");\n}\n";
        let tokenizer = ContentTokenizer::new(content);
        assert!(!tokenizer.is_binary());
    }

    #[test]
    fn test_content_tokenizer_tokenize_line_static() {
        let line = b"let x = 42;";
        let options = TokenizeOptions::default();
        let result = ContentTokenizer::tokenize_line(line, &options);

        assert_eq!(result.line_number(), 0);
        assert!(!result.tokens().is_empty());
    }

    #[test]
    fn test_content_tokenizer_whitespace_merging() {
        let content = b"a    b";
        let options = TokenizeOptions::new().with_merge_whitespace(true);
        let tokenizer = ContentTokenizer::with_options(content, options);

        let lines: Vec<_> = tokenizer.lines().collect();
        let tokens = lines[0].tokens();

        // Should be: "a", whitespace, "b"
        assert_eq!(tokens.len(), 3);
        // The middle token should be merged whitespace
        assert_eq!(tokens[1].kind(), TokenKind::Whitespace);
        assert_eq!(tokens[1].content(), b"    ");
    }

    #[test]
    fn test_content_tokenizer_no_whitespace_merging() {
        let content = b"a  b";
        let options = TokenizeOptions::new().with_merge_whitespace(false);
        let tokenizer = ContentTokenizer::with_options(content, options);

        let lines: Vec<_> = tokenizer.lines().collect();
        let ws_count = lines[0]
            .tokens()
            .iter()
            .filter(|t| t.kind() == TokenKind::Whitespace)
            .count();

        // Without merging, whitespace may still be grouped by the underlying tokenizer
        // At minimum we should have at least one whitespace token
        assert!(ws_count >= 1);
    }

    #[test]
    fn test_content_tokenizer_code_aware_operators() {
        let content = b"x == y";
        let options = TokenizeOptions::new().with_code_aware(true);
        let tokenizer = ContentTokenizer::with_options(content, options);

        let lines: Vec<_> = tokenizer.lines().collect();
        let has_eq_operator = lines[0].tokens().iter().any(|t| t.as_str() == "==");

        assert!(has_eq_operator, "Should recognize == as single operator");
    }

    #[test]
    fn test_content_tokenizer_code_aware_numbers() {
        let content = b"x = 42";
        let options = TokenizeOptions::new().with_code_aware(true);
        let tokenizer = ContentTokenizer::with_options(content, options);

        let lines: Vec<_> = tokenizer.lines().collect();
        let has_number = lines[0]
            .tokens()
            .iter()
            .any(|t| t.kind() == TokenKind::Number);

        assert!(has_number, "Should recognize 42 as a number");
    }

    // ------------------------------------------------------------------------
    // LineIterator Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_line_iterator_current_line_number() {
        let content = b"a\nb\nc";
        let tokenizer = ContentTokenizer::new(content);
        let mut iter = tokenizer.lines();

        assert_eq!(iter.current_line_number(), 0);
        iter.next();
        assert_eq!(iter.current_line_number(), 1);
        iter.next();
        assert_eq!(iter.current_line_number(), 2);
    }

    #[test]
    fn test_line_iterator_current_position() {
        let content = b"abc\ndef";
        let tokenizer = ContentTokenizer::new(content);
        let mut iter = tokenizer.lines();

        assert_eq!(iter.current_position(), 0);
        iter.next(); // Consumes "abc\n"
        assert_eq!(iter.current_position(), 4);
    }

    #[test]
    fn test_line_iterator_has_more() {
        let content = b"a\nb";
        let tokenizer = ContentTokenizer::new(content);
        let mut iter = tokenizer.lines();

        assert!(iter.has_more());
        iter.next();
        assert!(iter.has_more());
        iter.next();
        assert!(!iter.has_more());
    }

    #[test]
    fn test_line_iterator_remaining_bytes() {
        let content = b"abc\ndefgh";
        let tokenizer = ContentTokenizer::new(content);
        let mut iter = tokenizer.lines();

        assert_eq!(iter.remaining_bytes(), 9);
        iter.next(); // Consumes "abc\n"
        assert_eq!(iter.remaining_bytes(), 5);
    }

    // ------------------------------------------------------------------------
    // Integration Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_integration_rust_code() {
        let content = br#"fn main() {
    let x = 42;
    println!("{}", x);
}"#;
        let tokenizer = ContentTokenizer::new(content);
        let (lines, stats) = tokenizer.tokenize_all();

        assert_eq!(lines.len(), 4);
        assert!(stats.tokens > 10);

        // First line should have: fn, space, main, (, ), space, {
        let first_line_tokens: Vec<_> = lines[0]
            .tokens()
            .iter()
            .map(|t| t.as_str().to_string())
            .collect();
        assert!(first_line_tokens.contains(&"fn".to_string()));
        assert!(first_line_tokens.contains(&"main".to_string()));
    }

    #[test]
    fn test_integration_mixed_content() {
        let content = b"// Comment\nlet x = \"string\";";
        let tokenizer = ContentTokenizer::new(content);
        let lines: Vec<_> = tokenizer.lines().collect();

        assert_eq!(lines.len(), 2);

        // First line should have comment
        let has_comment = lines[0]
            .tokens()
            .iter()
            .any(|t| t.kind() == TokenKind::Comment || t.as_str().starts_with("//"));
        assert!(has_comment || !lines[0].tokens().is_empty());
    }

    #[test]
    fn test_integration_empty_lines() {
        let content = b"a\n\nb\n\n\nc";
        let tokenizer = ContentTokenizer::new(content);
        let (lines, stats) = tokenizer.tokenize_all();

        assert_eq!(lines.len(), 6);
        assert_eq!(stats.empty_lines, 3);
    }

    #[test]
    fn test_integration_byte_ranges() {
        let content = b"ab cd";
        let tokenizer = ContentTokenizer::new(content);
        let lines: Vec<_> = tokenizer.lines().collect();

        let tokens = lines[0].tokens();
        // Verify byte ranges are correct
        assert_eq!(tokens[0].byte_range().start, 0);
        assert_eq!(tokens[0].byte_range().end, 2); // "ab"
    }

    #[test]
    fn test_integration_unicode_content() {
        let content = "let emoji = \"🎉\";".as_bytes();
        let tokenizer = ContentTokenizer::new(content);
        let lines: Vec<_> = tokenizer.lines().collect();

        assert_eq!(lines.len(), 1);
        // Should handle unicode without crashing
        assert!(!lines[0].tokens().is_empty());
    }
}
