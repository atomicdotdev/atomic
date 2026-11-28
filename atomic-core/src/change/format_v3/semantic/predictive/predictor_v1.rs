//! Frozen portable predictor 1. Only ASCII byte rules are used. Do not replace
//! this with Tokenizer or Unicode tables; changes require a new predictor ID.
use crate::diff::token::TokenKind;

fn first(b: &[u8]) -> usize {
    if b.is_empty() {
        return 0;
    }
    let c = b[0];
    if c == b'\r' {
        return if b.get(1) == Some(&b'\n') { 2 } else { 1 };
    }
    if c == b'\n' {
        return 1;
    }
    if matches!(c, b' ' | b'\t') {
        return b.iter().take_while(|c| matches!(c, b' ' | b'\t')).count();
    }
    if b.starts_with(b"//") {
        return b
            .iter()
            .position(|c| matches!(c, b'\r' | b'\n'))
            .unwrap_or(b.len());
    }
    if matches!(c, b'\'' | b'"') {
        let mut i = 1;
        while i < b.len() {
            match b[i] {
                v if v == c => return i + 1,
                b'\\' => i = (i + 2).min(b.len()),
                b'\r' | b'\n' => return i,
                _ => i += 1,
            }
        }
        return i;
    }
    if c.is_ascii_digit() {
        let mut i = 0;
        if c == b'0' {
            i = 1;
            if b.get(i)
                .is_some_and(|c| matches!(c, b'x' | b'X' | b'o' | b'O' | b'b' | b'B'))
            {
                i += 1;
            }
        }
        while b
            .get(i)
            .is_some_and(|c| c.is_ascii_hexdigit() || *c == b'_')
        {
            i += 1;
        }
        if b.get(i) == Some(&b'.') && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
            i += 1;
            while b.get(i).is_some_and(|c| c.is_ascii_digit() || *c == b'_') {
                i += 1;
            }
        }
        if b.get(i).is_some_and(|c| matches!(c, b'e' | b'E')) {
            i += 1;
            if b.get(i).is_some_and(|c| matches!(c, b'+' | b'-')) {
                i += 1;
            }
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        }
        while b.get(i).is_some_and(u8::is_ascii_alphabetic) {
            i += 1;
        }
        return i;
    }
    if c.is_ascii_alphabetic() || c == b'_' {
        return b
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_')
            .count();
    }
    if b.len() >= 2
        && matches!(
            (c, b[1]),
            (b'=', b'=')
                | (b'!', b'=')
                | (b'<', b'=')
                | (b'>', b'=')
                | (b'&', b'&')
                | (b'|', b'|')
                | (b'+', b'+')
                | (b'-', b'-')
                | (b'<', b'<')
                | (b'>', b'>')
                | (b'-', b'>')
                | (b'=', b'>')
                | (b':', b':')
                | (b'+', b'=')
                | (b'-', b'=')
                | (b'*', b'=')
                | (b'/', b'=')
                | (b'%', b'=')
                | (b'&', b'=')
                | (b'|', b'=')
                | (b'^', b'=')
        )
    {
        return if b.len() > 2 && matches!((c, b[1], b[2]), (b'<', b'<', b'=') | (b'>', b'>', b'='))
        {
            3
        } else {
            2
        };
    }
    1
}
pub(super) fn length(bytes: &[u8]) -> usize {
    let cap = bytes.len().min(4096);
    let mut width = cap.min(16);
    loop {
        let window = &bytes[..width];
        let ascii = window.iter().position(|c| !c.is_ascii()).unwrap_or(width);
        if ascii == 0 {
            return usize::from(!bytes.is_empty());
        }
        let n = first(&window[..ascii]);
        if n < ascii || ascii < width || width == cap {
            return n;
        }
        width = (width * 2).min(cap);
    }
}
pub(super) fn kind(b: &[u8]) -> TokenKind {
    use TokenKind::*;
    if matches!(b, b"\n" | b"\r\n" | b"\r") {
        Newline
    } else if !b.is_empty() && b.iter().all(u8::is_ascii_whitespace) {
        Whitespace
    } else if b.starts_with(b"//") {
        Comment
    } else if b.len() >= 2 && matches!(b[0], b'\'' | b'"') && b.last() == b.first() {
        String
    } else if b.first().is_some_and(u8::is_ascii_digit) {
        Number
    } else if b.len() == 1 && b"(){}[];,.".contains(&b[0]) {
        Punctuation
    } else if !b.is_empty() && b.iter().all(|c| b"+-*/%=!<>|&^~?:@".contains(c)) {
        Operator
    } else if b
        .first()
        .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
    {
        Word
    } else {
        Other
    }
}
pub(super) fn code(kind: TokenKind) -> u64 {
    match kind {
        TokenKind::Word => 0,
        TokenKind::Whitespace => 1,
        TokenKind::Operator => 2,
        TokenKind::Punctuation => 3,
        TokenKind::String => 4,
        TokenKind::Number => 5,
        TokenKind::Comment => 6,
        TokenKind::Newline => 7,
        TokenKind::Other => 8,
    }
}
pub(super) fn from_code(n: u64) -> Option<TokenKind> {
    Some(match n {
        0 => TokenKind::Word,
        1 => TokenKind::Whitespace,
        2 => TokenKind::Operator,
        3 => TokenKind::Punctuation,
        4 => TokenKind::String,
        5 => TokenKind::Number,
        6 => TokenKind::Comment,
        7 => TokenKind::Newline,
        8 => TokenKind::Other,
        _ => return None,
    })
}
