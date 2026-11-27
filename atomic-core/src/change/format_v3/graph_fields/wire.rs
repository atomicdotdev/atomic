use super::super::{FormatError, FormatResult};
use serde::Serialize;

pub(super) const MAGIC: &[u8; 5] = b"ATGF\x01";
pub(super) const MAX: usize = 512 * 1024 * 1024;
pub(super) const ALLOC: usize = 1024 * 1024 * 1024;
pub(super) const ROLES: usize = 272;
pub(super) const PATH: usize = 264;
pub(super) const LOCAL: usize = 265;
pub(super) const ATTR: usize = 270;
// Insertion roles 0..8, twelve slots each; edge roles 0..14 start at 96.
pub(super) fn ins(role: usize, field: usize) -> usize {
    role * 12 + field
}
pub(super) fn edge(role: usize, field: usize) -> usize {
    96 + role * 12 + field
}
pub(super) fn invalid(message: &str) -> FormatError {
    FormatError::InvalidChangeMetadata {
        reason: format!("graph fields: {message}"),
    }
}
pub(super) fn charge(budget: &mut usize, count: usize, cost: usize) -> FormatResult<()> {
    *budget = count
        .checked_mul(cost)
        .and_then(|n| budget.checked_sub(n))
        .ok_or(FormatError::GraphFieldLimitExceeded)?;
    Ok(())
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
#[derive(Serialize)]
pub(super) struct Column {
    pub role: u16,
    pub count: u32,
    pub constant: Option<u64>,
    pub data: Vec<u8>,
}
#[derive(Serialize)]
pub(super) struct Payload<'a> {
    pub path: &'a str,
    pub start: u64,
    pub end: u64,
    pub control: Vec<u8>,
    pub columns: Vec<Column>,
    pub dictionary: Vec<String>,
    pub aux: Vec<u8>,
}
