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
use crate::pristine::{GraphTxnT, TreeTxnT};
use crate::types::Hash;

use super::globalize::{globalize_recorded_file, GlobalizeContext};
use super::record::RecordedFile;

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
    /// # Arguments
    ///
    /// * `ops` - The file operation to add
    pub fn add_file_ops(&mut self, ops: FileOps) {
        self.file_ops.push(ops);
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
    T: GraphTxnT + TreeTxnT + crate::pristine::InodeGraphOps,
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
    let mut globalize_errors = Vec::new();

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
        if file.inode().is_none()
            && file.content().is_empty()
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
                if let Some(enriched_ops) = globalized.file_ops() {
                    ctx.add_file_ops(enriched_ops.clone());
                } else if let Some(crdt_ops) = file.crdt_ops() {
                    ctx.add_file_ops(crdt_ops.clone());
                }

                let hunk_count = globalized.hunks().len();
                // Add hunks from the globalized file
                for graph_op in globalized.hunks() {
                    ctx.add_hunk(graph_op.clone());
                }

                if glob_ms > 100 {
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
            Err(e) => {
                let glob_ms = glob_start.elapsed().as_millis();
                log::debug!(
                    "assemble_change: file {}/{} '{}' globalize error in {}ms: {}",
                    file_idx + 1,
                    total_files,
                    file.path(),
                    glob_ms,
                    e,
                );
                stats.record_error();
                globalize_errors.push((file.path().to_string(), e));
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
/// let change = create_empty_change(header);
///
/// assert!(change.hunks().is_empty());
/// ```
#[must_use]
pub fn create_empty_change(header: ChangeHeader) -> Change {
    Change::empty(header)
}
