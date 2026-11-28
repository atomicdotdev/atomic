//! Predictive semantic v2: frozen ASCII predictor 1 and explicit exceptions.
//! The typed reader consumes these columns directly; ATSC v1 is never rebuilt.
mod encode;
mod predictor_v1;
#[cfg(test)]
mod tests;
pub(super) mod wire;
use super::super::FormatResult;
use super::wire::{
    get, invalid, number, AllocationCost, BorrowedColumns, MAX_EXPANDED, MAX_PAYLOAD,
};
use crate::{crdt::LeafOp, diff::token::TokenKind};
pub(super) use encode::encode;

struct Mask<'a> {
    count: usize,
    index: usize,
    bits: &'a [u8],
    exceptions: &'a [u8],
}
impl<'a> Mask<'a> {
    fn new(count: u32, bits: &'a [u8], exceptions: &'a [u8]) -> FormatResult<Self> {
        let count = count as usize;
        if count > MAX_EXPANDED / LeafOp::COST
            || bits.len() != count.div_ceil(8)
            || (!count.is_multiple_of(8) && bits.last().is_none_or(|b| b >> (count % 8) != 0))
        {
            return Err(invalid("invalid predictive mask/count/padding"));
        }
        Ok(Self {
            count,
            index: 0,
            bits,
            exceptions,
        })
    }
    fn available(&self) -> FormatResult<()> {
        if self.index >= self.count {
            Err(invalid("predictive mask exhausted"))
        } else {
            Ok(())
        }
    }
    fn actual(&mut self) -> FormatResult<Option<u64>> {
        self.available()?;
        let flag = self.bits[self.index / 8] & (1 << (self.index % 8)) != 0;
        self.index += 1;
        if flag {
            Ok(Some(get(&mut self.exceptions)?))
        } else {
            Ok(None)
        }
    }
    fn complete(&self) -> bool {
        self.index == self.count && self.exceptions.is_empty()
    }
}
pub(super) struct Control<'a> {
    fields: [&'a [u8]; 11],
    lengths: Mask<'a>,
    kinds: Mask<'a>,
}
impl<'a> Control<'a> {
    pub fn parse(payload: &'a [u8]) -> FormatResult<(BorrowedColumns<'a>, Self)> {
        if payload.len() > MAX_PAYLOAD {
            return Err(super::super::FormatError::SemanticLimitExceeded);
        }
        let raw = payload
            .strip_prefix(wire::MAGIC)
            .ok_or_else(|| invalid("unknown predictive version"))?;
        let (p, rest): (wire::Borrowed<'_>, _) = postcard::take_from_bytes(raw)?;
        if !rest.is_empty() || p.predictor != wire::PREDICTOR || p.kind_count > p.length_count {
            return Err(invalid("unknown predictor or trailing predictive data"));
        }
        let control = Self {
            fields: p.controls,
            lengths: Mask::new(p.length_count, p.length_mask, p.length_exceptions)?,
            kinds: Mask::new(p.kind_count, p.kind_mask, p.kind_exceptions)?,
        };
        let columns = BorrowedColumns {
            split_ids: true,
            content_hash: p.content_hash,
            metadata: p.metadata,
            control: &[],
            branches: p.branches,
            leaves: p.leaves,
            old_numbers: p.old_numbers,
            new_numbers: p.new_numbers,
            positions: p.positions,
            kinds: &[],
            references: p.references,
            literals: p.literals,
        };
        Ok((columns, control))
    }
    pub fn value(&mut self, role: usize) -> FormatResult<u64> {
        get(&mut self.fields[role])
    }
    pub fn remaining(&self) -> usize {
        self.fields.iter().map(|s| s.len()).sum()
    }
    pub fn require_kind(&self) -> FormatResult<()> {
        self.kinds.available()
    }
    pub fn length(&mut self, tail: &[u8]) -> FormatResult<usize> {
        match self.lengths.actual()? {
            Some(n) => number(n),
            None => Ok(predictor_v1::length(tail)),
        }
    }
    pub fn kind(&mut self, bytes: &[u8]) -> FormatResult<TokenKind> {
        match self.kinds.actual()? {
            Some(n) => {
                predictor_v1::from_code(n).ok_or_else(|| invalid("unknown predictive token kind"))
            }
            None => Ok(predictor_v1::kind(bytes)),
        }
    }
    pub fn finish(&self) -> FormatResult<()> {
        if self.remaining() != 0 || !self.lengths.complete() || !self.kinds.complete() {
            return Err(invalid("unconsumed predictive columns or exceptions"));
        }
        Ok(())
    }
}
