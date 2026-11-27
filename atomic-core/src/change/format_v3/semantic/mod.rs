//! Lossless semantic payloads for schemas 4 and 5. Content references address authored
//! bytes in the same change; missing old/deleted text stays in the literal pool.
//! Decoding requires no repository database or moving tokenizer implementation;
//! predictive v2 retains its frozen ASCII predictor and all authored exceptions.

mod decode;
mod encode;
mod predictive;
#[cfg(test)]
pub(super) mod tests;
mod wire;

pub use decode::decode;
pub use encode::encode;
pub(crate) fn wire_limit() -> usize {
    wire::MAX_PAYLOAD
}
use super::{FormatError, FormatResult};
use crate::change::FileOps;

/// Preserve the representation when loading an existing object. Changing this
/// policy explicitly creates a new object identity and requires re-signing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SemanticEncoding {
    /// New objects choose the smaller complete compressed representation.
    #[default]
    Auto,
    /// Original expanded postcard operations (schema 2 or 3).
    Raw,
    /// Versioned columnar operations (schema 4).
    Compact,
    /// Accepted mixed-ID column layout, retained only for faithful re-encoding.
    /// New automatic writers always select the separated layout instead.
    CompactMixed,
    /// Frozen predictive ATSC v2, predictor 1; decoded directly into FileOps.
    PredictiveV2,
}

pub(crate) fn payload_encoding(payload: &[u8]) -> FormatResult<SemanticEncoding> {
    if payload.starts_with(b"ATSC\x02") {
        if payload.get(5) != Some(&1) {
            return Err(wire::invalid("unknown predictive semantic algorithm"));
        }
        return Ok(SemanticEncoding::PredictiveV2);
    }
    let bytes = payload
        .strip_prefix(wire::MAGIC)
        .ok_or_else(|| wire::invalid("unknown magic/version"))?;
    match bytes.first() {
        Some(1) => Ok(SemanticEncoding::Compact),
        Some(0) => Ok(SemanticEncoding::CompactMixed),
        _ => Err(wire::invalid("invalid layout discriminator")),
    }
}

pub(crate) struct SelectedSemantic {
    pub compact: bool,
    pub predictive: bool,
    pub compressed: Vec<u8>,
    pub uncompressed_len: usize,
    pub fallback: Option<Box<SelectedSemantic>>,
}

pub(crate) fn select(
    ops: &[FileOps],
    content: &[u8],
    policy: SemanticEncoding,
) -> FormatResult<SelectedSemantic> {
    if policy == SemanticEncoding::PredictiveV2 {
        return compressed_predictive(ops, content);
    }
    if matches!(
        policy,
        SemanticEncoding::Compact | SemanticEncoding::CompactMixed
    ) {
        return compressed_compact(ops, content, policy != SemanticEncoding::CompactMixed);
    }
    let raw = postcard::to_allocvec(ops)?;
    let compressed =
        zstd::encode_all(raw.as_slice(), 3).map_err(|e| FormatError::Compress(e.to_string()))?;
    let baseline = SelectedSemantic {
        compact: false,
        predictive: false,
        compressed,
        uncompressed_len: raw.len(),
        fallback: None,
    };
    // Do not impose compact-format limits on historical raw objects. Oversized
    // new objects likewise keep the raw representation rather than losing data.
    if policy == SemanticEncoding::Raw
        || (policy == SemanticEncoding::Auto
            && (raw.len() > wire::MAX_PAYLOAD || content.len() > wire::MAX_PAYLOAD))
    {
        return Ok(baseline);
    }
    drop(raw);
    let compact = match compressed_compact(ops, content, true) {
        Ok(payload) => payload,
        Err(FormatError::SemanticLimitExceeded) if policy == SemanticEncoding::Auto => {
            return Ok(baseline)
        }
        Err(error) => return Err(error),
    };
    let baseline = if compact.compressed.len() < baseline.compressed.len() {
        compact
    } else {
        baseline
    };
    match compressed_predictive(ops, content) {
        Ok(mut p) if p.compressed.len() < baseline.compressed.len() => {
            p.fallback = Some(Box::new(baseline));
            Ok(p)
        }
        Ok(_) | Err(FormatError::SemanticLimitExceeded) => Ok(baseline),
        Err(e) => Err(e),
    }
}

fn compressed_predictive(ops: &[FileOps], content: &[u8]) -> FormatResult<SelectedSemantic> {
    let payload = predictive::encode(ops, content)?;
    let compressed = zstd::encode_all(payload.as_slice(), 3)
        .map_err(|e| FormatError::Compress(e.to_string()))?;
    if compressed.len() > wire::MAX_PAYLOAD {
        return Err(FormatError::SemanticLimitExceeded);
    }
    Ok(SelectedSemantic {
        compact: true,
        predictive: true,
        compressed,
        uncompressed_len: payload.len(),
        fallback: None,
    })
}

fn compressed_compact(
    ops: &[FileOps],
    content: &[u8],
    split: bool,
) -> FormatResult<SelectedSemantic> {
    let payload = encode::encode_layout(ops, content, split)?;
    let compressed = zstd::encode_all(payload.as_slice(), 3)
        .map_err(|e| FormatError::Compress(e.to_string()))?;
    if compressed.len() > wire::MAX_PAYLOAD {
        return Err(FormatError::SemanticLimitExceeded);
    }
    Ok(SelectedSemantic {
        compact: true,
        predictive: false,
        compressed,
        uncompressed_len: payload.len(),
        fallback: None,
    })
}
