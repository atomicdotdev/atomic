//! Transpose ATSC-v1 control fields using its exact operation grammar.
use super::transforms;
use anyhow::{ensure, Context, Result};

pub(super) const FIELDS: usize = 12;
const FILES: usize = 0;
const LINES: usize = 1;
const OLD: usize = 2;
const NEW: usize = 3;
const RANGE: usize = 4;
const BRANCH: usize = 5;
const BRANCH_AFTER: usize = 6;
const TOKENS: usize = 7;
const LEAF: usize = 8;
const LEAF_AFTER: usize = 9;
pub(super) const REFERENCE: usize = 10;
pub(super) const LENGTH: usize = 11;

pub(super) fn get(input: &mut &[u8]) -> Result<u64> {
    let mut n = 0;
    for shift in (0..70).step_by(7) {
        let (&b, rest) = input.split_first().context("truncated control varint")?;
        *input = rest;
        ensure!(shift != 63 || b <= 1, "control varint overflow");
        n |= u64::from(b & 127) << shift;
        if b < 128 {
            ensure!(shift == 0 || b != 0, "noncanonical control varint");
            return Ok(n);
        }
    }
    anyhow::bail!("control varint overflow")
}
pub(super) trait Values {
    fn value(&mut self, role: usize) -> Result<u64>;
}
fn flag(values: &mut impl Values, role: usize) -> Result<()> {
    ensure!(values.value(role)? <= 1, "invalid semantic presence flag");
    Ok(())
}
fn count(values: &mut impl Values, role: usize, limit: usize) -> Result<usize> {
    let n = usize::try_from(values.value(role)?)?;
    ensure!(n <= limit, "semantic count limit");
    Ok(n)
}
fn leaves(
    values: &mut impl Values,
    limit: usize,
    visit: &mut impl FnMut(bool, u64, u64) -> Result<()>,
) -> Result<()> {
    for _ in 0..count(values, TOKENS, limit)? {
        let op = values.value(LEAF)?;
        match op {
            0 => flag(values, LEAF_AFTER)?,
            1..=3 => {}
            _ => anyhow::bail!("unknown leaf operation"),
        }
        if op == 0 || op == 2 {
            let tag = values.value(REFERENCE)?;
            ensure!(tag <= 2, "unknown byte reference");
            let len = values.value(LENGTH)?;
            visit(op == 0, tag, len)?;
        }
    }
    Ok(())
}
pub(super) fn walk(
    values: &mut impl Values,
    limit: usize,
    visit: &mut impl FnMut(bool, u64, u64) -> Result<()>,
) -> Result<()> {
    for _ in 0..count(values, FILES, limit)? {
        for _ in 0..count(values, LINES, limit)? {
            flag(values, OLD)?;
            flag(values, NEW)?;
            flag(values, RANGE)?;
            match values.value(BRANCH)? {
                0 => {
                    flag(values, BRANCH_AFTER)?;
                    leaves(values, limit, visit)?;
                }
                1 => leaves(values, limit, visit)?,
                2 => {
                    leaves(values, limit, visit)?;
                    leaves(values, limit, visit)?;
                }
                3 => {}
                4 => flag(values, BRANCH_AFTER)?,
                _ => anyhow::bail!("unknown branch operation"),
            }
        }
    }
    Ok(())
}
struct Split<'a> {
    input: &'a [u8],
    columns: [Vec<u8>; FIELDS],
}
impl Values for Split<'_> {
    fn value(&mut self, role: usize) -> Result<u64> {
        let n = get(&mut self.input)?;
        transforms::put(&mut self.columns[role], n);
        Ok(n)
    }
}

pub(super) fn fields(raw: &[u8]) -> Result<[Vec<u8>; FIELDS]> {
    ensure!(raw.len() <= transforms::LIMIT, "control limit");
    let mut values = Split {
        input: raw,
        columns: std::array::from_fn(|_| vec![]),
    };
    walk(&mut values, raw.len(), &mut |_, _, _| Ok(()))?;
    ensure!(values.input.is_empty(), "trailing control values");
    Ok(values.columns)
}
pub fn split(raw: &[u8]) -> Result<Vec<u8>> {
    Ok(postcard::to_allocvec(&fields(raw)?)?)
}
struct Join<'a> {
    columns: [&'a [u8]; FIELDS],
    output: Vec<u8>,
    limit: usize,
}
impl Values for Join<'_> {
    fn value(&mut self, role: usize) -> Result<u64> {
        let n = get(&mut self.columns[role])?;
        let len = ((64 - n.leading_zeros()).max(1)).div_ceil(7) as usize;
        ensure!(
            self.output
                .len()
                .checked_add(len)
                .is_some_and(|n| n <= self.limit),
            "expanded control limit"
        );
        transforms::put(&mut self.output, n);
        Ok(n)
    }
}
pub fn join(data: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    ensure!(
        raw_len <= transforms::LIMIT && data.len() <= transforms::LIMIT + 256,
        "control limit"
    );
    let (columns, rest): ([&[u8]; FIELDS], _) = postcard::take_from_bytes(data)?;
    ensure!(rest.is_empty(), "trailing control frame");
    let mut values = Join {
        columns,
        output: vec![],
        limit: raw_len,
    };
    walk(&mut values, raw_len, &mut |_, _, _| Ok(()))?;
    ensure!(
        values.columns.iter().all(|c| c.is_empty()) && values.output.len() == raw_len,
        "unconsumed control fields"
    );
    Ok(values.output)
}
struct Flat<'a>(&'a [u8]);
impl Values for Flat<'_> {
    fn value(&mut self, _: usize) -> Result<u64> {
        get(&mut self.0)
    }
}
pub fn visit_bytes(
    control: &[u8],
    mut visit: impl FnMut(bool, u64, u64) -> Result<()>,
) -> Result<()> {
    let mut values = Flat(control);
    walk(&mut values, control.len(), &mut visit)?;
    ensure!(values.0.is_empty(), "trailing controls");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::{
        change::{format_v3::semantic, FileOps, LineOps},
        crdt::{BranchId, BranchOp, LeafId, LeafOp, TrunkId},
        diff::token::TokenKind,
    };
    #[test]
    fn grammar_preserves_all_leaf_and_branch_shapes() -> Result<()> {
        let tokens = vec![
            LeafOp::Insert {
                after: Some(LeafId::ROOT),
                kind: TokenKind::Word,
                content: b"x".to_vec(),
            },
            LeafOp::Delete { leaf: LeafId::ROOT },
            LeafOp::Replace {
                leaf: LeafId::ROOT,
                new_content: b"y".to_vec(),
            },
            LeafOp::Restore { leaf: LeafId::ROOT },
        ];
        let mut file = FileOps::edit(TrunkId::ROOT, "f".into());
        for branch in [
            BranchOp::Insert {
                after: None,
                content: tokens.clone(),
            },
            BranchOp::Delete {
                branch: BranchId::ROOT,
                content: tokens.clone(),
            },
            BranchOp::Modify {
                branch: BranchId::ROOT,
                old_content: tokens.clone(),
                new_content: tokens,
            },
            BranchOp::Restore {
                branch: BranchId::ROOT,
            },
            BranchOp::Reparent {
                branch: BranchId::ROOT,
                new_after: Some(BranchId::ROOT),
            },
        ] {
            file.add_line_op(LineOps::new(BranchId::ROOT, branch));
        }
        let encoded = semantic::encode(&[file], b"xy")?;
        let view: super::super::View<'_> = postcard::from_bytes(&encoded[5..])?;
        let data = split(view.control)?;
        assert_eq!(join(&data, view.control.len())?, view.control);
        for end in 0..data.len() {
            assert!(join(&data[..end], view.control.len()).is_err());
        }
        let mut count = 0;
        visit_bytes(view.control, |_, _, _| {
            count += 1;
            Ok(())
        })?;
        assert_eq!(count, 8);
        assert!(split(&[0, 1]).is_err());
        assert!(join(&data, 0).is_err());
        Ok(())
    }
}
