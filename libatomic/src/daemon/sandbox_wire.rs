//! The remote-sandbox kernel types on the wire, both ways.
//!
//! The handlers encode with these; a host's cache-side
//! [`atomic_repository::RemoteSandboxLink`] decodes with the same functions,
//! so the two sides cannot drift. Everything a cache imports travels as the
//! kernel's own encoding inside schema-tagged [`pb::VersionedBytes`]; the
//! typed proto fields beside it are a readable projection.

use std::collections::{BTreeMap, HashMap};

use atomic_core::change::session::{SessionCheckpointPublication, SessionTurn};
use atomic_core::types::{Base32, Hash};
use atomic_repository::{
    SandboxSkeleton, SandboxSlice, SpanBytes, SubmitRejection, ViewEntry, ViewEntryKind,
    SANDBOX_GRAPH_SLICE_SCHEMA, SANDBOX_SKELETON_SCHEMA,
};

use super::convert::hash_proto;
use super::sandbox_grants::view_ref_id;
use crate::atomic as pb;
use crate::atomic::ErrorCode;

/// V3 change bytes in a [`pb::ChangeBundle`].
pub const CHANGE_SCHEMA: &str = "atomic.change.v3";
/// A serialized provenance graph (`ProvenanceGraph::serialize`).
pub const PROVENANCE_GRAPH_SCHEMA: &str = "atomic.prov.graph.v1";
/// A [`SessionTurn`] as JSON.
pub const SESSION_TURN_SCHEMA: &str = "atomic.session.turn.v1";
/// A [`SessionCheckpointPublication`] as JSON.
pub const PUBLICATION_SCHEMA: &str = "atomic.session.checkpoint-publication.v1";

/// `ErrorInfo.context` key naming a [`SubmitRejection`] variant.
pub const REJECTION_REASON: &str = "reason";

pub fn hash_from_proto(hash: Option<&pb::Hash>) -> Result<Hash, String> {
    let hash = hash.ok_or("hash is required")?;
    if hash.algorithm != pb::HashAlgorithm::Blake3 as i32 && hash.algorithm != 0 {
        return Err("only blake3 hashes are accepted".to_string());
    }
    let bytes: [u8; 32] = hash
        .value
        .as_slice()
        .try_into()
        .map_err(|_| "a hash is exactly 32 bytes".to_string())?;
    Ok(Hash::from_bytes(bytes))
}

fn versioned(schema: &str, payload: Vec<u8>) -> pb::VersionedBytes {
    pb::VersionedBytes {
        schema: schema.to_string(),
        payload,
    }
}

fn payload<'a>(bytes: Option<&'a pb::VersionedBytes>, schema: &str) -> Result<&'a [u8], String> {
    let bytes = bytes.ok_or_else(|| format!("missing {schema} payload"))?;
    if bytes.schema != schema {
        return Err(format!("expected {schema}, got '{}'", bytes.schema));
    }
    Ok(&bytes.payload)
}

/// A view as the handlers name it.
pub fn view_ref(name: &str) -> pb::ViewRef {
    pb::ViewRef {
        view_id: view_ref_id(name),
        name: Some(name.to_string()),
    }
}

/// `view`'s snapshot: its effective state (the fence) and its own Merkle
/// state (diagnostic).
pub fn view_snapshot(view: &str, effective: &Hash, own: Option<&Hash>) -> pb::ViewSnapshot {
    pb::ViewSnapshot {
        view: Some(view_ref(view)),
        effective_state: Some(hash_proto(effective)),
        own_merkle: own.map(hash_proto),
    }
}

/// The effective state a snapshot fences on, base32 (what
/// `Repository::insert_submitted_change` compares).
pub fn snapshot_fence(snapshot: &pb::ViewSnapshot) -> Result<String, String> {
    Ok(hash_from_proto(snapshot.effective_state.as_ref())?.to_base32())
}

/// A rendered entry as a [`pb::TreeEntry`]: the content rides inline.
pub fn tree_entry_proto(entry: ViewEntry) -> pb::TreeEntry {
    let kind = match entry.kind {
        ViewEntryKind::File => pb::TreeEntryKind::File,
        ViewEntryKind::Directory => pb::TreeEntryKind::Directory,
        ViewEntryKind::Symlink => pb::TreeEntryKind::Symlink,
    };
    pb::TreeEntry {
        path: entry.path,
        kind: kind as i32,
        content: (entry.kind != ViewEntryKind::Directory).then(|| hash_proto(&entry.hash)),
        inline: (entry.kind != ViewEntryKind::Directory).then_some(entry.content),
        mode: Some(u32::from(entry.mode)),
    }
}

pub fn tree_entry_kind(entry: &pb::TreeEntry) -> Result<ViewEntryKind, String> {
    match pb::TreeEntryKind::try_from(entry.kind) {
        Ok(pb::TreeEntryKind::File) => Ok(ViewEntryKind::File),
        Ok(pb::TreeEntryKind::Directory) => Ok(ViewEntryKind::Directory),
        Ok(pb::TreeEntryKind::Symlink) => Ok(ViewEntryKind::Symlink),
        _ => Err(format!("entry {:?} has no kind", entry.path)),
    }
}

/// A kernel skeleton on the wire, with `snapshot` the view's and `live` its
/// rendered tree (inode → path) when the caller has it.
pub fn skeleton_proto(
    skeleton: &SandboxSkeleton,
    snapshot: pb::ViewSnapshot,
    live: Option<&BTreeMap<u64, String>>,
) -> Result<pb::SandboxSkeleton, String> {
    let names: HashMap<u64, &str> = std::iter::once(&skeleton.view)
        .chain(&skeleton.ancestors)
        .map(|v| (v.id, v.name.as_str()))
        .collect();
    let rows = match live {
        Some(live) => live
            .iter()
            .map(|(inode, path)| pb::SandboxViewRow {
                path: path.clone(),
                inode: *inode,
                content: None,
            })
            .collect(),
        None => skeleton
            .rows
            .tree
            .iter()
            .map(|(path, inode)| pb::SandboxViewRow {
                path: path.clone(),
                inode: *inode,
                content: None,
            })
            .collect(),
    };
    Ok(pb::SandboxSkeleton {
        rows,
        ancestors: skeleton
            .ancestors
            .iter()
            .map(|a| pb::SandboxAncestorView {
                view: a.name.clone(),
                head: None,
                real_state: Some(hash_proto(&a.state)),
                parent: a.parent.and_then(|p| names.get(&p)).map(|n| n.to_string()),
                snapshot: None,
                log: Vec::new(),
            })
            .collect(),
        graph_slice: Some(versioned(
            SANDBOX_SKELETON_SCHEMA,
            skeleton.to_bytes().map_err(|e| e.to_string())?,
        )),
        snapshot: Some(snapshot),
        own_log: Vec::new(),
        vault: None,
    })
}

/// What a cache imports from a [`pb::SandboxSkeleton`].
pub fn skeleton_from_proto(skeleton: &pb::SandboxSkeleton) -> Result<SandboxSkeleton, String> {
    SandboxSkeleton::from_bytes(payload(
        skeleton.graph_slice.as_ref(),
        SANDBOX_SKELETON_SCHEMA,
    )?)
    .map_err(|e| e.to_string())
}

pub fn slice_proto(
    slice: &SandboxSlice,
    snapshot: pb::ViewSnapshot,
) -> Result<pb::SandboxSlice, String> {
    Ok(pb::SandboxSlice {
        rows: slice
            .rows
            .rev_tree
            .iter()
            .map(|(inode, path)| pb::SandboxViewRow {
                path: path.clone(),
                inode: *inode,
                content: None,
            })
            .collect(),
        content_spans: slice
            .spans
            .iter()
            .map(|span| pb::SandboxContentSpan {
                inode: 0,
                offset: span.start,
                length: span.bytes.len() as u64,
                change: Some(hash_proto(&span.change)),
                content: span.bytes.clone(),
            })
            .collect(),
        snapshot: Some(snapshot),
        graph_slice: Some(versioned(
            SANDBOX_GRAPH_SLICE_SCHEMA,
            slice.rows_to_bytes().map_err(|e| e.to_string())?,
        )),
    })
}

pub fn slice_from_proto(slice: &pb::SandboxSlice) -> Result<SandboxSlice, String> {
    let spans = slice
        .content_spans
        .iter()
        .map(|span| {
            if span.length != span.content.len() as u64 {
                return Err(format!(
                    "a span says {} bytes and carries {}",
                    span.length,
                    span.content.len()
                ));
            }
            Ok(SpanBytes {
                change: hash_from_proto(span.change.as_ref())?,
                start: span.offset,
                bytes: span.content.clone(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    SandboxSlice::from_parts(
        payload(slice.graph_slice.as_ref(), SANDBOX_GRAPH_SLICE_SCHEMA)?,
        spans,
    )
    .map_err(|e| e.to_string())
}

/// A refusal as the contract's [`pb::ErrorInfo`]: `VIEW_STALE` for a stale
/// view, `CHANGE_REJECTED` otherwise, with the variant in
/// [`REJECTION_REASON`] and its fields beside it so the other side can
/// rebuild it ([`rejection_from_error_info`]).
pub fn rejection_error_info(rejection: &SubmitRejection) -> pb::ErrorInfo {
    let mut context = HashMap::new();
    let mut put = |k: &str, v: &str| {
        context.insert(k.to_string(), v.to_string());
    };
    let (code, reason) = match rejection {
        SubmitRejection::HashMismatch { claimed, computed } => {
            put("claimed", claimed);
            put("computed", computed);
            (ErrorCode::ChangeRejected, "hash_mismatch")
        }
        SubmitRejection::Malformed(why) => {
            put("detail", why);
            (ErrorCode::ChangeRejected, "malformed")
        }
        SubmitRejection::StaleView { current } => {
            put("current", current);
            (ErrorCode::ViewStale, "stale_view")
        }
        SubmitRejection::ForeignChange(change) => {
            put("change", change);
            (ErrorCode::ChangeRejected, "foreign_change")
        }
        SubmitRejection::ForeignNode(node) => {
            put("node", &node.to_string());
            (ErrorCode::ChangeRejected, "foreign_node")
        }
        SubmitRejection::ForbiddenPath(path) => {
            put("path", path);
            (ErrorCode::ChangeRejected, "forbidden_path")
        }
        SubmitRejection::AlreadyPresent(change) => {
            put("change", change);
            (ErrorCode::ChangeRejected, "already_present")
        }
    };
    put(REJECTION_REASON, reason);
    pb::ErrorInfo {
        code: code as i32,
        message: rejection.to_string(),
        context,
        request_id: None,
    }
}

/// The [`SubmitRejection`] an [`pb::ErrorInfo`] from
/// [`rejection_error_info`] describes, if it is one.
pub fn rejection_from_error_info(info: &pb::ErrorInfo) -> Option<SubmitRejection> {
    let get = |k: &str| info.context.get(k).cloned();
    Some(match info.context.get(REJECTION_REASON)?.as_str() {
        "hash_mismatch" => SubmitRejection::HashMismatch {
            claimed: get("claimed")?,
            computed: get("computed")?,
        },
        "malformed" => SubmitRejection::Malformed(get("detail")?),
        "stale_view" => SubmitRejection::StaleView {
            current: get("current")?,
        },
        "foreign_change" => SubmitRejection::ForeignChange(get("change")?),
        "foreign_node" => SubmitRejection::ForeignNode(get("node")?.parse().ok()?),
        "forbidden_path" => SubmitRejection::ForbiddenPath(get("path")?),
        "already_present" => SubmitRejection::AlreadyPresent(get("change")?),
        _ => return None,
    })
}

pub fn change_bundle(hash: &Hash, bytes: Vec<u8>) -> pb::ChangeBundle {
    pb::ChangeBundle {
        hash: Some(hash_proto(hash)),
        schema: CHANGE_SCHEMA.to_string(),
        payload: bytes,
    }
}

/// A bundle's hash and V3 bytes. An empty schema reads as V3.
pub fn change_from_bundle(bundle: &pb::ChangeBundle) -> Result<(Hash, Vec<u8>), String> {
    if !bundle.schema.is_empty() && bundle.schema != CHANGE_SCHEMA {
        return Err(format!(
            "expected {CHANGE_SCHEMA} change bytes, got '{}'",
            bundle.schema
        ));
    }
    Ok((
        hash_from_proto(bundle.hash.as_ref())?,
        bundle.payload.clone(),
    ))
}

pub fn provenance_graph_bytes(graph: Vec<u8>) -> pb::VersionedBytes {
    versioned(PROVENANCE_GRAPH_SCHEMA, graph)
}

pub fn provenance_graph_from(bytes: Option<&pb::VersionedBytes>) -> Result<Vec<u8>, String> {
    Ok(payload(bytes, PROVENANCE_GRAPH_SCHEMA)?.to_vec())
}

pub fn session_turn_bytes(turn: &SessionTurn) -> Result<pb::VersionedBytes, String> {
    Ok(versioned(
        SESSION_TURN_SCHEMA,
        serde_json::to_vec(turn).map_err(|e| e.to_string())?,
    ))
}

pub fn session_turn_from(bytes: Option<&pb::VersionedBytes>) -> Result<SessionTurn, String> {
    serde_json::from_slice(payload(bytes, SESSION_TURN_SCHEMA)?).map_err(|e| e.to_string())
}

pub fn publication_proto(
    publication: &SessionCheckpointPublication,
) -> Result<pb::ProvenancePublished, String> {
    Ok(pb::ProvenancePublished {
        provenance_hash: Some(hash_proto(&publication.turn.provenance_hash)),
        published_at: Some(prost_types::Timestamp {
            seconds: publication.turn.timestamp,
            nanos: 0,
        }),
        manifest_hash: Some(hash_proto(&publication.manifest_hash)),
        publication: Some(versioned(
            PUBLICATION_SCHEMA,
            serde_json::to_vec(publication).map_err(|e| e.to_string())?,
        )),
    })
}

pub fn publication_from(
    published: &pb::ProvenancePublished,
) -> Result<SessionCheckpointPublication, String> {
    serde_json::from_slice(payload(published.publication.as_ref(), PUBLICATION_SCHEMA)?)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rejection_survives_the_wire() {
        for rejection in [
            SubmitRejection::HashMismatch {
                claimed: "a".into(),
                computed: "b".into(),
            },
            SubmitRejection::Malformed("bad".into()),
            SubmitRejection::StaleView {
                current: "c".into(),
            },
            SubmitRejection::ForeignChange("d".into()),
            SubmitRejection::ForeignNode(42),
            SubmitRejection::ForbiddenPath("../x".into()),
            SubmitRejection::AlreadyPresent("e".into()),
        ] {
            let info = rejection_error_info(&rejection);
            let stale = matches!(rejection, SubmitRejection::StaleView { .. });
            assert_eq!(
                info.code == ErrorCode::ViewStale as i32,
                stale,
                "{rejection:?}"
            );
            assert_eq!(rejection_from_error_info(&info), Some(rejection));
        }
    }
}
