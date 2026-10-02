//! Stable identity for one Atomic working directory or Git worktree.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Stable ULID identity for one working directory or linked Git worktree.
///
/// The canonical database representation is the 16-byte ULID value. Text files
/// and user-facing APIs use the canonical 26-character Crockford Base32 form.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkingCopyId([u8; 16]);

impl WorkingCopyId {
    /// Encoded size of a ULID.
    pub const SIZE: usize = 16;

    /// Generate a fresh time-sortable working-copy identity.
    ///
    /// There is deliberately no `Default`: callers must make identity creation
    /// explicit rather than accidentally allocating or accepting a sentinel ID.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self::from(ulid::Ulid::new())
    }

    /// Construct an identity from its canonical 16-byte representation.
    #[inline]
    pub const fn from_bytes(bytes: [u8; Self::SIZE]) -> Self {
        Self(bytes)
    }

    /// Borrow the canonical 16-byte representation.
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; Self::SIZE] {
        &self.0
    }

    /// Convert this identity to the underlying ULID value.
    #[inline]
    pub fn as_ulid(self) -> ulid::Ulid {
        ulid::Ulid::from_bytes(self.0)
    }
}

impl From<ulid::Ulid> for WorkingCopyId {
    fn from(value: ulid::Ulid) -> Self {
        Self(value.to_bytes())
    }
}

impl From<WorkingCopyId> for ulid::Ulid {
    fn from(value: WorkingCopyId) -> Self {
        value.as_ulid()
    }
}

impl FromStr for WorkingCopyId {
    type Err = ulid::DecodeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse::<ulid::Ulid>().map(Self::from)
    }
}

impl fmt::Display for WorkingCopyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_ulid().fmt(f)
    }
}

impl fmt::Debug for WorkingCopyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("WorkingCopyId")
            .field(&self.to_string())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KNOWN_ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    #[test]
    fn working_copy_id_roundtrips_canonical_text_and_bytes() {
        let parsed: WorkingCopyId = KNOWN_ULID.parse().unwrap();
        assert_eq!(parsed.to_string(), KNOWN_ULID);
        assert_eq!(WorkingCopyId::from_bytes(*parsed.as_bytes()), parsed);
        assert_eq!(WorkingCopyId::from(parsed.as_ulid()), parsed);
    }

    #[test]
    fn working_copy_id_accepts_case_insensitive_input_and_canonicalizes_output() {
        let parsed: WorkingCopyId = KNOWN_ULID.to_ascii_lowercase().parse().unwrap();
        assert_eq!(parsed.to_string(), KNOWN_ULID);
    }

    #[test]
    fn working_copy_id_rejects_invalid_text() {
        assert!("not-a-ulid".parse::<WorkingCopyId>().is_err());
        assert!("".parse::<WorkingCopyId>().is_err());
    }

    #[test]
    fn generated_working_copy_ids_are_distinct() {
        assert_ne!(WorkingCopyId::new(), WorkingCopyId::new());
    }
}
