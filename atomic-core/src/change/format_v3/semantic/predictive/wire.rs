use serde::{Deserialize, Serialize};
pub(super) const MAGIC: &[u8; 5] = b"ATSC\x02";
pub(super) const PREDICTOR: u8 = 1;
pub(in crate::change::format_v3::semantic) const FILES: usize = 0;
pub(in crate::change::format_v3::semantic) const LINES: usize = 1;
pub(in crate::change::format_v3::semantic) const OLD: usize = 2;
pub(in crate::change::format_v3::semantic) const NEW: usize = 3;
pub(in crate::change::format_v3::semantic) const RANGE: usize = 4;
pub(in crate::change::format_v3::semantic) const BRANCH: usize = 5;
pub(in crate::change::format_v3::semantic) const BRANCH_AFTER: usize = 6;
pub(in crate::change::format_v3::semantic) const TOKENS: usize = 7;
pub(in crate::change::format_v3::semantic) const LEAF: usize = 8;
pub(in crate::change::format_v3::semantic) const LEAF_AFTER: usize = 9;
pub(in crate::change::format_v3::semantic) const REFERENCE: usize = 10;

#[derive(Serialize)]
pub(super) struct Payload {
    pub predictor: u8,
    pub content_hash: [u8; 32],
    pub metadata: Vec<u8>,
    pub controls: [Vec<u8>; 11],
    pub branches: Vec<u8>,
    pub leaves: Vec<u8>,
    pub old_numbers: Vec<u8>,
    pub new_numbers: Vec<u8>,
    pub positions: Vec<u8>,
    pub references: Vec<u8>,
    pub literals: Vec<u8>,
    pub length_count: u32,
    pub length_mask: Vec<u8>,
    pub length_exceptions: Vec<u8>,
    pub kind_count: u32,
    pub kind_mask: Vec<u8>,
    pub kind_exceptions: Vec<u8>,
}
#[derive(Deserialize)]
pub(super) struct Borrowed<'a> {
    pub predictor: u8,
    pub content_hash: [u8; 32],
    #[serde(borrow)]
    pub metadata: &'a [u8],
    #[serde(borrow)]
    pub controls: [&'a [u8]; 11],
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
    pub references: &'a [u8],
    #[serde(borrow)]
    pub literals: &'a [u8],
    pub length_count: u32,
    #[serde(borrow)]
    pub length_mask: &'a [u8],
    #[serde(borrow)]
    pub length_exceptions: &'a [u8],
    pub kind_count: u32,
    #[serde(borrow)]
    pub kind_mask: &'a [u8],
    #[serde(borrow)]
    pub kind_exceptions: &'a [u8],
}
