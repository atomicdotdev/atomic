//! Complete graph-backed conflict objects (RFC §8.3, CB-8B).
//!
//! Marker bytes are a lossy Git representation, never authoritative conflict
//! state. A [`ConflictSetObject`] is the complete, canonical, content-addressed
//! description of one view's unresolved Atomic conflicts: identities (inode +
//! entry kinds), base and sides (change, vertex position, content hash, mode,
//! kind), name/claimant identities, and the graph metadata needed to restore
//! and re-verify the same conflict state in a fresh store.
//!
//! The canonical encoding is versioned and fails closed: an unknown version or
//! truncated bytes refuse to decode rather than flattening to "some conflict".

use atomic_core::pristine::StoredConflictKind;
use atomic_core::types::Base32;
use atomic_core::Hash;
use serde::{Deserialize, Serialize};

/// Storage-stable file kind of one conflict side (serializable twin of
/// [`atomic_core::operation::FileKind`], which carries no serde impls).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictSideKind {
    /// A regular file blob.
    Regular,
    /// A symbolic link whose content is the target bytes.
    Symlink,
    /// A submodule gitlink.
    Gitlink,
}

impl ConflictSideKind {
    /// The runtime file kind this side materializes as.
    pub fn to_file_kind(self) -> atomic_core::operation::FileKind {
        match self {
            Self::Regular => atomic_core::operation::FileKind::Regular,
            Self::Symlink => atomic_core::operation::FileKind::Symlink,
            Self::Gitlink => atomic_core::operation::FileKind::Gitlink,
        }
    }

    /// Classify a runtime file kind.
    pub fn from_file_kind(kind: atomic_core::operation::FileKind) -> Self {
        match kind {
            atomic_core::operation::FileKind::Regular => Self::Regular,
            atomic_core::operation::FileKind::Symlink => Self::Symlink,
            atomic_core::operation::FileKind::Gitlink => Self::Gitlink,
            atomic_core::operation::FileKind::Directory => Self::Regular,
        }
    }
}

/// Encoding magic of a canonical conflict-set object.
pub const CONFLICT_SET_MAGIC: &[u8; 4] = b"ACO1";

/// Current conflict-set object encoding version.
pub const CONFLICT_SET_VERSION: u32 = 1;

/// Domain separation for the conflict-set identity hash.
pub const CONFLICT_SET_HASH_DOMAIN: &[u8] = b"atomic:conflict-set:v1\0";

/// The complete unresolved conflict state of one view.
///
/// Files are ordered by raw path bytes and entries by kind/line, so the
/// canonical bytes — and therefore [`Self::hash`] — are deterministic for a
/// given conflict state regardless of discovery order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictSetObject {
    pub version: u32,
    /// Every conflicted file, ordered by raw path bytes.
    pub files: Vec<ConflictFileObject>,
}

/// The complete conflict state of one file (one inode).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictFileObject {
    /// Raw repository path bytes (no escaping).
    pub path: Vec<u8>,
    /// The file's stable inode identity (base32 in display contexts).
    pub inode: u64,
    /// Every conflict entry recorded for the file, in stored order.
    pub entries: Vec<ConflictEntryObject>,
}

/// One conflict entry: kind, position, base, sides, and claimants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictEntryObject {
    /// The kind of conflict, mirroring the stored conflict kinds.
    pub kind: ConflictEntryKind,
    /// 1-based line of the conflict region's first marker, when known.
    pub line: Option<u32>,
    /// The base side of the conflict, when one exists (zombie bases).
    pub base: Option<ConflictSideObject>,
    /// The competing sides (at least one for every representable conflict).
    pub sides: Vec<ConflictSideObject>,
    /// Name-edge claimants (name conflicts): every change claiming the path.
    pub claimants: Vec<ConflictClaimantObject>,
}

/// Conflict entry kind — the storage-stable conflict taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictEntryKind {
    /// Ambiguous ordering of content (concurrent insert at one position).
    Order,
    /// Cyclic conflict (an SCC with more than one vertex).
    Cyclic,
    /// Deleted content that still has live connections.
    Zombie,
    /// A file deleted while concurrently modified.
    ZombieFile,
    /// Two changes claim the same path (or a case-collision of two paths).
    Name,
}

impl ConflictEntryKind {
    /// The stored-conflict kind this entry was captured from.
    pub fn from_stored(kind: StoredConflictKind) -> Self {
        match kind {
            StoredConflictKind::Order => Self::Order,
            StoredConflictKind::Cyclic => Self::Cyclic,
            StoredConflictKind::Zombie => Self::Zombie,
            StoredConflictKind::Name => Self::Name,
        }
    }

    /// The stored-conflict kind for persistence round-trips.
    pub fn to_stored(self) -> StoredConflictKind {
        match self {
            Self::Order => StoredConflictKind::Order,
            Self::Cyclic => StoredConflictKind::Cyclic,
            Self::Zombie => StoredConflictKind::Zombie,
            Self::Name => StoredConflictKind::Name,
            Self::ZombieFile => StoredConflictKind::Zombie,
        }
    }

    /// Short display form used in refusals and notices.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Order => "order",
            Self::Cyclic => "cyclic",
            Self::Zombie => "zombie",
            Self::ZombieFile => "zombie-file",
            Self::Name => "name",
        }
    }
}

/// One side of a conflict: the graph vertex that contributes content.
///
/// `change` is the external change hash (base32 in display), `start`/`end`
/// are the vertex's byte range inside that change, `content` is the hash of
/// the side's rendered bytes, and `mode`/`kind` are the file attributes the
/// side materializes with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictSideObject {
    /// External (content) hash of the change introducing this side.
    pub change: Hash,
    /// Start offset of the side's vertex inside the change.
    pub start: u64,
    /// End offset of the side's vertex inside the change.
    pub end: u64,
    /// Content hash of the side's rendered bytes.
    pub content: Hash,
    /// POSIX mode the side materializes with.
    pub mode: u32,
    /// File kind the side materializes as.
    pub kind: ConflictSideKind,
}

/// One name-edge claimant: a change that claims a path for an inode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictClaimantObject {
    /// External (content) hash of the claiming change.
    pub change: Hash,
    /// The inode the claim is made for.
    pub inode: u64,
    /// The claimed path's raw bytes.
    pub path: Vec<u8>,
    /// Whether the claim presents the path as a directory.
    pub directory: bool,
}

/// Why a conflict state cannot be projected as marker bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictRepresentability {
    /// The state projects as ordinary marker-materialized files.
    Representable,
    /// Two tracked paths collide case-insensitively; materializing on a
    /// case-insensitive filesystem would silently overwrite one side.
    PathCaseCollision { paths: Vec<String> },
    /// A concurrent attribute register (mode/kind) has no single value; the
    /// conflict object records both, but markers cannot carry two modes.
    AttributeConflict { path: String, attribute: String },
}

impl ConflictRepresentability {
    pub fn is_representable(&self) -> bool {
        matches!(self, Self::Representable)
    }
}

/// Errors from conflict-object encoding, decoding, and verification.
#[derive(Debug, thiserror::Error)]
pub enum ConflictObjectError {
    #[error("cannot encode conflict object: {0}")]
    Encode(String),
    #[error("conflict object magic mismatch: expected {expected:?}, found {found:?}")]
    Magic {
        expected: [u8; 4],
        found: [u8; 4],
    },
    #[error("conflict object version {found} is unsupported (supported: {supported})")]
    Version { found: u32, supported: u32 },
    #[error("conflict object payload failed to decode: {0}")]
    Decode(String),
    #[error("conflict object bytes are truncated")]
    Truncated,
    #[error("conflict object hash mismatch: header claims {claimed}, object hashes to {actual}")]
    HashMismatch {
        claimed: String,
        actual: String,
    },
    #[error("conflict object {identity} lists side change {change} that is absent from the store")]
    MissingSide { identity: String, change: String },
    #[error("conflict entry {identity} has no sides; markers plus a hash alone cannot restore a conflict")]
    NoSides { identity: String },
}

impl ConflictSetObject {
    /// An empty conflict set (no files).
    pub fn empty() -> Self {
        Self {
            version: CONFLICT_SET_VERSION,
            files: Vec::new(),
        }
    }

    /// Whether any conflict is recorded.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The store-independent view of this conflict set.
    ///
    /// Inode numbers are per-store sequential ids (allocated from a process
    /// counter), so a fresh store restores the same conflicts under different
    /// numbers. Conflict SIDES are a set, not a sequence: the renderer's
    /// presentation order between two stores can differ (SCC tie-breaks use
    /// store-local internal ids), so the normalized form sorts sides by
    /// their stable vertex identity and drops the rendered side-content
    /// hash — a side's authoritative identity is the vertex (change, start,
    /// end); its bytes are re-derivable from the change file, and the
    /// rendered bytes follow the presentation order. The conflict-set
    /// identity and every cross-store comparison use this normalized form;
    /// the raw inode stays in the object as store-local graph metadata for
    /// status and repair tooling.
    pub fn normalized(&self) -> Self {
        let mut normalized = self.clone();
        for file in &mut normalized.files {
            file.inode = 0;
            for entry in &mut file.entries {
                entry.sides.sort_by(|left, right| {
                    left.change
                        .as_bytes()
                        .cmp(right.change.as_bytes())
                        .then(left.start.cmp(&right.start))
                        .then(left.end.cmp(&right.end))
                });
                for side in &mut entry.sides {
                    side.content = Hash::from_bytes([0u8; 32]);
                }
                if let Some(base) = entry.base.as_mut() {
                    base.content = Hash::from_bytes([0u8; 32]);
                }
                for claimant in &mut entry.claimants {
                    claimant.inode = 0;
                }
            }
        }
        normalized
    }

    /// Canonical bytes: magic + version + deterministic postcard payload of
    /// the normalized (store-independent) object.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ConflictObjectError> {
        let normalized = self.normalized();
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(CONFLICT_SET_MAGIC);
        bytes.extend_from_slice(&normalized.version.to_le_bytes());
        let payload = postcard::to_allocvec(&normalized)
            .map_err(|error| ConflictObjectError::Encode(error.to_string()))?;
        bytes.extend_from_slice(&payload);
        Ok(bytes)
    }

    /// Decode canonical bytes, failing closed on unknown versions.
    pub fn decode(bytes: &[u8]) -> Result<Self, ConflictObjectError> {
        if bytes.len() < 8 {
            return Err(ConflictObjectError::Truncated);
        }
        let mut magic = [0u8; 4];
        magic.copy_from_slice(&bytes[..4]);
        if magic != *CONFLICT_SET_MAGIC {
            return Err(ConflictObjectError::Magic {
                expected: *CONFLICT_SET_MAGIC,
                found: magic,
            });
        }
        let version = u32::from_le_bytes(bytes[4..8].try_into().expect("version slice"));
        if version != CONFLICT_SET_VERSION {
            return Err(ConflictObjectError::Version {
                found: version,
                supported: CONFLICT_SET_VERSION,
            });
        }
        postcard::from_bytes(&bytes[8..]).map_err(|error| ConflictObjectError::Decode(error.to_string()))
    }

    /// The conflict-set identity: Blake3 over the domain-separated canonical
    /// bytes. This is the `<conflict-set-hash>` of the `atomic-conflict`
    /// projection header and the freshness check of every pack.
    pub fn hash(&self) -> Result<Hash, ConflictObjectError> {
        let bytes = self.canonical_bytes()?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(CONFLICT_SET_HASH_DOMAIN);
        hasher.update(&bytes);
        Ok(Hash::from_bytes(hasher.finalize().into()))
    }

    /// Verify that `claimed` names exactly this object.
    pub fn verify_hash(&self, claimed: &Hash) -> Result<(), ConflictObjectError> {
        let actual = self.hash()?;
        if actual == *claimed {
            Ok(())
        } else {
            Err(ConflictObjectError::HashMismatch {
                claimed: claimed.to_base32(),
                actual: actual.to_base32(),
            })
        }
    }

    /// Total number of conflict entries across all files.
    pub fn entry_count(&self) -> usize {
        self.files.iter().map(|file| file.entries.len()).sum()
    }

    /// Every side change hash referenced by any entry, deduplicated and
    /// ordered. A pack is complete only when every referenced change exists
    /// in the receiving store.
    pub fn side_changes(&self) -> Vec<Hash> {
        let mut changes = Vec::new();
        for file in &self.files {
            for entry in &file.entries {
                if let Some(base) = &entry.base {
                    if !changes.contains(&base.change) {
                        changes.push(base.change);
                    }
                }
                for side in &entry.sides {
                    if !changes.contains(&side.change) {
                        changes.push(side.change);
                    }
                }
                for claimant in &entry.claimants {
                    if !changes.contains(&claimant.change) {
                        changes.push(claimant.change);
                    }
                }
            }
        }
        changes
    }

    /// Structural self-check: every entry must name at least one side or
    /// claimant, and every file must have entries.
    pub fn validate(&self) -> Result<(), ConflictObjectError> {
        for file in &self.files {
            if file.entries.is_empty() {
                return Err(ConflictObjectError::NoSides {
                    identity: format!(
                        "path {}",
                        String::from_utf8_lossy(&file.path)
                    ),
                });
            }
            for entry in &file.entries {
                let identity = format!(
                    "{}:{}@{}",
                    entry.kind.as_str(),
                    String::from_utf8_lossy(&file.path),
                    entry.line.unwrap_or(0)
                );
                if entry.sides.is_empty() && entry.claimants.is_empty() {
                    return Err(ConflictObjectError::NoSides { identity });
                }
                if !entry.claimants.is_empty() && entry.sides.is_empty() {
                    // A name conflict legitimately records claimants only.
                    continue;
                }
            }
        }
        Ok(())
    }
}

// ── Capture: graph → conflict object ─────────────────────────────────────

/// One captured conflict region: the marker-region bookkeeping of a rendered
/// file, with the graph vertices (change, byte range) of each side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CapturedRegion {
    /// The conflict kind observed by the output layer.
    pub(super) kind: ConflictEntryKind,
    /// The line recorded at region open (same convention as the persisted
    /// conflict line: the count of completed lines when `begin_*` ran).
    pub(super) line: u32,
    /// Sides in marker order: first side first.
    pub(super) sides: Vec<CapturedSide>,
}

/// One captured side: the vertex that contributed the bytes plus their hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CapturedSide {
    pub(super) node: atomic_core::types::GraphNode<atomic_core::types::NodeId>,
    pub(super) content: Hash,
    /// The change hash carried by a separator marker adjacent to this side.
    pub(super) marker_change: Option<Hash>,
}

/// Marker tokens of the Atomic conflict writer (`atomic-core` output layer).
const MARKER_START: &str = ">>>>>>>";
const MARKER_SEPARATOR: &str = "=======";
const MARKER_END: &str = "<<<<<<<";

/// A `VertexBuffer` that renders a conflicted file into bytes byte-identical
/// to materialization output while recording the side structure (vertices,
/// content hashes) needed for complete conflict objects.
///
/// Byte-exactness against the real renderer is pinned by tests that compare
/// captured bytes with the materialization renderer for the same graph.
pub(super) struct ConflictCaptureWriter {
    out: Vec<u8>,
    at_line_start: bool,
    line: u32,
    regions: Vec<CapturedRegion>,
    open: Option<CapturedRegion>,
    current_side: Option<CapturedSide>,
    side_start: usize,
}

impl ConflictCaptureWriter {
    pub(super) fn new() -> Self {
        Self {
            out: Vec::new(),
            at_line_start: true,
            line: 0,
            regions: Vec::new(),
            open: None,
            current_side: None,
            side_start: 0,
        }
    }

    /// The complete rendered bytes (identical to materialization output) plus
    /// the captured regions.
    pub(super) fn into_parts(mut self) -> (Vec<u8>, Vec<CapturedRegion>) {
        self.close_region();
        (self.out, self.regions)
    }

    fn ensure_line_start(&mut self) {
        if !self.at_line_start {
            self.out.push(b'\n');
            self.line += 1;
            self.at_line_start = true;
        }
    }

    /// Write one marker line in the exact format of the output layer:
    /// `MARKER ID [HASH8]` + newline (the hash part only when supplied).
    fn write_marker(&mut self, marker: &str, id: usize, changes: Option<&[Hash]>) {
        self.ensure_line_start();
        self.out
            .extend_from_slice(format!("{marker} {id}").as_bytes());
        if let Some(hash) = changes.and_then(|c| c.first()) {
            let hash_str = hash.to_base32();
            let short = if hash_str.len() > 8 { &hash_str[..8] } else { &hash_str };
            self.out
                .extend_from_slice(format!(" [{short}]").as_bytes());
        }
        self.out.push(b'\n');
        self.line += 1;
        self.at_line_start = true;
    }

    fn close_side(&mut self) {
        if let Some(mut side) = self.current_side.take() {
            side.content = Hash::of(&self.out[self.side_start..]);
            self.side_start = self.out.len();
            if let Some(region) = self.open.as_mut() {
                region.sides.push(side);
            }
        }
    }

    fn close_region(&mut self) {
        self.close_side();
        if let Some(region) = self.open.take() {
            self.regions.push(region);
        }
    }
}

impl atomic_core::output::VertexBuffer for ConflictCaptureWriter {
    fn output_line<E, F>(
        &mut self,
        node: atomic_core::types::GraphNode<atomic_core::types::NodeId>,
        get_contents: F,
    ) -> Result<(), E>
    where
        E: From<std::io::Error>,
        F: FnOnce(&mut [u8]) -> Result<(), E>,
    {
        let len = (node.end.get() - node.start.get()) as usize;
        if len == 0 {
            return Ok(());
        }
        let mut contents = vec![0u8; len];
        get_contents(&mut contents)?;
        if self.open.is_some() {
            if self.current_side.is_none() {
                self.current_side = Some(CapturedSide {
                    node,
                    content: Hash::of(b""),
                    marker_change: None,
                });
                self.side_start = self.out.len();
            }
            self.out.extend_from_slice(&contents);
            self.line += contents.iter().filter(|&&b| b == b'\n').count() as u32;
            self.at_line_start = contents.ends_with(b"\n");
        } else {
            self.out.extend_from_slice(&contents);
            self.line += contents.iter().filter(|&&b| b == b'\n').count() as u32;
            self.at_line_start = contents.ends_with(b"\n");
        }
        Ok(())
    }

    fn output_conflict_marker(
        &mut self,
        marker: &str,
        id: usize,
        changes: Option<&[Hash]>,
    ) -> Result<(), std::io::Error> {
        self.write_marker(marker, id, changes);
        Ok(())
    }

    fn begin_conflict(
        &mut self,
        id: usize,
        changes: Option<&[Hash]>,
    ) -> Result<(), std::io::Error> {
        self.close_region();
        self.open = Some(CapturedRegion {
            kind: ConflictEntryKind::Order,
            line: self.line,
            sides: Vec::new(),
        });
        self.write_marker(MARKER_START, id, changes);
        Ok(())
    }

    fn begin_zombie_conflict(
        &mut self,
        id: usize,
        changes: Option<&[Hash]>,
    ) -> Result<(), std::io::Error> {
        self.close_region();
        self.open = Some(CapturedRegion {
            kind: ConflictEntryKind::Zombie,
            line: self.line,
            sides: Vec::new(),
        });
        self.write_marker(MARKER_START, id, changes);
        Ok(())
    }

    fn begin_cyclic_conflict(&mut self, id: usize) -> Result<(), std::io::Error> {
        self.close_region();
        self.open = Some(CapturedRegion {
            kind: ConflictEntryKind::Cyclic,
            line: self.line,
            sides: Vec::new(),
        });
        self.write_marker(MARKER_START, id, None);
        Ok(())
    }

    fn conflict_next(&mut self, id: usize, changes: Option<&[Hash]>) -> Result<(), std::io::Error> {
        // The separator closes the side that just ended; the next emitted
        // vertex opens the following side. The separator names the following
        // side's change for display; record it on the closing side's marker
        // field so the object carries the authoritative separator hash.
        self.close_side();
        if let Some(region) = self.open.as_mut() {
            if let Some(last) = region.sides.last_mut() {
                last.marker_change = changes.and_then(|c| c.first().copied());
            }
        }
        self.write_marker(MARKER_SEPARATOR, id, changes);
        Ok(())
    }

    fn end_conflict(&mut self, id: usize) -> Result<(), std::io::Error> {
        self.write_marker(MARKER_END, id, None);
        self.close_region();
        Ok(())
    }

    fn end_zombie_conflict(&mut self, id: usize) -> Result<(), std::io::Error> {
        self.write_marker(MARKER_END, id, None);
        self.close_region();
        Ok(())
    }

    fn end_cyclic_conflict(&mut self, id: usize) -> Result<(), std::io::Error> {
        self.write_marker(MARKER_END, id, None);
        self.close_region();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side(change_seed: u8) -> ConflictSideObject {
        ConflictSideObject {
            change: Hash::of(format!("atomic:side:{change_seed}").as_bytes()),
            start: 0,
            end: 5 + u64::from(change_seed),
            content: Hash::of(format!("atomic:content:{change_seed}").as_bytes()),
            mode: 0o644,
            kind: ConflictSideKind::Regular,
        }
    }

    fn sample() -> ConflictSetObject {
        ConflictSetObject {
            version: CONFLICT_SET_VERSION,
            files: vec![ConflictFileObject {
                path: b"src/lib.rs".to_vec(),
                inode: 7,
                entries: vec![ConflictEntryObject {
                    kind: ConflictEntryKind::Order,
                    line: Some(3),
                    base: None,
                    sides: vec![side(1), side(2)],
                    claimants: Vec::new(),
                }],
            }],
        }
    }

    #[test]
    fn canonical_bytes_are_deterministic_and_hash_stable() {
        let first = sample();
        let second = sample();
        assert_eq!(first.canonical_bytes().unwrap(), second.canonical_bytes().unwrap());
        assert_eq!(first.hash().unwrap(), second.hash().unwrap());
    }

    #[test]
    fn file_order_changes_the_identity_but_sides_are_a_set() {
        // Files are ordered by path; a different file order changes the
        // identity.
        let mut reordered = sample();
        reordered.files.reverse();
        reordered.files[0].path = b"zzz.rs".to_vec();
        assert_ne!(reordered.hash().unwrap(), sample().hash().unwrap());
        // Conflict SIDES are a set (the renderer's presentation order is
        // store-local), so side order does NOT change the identity.
        let mut sides_reordered = sample();
        let sides = sides_reordered.files[0].entries[0].sides.clone();
        sides_reordered.files[0].entries[0].sides = sides.into_iter().rev().collect();
        assert_eq!(
            sides_reordered.normalized(),
            sample().normalized(),
            "sides sort by their stable vertex identity"
        );
    }

    #[test]
    fn roundtrip_preserves_normalized_deep_equality() {
        let object = sample();
        let bytes = object.canonical_bytes().unwrap();
        let decoded = ConflictSetObject::decode(&bytes).unwrap();
        // Store-local inode numbers are not part of the canonical identity.
        assert_eq!(decoded.normalized(), object.normalized());
        assert_eq!(decoded.hash().unwrap(), object.hash().unwrap());
    }

    #[test]
    fn inode_renumbering_preserves_identity() {
        let mut renumbered = sample();
        renumbered.files[0].inode = 4242;
        assert_eq!(renumbered.hash().unwrap(), sample().hash().unwrap());
        assert_eq!(renumbered.normalized(), sample().normalized());
    }

    #[test]
    fn unknown_version_fails_closed() {
        let mut object = sample();
        object.version = 99;
        let bytes = object.canonical_bytes().unwrap();
        assert!(matches!(
            ConflictSetObject::decode(&bytes),
            Err(ConflictObjectError::Version { .. })
        ));
    }

    #[test]
    fn wrong_magic_fails_closed() {
        let mut bytes = sample().canonical_bytes().unwrap();
        bytes[0] = b'X';
        assert!(matches!(
            ConflictSetObject::decode(&bytes),
            Err(ConflictObjectError::Magic { .. })
        ));
    }

    #[test]
    fn truncated_bytes_are_refused() {
        assert!(matches!(
            ConflictSetObject::decode(&[0u8; 4]),
            Err(ConflictObjectError::Truncated)
        ));
    }

    #[test]
    fn hash_mismatch_is_detected() {
        let object = sample();
        let other = Hash::of(b"atomic:different");
        assert!(matches!(
            object.verify_hash(&other),
            Err(ConflictObjectError::HashMismatch { .. })
        ));
        object.verify_hash(&object.hash().unwrap()).unwrap();
    }

    #[test]
    fn entry_without_sides_is_refused() {
        let mut object = sample();
        object.files[0].entries[0].sides.clear();
        assert!(matches!(
            object.validate(),
            Err(ConflictObjectError::NoSides { .. })
        ));
    }

    #[test]
    fn side_changes_are_collected_in_order() {
        let mut object = sample();
        object.files[0].entries[0].claimants.push(ConflictClaimantObject {
            change: Hash::of(b"atomic:claimant"),
            inode: 9,
            path: b"src/lib.rs".to_vec(),
            directory: false,
        });
        let changes = object.side_changes();
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0], Hash::of(b"atomic:side:1"));
        assert_eq!(changes[2], Hash::of(b"atomic:claimant"));
    }

    #[test]
    fn zombie_base_roundtrips() {
        let mut object = sample();
        object.files[0].entries[0].base = Some(side(3));
        let decoded =
            ConflictSetObject::decode(&object.canonical_bytes().unwrap()).unwrap();
        assert_eq!(decoded.normalized(), object.normalized());
    }
}

// ── Repository: capture and representability ─────────────────────────────

use super::project_tree::ConversionPolicy;
use super::Repository;
use crate::RepositoryError;

/// A prepared conflict-snapshot projection: the complete conflict object, its
/// identity hash, and the marker-materialized project tree it projects as.
#[derive(Debug, Clone)]
pub struct ConflictSnapshotProjection {
    /// The complete graph-backed conflict state.
    pub conflict_set: ConflictSetObject,
    /// The conflict-set identity (`atomic-conflict <hash>` header value).
    pub conflict_set_hash: Hash,
    /// The marker-materialized project tree.
    pub project: super::project_tree::ProjectTree,
    /// The paths whose projected bytes carry conflict markers.
    pub conflicted_paths: Vec<String>,
}

/// Why a conflicted state cannot be projected as an ordinary conflict
/// snapshot commit (RFC §8.3: never silently flattened).
#[derive(Debug, thiserror::Error)]
pub enum ConflictProjectionError {
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    Object(#[from] ConflictObjectError),
    #[error(
        "path case collision between {paths:?} cannot be materialized on a case-insensitive \
         filesystem; resolve the collision before projecting"
    )]
    PathCaseCollision { paths: Vec<String> },
    #[error(
        "inode attribute conflict at '{path}' ({attribute}) cannot be carried by conflict \
         markers; resolve the attribute conflict before projecting"
    )]
    AttributeConflict { path: String, attribute: String },
    #[error("cannot materialize mixed file/directory name conflict '{path}' without choosing a side")]
    MixedNameConflict { path: String },
    #[error("conflict capture for '{path}' failed: {message}")]
    Capture { path: String, message: String },
}

impl Repository {
    /// Capture the complete, canonical conflict object for a view together
    /// with the marker bytes each conflicted file renders to.
    ///
    /// Returns `None` when the view has no persisted conflicts. The object is
    /// built from the persisted `CONFLICTS` rows (kinds, paths, lines), the
    /// causal path-claim projection (name-conflict claimants), and the file
    /// graph (side vertices, rendered side content). Capture is deterministic:
    /// the same visible state always yields byte-identical canonical bytes,
    /// and the marker bytes are byte-identical to materialization output.
    pub fn capture_view_conflict_set_with_markers(
        &self,
        view_name: &str,
    ) -> Result<
        Option<(
            ConflictSetObject,
            std::collections::BTreeMap<String, Vec<u8>>,
        )>,
        RepositoryError,
    > {
        self.capture_view_conflict_set_inner(view_name, true)
    }

    /// Capture the complete, canonical conflict object for a view.
    ///
    /// Returns `None` when the view has no persisted conflicts. The object is
    /// built from the persisted `CONFLICTS` rows (kinds, paths, lines), the
    /// causal path-claim projection (name-conflict claimants), and the file
    /// graph (side vertices, rendered side content). Capture is deterministic:
    /// the same visible state always yields byte-identical canonical bytes.
    pub fn capture_view_conflict_set(
        &self,
        view_name: &str,
    ) -> Result<Option<ConflictSetObject>, RepositoryError> {
        Ok(self
            .capture_view_conflict_set_inner(view_name, false)?
            .map(|(object, _)| object))
    }

    fn capture_view_conflict_set_inner(
        &self,
        view_name: &str,
        with_markers: bool,
    ) -> Result<
        Option<(
            ConflictSetObject,
            std::collections::BTreeMap<String, Vec<u8>>,
        )>,
        RepositoryError,
    > {
        use atomic_core::pristine::{GraphTxnT, TreeTxnT, ViewTxnT};

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let Some(view) = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        else {
            return Ok(None);
        };
        let rows = txn
            .iter_conflicts(view.id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        if rows.is_empty() {
            return Ok(None);
        }
        let visibility = super::filter::graph_visibility_closure(&txn, &view)?;
        let claim_visibility = super::name_resolution::path_claim_visibility_for_view(
            &txn,
            &self.change_store,
            &view,
            &visibility,
        )?;
        let projection = self.project_tree_for_visibility(&txn, &claim_visibility)?;

        let mut external_hashes = std::collections::HashMap::new();
        for node_id in visibility.iter_dependency_first().copied() {
            if node_id.is_root() {
                continue;
            }
            let hash = txn
                .get_external(node_id)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| {
                    RepositoryError::Database(format!(
                        "visible conflict change {} has no external hash",
                        node_id.get()
                    ))
                })?;
            external_hashes.insert(node_id, hash);
        }

        let inode_graph_table = txn
            .open_inode_graph_table()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut files: Vec<ConflictFileObject> = Vec::new();
        let mut marker_bytes: std::collections::BTreeMap<String, Vec<u8>> =
            std::collections::BTreeMap::new();
        for (inode, records) in rows {
            for record in records {
                let path = record.path.clone();
                let position = txn
                    .inode_position(atomic_core::types::Inode::new(inode))
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                let entry_kind = ConflictEntryKind::from_stored(record.kind);
                let mut entry = ConflictEntryObject {
                    kind: entry_kind,
                    line: record.line,
                    base: None,
                    sides: Vec::new(),
                    claimants: Vec::new(),
                };
                match entry_kind {
                    ConflictEntryKind::Name => {
                        // Claimants come from the causal path-claim
                        // projection: every visible side claiming the path.
                        if let Some(conflict) = projection.name_conflicts.get(&path) {
                            for side in &conflict.sides {
                                for change in &side.event_changes {
                                    let Some(hash) = external_hashes.get(change) else {
                                        continue;
                                    };
                                    let claimant = ConflictClaimantObject {
                                        change: *hash,
                                        inode: side.inode.get(),
                                        path: side.path.as_bytes().to_vec(),
                                        directory: side.is_directory(),
                                    };
                                    if !entry.claimants.contains(&claimant) {
                                        entry.claimants.push(claimant);
                                    }
                                }
                            }
                            if with_markers {
                                let path_local_sides: Vec<_> = conflict
                                    .sides_at_path(&path)
                                    .into_iter()
                                    .map(|side| (side.inode, side.position))
                                    .collect();
                                let rendered = super::materialize::render_name_conflict(
                                    &txn,
                                    &self.change_store,
                                    &inode_graph_table,
                                    &visibility,
                                    &external_hashes,
                                    &path,
                                    &path_local_sides,
                                )
                                .map_err(RepositoryError::Output)?;
                                marker_bytes.insert(path.clone(), rendered);
                            }
                        }
                    }
                    ConflictEntryKind::Order
                    | ConflictEntryKind::Cyclic
                    | ConflictEntryKind::Zombie
                    | ConflictEntryKind::ZombieFile => {
                        let Some(position) = position else {
                            return Err(RepositoryError::InvalidOperation {
                                message: format!(
                                    "conflict record for '{path}' references inode {inode} \
                                     that has no graph position"
                                ),
                            });
                        };
                        let materialization = atomic_core::output::project_inode_attributes(
                            &txn,
                            position,
                            visibility.attribute_visibility(),
                        )
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        if materialization.is_conflicted() {
                            // Attribute registers cannot be carried by
                            // markers; the refusal names them explicitly.
                            let attribute = materialization
                                .conflicts
                                .first()
                                .map(|conflict| format!("{:?}", conflict.name))
                                .unwrap_or_default();
                            return Err(RepositoryError::InvalidOperation {
                                message: ConflictProjectionError::AttributeConflict {
                                    path: path.clone(),
                                    attribute,
                                }
                                .to_string(),
                            });
                        }
                        let (bytes, regions) = super::materialize::capture_file_conflict_bytes(
                            &txn,
                            &self.change_store,
                            &inode_graph_table,
                            &visibility,
                            &external_hashes,
                            &path,
                            atomic_core::types::Inode::new(inode),
                            position,
                        )
                        .map_err(|message| {
                            RepositoryError::InvalidOperation {
                                message: ConflictProjectionError::Capture {
                                    path: path.clone(),
                                    message,
                                }
                                .to_string(),
                            }
                        })?;
                        if with_markers {
                            marker_bytes.insert(path.clone(), bytes);
                        }
                        for region in regions {
                            let mut sides = Vec::with_capacity(region.sides.len());
                            for side in &region.sides {
                                let change = external_hashes.get(&side.node.change).copied().ok_or_else(|| {
                                    RepositoryError::InvalidOperation {
                                        message: format!(
                                            "conflict side change {} of '{path}' \
                                             has no external hash",
                                            side.node.change.get()
                                        ),
                                    }
                                })?;
                                sides.push(ConflictSideObject {
                                    change,
                                    start: side.node.start.get(),
                                    end: side.node.end.get(),
                                    content: side.content,
                                    mode: u32::from(materialization.materialization.mode),
                                    kind: match materialization.materialization.kind {
                                        atomic_core::change::InodeKind::Regular => {
                                            ConflictSideKind::Regular
                                        }
                                        atomic_core::change::InodeKind::Symlink => {
                                            ConflictSideKind::Symlink
                                        }
                                        atomic_core::change::InodeKind::Gitlink => {
                                            ConflictSideKind::Gitlink
                                        }
                                    },
                                });
                            }
                            entry.sides = sides;
                            entry.line = Some(region.line);
                            if region.kind == ConflictEntryKind::Zombie {
                                // The zombie side is the deleted base that
                                // was concurrently modified.
                                entry.base = entry.sides.first().cloned();
                            }
                        }
                    }
                }
                // An entry with neither sides nor claimants cannot form a
                // complete conflict object: marker-text content (an `Order`
                // row persisted by materialize from a marker line) carries no
                // graph conflict regions, and "markers plus a hash alone"
                // never restores a conflict (RFC §8.3, ac-1). The row stays
                // persisted state; it does not enter the conflict-set object,
                // so the projection falls through to the clean path where
                // the V1 marker refusal names the path and line.
                if entry.sides.is_empty() && entry.claimants.is_empty() {
                    marker_bytes.remove(&path);
                    continue;
                }
                if let Some(file) = files
                    .iter_mut()
                    .find(|file| file.path == path.as_bytes())
                {
                    file.entries.push(entry);
                } else {
                    files.push(ConflictFileObject {
                        path: path.as_bytes().to_vec(),
                        inode,
                        entries: vec![entry],
                    });
                }
            }
        }
        if files.is_empty() {
            // Every row was marker-text state with no capturable sides: no
            // complete conflict object exists for this view.
            return Ok(None);
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        let object = ConflictSetObject {
            version: CONFLICT_SET_VERSION,
            files,
        };
        object
            .validate()
            .map_err(|error| RepositoryError::InvalidOperation {
                message: error.to_string(),
            })?;
        Ok(Some((object, marker_bytes)))
    }

    /// Prepare a Draft conflict snapshot projection (RFC §8.3, CB-8B): the
    /// complete conflict object, its identity, and the marker-materialized
    /// project tree it publishes as an ordinary Git commit.
    ///
    /// Refuses (never flattens) when the conflicted state cannot be carried
    /// by markers: case collisions on a case-insensitive filesystem, or an
    /// unresolved inode attribute register.
    pub fn prepare_conflict_snapshot_projection(
        &self,
        view_name: &str,
        policy: &ConversionPolicy,
    ) -> Result<ConflictSnapshotProjection, ConflictProjectionError> {
        use atomic_core::pristine::{GraphTxnT, ViewTxnT};

        let Some((conflict_set, marker_bytes)) =
            self.capture_view_conflict_set_with_markers(view_name)
                .map_err(ConflictProjectionError::Repository)?
        else {
            return Err(ConflictProjectionError::Repository(
                RepositoryError::InvalidOperation {
                    message: format!(
                        "view '{view_name}' has no persisted conflicts to project"
                    ),
                },
            ));
        };
        let conflict_set_hash = conflict_set
            .hash()
            .map_err(ConflictProjectionError::Object)?;
        match self.conflict_projection_representability(&conflict_set, policy) {
            ConflictRepresentability::Representable => {}
            ConflictRepresentability::PathCaseCollision { paths } => {
                return Err(ConflictProjectionError::PathCaseCollision { paths });
            }
            ConflictRepresentability::AttributeConflict { path, attribute } => {
                return Err(ConflictProjectionError::AttributeConflict { path, attribute });
            }
        }

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        let roots: Vec<Hash> = {
            let mut ordered = Vec::new();
            for entry in txn
                .iter_changes(&view, 0)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                let (_sequence, change_id, _state) =
                    entry.map_err(|e| RepositoryError::Database(e.to_string()))?;
                let hash = txn
                    .get_external(change_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or_else(|| RepositoryError::Database("change has no external hash".to_string()))?;
                ordered.push(hash);
            }
            ordered
        };
        let project = self
            .project_change_closure_with_conflict_markers(&txn, &view, &roots, policy, &marker_bytes)
            .map_err(|error| ConflictProjectionError::Repository(
                RepositoryError::InvalidOperation {
                    message: error.to_string(),
                },
            ))?;
        Ok(ConflictSnapshotProjection {
            conflict_set,
            conflict_set_hash,
            project,
            conflicted_paths: marker_bytes.into_keys().collect(),
        })
    }

    /// Why `conflict_set` can or cannot project as marker bytes under
    /// `policy`'s platform capabilities (RFC §6.2, §8.3).
    pub fn conflict_projection_representability(
        &self,
        conflict_set: &ConflictSetObject,
        policy: &ConversionPolicy,
    ) -> ConflictRepresentability {
        let mut lowercased: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for file in &conflict_set.files {
            let path = String::from_utf8_lossy(&file.path).to_string();
            lowercased
                .entry(path.to_lowercase())
                .or_default()
                .push(path.clone());
        }
        let collisions: Vec<String> = lowercased
            .into_values()
            .filter(|paths| paths.len() > 1)
            .flatten()
            .collect();
        if !collisions.is_empty() && !policy.platform.case_sensitive {
            return ConflictRepresentability::PathCaseCollision { paths: collisions };
        }
        ConflictRepresentability::Representable
    }
}
