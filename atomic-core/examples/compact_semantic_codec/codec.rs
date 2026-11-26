//! Experiment only: columnar, reversible FileOps encoding. Not a wire contract.
use anyhow::{bail, ensure, Context, Result};
use atomic_core::{
    change::{FileOps, LineOps},
    crdt::{BranchId, BranchOp, LeafId, LeafOp, TrunkId, TrunkOp},
    diff::token::TokenKind,
    types::{ChangePosition, NodeId},
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, io::Read};

const MAGIC: &[u8; 5] = b"ATSC\x01";
const MAX_DECODED: u64 = 512 * 1024 * 1024;

#[derive(Default, Serialize, Deserialize)]
struct Columns {
    content_hash: [u8; 32],
    files: Vec<(TrunkId, String, Option<TrunkOp>)>,
    control: Vec<u8>,
    owners: Vec<u8>,
    indices: Vec<u8>,
    numbers: Vec<u8>,
    positions: Vec<u8>,
    kinds: Vec<u8>,
    references: Vec<u8>,
    literals: Vec<u8>,
}

fn put(out: &mut Vec<u8>, mut n: u64) {
    while n >= 128 {
        out.push((n as u8) | 128);
        n >>= 7;
    }
    out.push(n as u8);
}

fn get(input: &mut &[u8]) -> Result<u64> {
    let mut n = 0;
    for shift in (0..70).step_by(7) {
        let (&b, rest) = input.split_first().context("truncated varint")?;
        *input = rest;
        ensure!(shift != 63 || b <= 1, "varint overflow");
        n |= u64::from(b & 127) << shift;
        if b < 128 {
            ensure!(shift == 0 || b != 0, "noncanonical varint");
            return Ok(n);
        }
    }
    bail!("varint overflow")
}

// Wrapping differences cover the entire u64 domain, including descending IDs.
fn delta(out: &mut Vec<u8>, previous: &mut u64, next: u64) {
    let d = next.wrapping_sub(*previous);
    put(out, (d << 1) ^ ((d as i64 >> 63) as u64));
    *previous = next;
}

fn undelta(input: &mut &[u8], previous: &mut u64) -> Result<u64> {
    let z = get(input)?;
    let d = (z >> 1) ^ 0u64.wrapping_sub(z & 1);
    *previous = previous.wrapping_add(d);
    Ok(*previous)
}

#[derive(Default)]
struct State {
    owner: u64,
    index: u64,
    number: u64,
    position: u64,
    reference: u64,
    content_cursor: usize,
}

struct Encoder<'a> {
    c: Columns,
    s: State,
    content: &'a [u8],
    lines: HashMap<&'a [u8], usize>,
    literals: HashMap<Vec<u8>, usize>,
}

impl Encoder<'_> {
    fn id(&mut self, owner: NodeId, index: u32) {
        delta(&mut self.c.owners, &mut self.s.owner, owner.get());
        delta(&mut self.c.indices, &mut self.s.index, index.into());
    }

    fn branch(&mut self, id: BranchId) {
        self.id(id.change_id(), id.branch_idx());
    }

    fn optional_branch(&mut self, id: Option<BranchId>) {
        put(&mut self.c.control, u64::from(id.is_some()));
        if let Some(id) = id {
            self.branch(id);
        }
    }

    fn leaf(&mut self, id: LeafId) {
        self.id(id.change_id(), id.leaf_idx());
    }

    fn optional_number(&mut self, n: Option<usize>) {
        put(&mut self.c.control, u64::from(n.is_some()));
        if let Some(n) = n {
            delta(&mut self.c.numbers, &mut self.s.number, n as u64);
        }
    }

    fn bytes(&mut self, bytes: &[u8], offset: Option<usize>) {
        // Range references never stand in for a token's kind or identity.
        let (tag, at) = if let Some(at) = offset {
            if at == self.s.content_cursor {
                (0, at)
            } else {
                (1, at)
            }
        } else {
            let at = *self.literals.entry(bytes.to_vec()).or_insert_with(|| {
                let at = self.c.literals.len();
                self.c.literals.extend_from_slice(bytes);
                at
            });
            (2, at)
        };
        put(&mut self.c.control, tag);
        put(&mut self.c.control, bytes.len() as u64);
        if tag != 0 {
            delta(&mut self.c.references, &mut self.s.reference, at as u64);
        }
        if tag != 2 {
            self.s.content_cursor = at + bytes.len();
        }
    }

    fn leaves(
        &mut self,
        ops: &[LeafOp],
        range: Option<(ChangePosition, ChangePosition)>,
    ) -> Result<()> {
        // Match whole authored fragments, not each token against the whole blob.
        // Old/deleted text absent from this change goes into a lossless literal pool.
        let joined: Vec<u8> = ops
            .iter()
            .filter_map(LeafOp::content)
            .flatten()
            .copied()
            .collect();
        let hinted = range.and_then(|(a, b)| {
            let a = usize::try_from(a.get()).ok()?;
            let b = usize::try_from(b.get()).ok()?;
            (self.content.get(a..b)? == joined).then_some(a)
        });
        let sequential = self
            .content
            .get(self.s.content_cursor..)
            .filter(|rest| rest.starts_with(&joined))
            .map(|_| self.s.content_cursor);
        let mut offset = hinted
            .or(sequential)
            .or_else(|| self.lines.get(joined.as_slice()).copied());
        put(&mut self.c.control, ops.len() as u64);
        for op in ops {
            match op {
                LeafOp::Insert {
                    after,
                    kind,
                    content,
                } => {
                    put(&mut self.c.control, 0);
                    put(&mut self.c.control, u64::from(after.is_some()));
                    if let Some(after) = after {
                        self.leaf(*after);
                    }
                    self.c.kinds.extend(postcard::to_allocvec(kind)?);
                    self.bytes(content, offset);
                }
                LeafOp::Delete { leaf } => {
                    put(&mut self.c.control, 1);
                    self.leaf(*leaf);
                }
                LeafOp::Replace { leaf, new_content } => {
                    put(&mut self.c.control, 2);
                    self.leaf(*leaf);
                    self.bytes(new_content, offset);
                }
                LeafOp::Restore { leaf } => {
                    put(&mut self.c.control, 3);
                    self.leaf(*leaf);
                }
            }
            if let (Some(at), Some(bytes)) = (&mut offset, op.content()) {
                *at += bytes.len();
            }
        }
        Ok(())
    }
}

pub(super) fn encode(ops: &[FileOps], content: &[u8], level: i32) -> Result<Vec<u8>> {
    let mut lines = HashMap::new();
    let mut offset = 0;
    for line in content.split_inclusive(|b| *b == b'\n') {
        lines.entry(line).or_insert(offset);
        // Semantic snapshots sometimes omit the line ending.
        let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
        lines.entry(trimmed).or_insert(offset);
        offset += line.len();
    }
    let mut e = Encoder {
        c: Columns {
            content_hash: *blake3::hash(content).as_bytes(),
            ..Columns::default()
        },
        s: State::default(),
        content,
        lines,
        literals: HashMap::new(),
    };
    for file in ops {
        e.c.files.push((
            file.trunk_id(),
            file.path().into(),
            file.trunk_op().cloned(),
        ));
        put(&mut e.c.control, file.line_ops().len() as u64);
        for line in file.line_ops() {
            e.branch(line.branch_id());
            e.optional_number(line.old_line_num());
            e.optional_number(line.new_line_num());
            put(&mut e.c.control, u64::from(line.content_range().is_some()));
            if let Some((a, b)) = line.content_range() {
                delta(&mut e.c.positions, &mut e.s.position, a.get());
                delta(&mut e.c.positions, &mut e.s.position, b.get());
            }
            match line.operation() {
                BranchOp::Insert { after, content } => {
                    put(&mut e.c.control, 0);
                    e.optional_branch(*after);
                    e.leaves(content, line.content_range())?;
                }
                BranchOp::Delete { branch, content } => {
                    put(&mut e.c.control, 1);
                    e.branch(*branch);
                    e.leaves(content, None)?;
                }
                BranchOp::Modify {
                    branch,
                    old_content,
                    new_content,
                } => {
                    put(&mut e.c.control, 2);
                    e.branch(*branch);
                    e.leaves(old_content, None)?;
                    e.leaves(new_content, line.content_range())?;
                }
                BranchOp::Restore { branch } => {
                    put(&mut e.c.control, 3);
                    e.branch(*branch);
                }
                BranchOp::Reparent { branch, new_after } => {
                    put(&mut e.c.control, 4);
                    e.branch(*branch);
                    e.optional_branch(*new_after);
                }
            }
        }
    }
    let raw = postcard::to_allocvec(&e.c)?;
    let mut out = MAGIC.to_vec();
    out.extend(zstd::encode_all(raw.as_slice(), level)?);
    Ok(out)
}

struct Decoder<'a> {
    control: &'a [u8],
    owners: &'a [u8],
    indices: &'a [u8],
    numbers: &'a [u8],
    positions: &'a [u8],
    kinds: &'a [u8],
    references: &'a [u8],
    literals: &'a [u8],
    content: &'a [u8],
    s: State,
    output_bytes: u64,
}

impl Decoder<'_> {
    fn flag(&mut self) -> Result<bool> {
        let flag = get(&mut self.control)?;
        ensure!(flag <= 1, "invalid optional flag");
        Ok(flag == 1)
    }

    fn count(&mut self) -> Result<usize> {
        let n = usize::try_from(get(&mut self.control)?)?;
        ensure!(
            n <= self.control.len(),
            "count exceeds available control data"
        );
        Ok(n)
    }

    fn id(&mut self) -> Result<(NodeId, u32)> {
        Ok((
            NodeId::new(undelta(&mut self.owners, &mut self.s.owner)?),
            u32::try_from(undelta(&mut self.indices, &mut self.s.index)?)?,
        ))
    }

    fn branch(&mut self) -> Result<BranchId> {
        let (owner, index) = self.id()?;
        Ok(BranchId::new(owner, index))
    }

    fn optional_branch(&mut self) -> Result<Option<BranchId>> {
        if self.flag()? {
            Ok(Some(self.branch()?))
        } else {
            Ok(None)
        }
    }

    fn leaf(&mut self) -> Result<LeafId> {
        let (owner, index) = self.id()?;
        Ok(LeafId::new(owner, index))
    }

    fn optional_number(&mut self) -> Result<Option<usize>> {
        if self.flag()? {
            Ok(Some(usize::try_from(undelta(
                &mut self.numbers,
                &mut self.s.number,
            )?)?))
        } else {
            Ok(None)
        }
    }

    fn bytes(&mut self) -> Result<Vec<u8>> {
        let tag = get(&mut self.control)?;
        ensure!(tag <= 2, "unknown byte reference");
        let len = usize::try_from(get(&mut self.control)?)?;
        let at = if tag == 0 {
            self.s.content_cursor
        } else {
            usize::try_from(undelta(&mut self.references, &mut self.s.reference)?)?
        };
        let end = at.checked_add(len).context("byte range overflow")?;
        let pool = if tag == 2 {
            self.literals
        } else {
            self.content
        };
        let bytes = pool.get(at..end).context("byte range outside pool")?;
        self.output_bytes = self
            .output_bytes
            .checked_add(len as u64)
            .context("output overflow")?;
        ensure!(
            self.output_bytes <= MAX_DECODED,
            "expanded token byte limit"
        );
        if tag != 2 {
            self.s.content_cursor = end;
        }
        Ok(bytes.to_vec())
    }

    fn leaves(&mut self) -> Result<Vec<LeafOp>> {
        let count = self.count()?;
        let mut ops = Vec::new();
        for _ in 0..count {
            ops.push(match get(&mut self.control)? {
                0 => {
                    let after = if self.flag()? {
                        Some(self.leaf()?)
                    } else {
                        None
                    };
                    let (kind, rest): (TokenKind, _) = postcard::take_from_bytes(self.kinds)?;
                    self.kinds = rest;
                    LeafOp::Insert {
                        after,
                        kind,
                        content: self.bytes()?,
                    }
                }
                1 => LeafOp::Delete { leaf: self.leaf()? },
                2 => LeafOp::Replace {
                    leaf: self.leaf()?,
                    new_content: self.bytes()?,
                },
                3 => LeafOp::Restore { leaf: self.leaf()? },
                _ => bail!("unknown leaf operation"),
            });
        }
        Ok(ops)
    }
}

pub(super) fn decode(encoded: &[u8], content: &[u8]) -> Result<Vec<FileOps>> {
    let compressed = encoded
        .strip_prefix(MAGIC)
        .context("wrong codec magic/version")?;
    let mut raw = Vec::new();
    zstd::stream::read::Decoder::new(compressed)?
        .take(MAX_DECODED + 1)
        .read_to_end(&mut raw)?;
    ensure!(raw.len() as u64 <= MAX_DECODED, "decompression limit");
    let (c, trailing): (Columns, _) = postcard::take_from_bytes(&raw)?;
    ensure!(trailing.is_empty(), "trailing codec data");
    ensure!(
        c.content_hash == *blake3::hash(content).as_bytes(),
        "authored content mismatch"
    );
    let mut d = Decoder {
        control: &c.control,
        owners: &c.owners,
        indices: &c.indices,
        numbers: &c.numbers,
        positions: &c.positions,
        kinds: &c.kinds,
        references: &c.references,
        literals: &c.literals,
        content,
        s: State::default(),
        output_bytes: 0,
    };
    let mut files = Vec::new();
    for (trunk, path, op) in c.files {
        let mut file = FileOps::new(trunk, path, op);
        let count = d.count()?;
        for _ in 0..count {
            let branch_id = d.branch()?;
            let old = d.optional_number()?;
            let new = d.optional_number()?;
            let range = if d.flag()? {
                Some((
                    undelta(&mut d.positions, &mut d.s.position)?,
                    undelta(&mut d.positions, &mut d.s.position)?,
                ))
            } else {
                None
            };
            let op = match get(&mut d.control)? {
                0 => BranchOp::Insert {
                    after: d.optional_branch()?,
                    content: d.leaves()?,
                },
                1 => BranchOp::Delete {
                    branch: d.branch()?,
                    content: d.leaves()?,
                },
                2 => BranchOp::Modify {
                    branch: d.branch()?,
                    old_content: d.leaves()?,
                    new_content: d.leaves()?,
                },
                3 => BranchOp::Restore {
                    branch: d.branch()?,
                },
                4 => BranchOp::Reparent {
                    branch: d.branch()?,
                    new_after: d.optional_branch()?,
                },
                _ => bail!("unknown branch operation"),
            };
            let mut line = LineOps::new_with_line_nums(branch_id, op, old, new);
            if let Some((a, b)) = range {
                line.set_content_range(ChangePosition::new(a), ChangePosition::new(b));
            }
            file.add_line_op(line);
        }
        files.push(file);
    }
    ensure!(
        [
            d.control,
            d.owners,
            d.indices,
            d.numbers,
            d.positions,
            d.kinds,
            d.references
        ]
        .iter()
        .all(|s| s.is_empty()),
        "unconsumed column data"
    );
    Ok(files)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
