//! Contextual transforms retain original reference values and token kinds.
use super::{control, transforms, View};
use anyhow::{ensure, Context, Result};

fn undelta(input: &mut &[u8], previous: &mut u64) -> Result<u64> {
    let z = control::get(input)?;
    *previous = previous.wrapping_add((z >> 1) ^ 0u64.wrapping_sub(z & 1));
    Ok(*previous)
}
fn delta(out: &mut Vec<u8>, previous: &mut u64, n: u64) {
    let d = n.wrapping_sub(*previous);
    transforms::put(out, (d << 1) ^ ((d as i64 >> 63) as u64));
    *previous = n;
}

pub fn split_references(view: &View<'_>) -> Result<Vec<u8>> {
    let mut input = view.references;
    let mut previous = 0;
    let mut states = [0; 2];
    let mut columns: [Vec<u8>; 2] = std::array::from_fn(|_| vec![]);
    control::visit_bytes(view.control, |_, tag, _| {
        if tag != 0 {
            let n = undelta(&mut input, &mut previous)?;
            let i = (tag - 1) as usize;
            delta(&mut columns[i], &mut states[i], n);
        }
        Ok(())
    })?;
    ensure!(input.is_empty(), "unconsumed original references");
    Ok(postcard::to_allocvec(&columns)?)
}
pub fn join_references(data: &[u8], control: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    ensure!(raw_len <= transforms::LIMIT, "reference limit");
    let (mut columns, rest): ([&[u8]; 2], _) = postcard::take_from_bytes(data)?;
    ensure!(rest.is_empty(), "trailing reference frame");
    let mut previous = 0;
    let mut states = [0; 2];
    let mut out = vec![];
    control::visit_bytes(control, |_, tag, _| {
        if tag != 0 {
            let i = (tag - 1) as usize;
            let n = undelta(&mut columns[i], &mut states[i])?;
            let mut encoded = vec![];
            delta(&mut encoded, &mut previous, n);
            ensure!(
                out.len()
                    .checked_add(encoded.len())
                    .is_some_and(|n| n <= raw_len),
                "expanded reference limit"
            );
            out.extend(encoded);
        }
        Ok(())
    })?;
    ensure!(
        columns.iter().all(|c| c.is_empty()) && out.len() == raw_len,
        "unconsumed references"
    );
    Ok(out)
}

fn token_bytes(
    view: &View<'_>,
    content: &[u8],
    mut visit: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    ensure!(
        view.content_hash == *blake3::hash(content).as_bytes(),
        "predictor content binding mismatch"
    );
    let mut input = view.references;
    let mut previous = 0;
    let mut cursor = 0usize;
    control::visit_bytes(view.control, |has_kind, tag, len| {
        let at = if tag == 0 {
            cursor
        } else {
            usize::try_from(undelta(&mut input, &mut previous)?)?
        };
        let end = at
            .checked_add(usize::try_from(len)?)
            .context("token range overflow")?;
        let pool = if tag == 2 { view.literals } else { content };
        let bytes = pool.get(at..end).context("predictor byte range")?;
        if tag != 2 {
            cursor = end;
        }
        if has_kind {
            visit(bytes)?;
        }
        Ok(())
    })?;
    ensure!(input.is_empty(), "trailing predictor references");
    Ok(())
}

// Frozen experimental predictor: boundaries and IDs are never inferred. Every
// classification that differs from the author's value is stored explicitly.
fn predict(bytes: &[u8]) -> u64 {
    if matches!(bytes, b"\n" | b"\r\n" | b"\r") {
        7
    } else if !bytes.is_empty() && bytes.iter().all(u8::is_ascii_whitespace) {
        1
    } else if bytes.starts_with(b"//") {
        6
    } else if bytes.len() >= 2 && matches!(bytes[0], b'\'' | b'"') && bytes.last() == bytes.first()
    {
        4
    } else if bytes.first().is_some_and(u8::is_ascii_digit) {
        5
    } else if bytes.len() == 1 && b"(){}[];,.".contains(&bytes[0]) {
        3
    } else if !bytes.is_empty() && bytes.iter().all(|b| b"+-*/%=!<>|&^~?:@".contains(b)) {
        2
    } else if std::str::from_utf8(bytes).is_ok_and(|s| {
        s.chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
            && s.chars().all(|c| c.is_alphanumeric() || c == '_')
    }) {
        0
    } else {
        8
    }
}

pub fn predict_kinds(view: &View<'_>, content: &[u8]) -> Result<Vec<u8>> {
    let mut actual = view.kinds;
    let mut mask: Vec<u8> = vec![];
    let mut exceptions = vec![];
    let mut n = 0usize;
    token_bytes(view, content, |bytes| {
        let kind = control::get(&mut actual)?;
        if n.is_multiple_of(8) {
            mask.push(0);
        }
        if kind != predict(bytes) {
            mask[n / 8] |= 1 << (n % 8);
            transforms::put(&mut exceptions, kind);
        }
        n += 1;
        Ok(())
    })?;
    ensure!(actual.is_empty(), "trailing actual token kinds");
    Ok(postcard::to_allocvec(&(
        u32::try_from(n)?,
        mask,
        exceptions,
    ))?)
}

pub fn restore_kinds(
    view: &View<'_>,
    content: &[u8],
    data: &[u8],
    raw_len: usize,
) -> Result<Vec<u8>> {
    ensure!(raw_len <= transforms::LIMIT, "kind limit");
    let ((count, mask, mut exceptions), rest): ((u32, &[u8], &[u8]), _) =
        postcard::take_from_bytes(data)?;
    let count = count as usize;
    ensure!(
        rest.is_empty() && count <= raw_len && mask.len() == count.div_ceil(8),
        "invalid kind prediction frame"
    );
    if !count.is_multiple_of(8) {
        ensure!(
            mask.last().is_some_and(|b| b >> (count % 8) == 0),
            "nonzero kind mask padding"
        );
    }
    let mut out = vec![];
    let mut n = 0;
    token_bytes(view, content, |bytes| {
        ensure!(n < count, "more tokens than kind frame");
        let kind = if mask[n / 8] & (1 << (n % 8)) != 0 {
            control::get(&mut exceptions)?
        } else {
            predict(bytes)
        };
        let mut encoded = vec![];
        transforms::put(&mut encoded, kind);
        ensure!(
            out.len()
                .checked_add(encoded.len())
                .is_some_and(|n| n <= raw_len),
            "expanded kind limit"
        );
        out.extend(encoded);
        n += 1;
        Ok(())
    })?;
    ensure!(
        n == count && out.len() == raw_len && exceptions.is_empty(),
        "kind count/length mismatch"
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::{
        change::{format_v3::semantic, FileOps, LineOps},
        crdt::{BranchId, LeafOp, TrunkId},
        diff::token::TokenKind,
    };
    #[test]
    fn arbitrary_authored_kinds_are_restored_with_exceptions() -> Result<()> {
        let mut file = FileOps::edit(TrunkId::ROOT, "f".into());
        // Deliberately incorrect-looking classifications must remain authored.
        let mut tokens = vec![
            LeafOp::Insert {
                after: None,
                kind: TokenKind::Comment,
                content: b"identifier".to_vec(),
            },
            LeafOp::Insert {
                after: None,
                kind: TokenKind::Number,
                content: b" \n".to_vec(),
            },
            LeafOp::Insert {
                after: None,
                kind: TokenKind::Word,
                content: vec![255],
            },
        ];
        for _ in 0..6 {
            tokens.push(LeafOp::Insert {
                after: None,
                kind: TokenKind::Comment,
                content: b"x".to_vec(),
            });
        }
        file.add_line_op(LineOps::insert(BranchId::ROOT, None, tokens));
        let original = semantic::encode(&[file], b"identifier \n\xff")?;
        let view: View<'_> = postcard::from_bytes(&original[5..])?;
        let data = predict_kinds(&view, b"identifier \n\xff")?;
        assert_eq!(
            restore_kinds(&view, b"identifier \n\xff", &data, view.kinds.len())?,
            view.kinds
        );
        assert!(restore_kinds(&view, b"wrong", &data, view.kinds.len()).is_err());
        let refs = split_references(&view)?;
        assert_eq!(
            join_references(&refs, view.control, view.references.len())?,
            view.references
        );
        for end in 0..data.len() {
            assert!(
                restore_kinds(&view, b"identifier \n\xff", &data[..end], view.kinds.len()).is_err()
            );
        }
        Ok(())
    }
}
