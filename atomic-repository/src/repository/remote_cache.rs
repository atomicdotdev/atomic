//! A remote sandbox's local cache: the repository's own rows for one view,
//! enough for `status` and `record` without the repository.
//!
//! The serving repository exports ([`Repository::export_sandbox_skeleton`],
//! [`Repository::export_sandbox_slice`]); the sandbox's cache imports
//! ([`Repository::import_sandbox_skeleton`],
//! [`Repository::import_sandbox_slice`]). See `atomic_core::pristine::slice`
//! for what the rows are and why they are copied rather than re-derived.

use std::collections::BTreeSet;

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

    /// Serve side: `view`'s skeleton, for the inodes `live` (the ones its
    /// materialized tree holds).
    pub fn export_sandbox_skeleton(
        &self,
        view: &str,
        live: &BTreeSet<u64>,
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
