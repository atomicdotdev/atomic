//! Canonical, content-addressed object encodings shared by the Atomic client
//! and server.
//!
//! This crate is the thin, engine-free foundation of the **bare object + ref**
//! sync model (see `docs/view-snapshot-sync-design.md`). It defines the on-wire
//! *objects* — content-addressed, immutable, byte-identical on both sides — with
//! **no dependency on the redb graph engine** (`atomic-core`) or the repository
//! layer (`atomic-repository`). That decoupling is deliberate:
//!
//! - The **client** derives these objects from its redb truth on push.
//! - The **server** stores the object bytes verbatim and can compose
//!   *structure* reads (view inventory, membership) straight from them — no
//!   graph apply.
//! - The **bare transport** (`ObjectStore`/`RefStore`) moves these as opaque
//!   bytes keyed by their content address.
//!
//! Because both sides link this exact crate, `blake3(canonical_bytes)` is
//! guaranteed identical everywhere, which is what makes content addressing and
//! CAS-on-`prev` sound.
//!
//! The first object family is [`ViewSnapshot`]; vault objects
//! (`VaultEntry`/`VaultManifest`) will land here later as additional families
//! riding the identical transport.
//!
//! The synchronization types define the **single `/code` transport** wire format —
//! the header-negotiated, postcard+zstd [`SyncPack`]/[`SyncWants`] exchange that
//! carries every object family and ref move for push/pull/clone.

mod sync;
mod view_snapshot;

pub use sync::{
    decode, decode_with_limit, encode, ObjectFamily, ObjectRecord, RefRecord, SyncError, SyncPack,
    SyncWants, DEFAULT_MAX_DECOMPRESSED, DEFAULT_ZSTD_LEVEL, PROTOCOL_HEADER, PROTOCOL_SYNC_V1,
    SYNC_MEDIA_TYPE,
};
pub use view_snapshot::{ViewScopeLabel, ViewSnapshot};

/// The content address of an object: the lowercase-hex Blake3 of its canonical
/// serialization. Kept as a plain `String` (rather than a newtype) so it drops
/// straight into object-store keys, ref values, and REST paths without
/// conversion; helpers here centralize the hashing so every producer agrees.
pub type ObjectKey = String;

/// Compute the canonical content address (lowercase hex Blake3) of arbitrary
/// object bytes. Every object family keys itself with this, so the transport
/// never needs to know an object's type to verify it: `content_key(bytes) ==
/// key` is a universal, type-agnostic integrity check.
pub fn content_key(bytes: &[u8]) -> ObjectKey {
    blake3::hash(bytes).to_hex().to_string()
}

/// Whether `bytes` hash to `key` — the content-addressed integrity check the
/// bare server runs on every `PUT /{family}/{key}` before storing.
pub fn verify_key(key: &str, bytes: &[u8]) -> bool {
    content_key(bytes) == key
}
