//! Authenticated schema-5 codec manifest. File headers are not part of the
//! historical change hash; this section binds their interpretation to it.
use super::{FileHeader, FileHeaderFlags, FormatError, FormatResult, FIELD_CODEC_FORMAT_VERSION};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodecDescriptor {
    pub graph_fields: bool,
    /// 0 = raw, 1 = ATSC v1, 2 = predictive ATSC v2.
    pub semantic: u8,
    pub span: bool,
}
impl CodecDescriptor {
    pub fn to_bytes(self) -> [u8; 4] {
        [
            5,
            u8::from(self.graph_fields),
            self.semantic,
            u8::from(self.span),
        ]
    }
    pub fn from_bytes(bytes: &[u8]) -> FormatResult<Self> {
        if bytes.len() != 4 || bytes[0] != 5 || bytes[1] > 1 || bytes[2] > 2 || bytes[3] > 1 {
            return Err(FormatError::InvalidChangeMetadata {
                reason: "invalid authenticated codec descriptor".into(),
            });
        }
        let value = Self {
            graph_fields: bytes[1] == 1,
            semantic: bytes[2],
            span: bytes[3] == 1,
        };
        if !value.graph_fields && value.semantic != 2 {
            return Err(FormatError::InvalidChangeMetadata {
                reason: "schema 5 requires a new codec".into(),
            });
        }
        Ok(value)
    }
    pub fn from_header(header: &FileHeader) -> FormatResult<Self> {
        header.validate()?;
        if header.version != FIELD_CODEC_FORMAT_VERSION {
            return Err(FormatError::InvalidHeader {
                reason: "CODECS requires schema 5".into(),
            });
        }
        Self::from_bytes(&[
            5,
            u8::from(header.has_graph_fields()),
            if header.has_predictive_semantics() {
                2
            } else if header.has_compact_semantics() {
                1
            } else {
                0
            },
            u8::from(header.has_span()),
        ])
    }
    pub fn apply(self, header: &mut FileHeader) -> FormatResult<()> {
        Self::from_bytes(&self.to_bytes())?;
        header.version = FIELD_CODEC_FORMAT_VERSION;
        let mut bits = header.flags.to_raw()
            & !(FileHeaderFlags::HAS_SPAN
                | FileHeaderFlags::GRAPH_FIELDS
                | FileHeaderFlags::PREDICTIVE_SEMANTICS
                | FileHeaderFlags::COMPACT_SEMANTICS);
        if self.span {
            bits |= FileHeaderFlags::HAS_SPAN;
        }
        if self.graph_fields {
            bits |= FileHeaderFlags::GRAPH_FIELDS;
        }
        if self.semantic > 0 {
            bits |= FileHeaderFlags::COMPACT_SEMANTICS;
        }
        if self.semantic == 2 {
            bits |= FileHeaderFlags::PREDICTIVE_SEMANTICS;
        }
        header.flags = FileHeaderFlags::from_raw(bits);
        header.validate()
    }
}
