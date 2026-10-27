//! Bounded evidence from immutable change objects, never from a working copy.
//!
//! The content blob contains insertion bytes (and structural data such as names),
//! addressed by the recorded operations. It is NOT a reconstructed historical file
//! or a textual diff. Deletion operations are returned verbatim; resolving their
//! preimages through dependency graphs is deliberately unsupported here.

use std::io::{self, Write};

use atomic_core::change::{FileOps, GraphOp};
use atomic_core::kg::iri;
use atomic_core::types::{Base32, Hash};
use serde::{Deserialize, Serialize};

use crate::{Repository, RepositoryError};

pub const MAX_EVIDENCE_OPERATIONS: usize = 64;
pub const MAX_EVIDENCE_OPERATION_BYTES: usize = 65_536;
pub const MAX_EVIDENCE_CONTENT_BYTES: usize = 16_384;
/// Bounds accepted encoded objects before invoking the existing change decoder.
/// This is not a bound on decoder memory (compressed objects can expand).
pub const MAX_EVIDENCE_OBJECT_BYTES: u64 = 8 * 1024 * 1024;

/// Limits apply to the combined GraphOps/FileOps count and compact JSON bytes,
/// and separately to raw content-blob bytes. Oversized limits are rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PatchEvidenceLimits {
    pub max_operations: usize,
    pub max_operation_bytes: usize,
    pub max_content_bytes: usize,
}

impl Default for PatchEvidenceLimits {
    fn default() -> Self {
        Self {
            max_operations: 32,
            max_operation_bytes: 32_768,
            max_content_bytes: 8192,
        }
    }
}

impl PatchEvidenceLimits {
    pub fn validate(&self) -> Result<(), RepositoryError> {
        if self.max_operations > MAX_EVIDENCE_OPERATIONS
            || self.max_operation_bytes > MAX_EVIDENCE_OPERATION_BYTES
            || self.max_content_bytes > MAX_EVIDENCE_CONTENT_BYTES
        {
            return Err(RepositoryError::Serialization(
                "patch evidence limits exceed hard maxima".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchEvidenceStatus {
    Found,
    /// No locally stored change object. Does not mean that the change never existed.
    Missing,
    /// Encoded object exceeds the hard source-size limit; nothing was decoded.
    Unsupported,
}

/// Exact prefixes of the stored operation sequences, cut only at whole-operation
/// boundaries. An empty sequence with total=0 means no operations were recorded,
/// not a regenerated semantic layer. `None` means the object was not loaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedOperations<T> {
    pub operations: Vec<T>,
    pub total: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedContentBlob {
    /// Canonical hash of the entire recorded blob, verified before returning bytes.
    pub hash: String,
    /// Exact bytes at offset zero, not lossy UTF-8 or current filesystem content.
    pub bytes: Vec<u8>,
    pub total_bytes: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovedPreimageStatus {
    /// Recorded delete/replace references remain in GraphOps/FileOps. This small
    /// operator does not reconstruct removed bytes or follow dependency objects.
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchEvidence {
    pub change_iri: String,
    pub status: PatchEvidenceStatus,
    pub graph_ops: Option<RecordedOperations<GraphOp<Option<Hash>>>>,
    pub file_ops: Option<RecordedOperations<FileOps>>,
    pub content_blob: Option<RecordedContentBlob>,
    pub removed_preimage: RemovedPreimageStatus,
}

impl Repository {
    /// Retrieve exact recorded patch data using only a canonical full-hash change
    /// IRI (`urn:atomic:change:blake3-256:<52 uppercase Base32 characters>`).
    ///
    /// Hash prefixes, raw hashes, other resource kinds and noncanonical spellings
    /// are errors. Missing objects and oversized encoded objects have explicit
    /// statuses; corruption and I/O errors fail closed. No view or working-copy
    /// state is consulted, and no KG nodes or semantic claims are synthesized.
    ///
    /// GraphOps consume the shared operation budget first, then FileOps. Result
    /// limits bound returned data, not the existing decoder's memory usage.
    pub fn patch_evidence(
        &self,
        change_iri: &str,
        limits: PatchEvidenceLimits,
    ) -> Result<PatchEvidence, RepositoryError> {
        limits.validate()?;
        let hash = iri::parse_change(change_iri)
            .map_err(|error| RepositoryError::Serialization(error.to_string()))?;
        let mut result = PatchEvidence {
            change_iri: iri::mint_change(&hash),
            status: PatchEvidenceStatus::Missing,
            graph_ops: None,
            file_ops: None,
            content_blob: None,
            removed_preimage: RemovedPreimageStatus::Unsupported,
        };
        let metadata = match std::fs::metadata(self.change_store().change_path(&hash)) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(result),
            Err(error) => return Err(error.into()),
        };
        if metadata.len() > MAX_EVIDENCE_OBJECT_BYTES {
            result.status = PatchEvidenceStatus::Unsupported;
            return Ok(result);
        }
        let change = match self.load_change(&hash) {
            Ok(change) => change,
            Err(RepositoryError::ChangeNotFound { .. }) => return Ok(result),
            Err(error) => return Err(error),
        };
        if Hash::of(&change.contents) != change.hashed.contents_hash {
            return Err(RepositoryError::Serialization(
                "patch evidence content-blob hash mismatch".into(),
            ));
        }
        let mut count = limits.max_operations;
        let mut bytes = limits.max_operation_bytes;
        result.graph_ops = Some(bounded_operations(
            &change.hashed.hunks,
            &mut count,
            &mut bytes,
        )?);
        result.file_ops = Some(bounded_operations(
            change.file_ops(),
            &mut count,
            &mut bytes,
        )?);
        result.content_blob = Some(RecordedContentBlob {
            hash: change.hashed.contents_hash.to_base32(),
            bytes: change
                .contents
                .iter()
                .take(limits.max_content_bytes)
                .copied()
                .collect(),
            total_bytes: change.contents.len(),
            truncated: change.contents.len() > limits.max_content_bytes,
        });
        result.status = PatchEvidenceStatus::Found;
        Ok(result)
    }
}

// Count serialization through a bounded sink rather than allocating an unbounded
// JSON string for a single potentially enormous FileOps/GraphOp.
struct ByteBudget(usize);
impl Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.0 {
            return Err(io::Error::other("operation byte budget exhausted"));
        }
        self.0 -= bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn bounded_operations<T: Clone + Serialize>(
    source: &[T],
    remaining_count: &mut usize,
    remaining_bytes: &mut usize,
) -> Result<RecordedOperations<T>, RepositoryError> {
    let mut operations = Vec::new();
    for operation in source.iter().take(*remaining_count) {
        let mut budget = ByteBudget(*remaining_bytes);
        if let Err(error) = serde_json::to_writer(&mut budget, operation) {
            if error.is_io() {
                break;
            }
            return Err(RepositoryError::Serialization(error.to_string()));
        }
        *remaining_bytes = budget.0;
        operations.push(operation.clone());
    }
    *remaining_count -= operations.len();
    Ok(RecordedOperations {
        truncated: operations.len() < source.len(),
        operations,
        total: source.len(),
    })
}
