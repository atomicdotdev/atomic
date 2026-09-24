//! Persistent, mutable ref/view mappings (RFC §8.1, CB-10A).
//!
//! A [`RefMapping`] records the last mutually observed state of one Atomic
//! view ↔ Git ref pair so reconciliation can decide which side advanced
//! instead of guessing from ref names or current tips alone:
//!
//! - it is keyed by the **durable internal view id**, never by the view name
//!   (renames must not silently fork a mapping);
//! - it is versioned bytes so old rows keep decoding as the record evolves;
//! - mappings are mutable bookkeeping, kept strictly separate from immutable
//!   Git state bindings (RFC §5.1): a mapping row never proves identity —
//!   only the signed bindings and the recorded closures do (CB-10A constraint:
//!   "SetId, ref names and similarity alone cannot prove containment").
//!
//! The table is repository-global (shared by linked worktrees), which is what
//! makes cross-worktree divergence observable.

use super::{PristineError, PristineResult, ViewScope};

/// Serialized-record version for [`RefMapping`].
///
/// v2 appends the state-bound export field (`last_exported_state`) after the
/// v1 layout; v1 rows keep decoding with that field unbound (CB-10A review
/// R1: a cached export must be bound to the exact exported state to prove
/// anything).
pub const REF_MAPPING_VERSION: u8 = 2;

/// Reconciliation status of one mapped ref (RFC §8.1/§8.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefSyncStatus {
    /// Both sides sit at the last mutually observed values.
    Synchronized,
    /// The Git ref advanced beyond `last_observed_local`; Atomic has not.
    GitAhead,
    /// The Atomic view advanced beyond `last_observed_atomic`; Git has not.
    AtomicAhead,
    /// Both sides moved incompatibly; neither may be moved automatically.
    Diverged,
    /// The mapping cannot be represented on Git (deleted Shared view, or a
    /// view that legitimately has no Git ref of its own).
    Unrepresentable,
}

impl RefSyncStatus {
    /// Decode from the versioned byte encoding.
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Synchronized),
            1 => Some(Self::GitAhead),
            2 => Some(Self::AtomicAhead),
            3 => Some(Self::Diverged),
            4 => Some(Self::Unrepresentable),
            _ => None,
        }
    }

    /// Encode for the versioned byte encoding.
    pub fn to_u8(self) -> u8 {
        match self {
            Self::Synchronized => 0,
            Self::GitAhead => 1,
            Self::AtomicAhead => 2,
            Self::Diverged => 3,
            Self::Unrepresentable => 4,
        }
    }
}

impl std::fmt::Display for RefSyncStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Synchronized => "synchronized",
            Self::GitAhead => "git-ahead",
            Self::AtomicAhead => "atomic-ahead",
            Self::Diverged => "diverged",
            Self::Unrepresentable => "unrepresentable",
        };
        f.write_str(name)
    }
}

/// One persistent view ↔ Git ref mapping (RFC §8.1).
///
/// Hex Git oids are stored as strings (Git objects are hash-algorithm
/// agnostic here); Atomic states are the base32 Merkle of the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefMapping {
    /// Record encoding version (currently [`REF_MAPPING_VERSION`]).
    pub version: u8,
    /// Durable internal view id — the table key.
    pub view_id: u64,
    /// View name at mapping time (display/audit only; the key is `view_id`).
    pub view_name: String,
    /// [`super::traits::ViewScope`] of the view when the mapping was written.
    pub scope: u8,
    /// The Git ref this view maps to, when it has one.
    pub local_ref: Option<String>,
    /// Optional `(remote-name, remote-ref)` tracking pair.
    pub remote: Option<(String, String)>,
    /// Last local ref tip both sides observed (hex Git oid).
    pub last_observed_local: Option<String>,
    /// Last remote ref tip both sides observed (hex Git oid).
    pub last_observed_remote: Option<String>,
    /// The local ref tip this view was last exported to (hex Git oid).
    pub last_exported: Option<String>,
    /// The Atomic state (base32 Merkle) the last export was verified against.
    ///
    /// v2 (CB-10A review R1): `last_exported` alone is only a hint; it proves
    /// "Git contains Atomic's current projection" solely when paired with the
    /// exact state it was exported at. `None` means the export is unbound and
    /// can never prove containment.
    pub last_exported_state: Option<String>,
    /// The Atomic view state at the last observation (base32 Merkle).
    pub last_observed_atomic: Option<String>,
    /// Current reconciliation status.
    pub status: RefSyncStatus,
}

impl RefMapping {
    /// Encode with the versioned layout. The version byte leads the value so
    /// future formats can be detected instead of misdecoded; v2 appends the
    /// state-bound export field after the v1 layout.
    ///
    /// CB-10A review R7: the encoder validates its own fields — the scope
    /// byte must be a real [`ViewScope`], oid fields must be hex of the
    /// known Git digest widths, state fields must be base32, and ref names
    /// must be well-formed — so a malformed row can never enter storage
    /// through this build.
    pub fn encode(&self) -> PristineResult<Vec<u8>> {
        if self.version != REF_MAPPING_VERSION {
            return Err(PristineError::Serialization {
                message: format!(
                    "cannot encode ref mapping version {} (this build supports {REF_MAPPING_VERSION})",
                    self.version
                ),
            });
        }
        if ViewScope::from_u8(self.scope).is_none() {
            return Err(PristineError::Serialization {
                message: format!(
                    "ref mapping scope byte {} is not a known ViewScope",
                    self.scope
                ),
            });
        }
        if self.view_name.is_empty() || self.view_name.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(PristineError::Serialization {
                message: "ref mapping view name is empty or contains control characters"
                    .to_string(),
            });
        }
        validate_ref_name(self.local_ref.as_deref())?;
        if let Some((remote, reference)) = &self.remote {
            validate_ref_name(Some(reference))?;
            if remote.is_empty() || remote.bytes().any(|b| b < 0x20 || b == 0x7f) {
                return Err(PristineError::Serialization {
                    message: "ref mapping remote name is empty or contains control characters"
                        .to_string(),
                });
            }
        }
        for oid in [
            self.last_observed_local.as_deref(),
            self.last_observed_remote.as_deref(),
            self.last_exported.as_deref(),
        ] {
            validate_oid(oid)?;
        }
        for state in [
            self.last_exported_state.as_deref(),
            self.last_observed_atomic.as_deref(),
        ] {
            validate_state(state)?;
        }
        let mut bytes = Vec::with_capacity(128);
        bytes.push(self.version);
        bytes.extend_from_slice(&self.view_id.to_le_bytes());
        encode_len_prefixed(&mut bytes, self.view_name.as_bytes())?;
        bytes.push(self.scope);
        encode_option_ref(&mut bytes, self.local_ref.as_deref())?;
        match &self.remote {
            None => bytes.push(0),
            Some((remote, reference)) => {
                bytes.push(1);
                encode_len_prefixed(&mut bytes, remote.as_bytes())?;
                encode_len_prefixed(&mut bytes, reference.as_bytes())?;
            }
        }
        encode_option_ref(&mut bytes, self.last_observed_local.as_deref())?;
        encode_option_ref(&mut bytes, self.last_observed_remote.as_deref())?;
        encode_option_ref(&mut bytes, self.last_exported.as_deref())?;
        encode_option_ref(&mut bytes, self.last_observed_atomic.as_deref())?;
        bytes.push(self.status.to_u8());
        // v2: the state the last export was bound to (appended after v1).
        encode_option_ref(&mut bytes, self.last_exported_state.as_deref())?;
        Ok(bytes)
    }

    /// Decode with strict bounds checking; unknown versions fail closed, the
    /// record must end exactly at the layout end (no trailing bytes), and a
    /// v1 row decodes with the v2 export state unbound.
    pub fn decode(bytes: &[u8]) -> PristineResult<Self> {
        let &version = bytes.first().ok_or(decode_error())?;
        // CB-10A review R7: version 0 never existed as a written layout — a
        // zero first byte is corruption (or a zeroed region), not a
        // decodable record.
        if version == 0 || version > REF_MAPPING_VERSION {
            return Err(PristineError::Serialization {
                message: format!(
                    "ref mapping record version {version} is not supported (this build supports {REF_MAPPING_VERSION})"
                ),
            });
        }
        let mut cursor = 1;
        let mut take = |count: usize| -> Result<&[u8], PristineError> {
            if cursor + count > bytes.len() {
                return Err(decode_error());
            }
            let slice = &bytes[cursor..cursor + count];
            cursor += count;
            Ok(slice)
        };
        let view_id = u64::from_le_bytes(take(8)?.try_into().expect("fixed-size u64"));
        let view_name = take_string(&mut take)?;
        let scope = take(1)?[0];
        if ViewScope::from_u8(scope).is_none() {
            return Err(decode_error());
        }
        let local_ref = take_option_ref(&mut take)?;
        let remote = match take(1)?[0] {
            0 => None,
            1 => {
                let remote_name = take_string(&mut take)?;
                let remote_ref = take_string(&mut take)?;
                Some((remote_name, remote_ref))
            }
            _ => return Err(decode_error()),
        };
        let last_observed_local = take_option_ref(&mut take)?;
        let last_observed_remote = take_option_ref(&mut take)?;
        let last_exported = take_option_ref(&mut take)?;
        let last_observed_atomic = take_option_ref(&mut take)?;
        let status = RefSyncStatus::from_u8(take(1)?[0]).ok_or(decode_error())?;
        let last_exported_state = if version >= 2 {
            take_option_ref(&mut take)?
        } else {
            None
        };
        if cursor != bytes.len() {
            // Trailing bytes mean the record is not the encoding this build
            // wrote: refuse rather than silently ignoring appended data.
            return Err(decode_error());
        }
        // CB-10A review R7: the decoded strings are validated with the same
        // rules the encoder enforces, so a corrupted-but-well-formed row is
        // rejected at the boundary instead of flowing into reconciliation.
        if view_name.is_empty() || view_name.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(decode_error());
        }
        validate_ref_name(local_ref.as_deref())?;
        if let Some((remote_name, remote_ref)) = &remote {
            validate_ref_name(Some(remote_ref))?;
            if remote_name.is_empty() || remote_name.bytes().any(|b| b < 0x20 || b == 0x7f) {
                return Err(decode_error());
            }
        }
        for oid in [
            last_observed_local.as_deref(),
            last_observed_remote.as_deref(),
            last_exported.as_deref(),
        ] {
            validate_oid(oid)?;
        }
        for state in [
            last_exported_state.as_deref(),
            last_observed_atomic.as_deref(),
        ] {
            validate_state(state)?;
        }
        Ok(Self {
            version,
            view_id,
            view_name,
            scope,
            local_ref,
            remote,
            last_observed_local,
            last_observed_remote,
            last_exported,
            last_exported_state,
            last_observed_atomic,
            status,
        })
    }
}

/// A Git oid field: 40 (SHA-1) or 64 (SHA-256) lowercase hex characters.
fn validate_oid(value: Option<&str>) -> PristineResult<()> {
    let Some(value) = value else {
        return Ok(());
    };
    let valid_len = value.len() == 40 || value.len() == 64;
    let hex = value.bytes().all(|b| b.is_ascii_hexdigit())
        && !value.bytes().any(|b| b.is_ascii_uppercase());
    if !valid_len || !hex {
        return Err(PristineError::Serialization {
            message: format!(
                "ref mapping oid field is not a 40/64-character lowercase hex Git oid: {value:?}"
            ),
        });
    }
    Ok(())
}

/// An Atomic state field: non-empty base32 (the Merkle's base32 encoding).
fn validate_state(value: Option<&str>) -> PristineResult<()> {
    let Some(value) = value else {
        return Ok(());
    };
    let base32 = !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b));
    if !base32 {
        return Err(PristineError::Serialization {
            message: format!(
                "ref mapping state field is not non-empty uppercase base32: {value:?}"
            ),
        });
    }
    Ok(())
}

/// A mapped ref name: well-formed Git reference name under `refs/`
/// (git-check-ref-format's exclusions, simplified to the durable subset).
fn validate_ref_name(value: Option<&str>) -> PristineResult<()> {
    let Some(value) = value else {
        return Ok(());
    };
    let forbidden = [b'~', b'^', b':', b'?', b'*', b'[', b'\\', 0x7f];
    let well_formed = value.starts_with("refs/")
        && !value.ends_with('/')
        && !value.ends_with(".")
        && !value.contains("..")
        && !value.contains("//")
        && !value.contains("@{")
        && !value.ends_with(".lock")
        && value
            .bytes()
            .all(|b| b.is_ascii_graphic() && !forbidden.contains(&b));
    if !well_formed {
        return Err(PristineError::Serialization {
            message: format!("ref mapping ref name is not a well-formed Git reference: {value:?}"),
        });
    }
    Ok(())
}

fn encode_len_prefixed(bytes: &mut Vec<u8>, value: &[u8]) -> PristineResult<()> {
    if value.len() > u16::MAX as usize {
        return Err(PristineError::Serialization {
            message: format!(
                "ref mapping field of {} bytes exceeds the u16-length encoding limit",
                value.len()
            ),
        });
    }
    bytes.extend_from_slice(&(value.len() as u16).to_le_bytes());
    bytes.extend_from_slice(value);
    Ok(())
}

fn encode_option_ref(bytes: &mut Vec<u8>, value: Option<&str>) -> PristineResult<()> {
    match value {
        None => bytes.push(0),
        Some(value) => {
            bytes.push(1);
            encode_len_prefixed(bytes, value.as_bytes())?;
        }
    }
    Ok(())
}

type Take<'a> = dyn FnMut(usize) -> Result<&'a [u8], PristineError> + 'a;

fn take_string<'a>(take: &mut Take<'a>) -> Result<String, PristineError> {
    let length = u16::from_le_bytes(take(2)?.try_into().expect("fixed-size u16")) as usize;
    let slice = take(length)?;
    String::from_utf8(slice.to_vec()).map_err(|_| decode_error())
}

fn take_option_ref<'a>(take: &mut Take<'a>) -> Result<Option<String>, PristineError> {
    match take(1)?[0] {
        0 => Ok(None),
        1 => Some(take_string(take)).transpose(),
        _ => Err(decode_error()),
    }
}

fn decode_error() -> PristineError {
    PristineError::Serialization {
        message: "ref mapping record is truncated or malformed".to_string(),
    }
}

/// Read access to persistent ref mappings (CB-10A).
pub trait RefMappingTxnT {
    /// The versioned mapping bytes for `view_id`, or `None`.
    fn get_ref_mapping_bytes(&self, view_id: u64) -> PristineResult<Option<Vec<u8>>>;

    /// Every stored mapping as `(view_id, bytes)` in view-id order.
    fn iter_ref_mapping_bytes(&self) -> PristineResult<Vec<(u64, Vec<u8>)>>;
}

/// Mutable access to persistent ref mappings (CB-10A).
pub trait RefMappingMutTxnT: RefMappingTxnT {
    /// Upsert the versioned mapping bytes for `view_id`.
    fn put_ref_mapping_bytes(&mut self, view_id: u64, bytes: &[u8]) -> PristineResult<()>;

    /// Remove the mapping row for `view_id`; returns whether one existed.
    fn del_ref_mapping(&mut self, view_id: u64) -> PristineResult<bool>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(view_id: u64) -> RefMapping {
        RefMapping {
            version: REF_MAPPING_VERSION,
            view_id,
            view_name: "main".to_string(),
            scope: 1,
            local_ref: Some("refs/heads/main".to_string()),
            remote: Some(("origin".to_string(), "refs/heads/main".to_string())),
            last_observed_local: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
            last_observed_remote: None,
            last_exported: Some("89abcdef0123456789abcdef0123456789abcdef".to_string()),
            last_exported_state: Some(
                "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB".to_string(),
            ),
            last_observed_atomic: Some(
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string(),
            ),
            status: RefSyncStatus::Synchronized,
        }
    }

    #[test]
    fn ref_mapping_roundtrips_all_fields() {
        let mapping = sample(7);
        let decoded = RefMapping::decode(&mapping.encode().unwrap()).unwrap();
        assert_eq!(decoded, mapping);
    }

    #[test]
    fn ref_mapping_roundtrips_absent_optionals() {
        let mut mapping = sample(3);
        mapping.local_ref = None;
        mapping.remote = None;
        mapping.last_observed_local = None;
        mapping.last_exported = None;
        mapping.last_exported_state = None;
        mapping.last_observed_atomic = None;
        mapping.status = RefSyncStatus::Unrepresentable;
        let decoded = RefMapping::decode(&mapping.encode().unwrap()).unwrap();
        assert_eq!(decoded, mapping);
    }

    #[test]
    fn ref_mapping_decode_rejects_unknown_version() {
        let mut bytes = sample(1).encode().unwrap();
        bytes[0] = 99;
        assert!(RefMapping::decode(&bytes).is_err());
    }

    #[test]
    fn ref_mapping_decode_rejects_truncation() {
        let bytes = sample(5).encode().unwrap();
        assert!(RefMapping::decode(&bytes[..bytes.len() - 1]).is_err());
        assert!(RefMapping::decode(&bytes[..2]).is_err());
    }

    #[test]
    fn ref_mapping_decode_rejects_trailing_bytes() {
        // Appended data after the v2 layout is corruption, not extra state:
        // the decode must refuse instead of silently ignoring it.
        let mut bytes = sample(5).encode().unwrap();
        bytes.extend_from_slice(b"junk");
        assert!(RefMapping::decode(&bytes).is_err());
    }

    #[test]
    fn ref_mapping_v1_rows_decode_with_unbound_export_state() {
        // A v1 row (previous implementation) keeps decoding: the layout ends
        // at the status byte and the export state stays unbound.
        let mapping = sample(9);
        let state_len = mapping.last_exported_state.as_ref().expect("bound").len();
        let mut bytes = mapping.encode().unwrap();
        // Drop the appended v2 option field (1 flag + u16 length + state).
        let v2_tail = 1 + 2 + state_len;
        bytes[0] = 1;
        bytes.truncate(bytes.len() - v2_tail);
        let decoded = RefMapping::decode(&bytes).expect("v1 row");
        assert_eq!(decoded.version, 1);
        assert!(decoded.last_exported_state.is_none());
        assert_eq!(
            decoded.last_exported.as_deref(),
            Some("89abcdef0123456789abcdef0123456789abcdef")
        );
    }

    #[test]
    fn ref_mapping_decode_rejects_invalid_scope() {
        let mut bytes = sample(1).encode().unwrap();
        // The scope byte follows version + view id + length-prefixed name.
        let name_len = u16::from_le_bytes([bytes[9], bytes[10]]) as usize;
        let scope_index = 11 + name_len;
        bytes[scope_index] = 7;
        assert!(RefMapping::decode(&bytes).is_err());
    }

    #[test]
    fn ref_mapping_encode_rejects_oversize_fields() {
        let mut mapping = sample(1);
        mapping.view_name = "x".repeat(u16::MAX as usize + 1);
        assert!(mapping.encode().is_err());
    }

    #[test]
    fn ref_sync_status_names_roundtrip() {
        for value in 0u8..5 {
            let status = RefSyncStatus::from_u8(value).expect("valid status value");
            assert_eq!(status.to_u8(), value);
            assert!(!status.to_string().is_empty());
        }
        assert!(RefSyncStatus::from_u8(5).is_none());
    }

    /// CB-10A review R7: version 0 never existed as a written layout — a
    /// zeroed record refuses instead of decoding as a v1-shaped row.
    #[test]
    fn ref_mapping_decode_rejects_version_zero() {
        let mut bytes = sample(1).encode().unwrap();
        bytes[0] = 0;
        assert!(RefMapping::decode(&bytes).is_err());
    }

    /// R7: oid fields must be 40/64-character lowercase hex; the encoder
    /// validates before anything reaches storage, and the decoder rejects a
    /// corrupted-but-well-formed row.
    #[test]
    fn ref_mapping_validates_oid_fields() {
        let mut mapping = sample(1);
        mapping.last_observed_local = Some("xyz".to_string());
        assert!(mapping.encode().is_err());
        mapping.last_observed_local = Some("ABCDEF0123456789ABCDEF0123456789ABCDEF01".to_string());
        assert!(mapping.encode().is_err(), "uppercase hex is rejected");
        // 63 characters: neither digest width.
        mapping.last_observed_local = Some("a".repeat(63));
        assert!(mapping.encode().is_err());
        // 64-character sha256 shape encodes and roundtrips.
        mapping.last_observed_local = Some("a".repeat(64));
        let decoded = RefMapping::decode(&mapping.encode().unwrap()).unwrap();
        assert_eq!(decoded.last_observed_local, Some("a".repeat(64)));
        // A corrupted stored row is refused by the decoder.
        let mut bytes = sample(1).encode().unwrap();
        let bad = b"not-an-oid-at-all";
        let start = bytes.len() - (1 + 2 + 40);
        bytes[start..start + bad.len()].copy_from_slice(bad);
        assert!(RefMapping::decode(&bytes).is_err());
    }

    /// R7: state fields must be non-empty uppercase base32.
    #[test]
    fn ref_mapping_validates_state_fields() {
        let mut mapping = sample(1);
        mapping.last_exported_state = Some("not base32!".to_string());
        assert!(mapping.encode().is_err());
        mapping.last_exported_state = Some(String::new());
        assert!(mapping.encode().is_err());
        mapping.last_exported_state =
            Some("BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB".to_string());
        let decoded = RefMapping::decode(&mapping.encode().unwrap()).unwrap();
        assert_eq!(
            decoded.last_exported_state.as_deref(),
            Some("BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB")
        );
    }

    /// R7: ref names must be well-formed Git references; remote refs are
    /// validated too.
    #[test]
    fn ref_mapping_validates_ref_names() {
        let mut mapping = sample(1);
        mapping.local_ref = Some("heads/main".to_string());
        assert!(mapping.encode().is_err(), "ref names must start with refs/");
        mapping.local_ref = Some("refs/heads/with space".to_string());
        assert!(mapping.encode().is_err());
        mapping.local_ref = Some("refs/heads/..".to_string());
        assert!(mapping.encode().is_err());
        mapping.local_ref = Some("refs/heads/main.lock".to_string());
        assert!(mapping.encode().is_err());
        // The remote pair's ref is validated as well.
        mapping.local_ref = Some("refs/heads/main".to_string());
        mapping.remote = Some(("origin".to_string(), "garbage".to_string()));
        assert!(mapping.encode().is_err());
    }

    /// R7: the scope byte must be a real ViewScope.
    #[test]
    fn ref_mapping_encode_validates_scope() {
        let mut mapping = sample(1);
        mapping.scope = 9;
        assert!(mapping.encode().is_err());
    }
}
