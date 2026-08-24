//! The `/code` git-shaped sync wire format — the single transport for
//! `atomic push`, `atomic pull`, and `atomic clone`.
//!
//! # One endpoint, header-negotiated
//!
//! All bare objects and refs move through **one** endpoint per project — the
//! remote URL `…/projects/{slug}/code`. There are no per-object or per-ref
//! transport URLs (those exist only as read-only *WebUI* resources). The client
//! selects this binary protocol with a request header
//! ([`PROTOCOL_HEADER`]`: `[`PROTOCOL_SYNC_V1`]); when the header is absent the
//! server answers with plain JSON (a project-info ping / browse response), so
//! the same URL serves both machine sync and human/WebUI reads.
//!
//! # The two directions
//!
//! - **push** = `POST …/code` with the sync header, body = an encoded
//!   [`SyncPack`]: the object bytes the client is contributing plus the
//!   [`RefRecord`] compare-and-swaps that move each view ref. The server stores
//!   every object (content-addressed, integrity-verified) and CAS-moves each
//!   ref (ancestry-gated, fast-forward only).
//! - **pull / clone** = `GET …/code` with the sync header, body = an encoded
//!   [`SyncWants`]: which view refs the client wants and which object keys it
//!   already `haves`. The server replies with a [`SyncPack`] containing only the
//!   objects the client is missing, plus the current ref targets.
//!
//! Negotiation (`haves`/`wants`) keeps a push/pull from re-sending objects the
//! other side already holds — the git-style "have/want" exchange, collapsed into
//! a single request/response because the object graph is content-addressed and
//! the membership set travels in the view snapshots.
//!
//! # Encoding
//!
//! The body is [`postcard`]-serialized (compact, `no_std`-friendly, deterministic
//! field order) and then **zstd**-compressed. Both sides link this exact crate,
//! so the framing is byte-for-byte agreed. Use [`encode`]/[`decode`] (or the
//! typed [`SyncPack::encode`]/[`SyncPack::decode`] wrappers) — never hand-roll
//! the two steps, so the compression envelope stays consistent.

use std::io::Read;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Request header that selects the binary sync protocol on `…/code`. Its
/// presence (value [`PROTOCOL_SYNC_V1`]) switches the endpoint from the default
/// JSON project-info/WebUI response to the object+ref sync exchange.
pub const PROTOCOL_HEADER: &str = "Atomic-Protocol";

/// The version-1 value of [`PROTOCOL_HEADER`]. Bump the suffix on any
/// wire-breaking change so an old server rejects (and an old client is told to
/// upgrade via `X-Atomic-Min-Version`).
pub const PROTOCOL_SYNC_V1: &str = "sync/1";

/// The media type of an encoded [`SyncPack`]/[`SyncWants`] body — set as the
/// `Content-Type` so proxies and logs can identify the payload.
pub const SYNC_MEDIA_TYPE: &str = "application/vnd.atomic.sync.v1";

/// zstd level used for outbound bodies. Level 3 is zstd's default: a good
/// size/speed tradeoff for the mixed text+binary change payloads.
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;

/// Default ceiling on a *decompressed* body (2 GiB). Bounds the zip-bomb blast
/// radius: [`decode`] refuses to inflate past this. Callers handling untrusted
/// input (the server) can tighten it with [`decode_with_limit`].
pub const DEFAULT_MAX_DECOMPRESSED: usize = 2 * 1024 * 1024 * 1024;

/// The object families that ride the transport. Each maps 1:1 to the storage
/// key segment under a project's object prefix (e.g. `…/changes/{key}`), so the
/// server can route a decoded [`ObjectRecord`] to storage without inspecting its
/// bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObjectFamily {
    /// A `.change` object — the unit of history.
    Change,
    /// A `.tag` object — a named, signed state.
    Tag,
    /// A `.provenance` object — the AI/decision graph sidecar for a change.
    Provenance,
    /// An `.attest` object — an attestation sidecar for a change.
    Attest,
    /// A view-snapshot object (`ViewSnapshot`) — a view's membership + lineage.
    View,
}

impl ObjectFamily {
    /// The storage key segment for this family (the `{family}` in
    /// `{prefix}/{family}/{key}`). Stable on the wire — do not rename without a
    /// protocol bump.
    pub fn as_str(self) -> &'static str {
        match self {
            ObjectFamily::Change => "changes",
            ObjectFamily::Tag => "tags",
            ObjectFamily::Provenance => "provenance",
            ObjectFamily::Attest => "attest",
            ObjectFamily::View => "views",
        }
    }

    /// Parse a family from its storage key segment; `None` for an unknown one.
    pub fn from_segment(s: &str) -> Option<Self> {
        match s {
            "changes" => Some(ObjectFamily::Change),
            "tags" => Some(ObjectFamily::Tag),
            "provenance" => Some(ObjectFamily::Provenance),
            "attest" => Some(ObjectFamily::Attest),
            "views" => Some(ObjectFamily::View),
            _ => None,
        }
    }

    /// Every family, for iteration (inventory scans, closure walks).
    pub fn all() -> [ObjectFamily; 5] {
        [
            ObjectFamily::Change,
            ObjectFamily::Tag,
            ObjectFamily::Provenance,
            ObjectFamily::Attest,
            ObjectFamily::View,
        ]
    }
}

/// A single content-addressed object in transit: its family (→ storage
/// segment), its Blake3 content key, and its raw bytes. The receiver verifies
/// `blake3(bytes) == key` before storing, so `family` never needs to be trusted
/// for integrity — only for routing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectRecord {
    /// Which object family (→ storage key segment) this belongs to.
    pub family: ObjectFamily,
    /// The object's content address (lowercase-hex Blake3 of `bytes`).
    pub key: String,
    /// The object's canonical bytes.
    pub bytes: Vec<u8>,
}

impl ObjectRecord {
    /// Construct a record. The caller supplies the already-computed content key
    /// (producers hash once on the way out of redb).
    pub fn new(family: ObjectFamily, key: impl Into<String>, bytes: Vec<u8>) -> Self {
        ObjectRecord {
            family,
            key: key.into(),
            bytes,
        }
    }
}

/// A view-ref compare-and-swap in a push. Moves `name` from the client's
/// last-known target (`expect_old`) to `new_target`; the server accepts it only
/// if `expect_old` is the current target *and* it is an ancestor of
/// `new_target` in the snapshot `prev` chain (fast-forward). `expect_old` is
/// `None` for a ref the client believes does not exist yet (genesis).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefRecord {
    /// The view name (the `{name}` in `refs/views/{name}`).
    pub name: String,
    /// The target the client expects the ref to currently hold, or `None` for a
    /// genesis create.
    #[serde(default)]
    pub expect_old: Option<String>,
    /// The view-snapshot object key the ref should point at after the CAS.
    pub new_target: String,
}

/// The push payload / pull response: a batch of objects plus the ref moves that
/// make them reachable. On **push** the client fills both; on **pull/clone** the
/// server fills `objects` with what the client lacks and `refs` with each
/// requested view's current target (`expect_old` unused in that direction).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncPack {
    /// Content-addressed objects being transferred.
    pub objects: Vec<ObjectRecord>,
    /// View-ref updates (push) or current targets (pull/clone).
    pub refs: Vec<RefRecord>,
}

impl SyncPack {
    /// An empty pack (no objects, no refs) — a no-op push or an up-to-date pull.
    pub fn empty() -> Self {
        SyncPack::default()
    }

    /// Whether this pack carries nothing.
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty() && self.refs.is_empty()
    }

    /// Encode to a compressed wire body. See [`encode`].
    pub fn encode(&self) -> Result<Vec<u8>, SyncError> {
        encode(self)
    }

    /// Decode from a compressed wire body, bounded by [`DEFAULT_MAX_DECOMPRESSED`].
    pub fn decode(bytes: &[u8]) -> Result<Self, SyncError> {
        decode(bytes)
    }
}

/// The pull/clone request: which view refs the client wants and which object
/// keys it already holds. An empty `refs` means "every view" (a full clone);
/// `haves` lets the server omit objects the client already has, so an
/// incremental pull transfers only the delta.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncWants {
    /// View names to fetch; empty = all views on the remote.
    pub refs: Vec<String>,
    /// Object keys (any family) the client already has, so the server can skip
    /// them. Order-insensitive; the server treats it as a set.
    pub haves: Vec<String>,
    /// Ref-advertisement mode: when `true`, the server returns only the view
    /// refs and their view-snapshot objects (for the requested views and their
    /// ancestor chains) — **no** change bodies or sidecars. This is the cheap
    /// "fetch refs" plane a push uses to negotiate what the remote already has
    /// before sending a pack (the git `info/refs` advertisement, collapsed onto
    /// `/code`). Defaults to `false` (a full pull/clone).
    #[serde(default)]
    pub refs_only: bool,
}

impl SyncWants {
    /// Want every view (a full clone), declaring the given `haves`.
    pub fn all(haves: Vec<String>) -> Self {
        SyncWants {
            refs: Vec::new(),
            haves,
            refs_only: false,
        }
    }

    /// A ref advertisement for the given views (and their ancestors): refs +
    /// view-snapshot objects only, no change bodies. Used by push negotiation.
    pub fn advertise(refs: Vec<String>) -> Self {
        SyncWants {
            refs,
            haves: Vec::new(),
            refs_only: true,
        }
    }

    /// Whether this asks for all views (empty `refs`).
    pub fn wants_all(&self) -> bool {
        self.refs.is_empty()
    }

    /// Encode to a compressed wire body. See [`encode`].
    pub fn encode(&self) -> Result<Vec<u8>, SyncError> {
        encode(self)
    }

    /// Decode from a compressed wire body, bounded by [`DEFAULT_MAX_DECOMPRESSED`].
    pub fn decode(bytes: &[u8]) -> Result<Self, SyncError> {
        decode(bytes)
    }
}

/// Errors from the sync codec: serialization or (de)compression failures.
#[derive(Debug)]
pub enum SyncError {
    /// postcard failed to (de)serialize the value.
    Codec(postcard::Error),
    /// zstd failed to compress or decompress the body.
    Compression(std::io::Error),
    /// The decompressed body exceeded the configured limit — a likely zip bomb.
    TooLarge {
        /// The limit that was exceeded (bytes).
        limit: usize,
    },
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Codec(e) => write!(f, "sync codec error: {e}"),
            SyncError::Compression(e) => write!(f, "sync compression error: {e}"),
            SyncError::TooLarge { limit } => {
                write!(f, "decompressed sync body exceeds {limit} bytes")
            }
        }
    }
}

impl std::error::Error for SyncError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SyncError::Codec(e) => Some(e),
            SyncError::Compression(e) => Some(e),
            SyncError::TooLarge { .. } => None,
        }
    }
}

/// Serialize `value` with postcard and zstd-compress it into a wire body.
///
/// Deterministic for a given value (postcard field order is fixed and zstd is
/// deterministic at a fixed level), which keeps request bodies reproducible for
/// tests and caches.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, SyncError> {
    let raw = postcard::to_allocvec(value).map_err(SyncError::Codec)?;
    zstd::encode_all(raw.as_slice(), DEFAULT_ZSTD_LEVEL).map_err(SyncError::Compression)
}

/// Decompress and deserialize a wire body, bounded by
/// [`DEFAULT_MAX_DECOMPRESSED`]. Use [`decode_with_limit`] to set a tighter cap
/// for untrusted input.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, SyncError> {
    decode_with_limit(bytes, DEFAULT_MAX_DECOMPRESSED)
}

/// Decompress and deserialize a wire body, refusing to inflate past `max_len`
/// bytes. The bound is enforced during decompression (streaming), so a zip bomb
/// is stopped before it is fully materialized.
pub fn decode_with_limit<T: DeserializeOwned>(
    bytes: &[u8],
    max_len: usize,
) -> Result<T, SyncError> {
    let raw = decompress(bytes, max_len)?;
    postcard::from_bytes(&raw).map_err(SyncError::Codec)
}

/// Streaming zstd decompress with a hard output ceiling. Reads at most
/// `max_len + 1` bytes so an overrun is detected without buffering the whole
/// bomb.
fn decompress(bytes: &[u8], max_len: usize) -> Result<Vec<u8>, SyncError> {
    let decoder = zstd::stream::read::Decoder::new(bytes).map_err(SyncError::Compression)?;
    let mut out = Vec::new();
    let read = decoder
        .take(max_len as u64 + 1)
        .read_to_end(&mut out)
        .map_err(SyncError::Compression)?;
    if read > max_len {
        return Err(SyncError::TooLarge { limit: max_len });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pack() -> SyncPack {
        SyncPack {
            objects: vec![
                ObjectRecord::new(ObjectFamily::Change, "aa", b"change-body".to_vec()),
                ObjectRecord::new(ObjectFamily::View, "bb", b"{\"scope\":\"shared\"}".to_vec()),
                ObjectRecord::new(ObjectFamily::Provenance, "cc", b"prov".to_vec()),
            ],
            refs: vec![
                RefRecord {
                    name: "dev".to_string(),
                    expect_old: None,
                    new_target: "bb".to_string(),
                },
                RefRecord {
                    name: "feature".to_string(),
                    expect_old: Some("bb".to_string()),
                    new_target: "cc".to_string(),
                },
            ],
        }
    }

    #[test]
    fn pack_roundtrips_through_the_wire() {
        let pack = sample_pack();
        let bytes = pack.encode().expect("encode");
        let decoded = SyncPack::decode(&bytes).expect("decode");
        assert_eq!(decoded, pack);
    }

    #[test]
    fn wants_roundtrip_and_all_helper() {
        let wants = SyncWants::all(vec!["aa".to_string(), "bb".to_string()]);
        assert!(wants.wants_all());
        let bytes = wants.encode().expect("encode");
        assert_eq!(SyncWants::decode(&bytes).expect("decode"), wants);

        let scoped = SyncWants {
            refs: vec!["dev".to_string()],
            haves: vec![],
            refs_only: false,
        };
        assert!(!scoped.wants_all());

        let adv = SyncWants::advertise(vec!["dev".to_string()]);
        assert!(adv.refs_only);
        assert!(!adv.wants_all());
        assert_eq!(SyncWants::decode(&adv.encode().unwrap()).unwrap(), adv);
    }

    #[test]
    fn empty_pack_is_empty() {
        let pack = SyncPack::empty();
        assert!(pack.is_empty());
        let decoded = SyncPack::decode(&pack.encode().unwrap()).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn family_segment_mapping_is_stable_and_reversible() {
        for f in ObjectFamily::all() {
            assert_eq!(ObjectFamily::from_segment(f.as_str()), Some(f));
        }
        assert_eq!(
            ObjectFamily::from_segment("changes"),
            Some(ObjectFamily::Change)
        );
        assert_eq!(
            ObjectFamily::from_segment("attest"),
            Some(ObjectFamily::Attest)
        );
        assert_eq!(ObjectFamily::from_segment("nope"), None);
    }

    #[test]
    fn body_is_compressed_smaller_than_raw_postcard_for_repetitive_input() {
        // A highly compressible object: zstd should shrink it well below the
        // raw postcard size, confirming compression is actually applied.
        let pack = SyncPack {
            objects: vec![ObjectRecord::new(
                ObjectFamily::Change,
                "k",
                vec![b'a'; 10_000],
            )],
            refs: vec![],
        };
        let raw = postcard::to_allocvec(&pack).unwrap();
        let wire = pack.encode().unwrap();
        assert!(
            wire.len() < raw.len(),
            "compressed {} should be < raw {}",
            wire.len(),
            raw.len()
        );
    }

    #[test]
    fn decode_rejects_a_body_over_the_limit() {
        // Compress 5000 bytes, then demand it decode within 1000 — the streaming
        // guard must reject it as TooLarge rather than allocating the full output.
        let pack = SyncPack {
            objects: vec![ObjectRecord::new(
                ObjectFamily::Change,
                "k",
                vec![7u8; 5000],
            )],
            refs: vec![],
        };
        let wire = pack.encode().unwrap();
        let err = decode_with_limit::<SyncPack>(&wire, 1000).unwrap_err();
        assert!(matches!(err, SyncError::TooLarge { limit: 1000 }));
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(SyncPack::decode(b"not a zstd frame").is_err());
    }
}
