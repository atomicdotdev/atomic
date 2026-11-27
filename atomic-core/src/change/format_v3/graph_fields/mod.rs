//! Frozen graph-field codec v1. Field roles, not Rust field names, determine
//! independent delta histories. No graph operations are combined or rewritten.
mod decode;
mod encode;
mod raw;
#[cfg(test)]
mod tests;
mod wire;
use super::{FormatError, FormatResult, GraphSectionPayload};
pub use decode::decode;
pub use encode::encode;
pub use raw::decode as decode_raw;

/// Retained on decode so historical graph payloads are never silently upgraded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GraphEncoding {
    #[default]
    /// New objects compare compressed graph fields against raw graph payloads.
    Auto,
    /// Historical postcard graph payload.
    Raw,
    /// Frozen field-role graph payload v1.
    FieldsV1,
}
pub(crate) struct SelectedGraph {
    pub fields: bool,
    pub compressed: Vec<u8>,
    pub uncompressed_len: usize,
    pub fallback: Option<Box<SelectedGraph>>,
}
pub(crate) fn select(
    graph: &GraphSectionPayload,
    policy: GraphEncoding,
) -> FormatResult<SelectedGraph> {
    if policy == GraphEncoding::FieldsV1 {
        return compressed_fields(graph);
    }
    let raw = graph.to_postcard_bytes()?;
    let compressed =
        zstd::encode_all(raw.as_slice(), 3).map_err(|e| FormatError::Compress(e.to_string()))?;
    let baseline = SelectedGraph {
        fields: false,
        compressed,
        uncompressed_len: raw.len(),
        fallback: None,
    };
    if policy == GraphEncoding::Raw || raw.len() > wire::MAX {
        return Ok(baseline);
    }
    drop(raw);
    let fields = match compressed_fields(graph) {
        Ok(fields) => fields,
        Err(FormatError::GraphFieldLimitExceeded | FormatError::UnsupportedGraphFieldOp) => {
            return Ok(baseline)
        }
        Err(error) => return Err(error),
    };
    if fields.compressed.len() < baseline.compressed.len() {
        let mut fields = fields;
        fields.fallback = Some(Box::new(baseline));
        Ok(fields)
    } else {
        Ok(baseline)
    }
}
fn compressed_fields(graph: &GraphSectionPayload) -> FormatResult<SelectedGraph> {
    let payload = encode(graph)?;
    let compressed = zstd::encode_all(payload.as_slice(), 3)
        .map_err(|e| FormatError::Compress(e.to_string()))?;
    if compressed.len() > wire::MAX {
        return Err(FormatError::GraphFieldLimitExceeded);
    }
    Ok(SelectedGraph {
        fields: true,
        compressed,
        uncompressed_len: payload.len(),
        fallback: None,
    })
}
