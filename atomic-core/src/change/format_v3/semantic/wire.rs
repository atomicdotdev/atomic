use super::super::{FormatError, FormatResult};
use serde::{Deserialize, Serialize};

pub(super) const MAGIC: &[u8; 5] = b"ATSC\x01";
pub(crate) const MAX_PAYLOAD: usize = 512 * 1024 * 1024;
pub(super) const MAX_EXPANDED: usize = 1024 * 1024 * 1024;

// Fixed costs keep automatic selection independent of pointer width and ABI.
pub(super) trait AllocationCost {
    const COST: usize;
}
impl AllocationCost for crate::change::FileOps {
    const COST: usize = 256;
}
impl AllocationCost for crate::change::LineOps {
    const COST: usize = 160;
}
impl AllocationCost for crate::crdt::LeafOp {
    const COST: usize = 64;
}
const _: () = {
    assert!(std::mem::size_of::<crate::change::FileOps>() <= 256);
    assert!(std::mem::size_of::<crate::change::LineOps>() <= 160);
    assert!(std::mem::size_of::<crate::crdt::LeafOp>() <= 64);
};

pub(super) fn charge(remaining: &mut usize, count: usize, size: usize) -> FormatResult<()> {
    *remaining = count
        .checked_mul(size)
        .and_then(|n| remaining.checked_sub(n))
        .ok_or(FormatError::SemanticLimitExceeded)?;
    Ok(())
}

pub(super) fn invalid(reason: &str) -> FormatError {
    FormatError::InvalidChangeMetadata {
        reason: format!("compact semantics: {reason}"),
    }
}

#[derive(Default, Serialize)]
pub(super) struct Columns {
    pub split_ids: bool,
    pub content_hash: [u8; 32],
    pub metadata: Vec<u8>,
    pub control: Vec<u8>,
    pub branches: Vec<u8>,
    pub leaves: Vec<u8>,
    pub old_numbers: Vec<u8>,
    pub new_numbers: Vec<u8>,
    pub positions: Vec<u8>,
    pub kinds: Vec<u8>,
    pub references: Vec<u8>,
    pub literals: Vec<u8>,
}

// Borrow every variable-sized column: untrusted declared lengths cannot cause
// allocations until the control stream has passed the explicit output budget.
#[derive(Deserialize)]
pub(super) struct BorrowedColumns<'a> {
    pub split_ids: bool,
    pub content_hash: [u8; 32],
    #[serde(borrow)]
    pub metadata: &'a [u8],
    #[serde(borrow)]
    pub control: &'a [u8],
    #[serde(borrow)]
    pub branches: &'a [u8],
    #[serde(borrow)]
    pub leaves: &'a [u8],
    #[serde(borrow)]
    pub old_numbers: &'a [u8],
    #[serde(borrow)]
    pub new_numbers: &'a [u8],
    #[serde(borrow)]
    pub positions: &'a [u8],
    #[serde(borrow)]
    pub kinds: &'a [u8],
    #[serde(borrow)]
    pub references: &'a [u8],
    #[serde(borrow)]
    pub literals: &'a [u8],
}

pub(super) fn put(out: &mut Vec<u8>, mut n: u64) {
    while n >= 128 {
        out.push((n as u8) | 128);
        n >>= 7;
    }
    out.push(n as u8);
}

pub(super) fn get(input: &mut &[u8]) -> FormatResult<u64> {
    let mut n = 0;
    for shift in (0..70).step_by(7) {
        let (&b, rest) = input
            .split_first()
            .ok_or_else(|| invalid("truncated varint"))?;
        *input = rest;
        if shift == 63 && b > 1 {
            return Err(invalid("varint overflow"));
        }
        n |= u64::from(b & 127) << shift;
        if b < 128 {
            if shift > 0 && b == 0 {
                return Err(invalid("noncanonical varint"));
            }
            return Ok(n);
        }
    }
    Err(invalid("varint overflow"))
}

#[derive(Default)]
pub(super) struct Delta(pub u64);
impl Delta {
    pub fn write(&mut self, out: &mut Vec<u8>, n: u64) {
        let d = n.wrapping_sub(self.0);
        put(out, (d << 1) ^ ((d as i64 >> 63) as u64));
        self.0 = n;
    }
    pub fn read(&mut self, input: &mut &[u8]) -> FormatResult<u64> {
        let z = get(input)?;
        self.0 = self.0.wrapping_add((z >> 1) ^ 0u64.wrapping_sub(z & 1));
        Ok(self.0)
    }
}

pub(super) fn flag(input: &mut &[u8]) -> FormatResult<bool> {
    match get(input)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid("invalid optional flag")),
    }
}

pub(super) fn number(n: u64) -> FormatResult<usize> {
    usize::try_from(n).map_err(|_| invalid("integer exceeds platform range"))
}
