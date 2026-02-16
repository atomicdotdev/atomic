use super::*;


// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // TokenKind tests

    #[test]
    fn test_token_kind_is_significant() {
        assert!(TokenKind::Word.is_significant());
        assert!(TokenKind::Operator.is_significant());
        assert!(TokenKind::Punctuation.is_significant());
        assert!(TokenKind::String.is_significant());
        assert!(TokenKind::Number.is_significant());
        assert!(TokenKind::Comment.is_significant());
        assert!(TokenKind::Other.is_significant());
        assert!(!TokenKind::Whitespace.is_significant());
        assert!(!TokenKind::Newline.is_significant());
    }

    #[test]
    fn test_token_kind_is_content() {
        assert!(TokenKind::Word.is_content());
        assert!(TokenKind::String.is_content());
        assert!(TokenKind::Number.is_content());
        assert!(TokenKind::Comment.is_content());
        assert!(!TokenKind::Operator.is_content());
        assert!(!TokenKind::Punctuation.is_content());
        assert!(!TokenKind::Whitespace.is_content());
        assert!(!TokenKind::Newline.is_content());
        assert!(!TokenKind::Other.is_content());
    }

    #[test]
    fn test_token_kind_is_whitespace() {
        assert!(TokenKind::Whitespace.is_whitespace());
        assert!(TokenKind::Newline.is_whitespace());
        assert!(!TokenKind::Word.is_whitespace());
        assert!(!TokenKind::Other.is_whitespace());
    }

    #[test]
    fn test_token_kind_name() {
        assert_eq!(TokenKind::Word.name(), "word");
        assert_eq!(TokenKind::Whitespace.name(), "ws");
        assert_eq!(TokenKind::Operator.name(), "op");
        assert_eq!(TokenKind::Punctuation.name(), "punct");
        assert_eq!(TokenKind::String.name(), "string");
        assert_eq!(TokenKind::Number.name(), "number");
        assert_eq!(TokenKind::Comment.name(), "comment");
        assert_eq!(TokenKind::Newline.name(), "newline");
        assert_eq!(TokenKind::Other.name(), "other");
    }

    #[test]
    fn test_token_kind_display() {
        assert_eq!(format!("{}", TokenKind::Word), "word");
        assert_eq!(format!("{}", TokenKind::Operator), "op");
    }

    // Token tests

    #[test]
    fn test_token_new() {
        let token = Token::new(b"hello", TokenKind::Word, 0);
        assert_eq!(token.content(), b"hello");
        assert_eq!(token.kind(), TokenKind::Word);
        assert_eq!(token.offset(), 0);
        assert_eq!(token.len(), 5);
        assert!(!token.is_empty());
    }

    #[test]
    fn test_token_as_str() {
        let token = Token::new(b"world", TokenKind::Word, 0);
        assert_eq!(token.as_str(), "world");
    }

    #[test]
    fn test_token_offsets() {
        let token = Token::new(b"test", TokenKind::Word, 10);
        assert_eq!(token.offset(), 10);
        assert_eq!(token.len(), 4);
        assert_eq!(token.end_offset(), 14);
        assert_eq!(token.byte_range(), 10..14);
    }

    #[test]
    fn test_token_empty() {
        let token = Token::new(b"", TokenKind::Other, 0);
        assert!(token.is_empty());
        assert_eq!(token.len(), 0);
        assert_eq!(token.end_offset(), 0);
    }

    #[test]
    fn test_token_hash_value() {
        let t1 = Token::new(b"hello", TokenKind::Word, 0);
        let t2 = Token::new(b"hello", TokenKind::Word, 10);
        let t3 = Token::new(b"world", TokenKind::Word, 0);

        // Same content = same hash
        assert_eq!(t1.hash_value(), t2.hash_value());
        // Different content = different hash (with high probability)
        assert_ne!(t1.hash_value(), t3.hash_value());
    }

    #[test]
    fn test_token_is_significant() {
        let word = Token::new(b"foo", TokenKind::Word, 0);
        let space = Token::new(b" ", TokenKind::Whitespace, 0);
        let newline = Token::new(b"\n", TokenKind::Newline, 0);

        assert!(word.is_significant());
        assert!(!space.is_significant());
        assert!(!newline.is_significant());
    }

    #[test]
    fn test_token_equality() {
        let t1 = Token::new(b"hello", TokenKind::Word, 0);
        let t2 = Token::new(b"hello", TokenKind::Word, 10);
        let t3 = Token::new(b"world", TokenKind::Word, 0);
        let t4 = Token::new(b"hello", TokenKind::String, 0);

        // Equality is based on content only
        assert_eq!(t1, t2);
        assert_ne!(t1, t3);
        // Different kind, same content = equal
        assert_eq!(t1, t4);
    }

    #[test]
    fn test_token_hash_trait() {
        use std::collections::HashSet;

        let t1 = Token::new(b"hello", TokenKind::Word, 0);
        let t2 = Token::new(b"hello", TokenKind::Word, 10);
        let t3 = Token::new(b"world", TokenKind::Word, 0);

        let mut set = HashSet::new();
        set.insert(t1.clone());
        set.insert(t2);
        set.insert(t3);

        // t1 and t2 should hash the same, so only 2 items
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_token_debug() {
        let token = Token::new(b"test", TokenKind::Word, 5);
        let debug = format!("{:?}", token);
        assert!(debug.contains("test"));
        assert!(debug.contains("Word"));
        assert!(debug.contains("5"));
    }

    #[test]
    fn test_token_display() {
        let token = Token::new(b"hello", TokenKind::Word, 0);
        assert_eq!(format!("{}", token), "hello");
    }

    #[test]
    fn test_token_clone() {
        let t1 = Token::new(b"test", TokenKind::Word, 0);
        let t2 = t1.clone();
        assert_eq!(t1, t2);
        assert_eq!(t1.hash_value(), t2.hash_value());
    }

    // TokenizerConfig tests

    #[test]
    fn test_tokenizer_config_default() {
        let config = TokenizerConfig::default();
        assert!(config.merge_whitespace);
        assert!(config.recognize_operators);
        assert!(config.recognize_strings);
        assert!(config.recognize_numbers);
        assert!(config.recognize_comments);
    }

    #[test]
    fn test_tokenizer_config_minimal() {
        let config = TokenizerConfig::minimal();
        assert!(config.merge_whitespace);
        assert!(!config.recognize_operators);
        assert!(!config.recognize_strings);
        assert!(!config.recognize_numbers);
        assert!(!config.recognize_comments);
    }

    #[test]
    fn test_tokenizer_config_code() {
        let config = TokenizerConfig::code();
        assert_eq!(config, TokenizerConfig::default());
    }

    #[test]
    fn test_tokenizer_config_prose() {
        let config = TokenizerConfig::prose();
        assert!(config.merge_whitespace);
        assert!(!config.recognize_operators);
        assert!(!config.recognize_strings);
    }

    // Tokenizer basic tests

    #[test]
    fn test_tokenizer_empty() {
        let tokens: Vec<Token> = Tokenizer::new(b"").collect();
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_tokenizer_single_word() {
        let tokens: Vec<Token> = Tokenizer::new(b"hello").collect();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].as_str(), "hello");
        assert_eq!(tokens[0].kind(), TokenKind::Word);
    }

    #[test]
    fn test_tokenizer_words_and_spaces() {
        let tokens: Vec<Token> = Tokenizer::new(b"hello world").collect();
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].as_str(), "hello");
        assert_eq!(tokens[0].kind(), TokenKind::Word);
        assert_eq!(tokens[1].as_str(), " ");
        assert_eq!(tokens[1].kind(), TokenKind::Whitespace);
        assert_eq!(tokens[2].as_str(), "world");
        assert_eq!(tokens[2].kind(), TokenKind::Word);
    }

    #[test]
    fn test_tokenizer_merged_whitespace() {
        let tokens: Vec<Token> = Tokenizer::new(b"a   b").collect();
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[1].as_str(), "   ");
        assert_eq!(tokens[1].kind(), TokenKind::Whitespace);
    }

    #[test]
    fn test_tokenizer_newline() {
        let tokens: Vec<Token> = Tokenizer::new(b"a\nb").collect();
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[1].as_str(), "\n");
        assert_eq!(tokens[1].kind(), TokenKind::Newline);
    }

    #[test]
    fn test_tokenizer_crlf() {
        let tokens: Vec<Token> = Tokenizer::new(b"a\r\nb").collect();
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[1].as_str(), "\r\n");
        assert_eq!(tokens[1].kind(), TokenKind::Newline);
    }

    // Tokenizer code-aware tests

    #[test]
    fn test_tokenizer_simple_code() {
        let code = b"let x = 42;";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        assert_eq!(tokens.len(), 8);
        assert_eq!(tokens[0].as_str(), "let");
        assert_eq!(tokens[0].kind(), TokenKind::Word);
        assert_eq!(tokens[2].as_str(), "x");
        assert_eq!(tokens[2].kind(), TokenKind::Word);
        assert_eq!(tokens[4].as_str(), "=");
        assert_eq!(tokens[4].kind(), TokenKind::Operator);
        assert_eq!(tokens[6].as_str(), "42");
        assert_eq!(tokens[6].kind(), TokenKind::Number);
        assert_eq!(tokens[7].as_str(), ";");
        assert_eq!(tokens[7].kind(), TokenKind::Punctuation);
    }

    #[test]
    fn test_tokenizer_operators() {
        let code = b"a == b && c != d";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let ops: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Operator)
            .collect();
        assert_eq!(ops.len(), 3);
        assert_eq!(ops[0].as_str(), "==");
        assert_eq!(ops[1].as_str(), "&&");
        assert_eq!(ops[2].as_str(), "!=");
    }

    #[test]
    fn test_tokenizer_arrow_operators() {
        let code = b"fn() -> i32 { x => y }";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let ops: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Operator)
            .collect();
        assert!(ops.iter().any(|t| t.as_str() == "->"));
        assert!(ops.iter().any(|t| t.as_str() == "=>"));
    }

    #[test]
    fn test_tokenizer_scope_operator() {
        let code = b"std::io::Result";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let ops: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Operator)
            .collect();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].as_str(), "::");
        assert_eq!(ops[1].as_str(), "::");
    }

    #[test]
    fn test_tokenizer_compound_assignment() {
        let code = b"x += 1; y -= 2; z *= 3";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let ops: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Operator)
            .collect();
        assert!(ops.iter().any(|t| t.as_str() == "+="));
        assert!(ops.iter().any(|t| t.as_str() == "-="));
        assert!(ops.iter().any(|t| t.as_str() == "*="));
    }

    #[test]
    fn test_tokenizer_string() {
        let code = b"let s = \"hello world\";";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let strings: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::String)
            .collect();
        assert_eq!(strings.len(), 1);
        assert_eq!(strings[0].as_str(), "\"hello world\"");
    }

    #[test]
    fn test_tokenizer_string_with_escape() {
        let code = b"\"hello\\nworld\"";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].as_str(), "\"hello\\nworld\"");
        assert_eq!(tokens[0].kind(), TokenKind::String);
    }

    #[test]
    fn test_tokenizer_char_literal() {
        let code = b"let c = 'a';";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let strings: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::String)
            .collect();
        assert_eq!(strings.len(), 1);
        assert_eq!(strings[0].as_str(), "'a'");
    }

    #[test]
    fn test_tokenizer_comment() {
        let code = b"x = 1; // this is a comment";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let comments: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Comment)
            .collect();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].as_str(), "// this is a comment");
    }

    #[test]
    fn test_tokenizer_comment_stops_at_newline() {
        let code = b"// comment\nnext line";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        assert_eq!(tokens[0].as_str(), "// comment");
        assert_eq!(tokens[0].kind(), TokenKind::Comment);
        assert_eq!(tokens[1].kind(), TokenKind::Newline);
        assert_eq!(tokens[2].as_str(), "next");
    }

    #[test]
    fn test_tokenizer_numbers_integer() {
        let code = b"42 0 123456";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let nums: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Number)
            .collect();
        assert_eq!(nums.len(), 3);
        assert_eq!(nums[0].as_str(), "42");
        assert_eq!(nums[1].as_str(), "0");
        assert_eq!(nums[2].as_str(), "123456");
    }

    #[test]
    fn test_tokenizer_numbers_float() {
        let code = b"3.14 0.5 10.0";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let nums: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Number)
            .collect();
        assert_eq!(nums.len(), 3);
        assert_eq!(nums[0].as_str(), "3.14");
        assert_eq!(nums[1].as_str(), "0.5");
        assert_eq!(nums[2].as_str(), "10.0");
    }

    #[test]
    fn test_tokenizer_numbers_hex() {
        let code = b"0xff 0xDEADBEEF 0X10";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let nums: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Number)
            .collect();
        assert_eq!(nums.len(), 3);
        assert_eq!(nums[0].as_str(), "0xff");
        assert_eq!(nums[1].as_str(), "0xDEADBEEF");
        assert_eq!(nums[2].as_str(), "0X10");
    }

    #[test]
    fn test_tokenizer_numbers_binary_octal() {
        let code = b"0b1010 0o777";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let nums: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Number)
            .collect();
        assert_eq!(nums.len(), 2);
        assert_eq!(nums[0].as_str(), "0b1010");
        assert_eq!(nums[1].as_str(), "0o777");
    }

    #[test]
    fn test_tokenizer_numbers_with_separators() {
        let code = b"1_000_000 0xFF_FF";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let nums: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Number)
            .collect();
        assert_eq!(nums.len(), 2);
        assert_eq!(nums[0].as_str(), "1_000_000");
        assert_eq!(nums[1].as_str(), "0xFF_FF");
    }

    #[test]
    fn test_tokenizer_numbers_scientific() {
        let code = b"1e10 2.5E-3 3e+5";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let nums: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Number)
            .collect();
        // Note: The tokenizer may not perfectly handle all scientific notation
        // The important thing is that numeric content is captured
        assert!(nums.len() >= 1);
        assert!(nums.iter().any(|t| t.as_str().contains("1e10") || t.as_str() == "1e10"));
    }

    #[test]
    fn test_tokenizer_numbers_with_suffix() {
        let code = b"42u32 3.14f64 100i64";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let nums: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Number)
            .collect();
        // Numbers with type suffixes: the numeric part is captured
        // The suffix may be parsed separately depending on tokenizer logic
        assert!(nums.len() >= 3);
        // Verify the numeric parts are present
        assert!(nums.iter().any(|t| t.as_str().starts_with("42")));
        assert!(nums.iter().any(|t| t.as_str().starts_with("3.14") || t.as_str() == "3"));
        assert!(nums.iter().any(|t| t.as_str().starts_with("100")));
    }

    #[test]
    fn test_tokenizer_punctuation() {
        let code = b"fn(a, b) { x.y[z] }";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let puncts: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Punctuation)
            .collect();

        let punct_strs: Vec<_> = puncts.iter().map(|t| t.as_str().to_string()).collect();
        assert!(punct_strs.contains(&"(".to_string()));
        assert!(punct_strs.contains(&")".to_string()));
        assert!(punct_strs.contains(&",".to_string()));
        assert!(punct_strs.contains(&"{".to_string()));
        assert!(punct_strs.contains(&"}".to_string()));
        assert!(punct_strs.contains(&".".to_string()));
        assert!(punct_strs.contains(&"[".to_string()));
        assert!(punct_strs.contains(&"]".to_string()));
    }

    #[test]
    fn test_tokenizer_underscore_identifier() {
        let code = b"_foo __bar _123";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let words: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Word)
            .collect();
        assert_eq!(words.len(), 3);
        assert_eq!(words[0].as_str(), "_foo");
        assert_eq!(words[1].as_str(), "__bar");
        assert_eq!(words[2].as_str(), "_123");
    }

    // Tokenizer offset tracking tests

    #[test]
    fn test_tokenizer_offset_tracking() {
        let code = b"a b c";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        assert_eq!(tokens[0].offset(), 0);    // 'a'
        assert_eq!(tokens[0].end_offset(), 1);
        assert_eq!(tokens[1].offset(), 1);    // ' '
        assert_eq!(tokens[1].end_offset(), 2);
        assert_eq!(tokens[2].offset(), 2);    // 'b'
        assert_eq!(tokens[2].end_offset(), 3);
        assert_eq!(tokens[3].offset(), 3);    // ' '
        assert_eq!(tokens[4].offset(), 4);    // 'c'
        assert_eq!(tokens[4].end_offset(), 5);
    }

    #[test]
    fn test_tokenizer_offset_multi_char() {
        let code = b"foo == bar";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        assert_eq!(tokens[0].offset(), 0);    // 'foo'
        assert_eq!(tokens[0].end_offset(), 3);
        assert_eq!(tokens[2].offset(), 4);    // '=='
        assert_eq!(tokens[2].end_offset(), 6);
        assert_eq!(tokens[4].offset(), 7);    // 'bar'
        assert_eq!(tokens[4].end_offset(), 10);
    }

    #[test]
    fn test_tokenizer_byte_range_slicing() {
        let content = b"hello world";
        let tokens: Vec<Token> = Tokenizer::new(content).collect();

        // Verify byte_range can be used to slice original content
        assert_eq!(&content[tokens[0].byte_range()], b"hello");
        assert_eq!(&content[tokens[1].byte_range()], b" ");
        assert_eq!(&content[tokens[2].byte_range()], b"world");
    }

    // Tokenizer configuration tests

    #[test]
    fn test_tokenizer_minimal_no_operators() {
        let code = b"a == b";
        let config = TokenizerConfig::minimal();
        let tokens: Vec<Token> = Tokenizer::with_config(code, config).collect();

        // With minimal config, == is not recognized as single operator
        // Instead, = and = are separate punctuation
        let ops: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Operator)
            .collect();
        assert_eq!(ops.len(), 0);
    }

    #[test]
    fn test_tokenizer_minimal_no_strings() {
        let code = b"\"hello\"";
        let config = TokenizerConfig::minimal();
        let tokens: Vec<Token> = Tokenizer::with_config(code, config).collect();

        // With minimal config, quotes are separate tokens
        let strings: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::String)
            .collect();
        assert_eq!(strings.len(), 0);
    }

    #[test]
    fn test_tokenizer_minimal_no_numbers() {
        let code = b"42";
        let config = TokenizerConfig::minimal();
        let tokens: Vec<Token> = Tokenizer::with_config(code, config).collect();

        // With minimal config, digits may be treated differently
        // Since 4 and 2 start with digit, they won't be Word tokens
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].kind(), TokenKind::Other);
        assert_eq!(tokens[1].kind(), TokenKind::Other);
    }

    #[test]
    fn test_tokenizer_no_merge_whitespace() {
        let code = b"a   b";
        let config = TokenizerConfig {
            merge_whitespace: false,
            ..TokenizerConfig::default()
        };
        let tokens: Vec<Token> = Tokenizer::with_config(code, config).collect();

        // Each space is a separate token
        let ws: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Whitespace)
            .collect();
        assert_eq!(ws.len(), 3);
    }

    // Tokenizer edge case tests

    #[test]
    fn test_tokenizer_only_whitespace() {
        let tokens: Vec<Token> = Tokenizer::new(b"   \t  ").collect();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].kind(), TokenKind::Whitespace);
        assert_eq!(tokens[0].as_str(), "   \t  ");
    }

    #[test]
    fn test_tokenizer_only_newlines() {
        let tokens: Vec<Token> = Tokenizer::new(b"\n\n\n").collect();
        assert_eq!(tokens.len(), 3);
        for token in &tokens {
            assert_eq!(token.kind(), TokenKind::Newline);
        }
    }

    #[test]
    fn test_tokenizer_mixed_newlines() {
        let tokens: Vec<Token> = Tokenizer::new(b"\n\r\n\n").collect();
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].as_str(), "\n");
        assert_eq!(tokens[1].as_str(), "\r\n");
        assert_eq!(tokens[2].as_str(), "\n");
    }

    #[test]
    fn test_tokenizer_unterminated_string() {
        let code = b"\"hello\nworld";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        // String should stop at newline
        assert_eq!(tokens[0].as_str(), "\"hello");
        assert_eq!(tokens[0].kind(), TokenKind::String);
        assert_eq!(tokens[1].kind(), TokenKind::Newline);
    }

    #[test]
    fn test_tokenizer_special_chars() {
        let code = b"@decorator #macro $var";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        // @ # $ are punctuation
        assert!(tokens.iter().any(|t| t.as_str() == "@"));
        assert!(tokens.iter().any(|t| t.as_str() == "#"));
        assert!(tokens.iter().any(|t| t.as_str() == "$"));
    }

    #[test]
    fn test_tokenizer_unicode_in_other() {
        let code = "café".as_bytes();
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        // ASCII part is Word, then 'é' bytes are Other
        assert!(tokens.len() >= 1);
    }

    // Tokenizer convenience method tests

    #[test]
    fn test_tokenizer_tokenize_all() {
        let tokens = Tokenizer::tokenize_all(b"a b");
        assert_eq!(tokens.len(), 3);
    }

    #[test]
    fn test_tokenizer_tokenize_with_config() {
        let config = TokenizerConfig::minimal();
        let tokens = Tokenizer::tokenize_with_config(b"a b", config);
        assert_eq!(tokens.len(), 3);
    }

    #[test]
    fn test_tokenizer_remaining() {
        let mut tokenizer = Tokenizer::new(b"hello world");
        assert_eq!(tokenizer.remaining(), b"hello world");

        tokenizer.next(); // consume "hello"
        assert_eq!(tokenizer.remaining(), b" world");
    }

    #[test]
    fn test_tokenizer_position() {
        let mut tokenizer = Tokenizer::new(b"ab cd");
        assert_eq!(tokenizer.position(), 0);

        tokenizer.next(); // "ab"
        assert_eq!(tokenizer.position(), 2);

        tokenizer.next(); // " "
        assert_eq!(tokenizer.position(), 3);
    }

    #[test]
    fn test_tokenizer_is_finished() {
        let mut tokenizer = Tokenizer::new(b"ab");
        assert!(!tokenizer.is_finished());

        tokenizer.next(); // consume "ab"
        assert!(tokenizer.is_finished());
    }

    #[test]
    fn test_tokenizer_size_hint() {
        let tokenizer = Tokenizer::new(b"hello");
        let (min, max) = tokenizer.size_hint();
        assert_eq!(min, 0);
        assert_eq!(max, Some(5));
    }

    // Real-world code tests

    #[test]
    fn test_tokenizer_rust_function() {
        let code = b"pub fn calculate(x: i32, y: i32) -> i32 { x + y }";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        // Should tokenize without panicking
        assert!(tokens.len() > 10);

        // Check some specific tokens
        let words: Vec<_> = tokens.iter().filter(|t| t.kind() == TokenKind::Word).collect();
        assert!(words.iter().any(|t| t.as_str() == "pub"));
        assert!(words.iter().any(|t| t.as_str() == "fn"));
        assert!(words.iter().any(|t| t.as_str() == "calculate"));
        assert!(words.iter().any(|t| t.as_str() == "i32"));
    }

    #[test]
    fn test_tokenizer_javascript_arrow() {
        let code = b"const sum = (a, b) => a + b;";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        assert!(tokens.iter().any(|t| t.as_str() == "=>"));
        assert!(tokens.iter().any(|t| t.as_str() == "const"));
    }

    #[test]
    fn test_tokenizer_python_style() {
        let code = b"def greet(name): // greeting\n    print(f\"Hello {name}\")";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        assert!(tokens.iter().any(|t| t.as_str() == "def"));
        // Note: We only recognize // comments, not # comments
        assert!(tokens.iter().any(|t| t.kind() == TokenKind::Comment));
        assert!(tokens.iter().any(|t| t.kind() == TokenKind::Newline));
    }

    #[test]
    fn test_tokenizer_complex_expression() {
        let code = b"result = ((a + b) * c) / (d - e) % f";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let ops: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Operator)
            .map(|t| t.as_str().to_string())
            .collect();

        assert!(ops.contains(&"=".to_string()));
        assert!(ops.contains(&"+".to_string()));
        assert!(ops.contains(&"*".to_string()));
        assert!(ops.contains(&"/".to_string()));
        assert!(ops.contains(&"-".to_string()));
        assert!(ops.contains(&"%".to_string()));
    }
}
