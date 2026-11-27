use super::super::FormatResult;
use super::wire::*;
use crate::{
    change::{FileOps, LineOps},
    crdt::{BranchId, BranchOp, LeafOp},
    types::ChangePosition,
};
use std::collections::HashMap;

#[derive(Default)]
struct State {
    branch_owner: Delta,
    branch_index: Delta,
    leaf_owner: Delta,
    leaf_index: Delta,
    old_number: Delta,
    new_number: Delta,
    position: Delta,
    reference: Delta,
    cursor: usize,
}

struct Encoder<'a> {
    c: Columns,
    s: State,
    content: &'a [u8],
    lines: HashMap<&'a [u8], usize>,
    literals: HashMap<Vec<u8>, usize>,
    remaining: usize,
}

impl Encoder<'_> {
    fn id(&mut self, branch: bool, owner: u64, index: u32) {
        let (out, o, i) = if branch && self.c.split_ids {
            (
                &mut self.c.branches,
                &mut self.s.branch_owner,
                &mut self.s.branch_index,
            )
        } else {
            (
                &mut self.c.leaves,
                &mut self.s.leaf_owner,
                &mut self.s.leaf_index,
            )
        };
        o.write(out, owner);
        i.write(out, index.into());
    }
    fn branch(&mut self, id: BranchId) {
        self.id(true, id.change_id().get(), id.branch_idx());
    }
    fn optional_branch(&mut self, id: Option<BranchId>) {
        put(&mut self.c.control, u64::from(id.is_some()));
        if let Some(id) = id {
            self.branch(id);
        }
    }
    fn bytes(&mut self, bytes: &[u8], offset: Option<usize>) {
        let (tag, at) = if let Some(at) = offset {
            (if at == self.s.cursor { 0 } else { 1 }, at)
        } else if let Some(&at) = self.literals.get(bytes) {
            (2, at)
        } else {
            let at = self.c.literals.len();
            self.c.literals.extend_from_slice(bytes);
            self.literals.insert(bytes.to_vec(), at);
            (2, at)
        };
        put(&mut self.c.control, tag);
        put(&mut self.c.control, bytes.len() as u64);
        if tag != 0 {
            self.s.reference.write(&mut self.c.references, at as u64);
        }
        if tag != 2 {
            self.s.cursor = at + bytes.len();
        }
    }
    fn leaves(
        &mut self,
        ops: &[LeafOp],
        range: Option<(ChangePosition, ChangePosition)>,
    ) -> FormatResult<()> {
        charge(&mut self.remaining, ops.len(), LeafOp::COST)?;
        for op in ops {
            if let Some(content) = op.content() {
                charge(&mut self.remaining, content.len(), 1)?;
            }
        }
        let joined: Vec<u8> = ops
            .iter()
            .filter_map(LeafOp::content)
            .flatten()
            .copied()
            .collect();
        let hint = range.and_then(|(a, b)| {
            let a = usize::try_from(a.get()).ok()?;
            let b = usize::try_from(b.get()).ok()?;
            (self.content.get(a..b)? == joined).then_some(a)
        });
        let sequential = self
            .content
            .get(self.s.cursor..)
            .filter(|rest| rest.starts_with(&joined))
            .map(|_| self.s.cursor);
        let mut offset = hint
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
                    if let Some(id) = after {
                        self.id(false, id.change_id().get(), id.leaf_idx());
                    }
                    self.c.kinds.extend(postcard::to_allocvec(kind)?);
                    self.bytes(content, offset);
                }
                LeafOp::Delete { leaf } | LeafOp::Restore { leaf } => {
                    put(
                        &mut self.c.control,
                        if matches!(op, LeafOp::Delete { .. }) {
                            1
                        } else {
                            3
                        },
                    );
                    self.id(false, leaf.change_id().get(), leaf.leaf_idx());
                }
                LeafOp::Replace { leaf, new_content } => {
                    put(&mut self.c.control, 2);
                    self.id(false, leaf.change_id().get(), leaf.leaf_idx());
                    self.bytes(new_content, offset);
                }
            }
            if let (Some(at), Some(bytes)) = (&mut offset, op.content()) {
                *at += bytes.len();
            }
        }
        Ok(())
    }
    fn line(&mut self, line: &LineOps) -> FormatResult<()> {
        self.branch(line.branch_id());
        for (n, col, state) in [
            (
                line.old_line_num(),
                &mut self.c.old_numbers,
                &mut self.s.old_number,
            ),
            (
                line.new_line_num(),
                &mut self.c.new_numbers,
                &mut self.s.new_number,
            ),
        ] {
            put(&mut self.c.control, u64::from(n.is_some()));
            if let Some(n) = n {
                state.write(col, n as u64);
            }
        }
        put(
            &mut self.c.control,
            u64::from(line.content_range().is_some()),
        );
        if let Some((a, b)) = line.content_range() {
            self.s.position.write(&mut self.c.positions, a.get());
            self.s.position.write(&mut self.c.positions, b.get());
        }
        match line.operation() {
            BranchOp::Insert { after, content } => {
                put(&mut self.c.control, 0);
                self.optional_branch(*after);
                self.leaves(content, line.content_range())?;
            }
            BranchOp::Delete { branch, content } => {
                put(&mut self.c.control, 1);
                self.branch(*branch);
                self.leaves(content, None)?;
            }
            BranchOp::Modify {
                branch,
                old_content,
                new_content,
            } => {
                put(&mut self.c.control, 2);
                self.branch(*branch);
                self.leaves(old_content, None)?;
                self.leaves(new_content, line.content_range())?;
            }
            BranchOp::Restore { branch } => {
                put(&mut self.c.control, 3);
                self.branch(*branch);
            }
            BranchOp::Reparent { branch, new_after } => {
                put(&mut self.c.control, 4);
                self.branch(*branch);
                self.optional_branch(*new_after);
            }
        }
        Ok(())
    }
}

/// Encode exact operations into an uncompressed versioned payload. The outer
/// change section supplies compression, authentication and framing.
pub fn encode(ops: &[FileOps], content: &[u8]) -> FormatResult<Vec<u8>> {
    encode_layout(ops, content, true)
}

pub(super) fn encode_layout(
    ops: &[FileOps],
    content: &[u8],
    split_ids: bool,
) -> FormatResult<Vec<u8>> {
    let c = columns(ops, content, split_ids)?;
    let mut out = MAGIC.to_vec();
    out.extend(postcard::to_allocvec(&c)?);
    if out.len() > MAX_PAYLOAD {
        return Err(super::super::FormatError::SemanticLimitExceeded);
    }
    Ok(out)
}

pub(super) fn columns(ops: &[FileOps], content: &[u8], split_ids: bool) -> FormatResult<Columns> {
    if content.len() > MAX_PAYLOAD {
        return Err(super::super::FormatError::SemanticLimitExceeded);
    }
    let mut lines = HashMap::new();
    let mut offset = 0;
    for line in content.split_inclusive(|b| *b == b'\n') {
        lines.entry(line).or_insert(offset);
        lines
            .entry(line.strip_suffix(b"\n").unwrap_or(line))
            .or_insert(offset);
        offset += line.len();
    }
    let mut e = Encoder {
        c: Columns {
            split_ids,
            content_hash: *blake3::hash(content).as_bytes(),
            ..Columns::default()
        },
        s: State::default(),
        content,
        lines,
        literals: HashMap::new(),
        remaining: MAX_EXPANDED,
    };
    charge(&mut e.remaining, ops.len(), FileOps::COST)?;
    put(&mut e.c.control, ops.len() as u64);
    for file in ops {
        let metadata = postcard::to_allocvec(&(file.trunk_id(), file.path(), file.trunk_op()))?;
        charge(&mut e.remaining, metadata.len(), 1)?;
        e.c.metadata.extend(metadata);
        charge(&mut e.remaining, file.line_count(), LineOps::COST)?;
        put(&mut e.c.control, file.line_count() as u64);
        for line in file.line_ops() {
            e.line(line)?;
        }
    }
    Ok(e.c)
}
