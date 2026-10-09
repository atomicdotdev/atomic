//! A remote sandbox's local cache: the repository's own rows for one view,
//! enough for `status` and `record` without the repository.
//!
//! The serving repository exports ([`Repository::export_sandbox_skeleton`],
//! [`Repository::export_sandbox_slice`]); the sandbox's cache imports
//! ([`Repository::import_sandbox_skeleton`],
//! [`Repository::import_sandbox_slice`]). See `atomic_core::pristine::slice`
//! for what the rows are and why they are copied rather than re-derived.

use std::collections::BTreeSet;
use std::path::Path;

use atomic_core::change::ChangeStore as _;
use atomic_core::pristine::slice::{GraphSlice, ViewSnapshotRows};
use atomic_core::pristine::{decode_vertex, GraphTxnT, MutTxnT, TreeTxnT, ViewTxnT};
use atomic_core::types::{GraphNode, Hash, NodeId};
use serde::{Deserialize, Serialize};

use super::Repository;
use crate::RepositoryError;

/// A view's tree for a fresh cache: the skeleton rows, the view itself, and
/// every entry (with content) the view has.
///
/// `ancestors` carries the views whose changes are in the view's effective
/// perspective — for a draft, its draft ancestors and the nearest shared
/// ancestor — each with its *own* log, real state and real parent. Without
/// them a cache could only hold the view parentless, count inherited history
/// as the view's own work, and invent empty placeholders for views it was
/// told about but never given.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSkeleton {
    pub view: ViewSnapshotRows,
    #[serde(default)]
    pub ancestors: Vec<ViewSnapshotRows>,
    pub rows: GraphSlice,
}

/// The rows and content `record` reads for some paths.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SandboxSlice {
    pub rows: GraphSlice,
    pub spans: Vec<SpanBytes>,
}

/// Content bytes of one vertex: `change`'s content at `start..start + len`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpanBytes {
    pub change: Hash,
    pub start: u64,
    #[serde(with = "serde_bytes_b64")]
    pub bytes: Vec<u8>,
}

mod serde_bytes_b64 {
    use data_encoding::BASE64;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&BASE64.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        BASE64
            .decode(s.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

/// Schema tag for [`SandboxSkeleton::to_bytes`]: how the skeleton travels
/// as opaque versioned bytes.
pub const SANDBOX_SKELETON_SCHEMA: &str = "atomic.sandbox.skeleton.v1";

/// Schema tag for [`GraphSlice`] rows as [`SandboxSlice::rows_to_bytes`]
/// encodes them.
pub const SANDBOX_GRAPH_SLICE_SCHEMA: &str = "atomic.sandbox.graph-slice.v1";

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, RepositoryError> {
    postcard::to_allocvec(value).map_err(|e| RepositoryError::Serialization(e.to_string()))
}

fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, RepositoryError> {
    postcard::from_bytes(bytes).map_err(|e| RepositoryError::Serialization(e.to_string()))
}

impl SandboxSkeleton {
    /// The skeleton as bytes ([`SANDBOX_SKELETON_SCHEMA`]): what a transport
    /// carries and [`SandboxSkeleton::from_bytes`] reads back.
    pub fn to_bytes(&self) -> Result<Vec<u8>, RepositoryError> {
        encode(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RepositoryError> {
        decode(bytes)
    }
}

impl SandboxSlice {
    /// The slice's graph rows as bytes ([`SANDBOX_GRAPH_SLICE_SCHEMA`]). The
    /// spans travel beside them, one by one.
    pub fn rows_to_bytes(&self) -> Result<Vec<u8>, RepositoryError> {
        encode(&self.rows)
    }

    /// A slice from its encoded rows and its spans.
    pub fn from_parts(rows: &[u8], spans: Vec<SpanBytes>) -> Result<Self, RepositoryError> {
        Ok(Self {
            rows: decode(rows)?,
            spans,
        })
    }
}

/// Why the repository refused a sandbox's change. Nothing is written when
/// a change is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum SubmitRejection {
    #[error("the change's bytes hash to {computed}, not {claimed}")]
    HashMismatch { claimed: String, computed: String },
    #[error("not a change: {0}")]
    Malformed(String),
    #[error("the view has moved on (now {current}); fetch and record again")]
    StaleView { current: String },
    #[error("change {0} is not on this view")]
    ForeignChange(String),
    #[error("node {0} is not on this view")]
    ForeignNode(u64),
    #[error("a change may not touch {0}")]
    ForbiddenPath(String),
    #[error("change {0} is already in the repository")]
    AlreadyPresent(String),
}

/// A change file: its hash and V3 bytes.
pub type ChangeFile = (Hash, Vec<u8>);

/// A submitted change, applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Submitted {
    pub hash: Hash,
    /// The view's own Merkle state after it, base32.
    pub state: String,
    /// The view's effective state after it, base32
    /// ([`Repository::sandbox_effective_state`]): the fence for the next
    /// change recorded on it.
    #[serde(default)]
    pub effective: String,
}

/// What `SubmitChange` came back with: the change landed, or it did not.
pub type SubmittedOutcome =
    Result<(Submitted, SandboxSkeleton), (SubmitRejection, Option<SandboxSkeleton>)>;

/// Whether `path` may not be recorded, from a sandbox or otherwise.
///
/// Refused in two cases: the path is not a plain relative path, or it names the
/// repository's own machinery. The first is not a nicety. A submitted change
/// lands in `TREE`, and the next materialize or view switch writes
/// `root.join(path)` — so `../../elsewhere/x` escapes the working tree, with no
/// local user in the loop when a remote token-holder sends it. A reserved-name
/// list cannot catch that, which is why the shape check comes first.
///
/// An empty path is not a path: a `FileOps` group for a token-level operation
/// names an inode and carries no path, and there is nothing to escape with.
pub(crate) fn forbidden_path(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let relative = std::path::Path::new(path);
    if relative.is_absolute() {
        return true;
    }
    // `components()` collapses repeated separators, so `src//main.rs` would
    // read here as two plain names — and then reach `TREE` as a path no
    // materialization can ever put a file at, because the write normalizes it
    // back to `src/main.rs`.
    if path.contains("//") || path.ends_with('/') {
        return true;
    }
    relative.components().any(|part| match part {
        // Not `Normal` means empty, `.`, `..`, or a root/prefix: none of which
        // a tracked file may have.
        std::path::Component::Normal(name) => {
            name == crate::DOT_DIR
                || name == super::SANDBOX_POINTER
                || name == super::SANDBOX_CACHE_DIR
        }
        _ => true,
    })
}

/// Where a materialized entry may land: inside `dir`, and nowhere else.
///
/// An entry's path arrives over a transport, and the pointer that chose `dir`
/// is itself a file in the tree the sandbox was handed — so a path that climbs
/// out, or names the pointer or the cache, would let whatever sent it rewrite
/// the sandbox's trust anchor or write somewhere it was never given. Only plain
/// relative components pass (no `..`, no root, no empty or `.`), and none may
/// be `.atomic`, the pointer or the cache directory.
fn sandbox_entry_path(dir: &Path, path: &str) -> Result<std::path::PathBuf, RepositoryError> {
    let refuse = |why: &str| RepositoryError::InvalidOperation {
        message: format!("refusing materialized entry {path:?}: {why}"),
    };
    let relative = Path::new(path);
    if relative.as_os_str().is_empty() {
        return Err(refuse("empty path"));
    }
    if !relative.is_relative() {
        return Err(refuse("not a relative path"));
    }
    for part in relative.components() {
        let std::path::Component::Normal(name) = part else {
            return Err(refuse("not a plain relative path"));
        };
        if name == crate::DOT_DIR
            || name == super::SANDBOX_POINTER
            || name == super::SANDBOX_CACHE_DIR
        {
            return Err(refuse("names the sandbox's own bookkeeping"));
        }
    }
    Ok(dir.join(relative))
}

/// The parent a write lands in, checked to still be inside `dir` after the
/// filesystem has resolved symlinks — a parts check cannot see a parent that
/// is a symlink out of the tree.
fn sandbox_entry_parent<'a>(dir: &'a Path, path: &'a Path) -> Result<&'a Path, RepositoryError> {
    let parent = path.parent().unwrap_or(dir);
    std::fs::create_dir_all(parent)?;
    if !parent.canonicalize()?.starts_with(dir.canonicalize()?) {
        return Err(RepositoryError::InvalidOperation {
            message: format!(
                "refusing materialized entry {}: its parent resolves outside the sandbox",
                path.display()
            ),
        });
    }
    Ok(parent)
}

/// Client side of materializing a view into a remote sandbox: write one
/// entry (as [`Repository::materialize_view_entries`] renders it) under
/// `dir`. An existing file at the path is replaced; nothing else is touched.
///
/// Every path is checked lexically first (plain relative components, no
/// reserved bookkeeping names), and every write's parent again after the
/// filesystem has resolved it, so an entry can neither climb out of `dir`
/// nor re-point the sandbox. Write every entry, then make the cache with
/// [`Repository::create_remote_sandbox_cache`].
pub fn write_sandbox_entry(
    dir: &Path,
    path: &str,
    kind: super::ViewEntryKind,
    mode: u32,
    content: &[u8],
) -> Result<(), RepositoryError> {
    use super::ViewEntryKind;
    let target = sandbox_entry_path(dir, path)?;
    match kind {
        ViewEntryKind::Directory => {
            sandbox_entry_parent(dir, &target)?;
            std::fs::create_dir_all(&target)?;
        }
        ViewEntryKind::Symlink => {
            sandbox_entry_parent(dir, &target)?;
            match std::fs::symlink_metadata(&target) {
                Ok(_) => std::fs::remove_file(&target)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            #[cfg(unix)]
            {
                let link = std::str::from_utf8(content).map_err(|e| {
                    RepositoryError::InvalidOperation {
                        message: format!("symlink {path:?} has a non-UTF-8 target: {e}"),
                    }
                })?;
                std::os::unix::fs::symlink(link, &target)?;
            }
            #[cfg(not(unix))]
            std::fs::write(&target, content)?;
        }
        ViewEntryKind::File => {
            sandbox_entry_parent(dir, &target)?;
            // Never write through a symlink already sitting at the path.
            if std::fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
                std::fs::remove_file(&target)?;
            }
            std::fs::write(&target, content)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = if mode & 0o777 == 0 {
                    0o644
                } else {
                    mode & 0o777
                };
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))?;
            }
            #[cfg(not(unix))]
            let _ = mode;
        }
    }
    Ok(())
}

/// How a remote sandbox's cache reaches its repository's owner. The
/// transport lives with whoever runs the process (the `atomic` CLI installs
/// one at startup); with it installed, `record`, `write_recorded` and the
/// readers below work in a remote sandbox for every caller.
pub trait RemoteSandboxLink: Send + Sync {
    /// `FileStates`: the rows and content for `inodes`.
    fn file_states(&self, root: &Path, inodes: Vec<u64>) -> Result<SandboxSlice, String>;
    /// `SubmitChange`: the change recorded against `base_state` — the view's
    /// effective state as the cache holds it
    /// ([`Repository::remote_sandbox_effective_state`]); the view's skeleton
    /// after it lands.
    ///
    /// A `StaleView` refusal comes back with the view's current skeleton, so
    /// the cache can resync from the refusal itself. Without it a sandbox that
    /// fell behind could never record again: its state is the thing that is
    /// stale, and the only way to learn the new one is a request the owner has
    /// no reason to distinguish from a fresh sandbox.
    fn submit(
        &self,
        root: &Path,
        base_state: String,
        hash: Hash,
        bytes: Vec<u8>,
    ) -> Result<SubmittedOutcome, String>;
    /// `Changes`: change files the view has.
    fn changes(&self, root: &Path, hashes: Vec<Hash>) -> Result<Vec<ChangeFile>, String>;
    /// `PublishProvenance`: a checkpoint's provenance graph (serialized) and
    /// session turn, published in the repository.
    fn publish_provenance(
        &self,
        root: &Path,
        graph: Vec<u8>,
        turn: atomic_core::change::session::SessionTurn,
    ) -> Result<
        Result<atomic_core::change::session::SessionCheckpointPublication, SubmitRejection>,
        String,
    >;
}

static LINK: std::sync::OnceLock<Box<dyn RemoteSandboxLink>> = std::sync::OnceLock::new();

/// Install the process's link to remote sandbox owners (once).
pub fn set_remote_sandbox_link(link: Box<dyn RemoteSandboxLink>) {
    let _ = LINK.set(link);
}

fn link() -> Result<&'static dyn RemoteSandboxLink, RepositoryError> {
    LINK.get()
        .map(|l| l.as_ref())
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: "this process can't reach a remote sandbox's owner".to_string(),
        })
}

/// Marks a cache whose rows moved past its working files (a stale refusal's
/// skeleton was imported); see [`Repository::remote_sandbox_is_behind`].
const BEHIND_MARKER: &str = "remote-sandbox-behind";

fn db(e: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::Database(e.to_string())
}

/// The views whose changes are in a draft's effective perspective: its draft
/// ancestors, and the nearest shared ancestor past them. A shared view needs
/// nobody — its perspective is its own log.
///
/// This mirrors the ancestor walk in [`collect_visible_change_ids`] exactly,
/// because the cache reconstructs the union from the chain this returns: if
/// the two ever disagree, the cache's perspective and the rows it was given
/// disagree with it.
fn sandbox_view_chain(
    txn: &atomic_core::pristine::ReadTxn,
    state: &atomic_core::pristine::ViewState,
) -> Result<Vec<atomic_core::pristine::ViewState>, RepositoryError> {
    if state.kind.is_shared() {
        return Ok(Vec::new());
    }
    let db = |e: atomic_core::pristine::PristineError| RepositoryError::Database(e.to_string());
    let mut chain = Vec::new();
    for id in txn.resolve_view_chain(state).map_err(db)? {
        if id == state.id {
            continue; // the view itself is exported separately
        }
        if let Some(ancestor) = txn.get_view_by_id(id).map_err(db)? {
            chain.push(ancestor);
        }
    }
    let mut cursor = state.parent;
    while let Some(pid) = cursor {
        match txn.get_view_by_id(pid).map_err(db)? {
            Some(p) if p.kind.is_shared() => {
                chain.push(p);
                break;
            }
            Some(p) => cursor = p.parent,
            None => break,
        }
    }
    Ok(chain)
}

/// The digest of `state`'s effective perspective: the view and each view in
/// [`sandbox_view_chain`], by id, scope, own Merkle state and change count.
///
/// The view's own Merkle state is not enough to fence a draft: a change
/// landing on an ancestor moves what the draft sees without touching its own
/// log. Every input here is a row a sandbox's cache holds verbatim (the
/// skeleton carries the view and its ancestors as themselves), so the cache
/// computes the same digest from its own pristine.
fn effective_state(
    txn: &atomic_core::pristine::ReadTxn,
    state: &atomic_core::pristine::ViewState,
) -> Result<Hash, RepositoryError> {
    let mut bytes = b"atomic.sandbox.effective-view.v1".to_vec();
    for view in std::iter::once(state.clone()).chain(sandbox_view_chain(txn, state)?) {
        // Hash input only, never stored or sorted.
        bytes.extend_from_slice(&view.id.to_le_bytes());
        bytes.push(view.kind as u8);
        bytes.extend_from_slice(view.state.as_bytes());
        bytes.extend_from_slice(&view.change_count.to_le_bytes());
    }
    Ok(Hash::of(&bytes))
}

impl Repository {
    /// The visible change ids of `view`, sorted.
    fn sandbox_visible(
        &self,
        txn: &atomic_core::pristine::ReadTxn,
        view: &atomic_core::pristine::ViewState,
    ) -> Result<Vec<u64>, RepositoryError> {
        let mut ids: Vec<u64> = super::collect_visible_change_ids(txn, view)?
            .into_iter()
            .map(|id| id.get())
            .collect();
        ids.sort_unstable();
        Ok(ids)
    }

    /// `view`'s effective state ([`effective_state`]): what a change recorded
    /// against the view is fenced on, and what
    /// [`Repository::insert_submitted_change`] compares `base_state` to.
    pub fn sandbox_effective_state(&self, view: &str) -> Result<Hash, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(db)?;
        let state =
            txn.get_view(view)
                .map_err(db)?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view.to_string(),
                })?;
        effective_state(&txn, &state)
    }

    /// Serve side: `view`'s skeleton, with `live` its rendered tree (inode →
    /// path, as [`Repository::materialize_view_entries`] gives them).
    ///
    /// The view goes out as itself — its scope, its parent, its *own* change
    /// log — and its ancestors beside it, each with their own. The rows are
    /// still filtered by the union: a cache reconstructs the union from the
    /// chain, and this builds that union the same way
    /// [`super::collect_visible_change_ids`] does, so what the cache
    /// reconstructs is what the rows were filtered by.
    pub fn export_sandbox_skeleton(
        &self,
        view: &str,
        live: &std::collections::BTreeMap<u64, String>,
    ) -> Result<SandboxSkeleton, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(db)?;
        let state =
            txn.get_view(view)
                .map_err(db)?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view.to_string(),
                })?;
        let own = super::collect_view_change_ids(&txn, &state)?;
        let chain = sandbox_view_chain(&txn, &state)?;
        let mut union: BTreeSet<u64> = own.iter().map(|id| id.get()).collect();
        for ancestor in &chain {
            union.extend(
                super::collect_view_change_ids(&txn, ancestor)?
                    .iter()
                    .map(|id| id.get()),
            );
        }
        let own_sorted = own.iter().map(|id| id.get()).collect();
        let rows = txn.export_skeleton(&union, live).map_err(db)?;
        Ok(SandboxSkeleton {
            view: txn.export_view_snapshot(&state, own_sorted),
            ancestors: chain
                .iter()
                .map(|a| {
                    let ids = super::collect_view_change_ids(&txn, a)?;
                    Ok(txn.export_view_snapshot(a, ids.iter().map(|id| id.get()).collect()))
                })
                .collect::<Result<Vec<_>, RepositoryError>>()?,
            rows,
        })
    }

    /// Serve side: what `record` on `view` reads for `inodes`, with the
    /// content of every vertex a visible change introduced.
    pub fn export_sandbox_slice(
        &self,
        view: &str,
        inodes: &[u64],
    ) -> Result<SandboxSlice, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(db)?;
        let state =
            txn.get_view(view)
                .map_err(db)?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view.to_string(),
                })?;
        let visible: BTreeSet<u64> = self.sandbox_visible(&txn, &state)?.into_iter().collect();
        // Only files the view has: an inode another view introduced is not
        // this sandbox's to read.
        let mut on_view = Vec::with_capacity(inodes.len());
        for &inode in inodes {
            let position = txn
                .inode_position(atomic_core::types::Inode::new(inode))
                .map_err(db)?;
            if position.is_some_and(|p| p.change.is_root() || visible.contains(&p.change.get())) {
                on_view.push(inode);
            }
        }
        let rows = txn
            .export_graph_slice(&on_view, state.id, &visible)
            .map_err(db)?;
        let mut spans = Vec::new();
        for (key, _) in &rows.graph {
            let (change, start, end) = decode_vertex(key);
            if start >= end || !visible.contains(&change) {
                continue;
            }
            let Some(hash) = txn.get_external(NodeId::new(change)).map_err(db)? else {
                continue;
            };
            let node = GraphNode {
                change: NodeId::new(change),
                start: atomic_core::types::ChangePosition::new(start),
                end: atomic_core::types::ChangePosition::new(end),
            };
            let mut bytes = vec![0u8; (end - start) as usize];
            self.change_store
                .get_contents(|_| Some(hash), node, &mut bytes)
                .map_err(db)?;
            spans.push(SpanBytes {
                change: hash,
                start,
                bytes,
            });
        }
        Ok(SandboxSlice { rows, spans })
    }

    /// Serve side: the view's skeleton as it is right now, with nothing of it
    /// applied. What a sandbox needs after being told its base state is stale:
    /// its own view row is the stale one, so it cannot work out the current
    /// state on its own, and asking again for a state it was just refused on
    /// is a round trip the owner already has the answer for.
    pub fn current_sandbox_skeleton(&self, view: &str) -> Result<SandboxSkeleton, RepositoryError> {
        let mut live = std::collections::BTreeMap::new();
        self.materialize_view_entries::<()>(view, |entry| {
            live.insert(entry.inode, entry.path.clone());
            Ok(())
        })?
        .map_err(|()| RepositoryError::Output("unreachable".to_string()))?;
        self.export_sandbox_skeleton(view, &live)
    }

    /// Serve side: take a change a remote sandbox recorded on `view` and
    /// apply it there — if it is exactly what it claims, recorded against
    /// the view as it is now, and names nothing outside the view.
    ///
    /// The checks, in order: the bytes hash to `hash`; the view's effective
    /// state ([`Repository::sandbox_effective_state`]) is still `base_state`
    /// (otherwise the sandbox fetches and records again); every
    /// change it depends on or refers to that the repository knows is
    /// visible on the view; every node its file operations name is a visible
    /// change's; no path touches `.atomic`, `.atomic-sandbox` or
    /// `.atomic-sandbox.d`; and it is new. Then it is saved and inserted
    /// into `view`. Callers serialize submissions (one writer).
    pub fn insert_submitted_change(
        &self,
        view: &str,
        base_state: &str,
        hash: &Hash,
        bytes: &[u8],
    ) -> Result<Result<Submitted, SubmitRejection>, RepositoryError> {
        use atomic_core::change::format_v3::reader::ChangeReader;
        use atomic_core::types::Base32;

        let (change, computed) = match atomic_core::change::Change::deserialize(&mut &bytes[..]) {
            Ok(parsed) => parsed,
            Err(e) => return Ok(Err(SubmitRejection::Malformed(e.to_string()))),
        };
        if computed != *hash {
            return Ok(Err(SubmitRejection::HashMismatch {
                claimed: hash.to_base32(),
                computed: computed.to_base32(),
            }));
        }
        let referenced: Vec<Hash> = match ChangeReader::open(&mut &bytes[..]) {
            Ok(reader) => reader
                .hash_table()
                .hashes()
                .iter()
                .map(|h| Hash::from(*h))
                .filter(|h| h != hash)
                .collect(),
            Err(e) => return Ok(Err(SubmitRejection::Malformed(e.to_string()))),
        };

        {
            let txn = self.pristine.read_txn().map_err(db)?;
            let state =
                txn.get_view(view)
                    .map_err(db)?
                    .ok_or_else(|| RepositoryError::ViewNotFound {
                        name: view.to_string(),
                    })?;
            let current = effective_state(&txn, &state)?.to_base32();
            if current != base_state {
                return Ok(Err(SubmitRejection::StaleView { current }));
            }
            if txn.get_internal(hash).map_err(db)?.is_some() {
                return Ok(Err(SubmitRejection::AlreadyPresent(hash.to_base32())));
            }
            let visible = super::collect_visible_change_ids(&txn, &state)?;
            for dep in change.dependencies() {
                match txn.get_internal(dep).map_err(db)? {
                    Some(id) if visible.contains(&id) => {}
                    _ => return Ok(Err(SubmitRejection::ForeignChange(dep.to_base32()))),
                }
            }
            for other in &referenced {
                if let Some(id) = txn.get_internal(other).map_err(db)? {
                    if !visible.contains(&id) {
                        return Ok(Err(SubmitRejection::ForeignChange(other.to_base32())));
                    }
                }
            }
            for ops in change.file_ops() {
                if let Some(id) = ops
                    .referenced_node_ids()
                    .into_iter()
                    .find(|id| !visible.contains(id))
                {
                    return Ok(Err(SubmitRejection::ForeignNode(id.get())));
                }
                if forbidden_path(ops.path()) {
                    return Ok(Err(SubmitRejection::ForbiddenPath(ops.path().to_string())));
                }
            }
            for hunk in change.hunks() {
                if let Some(path) = hunk.path().filter(|p| forbidden_path(p)) {
                    return Ok(Err(SubmitRejection::ForbiddenPath(path.to_string())));
                }
            }
        }

        self.save_change_bytes(hash, bytes, &change)?;
        let outcome = match self.insert_change(hash, crate::InsertOptions::default().view(view)) {
            Ok(outcome) => outcome,
            Err(e) => {
                // Nothing of it stays: the change file goes with the failure.
                let _ = std::fs::remove_file(self.change_store.change_path(hash));
                return Err(e);
            }
        };
        // Write the change's files into the working copy, when there is one on
        // this view.
        //
        // `insert_change` is a library function whose working-copy contract is
        // "clean up after deletions and moves" — it never writes new files, and
        // every other caller remembers to materialize afterwards. This caller
        // did not, and a view is not only moved by whoever is sitting on it: a
        // sandbox's change lands here through the owner, and the local tree
        // checked out on that view was left behind. `status` then saw the
        // view's new file missing from disk and reported it as `deleted:`, and
        // the next `record -a` committed that deletion — silently undoing the
        // sandbox's work, in the repository the sandbox was writing to.
        self.materialize_submitted_view(view, hash)?;
        Ok(Ok(Submitted {
            hash: *hash,
            state: outcome.new_state.to_base32(),
            effective: self.sandbox_effective_state(view)?.to_base32(),
        }))
    }

    /// Bring the working copy on `view` up to date with the change `hash`,
    /// touching nothing if `view` is not the checked-out one.
    ///
    /// Only the paths the change names, as every other insert caller does, and
    /// a full materialize when the change names none. A view that is not
    /// current is left alone on purpose: the user switches to it to see its
    /// files, exactly as a pull into another view does.
    fn materialize_submitted_view(&self, view: &str, hash: &Hash) -> Result<(), RepositoryError> {
        if view != self.current_view {
            return Ok(());
        }
        let mut affected = std::collections::HashSet::new();
        if let Ok(change) = self.load_change(hash) {
            for op in change.hunks() {
                if let Some(path) = op.path() {
                    affected.insert(path.to_string());
                }
            }
        }
        if affected.is_empty() {
            self.materialize()?;
        } else {
            self.materialize_paths(affected)?;
        }
        Ok(())
    }

    /// Serve side: the V3 bytes of each of `hashes` — changes `view` can
    /// see, and only those (anything else is refused, whole).
    pub fn export_sandbox_changes(
        &self,
        view: &str,
        hashes: &[Hash],
    ) -> Result<Result<Vec<ChangeFile>, SubmitRejection>, RepositoryError> {
        use atomic_core::types::Base32;
        let txn = self.pristine.read_txn().map_err(db)?;
        let state =
            txn.get_view(view)
                .map_err(db)?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view.to_string(),
                })?;
        let visible = super::collect_visible_change_ids(&txn, &state)?;
        let mut out = Vec::with_capacity(hashes.len());
        for hash in hashes {
            match txn.get_internal(hash).map_err(db)? {
                Some(id) if visible.contains(&id) => {}
                _ => return Ok(Err(SubmitRejection::ForeignChange(hash.to_base32()))),
            }
            out.push((*hash, std::fs::read(self.change_store.change_path(hash))?));
        }
        Ok(Ok(out))
    }

    /// The first of `hashes` that `view` cannot see, if any — for checking
    /// that what a sandbox's provenance explains is its own view's work.
    pub fn first_foreign_change(
        &self,
        view: &str,
        hashes: &[Hash],
    ) -> Result<Option<Hash>, RepositoryError> {
        if hashes.is_empty() {
            return Ok(None);
        }
        let txn = self.pristine.read_txn().map_err(db)?;
        let state =
            txn.get_view(view)
                .map_err(db)?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view.to_string(),
                })?;
        let visible = super::collect_visible_change_ids(&txn, &state)?;
        for hash in hashes {
            match txn.get_internal(hash).map_err(db)? {
                Some(id) if visible.contains(&id) => {}
                _ => return Ok(Some(*hash)),
            }
        }
        Ok(None)
    }

    /// Cache side: the changes the view has that this cache holds no file
    /// for.
    pub fn missing_sandbox_changes(&self) -> Result<Vec<Hash>, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(db)?;
        let state = txn
            .get_view(self.current_view())
            .map_err(db)?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view().to_string(),
            })?;
        let mut missing = Vec::new();
        for id in super::collect_visible_change_ids(&txn, &state)? {
            if let Some(hash) = txn.get_external(id).map_err(db)? {
                if !self.change_store.has_change(&hash) {
                    missing.push(hash);
                }
            }
        }
        Ok(missing)
    }

    /// Cache side: keep change files the owner sent — each only if its
    /// bytes hash to what it claims.
    pub fn hold_sandbox_changes(&self, changes: &[ChangeFile]) -> Result<(), RepositoryError> {
        use atomic_core::types::Base32;
        for (hash, bytes) in changes {
            let (change, computed) = atomic_core::change::Change::deserialize(&mut &bytes[..])
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            if computed != *hash {
                return Err(RepositoryError::Database(format!(
                    "the owner sent {} as {}",
                    computed.to_base32(),
                    hash.to_base32()
                )));
            }
            self.save_change_bytes(hash, bytes, &change)?;
        }
        Ok(())
    }

    /// In a remote sandbox: load what reading or recording the working
    /// tree needs from the owner (see [`Repository::sandbox_slice_inodes`]).
    /// Elsewhere, nothing.
    pub fn hydrate_remote_sandbox(&self) -> Result<(), RepositoryError> {
        if !self.is_remote_sandbox() {
            return Ok(());
        }
        if self.remote_sandbox_is_behind() {
            return Err(RepositoryError::InvalidOperation {
                message: "this sandbox's view moved on under it; materialize it again \
                          (`atomic sandbox materialize`: its working files predate what \
                          landed) and redo the edit"
                    .to_string(),
            });
        }
        let inodes = self.sandbox_slice_inodes()?;
        let slice = link()?
            .file_states(&self.root, inodes)
            .map_err(|message| RepositoryError::InvalidOperation { message })?;
        self.import_sandbox_slice(&slice)
    }

    /// In a remote sandbox: fetch the change files the view has and the
    /// cache lacks. Returns how many. Elsewhere, nothing.
    pub fn fetch_remote_sandbox_changes(&self) -> Result<usize, RepositoryError> {
        if !self.is_remote_sandbox() {
            return Ok(0);
        }
        let missing = self.missing_sandbox_changes()?;
        for batch in missing.chunks(32) {
            let changes = link()?
                .changes(&self.root, batch.to_vec())
                .map_err(|message| RepositoryError::InvalidOperation { message })?;
            self.hold_sandbox_changes(&changes)?;
        }
        Ok(missing.len())
    }

    /// In a remote sandbox, publishing a checkpoint means publishing it in
    /// the repository.
    pub(crate) fn publish_remote_provenance_checkpoint(
        &self,
        graph: &atomic_core::change::ProvenanceGraph,
        turn: atomic_core::change::session::SessionTurn,
    ) -> Result<atomic_core::change::session::SessionCheckpointPublication, RepositoryError> {
        let bytes = graph
            .serialize()
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
        link()?
            .publish_provenance(&self.root, bytes, turn)
            .map_err(|message| RepositoryError::InvalidOperation { message })?
            .map_err(|rejection| {
                RepositoryError::Apply(format!(
                    "the repository refused the provenance: {rejection}"
                ))
            })
    }

    /// In a remote sandbox, what applying a recorded change means: hand it
    /// to the owner, and take the view as it is once it lands.
    ///
    /// A refusal that says the view moved on is not a dead end: the owner
    /// sends the view as it is now, and the cache takes it, so the next
    /// `record` is computed against the graph this one was refused for. The
    /// change that was refused is not applied — the caller has to record it
    /// again — and the error says so.
    pub(crate) fn submit_recorded(
        &self,
        outcome: &crate::record::RecordOutcome,
    ) -> Result<crate::InsertOutcome, RepositoryError> {
        let bytes = outcome
            .v3_bytes()
            .ok_or_else(|| RepositoryError::Apply("the change has no bytes".to_string()))?
            .to_vec();
        let base_state = self.remote_sandbox_effective_state()?;
        let (submitted, skeleton) = match link()?
            .submit(&self.root, base_state, *outcome.hash(), bytes)
            .map_err(|message| RepositoryError::InvalidOperation { message })?
        {
            Ok(landed) => landed,
            Err((rejection, resync)) => {
                if let Some(resync) = resync {
                    self.import_sandbox_skeleton(&resync)?;
                    // The rows are current now; the files on disk are not.
                    // Recording over them would read whatever landed
                    // underneath as this sandbox's own edits — a file
                    // another writer added reads as deleted here, and the
                    // next record would delete it from the view.
                    std::fs::write(self.dot_dir.join(BEHIND_MARKER), rejection.to_string())?;
                }
                return Err(RepositoryError::Apply(match rejection {
                    SubmitRejection::StaleView { .. } => {
                        "the repository refused the change: the view has moved on since this \
                         sandbox was materialized (VIEW_STALE); materialize it again \
                         (`atomic sandbox materialize`) and redo the edit"
                            .to_string()
                    }
                    rejection => format!("the repository refused the change: {rejection}"),
                }));
            }
        };
        self.import_sandbox_skeleton(&skeleton)?;
        let state =
            <Hash as atomic_core::types::Base32>::from_base32(submitted.state.as_bytes())
                .ok_or_else(|| RepositoryError::Apply("the owner sent a bad state".to_string()))?;
        let mut stats = crate::InsertStats::new();
        stats.changes_applied = 1;
        stats.applied_hashes.push(submitted.hash);
        Ok(crate::InsertOutcome::new(
            state,
            skeleton.view.change_count,
            false,
            stats,
        ))
    }

    /// Serve side, once a sandbox's change `hash` has landed on `view`: the
    /// view's skeleton for the sandbox's cache, and — as a pull does for the
    /// files it writes — the vault files the change touched, indexed into
    /// the repository's (repository-wide) vault. One render of the view
    /// serves both.
    pub fn after_sandbox_submit(
        &self,
        view: &str,
        hash: &Hash,
    ) -> Result<SandboxSkeleton, RepositoryError> {
        let change = self.load_change(hash)?;
        let vault_paths: BTreeSet<String> = change
            .hunks()
            .iter()
            .filter_map(|h| h.path())
            .chain(change.file_ops().iter().map(|ops| ops.path()))
            .filter(|p| p.starts_with(".vault/") && p.ends_with(".md"))
            .map(str::to_string)
            .collect();
        let mut live = std::collections::BTreeMap::new();
        let mut files = std::collections::BTreeMap::new();
        self.materialize_view_entries::<()>(view, |entry| {
            live.insert(entry.inode, entry.path.clone());
            if vault_paths.contains(&entry.path) {
                files.insert(
                    entry.path,
                    String::from_utf8_lossy(&entry.content).into_owned(),
                );
            }
            Ok(())
        })?
        .map_err(|()| RepositoryError::Output("unreachable".to_string()))?;
        if !vault_paths.is_empty() && self.has_vault()? {
            let rows: Vec<(String, Option<String>)> = vault_paths
                .iter()
                .map(|p| (p[".vault/".len()..].to_string(), files.remove(p)))
                .collect();
            self.vault_record_files(&rows)?;
        }
        self.export_sandbox_skeleton(view, &live)
    }

    /// Serve side: publish a remote sandbox's checkpoint — if everything its
    /// provenance explains is on `view`. The graph arrives serialized, so
    /// what is published is exactly what the sandbox hashed.
    pub fn publish_sandbox_provenance(
        &self,
        view: &str,
        graph: &[u8],
        turn: atomic_core::change::session::SessionTurn,
    ) -> Result<
        Result<atomic_core::change::session::SessionCheckpointPublication, SubmitRejection>,
        RepositoryError,
    > {
        use atomic_core::types::Base32;
        let (graph, _) = match atomic_core::change::ProvenanceGraph::deserialize(graph) {
            Ok(parsed) => parsed,
            Err(e) => return Ok(Err(SubmitRejection::Malformed(e.to_string()))),
        };
        {
            let txn = self.pristine.read_txn().map_err(db)?;
            let state =
                txn.get_view(view)
                    .map_err(db)?
                    .ok_or_else(|| RepositoryError::ViewNotFound {
                        name: view.to_string(),
                    })?;
            let visible = super::collect_visible_change_ids(&txn, &state)?;
            for change in &graph.changes_explained {
                match txn.get_internal(change).map_err(db)? {
                    Some(id) if visible.contains(&id) => {}
                    _ => return Ok(Err(SubmitRejection::ForeignChange(change.to_base32()))),
                }
            }
        }
        Ok(Ok(self.publish_local_provenance_checkpoint(&graph, turn)?))
    }

    /// Cache side: the view's state as the cache has it (base32) — what a
    /// change recorded here is recorded against.
    pub fn remote_sandbox_view_state(&self) -> Result<String, RepositoryError> {
        use atomic_core::types::Base32;
        let txn = self.pristine.read_txn().map_err(db)?;
        let state = txn
            .get_view(self.current_view())
            .map_err(db)?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view().to_string(),
            })?;
        Ok(state.state.to_base32())
    }

    /// Cache side: whether a submission was refused as stale since this
    /// cache was made. Its rows were brought up to date by the refusal, but
    /// its working files were not, so it records nothing until it is
    /// materialized again ([`Repository::create_remote_sandbox_cache`]
    /// starts a fresh cache).
    pub fn remote_sandbox_is_behind(&self) -> bool {
        self.is_remote_sandbox() && self.dot_dir.join(BEHIND_MARKER).exists()
    }

    /// Cache side: the view's effective state as the cache has it (base32) —
    /// the fence a change recorded here is submitted with. The same digest
    /// the repository computes for the view, from the rows the cache holds.
    pub fn remote_sandbox_effective_state(&self) -> Result<String, RepositoryError> {
        use atomic_core::types::Base32;
        Ok(self
            .sandbox_effective_state(self.current_view())?
            .to_base32())
    }

    /// Cache side: replace this cache's tree and view with `skeleton`'s.
    pub fn import_sandbox_skeleton(
        &self,
        skeleton: &SandboxSkeleton,
    ) -> Result<(), RepositoryError> {
        let mut txn = self.pristine.write_txn().map_err(db)?;
        // Ancestors first, so the view's parent link lands on a row that
        // exists. Each import replaces any view with that name or id, which is
        // what turns a cache's invented placeholder for the parent into the
        // real one.
        for ancestor in &skeleton.ancestors {
            txn.import_view_snapshot(ancestor).map_err(db)?;
        }
        txn.import_view_snapshot(&skeleton.view).map_err(db)?;
        txn.import_skeleton(&skeleton.rows).map_err(db)?;
        txn.commit().map_err(db)?;
        Ok(())
    }

    /// Cache side: the inodes `record` will read — each tracked file that
    /// differs from its baseline, and every tracked directory on the way to
    /// any changed or new path (a new file's parent is where it is added).
    /// After [`Repository::reindex_working_copy`] on a fresh tree, a clean
    /// file costs nothing here.
    pub fn sandbox_slice_inodes(&self) -> Result<Vec<u64>, RepositoryError> {
        use crate::status::{FileStatus, StatusOptions};
        use atomic_core::pristine::slice::LOCAL_INODE_FLOOR;

        let status = self.status(StatusOptions::default())?;
        let mut out = BTreeSet::new();
        let keep = |inode: atomic_core::types::Inode, out: &mut BTreeSet<u64>| {
            if inode.get() < LOCAL_INODE_FLOOR {
                out.insert(inode.get());
            }
        };
        for entry in status.entries() {
            if entry.status() == FileStatus::Clean {
                continue;
            }
            if let Some(inode) = entry.inode() {
                keep(inode, &mut out);
            } else if let Some(inode) = self.get_file_inode(entry.path())? {
                keep(inode, &mut out);
            }
            let mut dir = entry.path().parent();
            while let Some(d) = dir.filter(|d| !d.as_os_str().is_empty()) {
                if let Some(inode) = self.get_file_inode(d)? {
                    keep(inode, &mut out);
                }
                dir = d.parent();
            }
        }
        Ok(out.into_iter().collect())
    }

    /// Cache side: replace this cache's graph rows and held content with
    /// `slice`'s.
    pub fn import_sandbox_slice(&self, slice: &SandboxSlice) -> Result<(), RepositoryError> {
        let view_id = {
            let txn = self.pristine.read_txn().map_err(db)?;
            txn.get_view(self.current_view())
                .map_err(db)?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: self.current_view().to_string(),
                })?
                .id
        };
        let mut txn = self.pristine.write_txn().map_err(db)?;
        txn.import_graph_slice(&slice.rows, view_id).map_err(db)?;
        txn.commit().map_err(db)?;
        self.change_store.clear_spans();
        for span in &slice.spans {
            self.change_store
                .hold_span(span.change, span.start, span.bytes.clone());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{forbidden_path, sandbox_entry_parent, sandbox_entry_path, write_sandbox_entry};
    use crate::{ViewEntryKind, DOT_DIR, SANDBOX_CACHE_DIR, SANDBOX_POINTER};

    /// A path off the wire only ever resolves inside the tree, and never
    /// onto the pointer or the cache — those two are how a sandbox picks who
    /// it trusts and which database it records into.
    #[test]
    fn a_materialized_path_stays_inside_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(
            sandbox_entry_path(root, "src/main.rs").unwrap(),
            root.join("src/main.rs")
        );
        for path in [
            "",
            "/etc/passwd",
            "..",
            "../elsewhere/x",
            "src/../../elsewhere/x",
            "./src/main.rs",
            "src/../..",
            DOT_DIR,
            ".atomic/config.toml",
            "src/.atomic",
            SANDBOX_POINTER,
            SANDBOX_CACHE_DIR,
            "nested/.atomic-sandbox.d",
        ] {
            assert!(
                sandbox_entry_path(root, path).is_err(),
                "{path:?} should be refused"
            );
            assert!(
                write_sandbox_entry(root, path, ViewEntryKind::File, 0o644, b"x").is_err(),
                "{path:?} should not be written"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_write_through_a_symlinked_parent_is_refused() {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::os::unix::fs::symlink(outside.path(), root.join("escape")).unwrap();

        let path = sandbox_entry_path(root, "escape/stolen").unwrap();
        assert!(sandbox_entry_parent(root, &path).is_err());
        assert!(
            write_sandbox_entry(root, "escape/stolen", ViewEntryKind::File, 0o644, b"x").is_err()
        );
        assert!(!outside.path().join("stolen").exists());

        write_sandbox_entry(
            root,
            "src/main.rs",
            ViewEntryKind::File,
            0o755,
            b"fn main() {}",
        )
        .unwrap();
        assert_eq!(
            std::fs::read(root.join("src/main.rs")).unwrap(),
            b"fn main() {}"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(root.join("src/main.rs"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn a_recorded_path_must_be_a_plain_relative_path() {
        for path in [
            "/etc/passwd",
            "/",
            "..",
            "../elsewhere/x",
            "src/../../elsewhere/x",
            "src/..",
            "./src/main.rs",
            "src//main.rs",
        ] {
            assert!(forbidden_path(path), "{path:?} should be refused");
        }
    }

    #[test]
    fn a_recorded_path_may_not_name_the_repositorys_own_machinery() {
        for path in [
            ".atomic",
            ".atomic/config.toml",
            "src/.atomic/config.toml",
            ".atomic-sandbox",
            ".atomic-sandbox.d",
            "nested/.atomic-sandbox.d/cache",
        ] {
            assert!(forbidden_path(path), "{path:?} should be refused");
        }
    }

    #[test]
    fn an_ordinary_path_is_allowed() {
        for path in [
            // A token-level op names an inode, not a path: nothing to escape.
            "",
            "README.md",
            "src/main.rs",
            "a/b/c/d.txt",
            "..hidden",
            "...hidden",
            "a..b",
            "atomic",
            "src/atomic-sandbox",
        ] {
            assert!(!forbidden_path(path), "{path:?} should be allowed");
        }
    }
}
