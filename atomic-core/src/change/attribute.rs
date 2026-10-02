//! Canonical graph-backed inode attributes.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The materialized kind of a non-directory inode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum InodeKind {
    /// A regular byte file.
    Regular = 0,
    /// A symbolic link whose target is stored as file content.
    Symlink = 1,
    /// A Git submodule link whose object ID is stored as file content.
    Gitlink = 2,
}

impl InodeKind {
    /// Decode the canonical one-byte representation.
    pub const fn from_byte(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Regular),
            1 => Some(Self::Symlink),
            2 => Some(Self::Gitlink),
            _ => None,
        }
    }

    /// Return the canonical one-byte representation.
    pub const fn as_byte(self) -> u8 {
        self as u8
    }
}

impl fmt::Display for InodeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Regular => "regular",
            Self::Symlink => "symlink",
            Self::Gitlink => "gitlink",
        })
    }
}

/// The name of a graph-backed inode attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum InodeAttrName {
    /// Unix permission bits, excluding file-type and set-id bits.
    Mode = 0,
    /// Non-directory inode kind.
    Kind = 1,
}

impl InodeAttrName {
    pub const fn from_byte(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Mode),
            1 => Some(Self::Kind),
            _ => None,
        }
    }

    pub const fn as_byte(self) -> u8 {
        self as u8
    }
}

/// A canonical graph-backed inode attribute value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum InodeAttr {
    /// Unix permission bits in the inclusive range `0o000..=0o777`.
    Mode(u16),
    /// The inode's materialized kind.
    Kind(InodeKind),
}

impl InodeAttr {
    pub const fn name(self) -> InodeAttrName {
        match self {
            Self::Mode(_) => InodeAttrName::Mode,
            Self::Kind(_) => InodeAttrName::Kind,
        }
    }

    /// Validate that this value has a supported canonical representation.
    pub fn validate(self) -> Result<(), AttrValueError> {
        match self {
            Self::Mode(mode) if mode > 0o777 => Err(AttrValueError::InvalidMode(mode)),
            Self::Mode(_) | Self::Kind(_) => Ok(()),
        }
    }

    /// Encode the canonical value as `(length, bytes)`.
    pub fn canonical_bytes(self) -> Result<(u8, [u8; 2]), AttrValueError> {
        self.validate()?;
        Ok(match self {
            Self::Mode(mode) => (2, mode.to_le_bytes()),
            Self::Kind(kind) => (1, [kind.as_byte(), 0]),
        })
    }

    /// Decode and validate a canonical value.
    pub fn from_canonical(name: InodeAttrName, bytes: &[u8]) -> Result<Self, AttrValueError> {
        let value = match name {
            InodeAttrName::Mode if bytes.len() == 2 => {
                Self::Mode(u16::from_le_bytes([bytes[0], bytes[1]]))
            }
            InodeAttrName::Kind if bytes.len() == 1 => Self::Kind(
                InodeKind::from_byte(bytes[0]).ok_or(AttrValueError::UnsupportedKind(bytes[0]))?,
            ),
            _ => {
                return Err(AttrValueError::InvalidLength {
                    name,
                    actual: bytes.len(),
                });
            }
        };
        value.validate()?;
        Ok(value)
    }
}

/// Rejection reason for a malformed or unsupported attribute value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttrValueError {
    InvalidMode(u16),
    UnsupportedKind(u8),
    InvalidLength { name: InodeAttrName, actual: usize },
}

impl fmt::Display for AttrValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMode(mode) => write!(f, "mode {mode:#o} contains unsupported bits"),
            Self::UnsupportedKind(kind) => write!(f, "unsupported inode kind {kind}"),
            Self::InvalidLength { name, actual } => {
                write!(f, "invalid canonical length {actual} for {name:?}")
            }
        }
    }
}

impl std::error::Error for AttrValueError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_values_roundtrip() {
        for value in [
            InodeAttr::Mode(0o755),
            InodeAttr::Kind(InodeKind::Regular),
            InodeAttr::Kind(InodeKind::Symlink),
            InodeAttr::Kind(InodeKind::Gitlink),
        ] {
            let (len, bytes) = value.canonical_bytes().unwrap();
            assert_eq!(
                InodeAttr::from_canonical(value.name(), &bytes[..usize::from(len)]).unwrap(),
                value
            );
        }
    }

    #[test]
    fn malformed_values_are_rejected() {
        assert!(InodeAttr::Mode(0o1000).validate().is_err());
        assert!(InodeAttr::from_canonical(InodeAttrName::Kind, &[3]).is_err());
        assert!(InodeAttr::from_canonical(InodeAttrName::Mode, &[0o7]).is_err());
    }
}
