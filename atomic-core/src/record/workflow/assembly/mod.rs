//! Change assembly from recorded files.
//!
//! Combines multiple [`RecordedFile`] results into a complete [`Change`]
//! that can be serialized and applied to other repositories.
//!
//! # Overview
//!
//! 1. **Globalization** — convert local hunks to graph-compatible hunks
//! 2. **Content Aggregation** — combine all content into a single blob
//! 3. **Offset Computation** — calculate byte offsets for each graph_op
//! 4. **Dependency Collection** — gather all change dependencies
//! 5. **Finalization** — create the complete Change structure
//!
//! # Module Structure
//!
//! - [`types`]: Error types, `AssemblyOptions`, `AssemblyResult_`
//! - [`helpers`]: `AssemblyStats`, utility functions
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::record::workflow::assembly::{
//!     AssemblyContext, AssemblyOptions, assemble_change,
//! };
//! use atomic_core::change::ChangeHeader;
//!
//! let header = ChangeHeader::builder()
//!     .message("Add new feature")
//!     .author(Author::new("Alice", Some("alice@example.com")))
//!     .build();
//!
//! let change = assemble_change(
//!     &txn, &recorded_files, header, &AssemblyOptions::default(),
//! )?;
//! ```
//!
//! See [`AssemblyError`] for the complete error list.

pub mod helpers;
pub mod types;

#[cfg(test)]
mod tests;

// Re-export all public items so external code sees the same API.
pub use helpers::{collect_dependencies, compute_content_offsets, finalize_hunks, AssemblyStats};
pub use types::{AssemblyError, AssemblyOptions, AssemblyResult, AssemblyResult_};

use std::collections::HashSet;
use std::time::Instant;

use crate::change::{Change, ChangeHeader, FileOps, GraphOp, Provenance};
use crate::crdt::{BranchId, BranchOp, LeafOp};
use crate::pristine::{CrdtTxnT, GraphTxnT, TreeTxnT};
use crate::types::{ChangePosition, Hash, Position};

use super::globalize::{globalize_recorded_file, globalize_set_attr, GlobalizeContext};
use super::record::RecordedFile;

/// Shift the content offsets of a pre-globalized op's inserted span(s) by
/// `shift`, mapping the file-local blob offsets to the change-global blob.
///
/// Only the Insertion `start`/`end` (the new content this change introduces)
/// are shifted; predecessor/successor positions reference existing graph
/// vertices in other changes and must not move.
fn shift_graph_op_content(op: &mut GraphOp<Option<Hash>>, shift: u64) {
    use crate::change::{Atom, Insertion};
    use crate::types::ChangePosition;

    fn shift_insertion(ins: &mut Insertion<Option<Hash>>, shift: u64) {
        ins.start = ChangePosition::new(ins.start.get() + shift);
        ins.end = ChangePosition::new(ins.end.get() + shift);
    }

    match op {
        GraphOp::Edit {
            change: Atom::Insertion(ins),
            ..
        } => shift_insertion(ins, shift),
        GraphOp::Replacement { replacement, .. } => shift_insertion(replacement, shift),
        _ => {}
    }
}

// ============================================================================
// ASSEMBLY CONTEXT
// ============================================================================

/// Context for change assembly.
///
/// Accumulates hunks, content, and dependencies during the assembly process.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::record::workflow::assembly::AssemblyContext;
/// use atomic_core::change::ChangeHeader;
///
/// let mut ctx = AssemblyContext::new(header);
///
/// // Add hunks from globalized files
/// for file in globalized_files {
///     for graph_op in file.hunks() {
///         ctx.add_hunk(graph_op.clone());
///     }
/// }
///
/// // Finalize the change
/// let change = ctx.finalize(content)?;
/// ```
pub struct AssemblyContext {
    /// The change header.
    header: ChangeHeader,

    /// Accumulated hunks (graph operations).
    hunks: Vec<GraphOp<Option<Hash>>>,

    /// Accumulated file operations (semantic layer).
    file_ops: Vec<FileOps>,

    /// Accumulated dependencies.
    dependencies: HashSet<Hash>,

    /// Extra known changes (not dependencies but referenced).
    extra_known: HashSet<Hash>,

    /// Statistics about the assembly.
    stats: AssemblyStats,

    /// Running base for the next FileOps entry's placeholder branch ids
    /// (CB-9B review F4): placeholder branch indices must stay unique
    /// across the change so the apply-time substitution cannot collide.
    placeholder_branch_base: u32,

    /// Running base for the next FileOps entry's placeholder leaf ids.
    placeholder_leaf_base: u32,
}

impl AssemblyContext {
    /// Create a new assembly context.
    ///
    /// # Arguments
    ///
    /// * `header` - The change header
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let header = ChangeHeader::builder().message("Test").build();
    /// let ctx = AssemblyContext::new(header);
    /// ```
    pub fn new(header: ChangeHeader) -> Self {
        Self {
            header,
            hunks: Vec::new(),
            file_ops: Vec::new(),
            dependencies: HashSet::new(),
            extra_known: HashSet::new(),
            stats: AssemblyStats::new(),
            placeholder_branch_base: 0,
            placeholder_leaf_base: 0,
        }
    }

    /// Create a context with pre-allocated capacity.
    ///
    /// # Arguments
    ///
    /// * `header` - The change header
    /// * `hunk_capacity` - Expected number of hunks
    pub fn with_capacity(header: ChangeHeader, hunk_capacity: usize) -> Self {
        Self {
            header,
            hunks: Vec::with_capacity(hunk_capacity),
            file_ops: Vec::new(),
            dependencies: HashSet::new(),
            extra_known: HashSet::new(),
            stats: AssemblyStats::new(),
            placeholder_branch_base: 0,
            placeholder_leaf_base: 0,
        }
    }

    /// Add a graph_op to the context.
    ///
    /// # Arguments
    ///
    /// * `graph_op` - The graph_op to add
    pub fn add_hunk(&mut self, graph_op: GraphOp<Option<Hash>>) {
        self.hunks.push(graph_op);
        self.stats.hunks_added += 1;
    }

    /// Add a file operation to the context (semantic layer).
    ///
    /// The entry's ROOT-placeholder identities are renumbered into a
    /// per-change unique namespace (CB-9B review F4): the trunk's file index
    /// becomes this entry's position, and branch/leaf placeholders shift by
    /// the running bases so two recorded files can never collide in the
    /// CRDT tables at apply time.
    ///
    /// # Errors
    ///
    /// Returns [`AssemblyError::PlaceholderNamespaceExhausted`] when the
    /// running placeholder namespace cannot advance: either the FileOps
    /// entry count or the accumulated placeholder span would exceed `u32`.
    pub fn add_file_ops(&mut self, ops: FileOps) -> AssemblyResult<()> {
        let mut ops = ops;
        // CB-9B review B1: the entry's local placeholder span must be
        // measured BEFORE renumbering. After `renumber_placeholder_ids` the
        // ops carry the shifted indices, so measuring them afterwards
        // double-counts the running base and grows it geometrically
        // (2·base + span) instead of linearly — a 34-entry one-line import
        // overflowed u32 and panicked.
        let branch_span = checked_placeholder_span(max_placeholder_branch_index(&ops))?;
        let leaf_span = checked_placeholder_span(max_placeholder_leaf_index(&ops))?;
        let trunk_file_idx = u32::try_from(self.file_ops.len()).map_err(|_| {
            AssemblyError::PlaceholderNamespaceExhausted {
                namespace: "trunk-file",
                next_index: self.file_ops.len() as u64,
                limit: u32::MAX as u64,
            }
        })?;
        let branch_base = self.placeholder_branch_base;
        let leaf_base = self.placeholder_leaf_base;
        // Checked before the shifts so every individual `base + local`
        // substitution inside renumber_placeholder_ids stays in range: each
        // local index is < span, so base + span is the upper bound.
        let next_branch_base = branch_base
            .checked_add(branch_span)
            .ok_or(AssemblyError::PlaceholderNamespaceExhausted {
                namespace: "branch",
                next_index: branch_base as u64 + branch_span as u64,
                limit: u32::MAX as u64,
            })?;
        let next_leaf_base =
            leaf_base
                .checked_add(leaf_span)
                .ok_or(AssemblyError::PlaceholderNamespaceExhausted {
                    namespace: "leaf",
                    next_index: leaf_base as u64 + leaf_span as u64,
                    limit: u32::MAX as u64,
                })?;
        ops.renumber_placeholder_ids(trunk_file_idx, branch_base, leaf_base);
        self.placeholder_branch_base = next_branch_base;
        self.placeholder_leaf_base = next_leaf_base;
        self.file_ops.push(ops);
        Ok(())
    }

    /// Add a dependency.
    ///
    /// # Arguments
    ///
    /// * `hash` - The dependency hash
    pub fn add_dependency(&mut self, hash: Hash) {
        if self.dependencies.insert(hash) {
            self.stats.dependencies_added += 1;
        }
    }

    /// Add multiple dependencies.
    ///
    /// # Arguments
    ///
    /// * `hashes` - Iterator of dependency hashes
    pub fn add_dependencies(&mut self, hashes: impl IntoIterator<Item = Hash>) {
        for hash in hashes {
            self.add_dependency(hash);
        }
    }

    /// Add an extra known change.
    ///
    /// Extra known changes are referenced but not direct dependencies.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash to add
    pub fn add_extra_known(&mut self, hash: Hash) {
        self.extra_known.insert(hash);
    }

    /// Get the number of hunks.
    #[must_use]
    pub fn hunk_count(&self) -> usize {
        self.hunks.len()
    }

    /// Get the number of dependencies.
    #[must_use]
    pub fn dependency_count(&self) -> usize {
        self.dependencies.len()
    }

    /// Get the assembly statistics.
    #[must_use]
    pub fn stats(&self) -> &AssemblyStats {
        &self.stats
    }

    /// Finalize the assembly and create the Change.
    ///
    /// # Arguments
    ///
    /// * `content` - The content blob
    /// * `provenance` - AI provenance information (empty if not AI-assisted)
    /// * `metadata_bytes` - Opaque metadata bytes for HashedChange.metadata
    ///
    /// # Returns
    ///
    /// The assembled Change.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let change = ctx.finalize(content_bytes, vec![], vec![])?;
    /// ```
    #[must_use]
    pub fn finalize(
        self,
        content: Vec<u8>,
        provenance: Vec<Provenance>,
        metadata_bytes: Vec<u8>,
    ) -> Change {
        let mut dependencies: Vec<Hash> = self.dependencies.into_iter().collect();
        dependencies.sort();

        let mut extra_known: Vec<Hash> = self.extra_known.into_iter().collect();
        extra_known.sort();

        let mut change = Change::with_file_ops(
            self.header,
            self.hunks,
            self.file_ops,
            content,
            dependencies,
        );
        change.hashed.extra_known = extra_known;

        // Set opaque metadata bytes (e.g., SessionEnvelope from atomic-agent)
        if !metadata_bytes.is_empty() {
            change.hashed.metadata = metadata_bytes;
        }

        // Add AI provenance information
        for entry in provenance {
            change.add_provenance(entry);
        }

        change
    }

    /// Get the number of file operations collected.
    #[must_use]
    pub fn file_ops_count(&self) -> usize {
        self.file_ops.len()
    }

    /// Get a reference to the header.
    #[must_use]
    pub fn header(&self) -> &ChangeHeader {
        &self.header
    }

    /// Get a reference to the hunks.
    #[must_use]
    pub fn hunks(&self) -> &[GraphOp<Option<Hash>>] {
        &self.hunks
    }
}

// ============================================================================
// MAIN ASSEMBLY FUNCTIONS
// ============================================================================

/// Emit the graph-backed inode attribute writes one recorded file carries.
///
/// Two target shapes are supported:
/// - **Existing inode**: the record carries the inode's graph position, so
///   each attribute write globalizes against that position and adds the
///   owning change as a dependency.
/// - **Created in this change**: the file's own `FileAdd` hunk names the new
///   inode placeholder, so the attribute op self-references the applying
///   change (`change: None`) at the created inode's start offset.
///
/// Each write also produces the semantic counterpart (`FileOps::set_mode` /
/// `set_kind`) bound to the file's trunk: the change's own generated trunk
/// when the record carries CRDT ops, otherwise the trunk already stored for
/// the inode.
fn emit_recorded_attributes<T>(
    ctx: &mut AssemblyContext,
    glob_ctx: &mut GlobalizeContext<'_, T>,
    file: &RecordedFile,
) -> AssemblyResult<()>
where
    T: GraphTxnT + TreeTxnT + CrdtTxnT + crate::pristine::InodeAttrTxnT,
{
    if file.attrs().is_empty() {
        return Ok(());
    }
    let path = file.path().to_string();

    let invalid = |reason: String| AssemblyError::Globalize {
        path: path.clone(),
        source: super::globalize::GlobalizeError::InvalidAttribute {
            path: path.clone(),
            reason,
        },
    };

    // Resolve the semantic trunk once: the file's generated ops carry it for
    // new files; existing files resolve through the CRDT inode index.
    let trunk = match file.crdt_ops() {
        Some(ops) => ops.trunk_id(),
        None => {
            let inode = file
                .inode()
                .ok_or_else(|| invalid("attribute write has no trunk or inode binding".into()))?;
            let key = glob_ctx
                .txn()
                .get_crdt_inode_trunk(inode.get())
                .map_err(|error| AssemblyError::Globalize {
                    path: path.clone(),
                    source: super::globalize::GlobalizeError::Pristine(Box::new(error)),
                })?
                .ok_or_else(|| invalid(format!("inode {inode:?} has no CRDT trunk")))?;
            crate::crdt::tables::decode_trunk_id(&key)
        }
    };

    // Resolve the attribute target: the record's bound position wins; a
    // `FileAdd` hunk for this path means the inode is created by this change.
    enum AttrTarget {
        /// Existing inode: globalize against the bound position.
        Existing(crate::types::Position<crate::types::NodeId>),
        /// Created by this change: self-referencing placeholder position.
        Created(u64),
    }
    let target = if let Some(position) = file.position() {
        AttrTarget::Existing(position)
    } else {
        let created = ctx.hunks().iter().rev().find_map(|operation| match operation {
            GraphOp::FileAdd {
                add_inode,
                path: added,
                ..
            } if added == file.path() => Some(add_inode.start.get()),
            _ => None,
        });
        created.map(AttrTarget::Created).ok_or_else(|| {
            invalid("attribute target has no inode binding and no FileAdd in this change".into())
        })?
    };

    for value in file.attrs() {
        let hunk = match &target {
            AttrTarget::Existing(position) => {
                // Wire causal dependencies on the register's current event
                // writers (the same rule the native record path applies):
                // without them a later attribute write would appear
                // concurrent to the previous writer instead of dominating
                // it, projecting a spurious register conflict.
                //
                // Review CB-9C R1: the events come from the exact assembly
                // view (a ViewGraph filters register events to its own
                // closure), and only the causally maximal writers become
                // dependencies — a dominated writer is already a transitive
                // dependency of its dominator, and invisible sibling writers
                // must never be imported as causality.
                let existing = glob_ctx
                    .txn()
                    .get_inode_attr_events(*position, value.name())
                    .map_err(|error| AssemblyError::Globalize {
                        path: path.clone(),
                        source: super::globalize::GlobalizeError::Pristine(Box::new(error)),
                    })?;
                let frontier = crate::pristine::attr_event_dependency_frontier(
                    glob_ctx.txn(),
                    existing,
                )
                .map_err(|error| AssemblyError::Globalize {
                    path: path.clone(),
                    source: super::globalize::GlobalizeError::Pristine(Box::new(error)),
                })?;
                for event in frontier {
                    glob_ctx
                        .add_dependency_by_id(event.introduced_by)
                        .map_err(|error| AssemblyError::Globalize {
                            path: path.clone(),
                            source: error,
                        })?;
                }
                globalize_set_attr(glob_ctx, *position, path.clone(), *value)
                    .map_err(|error| AssemblyError::Globalize {
                        path: path.clone(),
                        source: error,
                    })?
            }
            AttrTarget::Created(offset) => GraphOp::SetAttr {
                inode: Position {
                    change: None,
                    pos: ChangePosition::new(*offset),
                },
                path: path.clone(),
                value: *value,
            },
        };
        ctx.add_hunk(hunk);

        let semantic = match value {
            crate::change::InodeAttr::Mode(mode) => FileOps::set_mode(trunk, path.clone(), *mode)
                .map_err(|error| AssemblyError::Globalize {
                    path: path.clone(),
                    source: super::globalize::GlobalizeError::InvalidAttribute {
                        path: path.clone(),
                        reason: error.to_string(),
                    },
                })?,
            crate::change::InodeAttr::Kind(kind) => {
                FileOps::set_kind(trunk, path.clone(), *kind)
            }
        };
        ctx.add_file_ops(semantic)?;
    }
    Ok(())
}

/// Assemble a change from recorded files.
///
/// This is the main entry point for creating a Change from recording results.
/// It globalizes all hunks, collects dependencies, and creates the final
/// Change structure.
///
/// # Arguments
///
/// * `txn` - Transaction for graph lookups
/// * `files` - The recorded files to assemble
/// * `header` - The change header
/// * `options` - Assembly options
///
/// # Returns
///
/// The assembled change, or an error if assembly fails.
///
/// # Example
///
/// ```rust,ignore
/// let header = ChangeHeader::builder()
///     .message("Add feature")
///     .author(Author::new("Alice", None))
///     .build();
///
/// let change = assemble_change(&txn, &recorded_files, header, &AssemblyOptions::default())?;
/// ```
pub fn assemble_change<T>(
    txn: &T,
    files: &[RecordedFile],
    header: ChangeHeader,
    options: &AssemblyOptions,
) -> types::AssemblyResult<AssemblyResult_>
where
    T: GraphTxnT
        + TreeTxnT
        + CrdtTxnT
        + crate::pristine::InodeAttrTxnT
        + crate::pristine::InodeGraphOps,
{
    // Validate input
    if files.is_empty() {
        return Err(types::AssemblyError::NoFiles);
    }

    log::debug!("assemble_change: {} files", files.len(),);

    // Create globalization context
    let mut glob_ctx = GlobalizeContext::new(txn);
    let mut ctx = AssemblyContext::new(header);
    let mut stats = AssemblyStats::new();
    let mut globalized_files = Vec::new();
    let globalize_errors = Vec::new();

    // Process each file
    let total_files = files.len();
    let assembly_start = Instant::now();

    for (file_idx, file) in files.iter().enumerate() {
        stats.record_file();

        // Skip empty files if configured, but never skip directories
        // (directories generate hunks during globalization, not during recording)
        if file.is_empty()
            && !options.get_include_empty_files()
            && !file.is_directory()
            && !file.is_deleted_directory()
        {
            log::debug!(
                "assemble_change: file {}/{} '{}' skipped (no hunks)",
                file_idx + 1,
                total_files,
                file.path(),
            );
            stats.record_skip();
            continue;
        }

        // Skip newly-added files with empty content (e.g., 0-byte files like
        // .nojekyll, .gitkeep).  These have hunks from the recording phase but
        // no actual content bytes, so globalization will produce nothing —
        // avoid the expensive globalize_recorded_file call entirely.
        // Attribute-carrying records never skip: their FileAdd target and the
        // attribute ops must both be emitted for the staged state to hold.
        if file.inode().is_none()
            && file.content().is_empty()
            && file.attrs().is_empty()
            && !file.is_directory()
            && !file.is_deleted_directory()
            && !options.get_include_empty_files()
        {
            log::debug!(
                "assemble_change: file {}/{} '{}' skipped (0-byte added file, no inode)",
                file_idx + 1,
                total_files,
                file.path(),
            );
            stats.record_skip();
            continue;
        }

        // Globalize the file
        log::debug!(
            "assemble_change: file {}/{} '{}' globalizing (hunks={} content_bytes={} kind={:?})",
            file_idx + 1,
            total_files,
            file.path(),
            file.hunks().len(),
            file.content().len(),
            file.kind(),
        );
        let glob_start = Instant::now();

        // Fast path: if the file has pre-globalized ops (vertex map was
        // available during content retrieval), skip globalization entirely.
        if let Some(pre_ops) = file.pre_globalized() {
            // The pre-globalized Insertion offsets index into this file's own
            // inserted-span blob (base 0). Append that blob to the shared
            // change content and shift the offsets to their global position so
            // content retrieval can resolve each span.
            let (base, _) = glob_ctx.append_content(file.content());
            let shift = base.get();
            for op in pre_ops {
                let mut op = op.clone();
                shift_graph_op_content(&mut op, shift);
                ctx.add_hunk(op);
            }
            // Opaque files retain their trunk lifecycle but deliberately omit
            // line/token semantics. Attribute operations still need that stable
            // trunk identity.
            if let Some(crdt_ops) = file.crdt_ops() {
                let mut crdt_ops = crdt_ops.clone();
                if file.opaque_generated() {
                    crdt_ops.line_ops_mut().clear();
                }
                ctx.add_file_ops(crdt_ops)?;
            }
            emit_recorded_attributes(&mut ctx, &mut glob_ctx, file)?;
            let glob_ms = glob_start.elapsed().as_millis();
            if glob_ms > 100 {
                eprintln!(
                    "[assemble] pre-globalized '{}' {}ms ({} hunks)",
                    file.path(),
                    glob_ms,
                    pre_ops.len(),
                );
            }
            stats.record_file();
            continue;
        }

        match globalize_recorded_file(&mut glob_ctx, file, options.get_globalize_options()) {
            Ok(globalized) => {
                let glob_ms = glob_start.elapsed().as_millis();

                if globalized.is_empty() {
                    log::debug!(
                        "assemble_change: file {}/{} '{}' globalized empty in {}ms, skipping. \
                         is_directory={} is_deleted={} has_content={} has_position={:?}",
                        file_idx + 1,
                        total_files,
                        file.path(),
                        glob_ms,
                        file.is_directory(),
                        file.is_deleted_directory(),
                        !file.is_empty(),
                        file.position(),
                    );
                    // Attribute writes stand alone: a chmod-only or kind-only
                    // record globalizes empty but must still emit its SetAttr
                    // hunks and semantic counterparts.
                    emit_recorded_attributes(&mut ctx, &mut glob_ctx, file)?;
                    stats.record_skip();
                    continue;
                }

                // Collect CRDT file operations (semantic layer) AFTER
                // globalization so the LineOps carry the `content_range`
                // that globalize's enrich pass populated.  Apply uses
                // `content_range` to wire BRANCH_VERTEX → graph span;
                // without it, the CRDT-driven output walker raises
                // OrphanBranch on every line.  Falling back to
                // `file.crdt_ops()` (pre-enrichment) preserves the legacy
                // shape for files globalize couldn't enrich.
                if file.opaque_generated() {
                    if let Some(crdt_ops) = file.crdt_ops() {
                        let mut trunk_only = crdt_ops.clone();
                        trunk_only.line_ops_mut().clear();
                        ctx.add_file_ops(trunk_only)?;
                    }
                } else if let Some(enriched_ops) = globalized.file_ops() {
                    ctx.add_file_ops(enriched_ops.clone())?;
                } else if let Some(crdt_ops) = file.crdt_ops() {
                    ctx.add_file_ops(crdt_ops.clone())?;
                }

                let hunk_count = globalized.hunks().len();
                // Add hunks from the globalized file BEFORE the attribute
                // writes (CB-9C): a created FileAdd target must already be
                // in the context when its attribute op resolves.
                for graph_op in globalized.hunks() {
                    ctx.add_hunk(graph_op.clone());
                }
                emit_recorded_attributes(&mut ctx, &mut glob_ctx, file)?;

                if glob_ms > 100 {
                    eprintln!(
                        "[assemble] SLOW '{}' {}ms ({} hunks)",
                        file.path(),
                        glob_ms,
                        hunk_count,
                    );
                    log::warn!(
                        "assemble_change: SLOW file {}/{} '{}' took {}ms ({} hunks, {} bytes added)",
                        file_idx + 1,
                        total_files,
                        file.path(),
                        glob_ms,
                        hunk_count,
                        globalized.bytes_added(),
                    );
                } else {
                    log::debug!(
                        "assemble_change: file {}/{} '{}' globalized in {}ms ({} hunks)",
                        file_idx + 1,
                        total_files,
                        file.path(),
                        glob_ms,
                        hunk_count,
                    );
                }

                stats.add_content_bytes(globalized.bytes_added());
                globalized_files.push(globalized);
            }
            Err(source) => {
                log::debug!(
                    "assemble_change: file {}/{} '{}' globalization failed after {}ms: {}",
                    file_idx + 1,
                    total_files,
                    file.path(),
                    glob_start.elapsed().as_millis(),
                    source,
                );
                return Err(types::AssemblyError::Globalize {
                    path: file.path().to_string(),
                    source,
                });
            }
        }
    }

    let assembly_elapsed = assembly_start.elapsed();
    log::debug!(
        "assemble_change: all {} files globalized in {:.1}s, {} total hunks",
        total_files,
        assembly_elapsed.as_secs_f64(),
        ctx.hunk_count(),
    );

    // Check if we have any hunks
    if ctx.hunk_count() == 0 && !options.get_include_empty_files() {
        return Err(types::AssemblyError::AllEmpty);
    }

    // Add dependencies from globalization context
    ctx.add_dependencies(glob_ctx.dependencies().iter().copied());

    // Check content size limit
    let content = glob_ctx.take_content();
    if content.len() > options.get_max_content_size() {
        return Err(types::AssemblyError::ContentTooLarge {
            actual: content.len(),
            limit: options.get_max_content_size(),
        });
    }

    // Finalize the change with AI provenance and metadata bytes from options
    let change = ctx.finalize(
        content,
        options.get_provenance().to_vec(),
        options.get_metadata_bytes().to_vec(),
    );

    Ok(AssemblyResult_::new(
        change,
        stats,
        globalized_files,
        globalize_errors,
    ))
}

/// Create an empty change (with header only).
///
/// This is useful for creating placeholder changes or changes that
/// only modify metadata.
///
/// # Arguments
///
/// * `header` - The change header
///
/// # Returns
///
/// An empty change.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::assembly::create_empty_change;
/// use atomic_core::change::ChangeHeader;
///
/// let header = ChangeHeader::builder().message("Empty change").build();
#[must_use]
pub fn create_empty_change(header: ChangeHeader) -> Change {
    Change::empty(header)
}

/// The namespace span an entry occupies for one placeholder family: its
/// local maximum index plus one slot (CB-9B review B1).
///
/// `u32::MAX` as a local maximum leaves no free slot, so the span itself
/// saturates and the entry cannot fit — reported as exhaustion rather than
/// wrapping into a reused namespace.
fn checked_placeholder_span(max_index: u32) -> AssemblyResult<u32> {
    max_index.checked_add(1).ok_or(AssemblyError::PlaceholderNamespaceExhausted {
        namespace: "branch",
        next_index: u32::MAX as u64 + 1,
        limit: u32::MAX as u64,
    })
}

/// The highest ROOT-placeholder branch index used in `ops` (CB-9B review F4).
///
/// Real (non-placeholder) ids — e.g. delete ops bound to existing branches —
/// do not participate: only placeholders need a per-change unique namespace.
/// A ROOT-placeholder trunk's file index participates as a branch-slot
/// value because the apply substitutes both from the same placeholder
/// shape.
fn max_placeholder_branch_index(ops: &FileOps) -> u32 {
    let trunk = ops.trunk_id();
    let mut max = if trunk.change_id().is_root() {
        trunk.file_idx()
    } else {
        0
    };
    let mut consider = |id: &BranchId| {
        if id.change_id().is_root() && id.branch_idx() > max {
            max = id.branch_idx();
        }
    };
    for line_op in ops.line_ops() {
        consider(&copy_branch(line_op.branch_id()));
        match line_op.operation() {
            BranchOp::Insert { after, .. } => {
                if let Some(after) = after {
                    consider(after);
                }
            }
            BranchOp::Delete { branch, .. } | BranchOp::Modify { branch, .. } => {
                consider(&copy_branch(*branch))
            }
            BranchOp::Restore { branch } => consider(&copy_branch(*branch)),
            BranchOp::Reparent { branch, new_after } => {
                consider(&copy_branch(*branch));
                if let Some(after) = new_after {
                    consider(after);
                }
            }
        }
    }
    max
}

fn copy_branch(id: BranchId) -> BranchId {
    id
}

/// The highest ROOT-placeholder leaf index used in `ops`.
fn max_placeholder_leaf_index(ops: &FileOps) -> u32 {
    let mut max = 0u32;
    let mut consider = |leaf: &LeafOp| {
        if let LeafOp::Insert { after: Some(id), .. } = leaf {
            if id.change_id().is_root() && id.leaf_idx() > max {
                max = id.leaf_idx();
            }
        }
    };
    for line_op in ops.line_ops() {
        for leaf in line_op.leaf_ops() {
            consider(leaf);
        }
    }
    max
}
