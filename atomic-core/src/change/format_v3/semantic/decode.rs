use super::super::FormatResult;
use super::predictive::{self, wire as roles};
use super::wire::*;
use crate::{
    change::{FileOps, LineOps},
    crdt::{BranchId, BranchOp, LeafId, LeafOp, TrunkId, TrunkOp},
    diff::token::TokenKind,
    types::{ChangePosition, NodeId},
};

struct Decoder<'a> {
    c: BorrowedColumns<'a>,
    content: &'a [u8],
    branch_owner: Delta,
    branch_index: Delta,
    leaf_owner: Delta,
    leaf_index: Delta,
    old_number: Delta,
    new_number: Delta,
    position: Delta,
    reference: Delta,
    cursor: usize,
    literal_end: usize,
    remaining: usize,
    predictive: Option<predictive::Control<'a>>,
}

impl Decoder<'_> {
    fn charge(&mut self, count: usize, size: usize) -> FormatResult<()> {
        charge(&mut self.remaining, count, size)
    }
    fn value(&mut self, role: usize) -> FormatResult<u64> {
        if let Some(p) = &mut self.predictive {
            p.value(role)
        } else {
            get(&mut self.c.control)
        }
    }
    fn count<T: AllocationCost>(&mut self, role: usize) -> FormatResult<usize> {
        let n = number(self.value(role)?)?;
        let available = self
            .predictive
            .as_ref()
            .map_or(self.c.control.len(), |p| p.remaining());
        if n > available {
            return Err(invalid("count exceeds control stream"));
        }
        self.charge(n, T::COST)?;
        Ok(n)
    }
    fn flag(&mut self, role: usize) -> FormatResult<bool> {
        if self.predictive.is_none() {
            return flag(&mut self.c.control);
        }
        match self.value(role)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(invalid("invalid predictive presence flag")),
        }
    }
    fn id(&mut self, branch: bool) -> FormatResult<(NodeId, u32)> {
        let (input, o, i) = if branch && self.c.split_ids {
            (
                &mut self.c.branches,
                &mut self.branch_owner,
                &mut self.branch_index,
            )
        } else {
            (
                &mut self.c.leaves,
                &mut self.leaf_owner,
                &mut self.leaf_index,
            )
        };
        Ok((
            NodeId::new(o.read(input)?),
            u32::try_from(i.read(input)?).map_err(|_| invalid("ID index overflow"))?,
        ))
    }
    fn branch(&mut self) -> FormatResult<BranchId> {
        let (o, i) = self.id(true)?;
        Ok(BranchId::new(o, i))
    }
    fn leaf(&mut self) -> FormatResult<LeafId> {
        let (o, i) = self.id(false)?;
        Ok(LeafId::new(o, i))
    }
    fn optional_branch(&mut self) -> FormatResult<Option<BranchId>> {
        if self.flag(roles::BRANCH_AFTER)? {
            Ok(Some(self.branch()?))
        } else {
            Ok(None)
        }
    }
    fn bytes(&mut self) -> FormatResult<Vec<u8>> {
        let tag = self.value(roles::REFERENCE)?;
        if tag > 2 {
            return Err(invalid("unknown byte reference"));
        }
        let flat_len = if self.predictive.is_none() {
            Some(number(get(&mut self.c.control)?)?)
        } else {
            None
        };
        let at = if tag == 0 {
            self.cursor
        } else {
            number(self.reference.read(&mut self.c.references)?)?
        };
        let pool = if tag == 2 {
            self.c.literals
        } else {
            self.content
        };
        let len = if let Some(p) = &mut self.predictive {
            p.length(
                pool.get(at..)
                    .ok_or_else(|| invalid("predictive byte offset outside pool"))?,
            )?
        } else {
            flat_len.unwrap()
        };
        let end = at
            .checked_add(len)
            .ok_or_else(|| invalid("range overflow"))?;
        let bytes = pool
            .get(at..end)
            .ok_or_else(|| invalid("range outside byte pool"))?;
        if tag == 2 {
            if at > self.literal_end || (at < self.literal_end && end > self.literal_end) {
                return Err(invalid(
                    "literal references leave gaps or cross the pool frontier",
                ));
            }
            if at == self.literal_end {
                self.literal_end = end;
            }
        }
        self.charge(len, 1)?;
        let mut out = Vec::new();
        out.try_reserve_exact(len)
            .map_err(|_| invalid("token allocation failed"))?;
        out.extend_from_slice(bytes);
        if tag != 2 {
            self.cursor = end;
        }
        Ok(out)
    }
    fn leaves(&mut self) -> FormatResult<Vec<LeafOp>> {
        let count = self.count::<LeafOp>(roles::TOKENS)?;
        let mut ops = Vec::new();
        ops.try_reserve_exact(count)
            .map_err(|_| invalid("leaf allocation failed"))?;
        for _ in 0..count {
            ops.push(match self.value(roles::LEAF)? {
                0 => {
                    let after = if self.flag(roles::LEAF_AFTER)? {
                        Some(self.leaf()?)
                    } else {
                        None
                    };
                    if let Some(p) = &self.predictive {
                        p.require_kind()?;
                    }
                    if self.predictive.is_some() {
                        let content = self.bytes()?;
                        let kind = self.predictive.as_mut().unwrap().kind(&content)?;
                        LeafOp::Insert {
                            after,
                            kind,
                            content,
                        }
                    } else {
                        let (kind, rest): (TokenKind, _) = postcard::take_from_bytes(self.c.kinds)?;
                        self.c.kinds = rest;
                        LeafOp::Insert {
                            after,
                            kind,
                            content: self.bytes()?,
                        }
                    }
                }
                1 => LeafOp::Delete { leaf: self.leaf()? },
                2 => LeafOp::Replace {
                    leaf: self.leaf()?,
                    new_content: self.bytes()?,
                },
                3 => LeafOp::Restore { leaf: self.leaf()? },
                _ => return Err(invalid("unknown leaf operation")),
            });
        }
        Ok(ops)
    }
    fn line(&mut self) -> FormatResult<LineOps> {
        let id = self.branch()?;
        let old = if self.flag(roles::OLD)? {
            Some(number(self.old_number.read(&mut self.c.old_numbers)?)?)
        } else {
            None
        };
        let new = if self.flag(roles::NEW)? {
            Some(number(self.new_number.read(&mut self.c.new_numbers)?)?)
        } else {
            None
        };
        let range = if self.flag(roles::RANGE)? {
            Some((
                self.position.read(&mut self.c.positions)?,
                self.position.read(&mut self.c.positions)?,
            ))
        } else {
            None
        };
        let op = match self.value(roles::BRANCH)? {
            0 => BranchOp::Insert {
                after: self.optional_branch()?,
                content: self.leaves()?,
            },
            1 => BranchOp::Delete {
                branch: self.branch()?,
                content: self.leaves()?,
            },
            2 => BranchOp::Modify {
                branch: self.branch()?,
                old_content: self.leaves()?,
                new_content: self.leaves()?,
            },
            3 => BranchOp::Restore {
                branch: self.branch()?,
            },
            4 => BranchOp::Reparent {
                branch: self.branch()?,
                new_after: self.optional_branch()?,
            },
            _ => return Err(invalid("unknown branch operation")),
        };
        let mut line = LineOps::new_with_line_nums(id, op, old, new);
        if let Some((a, b)) = range {
            line.set_content_range(ChangePosition::new(a), ChangePosition::new(b));
        }
        Ok(line)
    }
}

/// Decode a decompressed ATSC-v1 or predictive-v2 section directly into FileOps,
/// using only this change's
/// authored bytes. A 512-MiB payload/content cap and 1-GiB expanded allocation
/// budget bound malformed counts and reference amplification.
pub fn decode(payload: &[u8], content: &[u8]) -> FormatResult<Vec<FileOps>> {
    if payload.len() > MAX_PAYLOAD || content.len() > MAX_PAYLOAD {
        return Err(super::super::FormatError::SemanticLimitExceeded);
    }
    let (c, predictive) = if payload.starts_with(b"ATSC\x02") {
        let (c, p) = predictive::Control::parse(payload)?;
        (c, Some(p))
    } else {
        let bytes = payload
            .strip_prefix(MAGIC)
            .ok_or_else(|| invalid("unknown magic/version"))?;
        let (c, rest): (BorrowedColumns<'_>, _) = postcard::take_from_bytes(bytes)?;
        if !rest.is_empty() {
            return Err(invalid("trailing payload bytes"));
        }
        (c, None)
    };
    if c.content_hash != *blake3::hash(content).as_bytes() {
        return Err(invalid("authored content binding mismatch"));
    }
    let mut d = Decoder {
        c,
        content,
        branch_owner: Delta::default(),
        branch_index: Delta::default(),
        leaf_owner: Delta::default(),
        leaf_index: Delta::default(),
        old_number: Delta::default(),
        new_number: Delta::default(),
        position: Delta::default(),
        reference: Delta::default(),
        cursor: 0,
        literal_end: 0,
        remaining: MAX_EXPANDED,
        predictive,
    };
    d.charge(d.c.metadata.len(), 1)?;
    let count = d.count::<FileOps>(roles::FILES)?;
    let mut files = Vec::new();
    files
        .try_reserve_exact(count)
        .map_err(|_| invalid("file allocation failed"))?;
    for _ in 0..count {
        let ((trunk, path, op), rest): ((TrunkId, String, Option<TrunkOp>), _) =
            postcard::take_from_bytes(d.c.metadata)?;
        d.c.metadata = rest;
        let mut file = FileOps::new(trunk, path, op);
        let lines = d.count::<LineOps>(roles::LINES)?;
        file.line_ops_mut()
            .try_reserve_exact(lines)
            .map_err(|_| invalid("line allocation failed"))?;
        for _ in 0..lines {
            file.add_line_op(d.line()?);
        }
        files.push(file);
    }
    if d.literal_end != d.c.literals.len()
        || [
            d.c.metadata,
            d.c.control,
            d.c.branches,
            d.c.leaves,
            d.c.old_numbers,
            d.c.new_numbers,
            d.c.positions,
            d.c.kinds,
            d.c.references,
        ]
        .iter()
        .any(|s| !s.is_empty())
    {
        return Err(invalid("unconsumed column bytes"));
    }
    if let Some(p) = &d.predictive {
        p.finish()?;
    }
    Ok(files)
}
