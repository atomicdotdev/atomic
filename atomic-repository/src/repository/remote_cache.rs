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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSkeleton {
    pub view: ViewSnapshotRows,
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
    /// The view's state after it, base32.
    pub state: String,
}

/// Paths only the repository's own machinery writes.
fn forbidden_path(path: &str) -> bool {
    path.split('/').any(|part| {
        part == crate::DOT_DIR || part == super::SANDBOX_POINTER || part == super::SANDBOX_CACHE_DIR
    })
}

/// How a remote sandbox's cache reaches its repository's owner. The
/// transport lives with whoever runs the process (the `atomic` CLI installs
/// one at startup); with it installed, `record`, `write_recorded` and the
/// readers below work in a remote sandbox for every caller.
pub trait RemoteSandboxLink: Send + Sync {
    /// `FileStates`: the rows and content for `inodes`.
    fn file_states(&self, root: &Path, inodes: Vec<u64>) -> Result<SandboxSlice, String>;
    /// `SubmitChange`: the change recorded against `base_state`; the view's
    /// skeleton after it lands.
    fn submit(
        &self,
        root: &Path,
        base_state: String,
        hash: Hash,
        bytes: Vec<u8>,
    ) -> Result<Result<(Submitted, SandboxSkeleton), SubmitRejection>, String>;
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

fn db(e: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::Database(e.to_string())
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

    /// Serve side: `view`'s skeleton, with `live` its rendered tree (inode →
    /// path, as [`Repository::materialize_view_entries`] gives them).
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
        let visible = self.sandbox_visible(&txn, &state)?;
        let set: BTreeSet<u64> = visible.iter().copied().collect();
        let rows = txn.export_skeleton(&set, live).map_err(db)?;
        Ok(SandboxSkeleton {
            view: txn.export_view_snapshot(&state, visible),
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
        let rows = txn.export_graph_slice(&on_view, state.id).map_err(db)?;
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

    /// Serve side: take a change a remote sandbox recorded on `view` and
    /// apply it there — if it is exactly what it claims, recorded against
    /// the view as it is now, and names nothing outside the view.
    ///
    /// The checks, in order: the bytes hash to `hash`; the view is still at
    /// `base_state` (otherwise the sandbox fetches and records again); every
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
            let current = state.state.to_base32();
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
        Ok(Ok(Submitted {
            hash: *hash,
            state: outcome.new_state.to_base32(),
        }))
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
    pub(crate) fn submit_recorded(
        &self,
        outcome: &crate::record::RecordOutcome,
    ) -> Result<crate::InsertOutcome, RepositoryError> {
        let bytes = outcome
            .v3_bytes()
            .ok_or_else(|| RepositoryError::Apply("the change has no bytes".to_string()))?
            .to_vec();
        let base_state = self.remote_sandbox_view_state()?;
        let (submitted, skeleton) = link()?
            .submit(&self.root, base_state, *outcome.hash(), bytes)
            .map_err(|message| RepositoryError::InvalidOperation { message })?
            .map_err(|rejection| {
                RepositoryError::Apply(format!("the repository refused the change: {rejection}"))
            })?;
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

    /// Cache side: replace this cache's tree and view with `skeleton`'s.
    pub fn import_sandbox_skeleton(
        &self,
        skeleton: &SandboxSkeleton,
    ) -> Result<(), RepositoryError> {
        let mut txn = self.pristine.write_txn().map_err(db)?;
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
