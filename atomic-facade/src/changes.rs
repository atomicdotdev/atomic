//! Change detail and history-log read operations.
//!
//! DTO field names and skip rules mirror `atomic change -f json` and
//! `atomic log -f json`.

use atomic_core::change::{Author, Change, GraphOp};
use atomic_core::types::{Base32, Hash};
use atomic_repository::{HistoryEntry, HistoryOptions, Repository};
use serde::{Deserialize, Serialize};

use crate::error::FacadeResult;
use crate::identifier::resolve_change;
use crate::provenance::ProvenanceDto;

/// A change author.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorDto {
    /// The author's name.
    pub name: String,
    /// The author's email (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl From<&Author> for AuthorDto {
    fn from(author: &Author) -> Self {
        Self {
            name: author.name.clone(),
            email: author.email.clone(),
        }
    }
}

/// A one-line summary of a graph op inside a change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HunkSummaryDto {
    /// Op type (FileAdd, FileDel, FileMove, Edit, …).
    pub hunk_type: String,
    /// Path affected (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// Full detail of a single change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeDetail {
    /// Full base32 hash.
    pub hash: String,
    /// The change message.
    pub message: String,
    /// The description (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The authors.
    pub authors: Vec<AuthorDto>,
    /// Timestamp (RFC 3339).
    pub timestamp: String,
    /// Dependencies as base32 hashes.
    pub dependencies: Vec<String>,
    /// Graph-op summaries.
    pub hunks: Vec<HunkSummaryDto>,
    /// Whether the change carries AI provenance.
    pub has_provenance: bool,
    /// First provenance record (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ProvenanceDto>,
    /// Unhashed payload (agent turn transcript etc.), verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unhashed: Option<serde_json::Value>,
    /// Sequence number on the resolved view (if the change is on its log).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
}

impl ChangeDetail {
    /// Build from a loaded change.
    pub fn from_change(change: &Change, hash: &Hash, sequence: Option<u64>) -> Self {
        Self {
            hash: hash.to_base32(),
            message: change.hashed.header.message.clone(),
            description: change.hashed.header.description.clone(),
            authors: change
                .hashed
                .header
                .authors
                .iter()
                .map(AuthorDto::from)
                .collect(),
            timestamp: change.hashed.header.timestamp.to_rfc3339(),
            dependencies: change
                .hashed
                .dependencies
                .iter()
                .map(Hash::to_base32)
                .collect(),
            hunks: change.hashed.hunks.iter().map(hunk_summary).collect(),
            has_provenance: change.has_provenance(),
            provenance: change.hashed.provenance.first().map(ProvenanceDto::from),
            unhashed: change.unhashed.clone(),
            sequence,
        }
    }
}

/// One entry of a view's history log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// Sequence number in the view.
    pub sequence: u64,
    /// The change hash (base32).
    pub hash: String,
    /// The Merkle state after this change (base32).
    pub state: String,
    /// The change message (if the header was loaded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The change description (if the header was loaded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The authors (if the header was loaded).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub authors: Vec<AuthorDto>,
    /// Timestamp (RFC 3339, if the header was loaded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Whether this change is tagged.
    pub is_tagged: bool,
}

impl From<&HistoryEntry> for LogEntry {
    fn from(entry: &HistoryEntry) -> Self {
        Self {
            sequence: entry.sequence,
            hash: entry.hash.to_base32(),
            state: entry.state.to_base32(),
            message: entry.message().map(String::from),
            description: entry.description().map(String::from),
            authors: entry
                .authors()
                .map(|authors| authors.iter().map(AuthorDto::from).collect())
                .unwrap_or_default(),
            timestamp: entry.timestamp().map(|t| t.to_rfc3339()),
            is_tagged: entry.is_tagged,
        }
    }
}

/// Options for [`list_log`].
#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    /// View to read (default: the repository's current view).
    pub view: Option<String>,
    /// Maximum number of entries (default: unlimited).
    pub limit: Option<usize>,
    /// Include changes inherited from ancestor views (draft views only).
    pub include_inherited: bool,
}

/// Resolve an identifier and return the change's full detail.
///
/// `spec` accepts a hash, hash prefix, `#seq`, bare sequence, `@`, or `None`
/// for the latest change on the view.
pub fn change_detail(
    repo: &Repository,
    view: Option<&str>,
    spec: Option<&str>,
) -> FacadeResult<ChangeDetail> {
    let (hash, sequence) = resolve_change(repo, view, spec)?;
    let change = repo.load_change(&hash)?;
    Ok(ChangeDetail::from_change(&change, &hash, sequence))
}

/// The view's history, newest first, with headers loaded.
pub fn list_log(repo: &Repository, query: &LogQuery) -> FacadeResult<Vec<LogEntry>> {
    let mut options = HistoryOptions::default()
        .load_headers(true)
        .include_inherited(query.include_inherited);
    if let Some(view) = &query.view {
        options = options.view(view);
    }
    if let Some(limit) = query.limit {
        options = options.limit(limit);
    }

    let entries = repo.reverse_log(options)?;
    Ok(entries.iter().map(LogEntry::from).collect())
}

fn hunk_summary<H>(op: &GraphOp<H>) -> HunkSummaryDto {
    let (hunk_type, path) = match op {
        GraphOp::FileAdd { path, .. } => ("FileAdd", Some(path.clone())),
        GraphOp::FileDel { path, .. } => ("FileDel", Some(path.clone())),
        GraphOp::FileMove { path, .. } => ("FileMove", Some(path.clone())),
        GraphOp::FileUndel { path, .. } => ("FileUndel", Some(path.clone())),
        GraphOp::DirAdd { path, .. } => ("DirAdd", Some(path.clone())),
        GraphOp::DirDel { path, .. } => ("DirDel", Some(path.clone())),
        GraphOp::DirUndel { path, .. } => ("DirUndel", Some(path.clone())),
        GraphOp::Edit { local, .. } => ("Edit", Some(local.path.clone())),
        GraphOp::Replacement { local, .. } => ("Replacement", Some(local.path.clone())),
        GraphOp::SolveNameConflict { path, .. } => ("SolveNameConflict", Some(path.clone())),
        GraphOp::UnsolveNameConflict { path, .. } => ("UnsolveNameConflict", Some(path.clone())),
        GraphOp::SolveOrderConflict { local, .. } => ("SolveOrderConflict", Some(local.path.clone())),
        GraphOp::UnsolveOrderConflict { local, .. } => {
            ("UnsolveOrderConflict", Some(local.path.clone()))
        }
        GraphOp::ResurrectZombies { local, .. } => ("ResurrectZombies", Some(local.path.clone())),
        GraphOp::AddRoot { .. } => ("AddRoot", None),
        GraphOp::DelRoot { .. } => ("DelRoot", None),
    };
    HunkSummaryDto {
        hunk_type: hunk_type.to_string(),
        path,
    }
}
