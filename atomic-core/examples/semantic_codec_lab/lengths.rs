//! Predict lengths, never invent boundaries: all disagreements are exceptions.
use super::{control, transforms, View};
use anyhow::{ensure, Context, Result};
use atomic_core::diff::token::Tokenizer;
use serde::{Deserialize, Serialize};

fn predictor_id() -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"lab-ascii-token-length-window4096-v1\0");
    for bytes in [
        include_bytes!("lengths.rs").as_slice(),
        include_bytes!("../../src/diff/token/tokenizer.rs").as_slice(),
        include_bytes!("../../src/diff/token/config.rs").as_slice(),
        include_bytes!("../../src/diff/token/kind.rs").as_slice(),
        include_bytes!("../../src/diff/token/token_type.rs").as_slice(),
        include_bytes!("../../src/diff/token/mod.rs").as_slice(),
    ] {
        h.update(&(bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    }
    *h.finalize().as_bytes()
}
fn predict(bytes: &[u8]) -> usize {
    let cap = bytes.len().min(4096);
    let mut width = cap.min(16);
    loop {
        let window = &bytes[..width];
        let ascii = window
            .iter()
            .position(|b| !b.is_ascii())
            .unwrap_or(window.len());
        if ascii == 0 {
            return usize::from(!bytes.is_empty());
        }
        let length = Tokenizer::new(&window[..ascii])
            .next()
            .map_or(0, |token| token.len());
        if length < ascii || ascii < width || width == cap {
            return length;
        }
        width = (width * 2).min(cap);
    }
}
fn offset(tag: u64, refs: &mut &[u8], previous: &mut u64, cursor: usize) -> Result<usize> {
    if tag == 0 {
        return Ok(cursor);
    }
    let z = control::get(refs)?;
    *previous = previous.wrapping_add((z >> 1) ^ 0u64.wrapping_sub(z & 1));
    Ok(usize::try_from(*previous)?)
}
fn pool<'a>(view: &'a View<'_>, content: &'a [u8], tag: u64) -> &'a [u8] {
    if tag == 2 {
        view.literals
    } else {
        content
    }
}

#[derive(Serialize)]
struct Frame {
    predictor: [u8; 32],
    fields: [Vec<u8>; control::FIELDS],
    count: u32,
    mask: Vec<u8>,
    exceptions: Vec<u8>,
}
#[derive(Deserialize)]
struct Borrowed<'a> {
    predictor: [u8; 32],
    #[serde(borrow)]
    fields: [&'a [u8]; control::FIELDS],
    count: u32,
    #[serde(borrow)]
    mask: &'a [u8],
    #[serde(borrow)]
    exceptions: &'a [u8],
}

pub fn split(view: &View<'_>, content: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        view.content_hash == *blake3::hash(content).as_bytes(),
        "length content mismatch"
    );
    let mut fields = control::fields(view.control)?;
    let mut lengths = fields[control::LENGTH].as_slice();
    let mut tags = fields[control::REFERENCE].as_slice();
    let mut refs = view.references;
    let mut previous = 0;
    let mut cursor = 0usize;
    let mut n = 0usize;
    let mut mask: Vec<u8> = vec![];
    let mut exceptions = vec![];
    while !lengths.is_empty() {
        let len = usize::try_from(control::get(&mut lengths)?)?;
        let tag = control::get(&mut tags)?;
        ensure!(tag <= 2, "length reference tag");
        let at = offset(tag, &mut refs, &mut previous, cursor)?;
        let bytes = pool(view, content, tag);
        let tail = bytes.get(at..).context("length predictor offset")?;
        if n.is_multiple_of(8) {
            mask.push(0);
        }
        if len != predict(tail) {
            mask[n / 8] |= 1 << (n % 8);
            transforms::put(&mut exceptions, len as u64);
        }
        let end = at.checked_add(len).context("length overflow")?;
        ensure!(end <= bytes.len(), "length outside pool");
        if tag != 2 {
            cursor = end;
        }
        n += 1;
    }
    ensure!(
        tags.is_empty() && refs.is_empty(),
        "unconsumed length contexts"
    );
    fields[control::LENGTH].clear();
    Ok(postcard::to_allocvec(&Frame {
        predictor: predictor_id(),
        fields,
        count: n.try_into()?,
        mask,
        exceptions,
    })?)
}

struct Join<'a> {
    frame: Borrowed<'a>,
    view: &'a View<'a>,
    content: &'a [u8],
    refs: &'a [u8],
    previous: u64,
    cursor: usize,
    n: usize,
    tag: Option<u64>,
    output: Vec<u8>,
    limit: usize,
}
impl control::Values for Join<'_> {
    fn value(&mut self, role: usize) -> Result<u64> {
        let value = if role == control::LENGTH {
            ensure!(
                self.n < (self.frame.count as usize),
                "more lengths than predictor mask"
            );
            let tag = self.tag.take().context("length without byte reference")?;
            let at = offset(tag, &mut self.refs, &mut self.previous, self.cursor)?;
            let bytes = pool(self.view, self.content, tag);
            let tail = bytes.get(at..).context("predicted byte offset")?;
            let len = if self.frame.mask[self.n / 8] & (1 << (self.n % 8)) != 0 {
                usize::try_from(control::get(&mut self.frame.exceptions)?)?
            } else {
                predict(tail)
            };
            let end = at
                .checked_add(len)
                .context("predicted byte length overflow")?;
            ensure!(end <= bytes.len(), "predicted bytes outside pool");
            if tag != 2 {
                self.cursor = end;
            }
            self.n += 1;
            len as u64
        } else {
            let n = control::get(&mut self.frame.fields[role])?;
            if role == control::REFERENCE {
                ensure!(n <= 2, "reference tag");
                self.tag = Some(n);
            }
            n
        };
        let len = ((64 - value.leading_zeros()).max(1)).div_ceil(7) as usize;
        ensure!(
            self.output
                .len()
                .checked_add(len)
                .is_some_and(|n| n <= self.limit),
            "expanded predicted control limit"
        );
        transforms::put(&mut self.output, value);
        Ok(value)
    }
}
pub fn join(data: &[u8], view: &View<'_>, content: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    ensure!(raw_len <= transforms::LIMIT, "predicted control limit");
    ensure!(
        view.content_hash == *blake3::hash(content).as_bytes(),
        "length content mismatch"
    );
    let (frame, rest): (Borrowed<'_>, _) = postcard::take_from_bytes(data)?;
    ensure!(
        rest.is_empty() && frame.predictor == predictor_id(),
        "length predictor implementation mismatch or trailing data"
    );
    let count = frame.count as usize;
    ensure!(
        count <= raw_len
            && frame.mask.len() == count.div_ceil(8)
            && frame.fields[control::LENGTH].is_empty(),
        "invalid length prediction frame"
    );
    if !count.is_multiple_of(8) {
        ensure!(
            frame.mask.last().is_some_and(|b| b >> (count % 8) == 0),
            "length mask padding"
        );
    }
    let mut values = Join {
        frame,
        view,
        content,
        refs: view.references,
        previous: 0,
        cursor: 0,
        n: 0,
        tag: None,
        output: vec![],
        limit: raw_len,
    };
    control::walk(&mut values, raw_len, &mut |_, _, _| Ok(()))?;
    ensure!(
        values.frame.fields.iter().all(|c| c.is_empty())
            && values.frame.exceptions.is_empty()
            && values.refs.is_empty()
            && values.n == count
            && values.tag.is_none()
            && values.output.len() == raw_len,
        "unconsumed predicted control"
    );
    Ok(values.output)
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
    fn unusual_boundaries_empty_unicode_and_long_tokens_are_exact() -> Result<()> {
        for (content, parts) in [
            (
                b"foobar  \xc3\xa9".to_vec(),
                vec![
                    b"fo".to_vec(),
                    b"obar".to_vec(),
                    b"  ".to_vec(),
                    vec![],
                    "é".as_bytes().to_vec(),
                ],
            ),
            (vec![b'a'; 5000], vec![vec![b'a'; 5000]]),
        ] {
            let mut file = FileOps::edit(TrunkId::ROOT, "f".into());
            file.add_line_op(LineOps::insert(
                BranchId::ROOT,
                None,
                parts
                    .into_iter()
                    .map(|content| LeafOp::Insert {
                        after: None,
                        kind: TokenKind::Other,
                        content,
                    })
                    .collect(),
            ));
            let raw = semantic::encode(&[file], &content)?;
            let view: View<'_> = postcard::from_bytes(&raw[5..])?;
            let data = split(&view, &content)?;
            assert_eq!(
                join(&data, &view, &content, view.control.len())?,
                view.control
            );
            for end in 0..data.len() {
                assert!(join(&data[..end], &view, &content, view.control.len()).is_err());
            }
            let mut unknown = data;
            unknown[0] ^= 1;
            assert!(join(&unknown, &view, &content, view.control.len()).is_err());
        }
        Ok(())
    }
}
