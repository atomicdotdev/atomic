//! Input bundle passed to every recipe.
//!
//! All recipes consume the same set of inputs and return the same output
//! shape: a [`FileOps`](crate::change::FileOps) carrying the line-level
//! CRDT ops plus per-build statistics.  Keeping the input bundled in a
//! single struct means new recipes don't ripple through call sites when
//! we add fields.

use crate::change::Encoding;
use crate::crdt::{BranchId, TrunkId};
use crate::diff::Algorithm;

/// Everything a recipe needs to produce CRDT operations for a modified
/// file.
///
/// Built once in `record_modified_file` and passed by reference to every
/// recipe — both the detector (which scores candidates) and the chosen
/// recipe (which builds the ops).
pub struct RecipeContext<'a> {
    /// The file's path (used for diagnostics and trunk lookups).
    pub path: &'a str,

    /// The previous content of the file — what's currently in the graph
    /// for this view, before applying the new content.
    pub old_content: &'a [u8],

    /// The new content the user is recording.
    pub new_content: &'a [u8],

    /// File-ordered list of *alive* `BranchId`s for `old_content`.
    /// `existing_branches[i]` is the branch representing line `i` of
    /// `old_content` (0-indexed).
    ///
    /// `None` when the caller has no access to the pristine state (tests,
    /// in-memory pipelines).  Recipes that need to look up an existing
    /// branch fall back to fresh placeholders.
    pub existing_branches: Option<&'a [BranchId]>,

    /// Existing trunk identity for the file being edited.
    ///
    /// Required when a modification inserts new branches: those new lines
    /// must join the file's existing trunk rather than a synthetic
    /// per-change trunk, otherwise the CRDT walker cannot see them when it
    /// traverses the file by path.
    pub existing_trunk_id: Option<TrunkId>,

    /// Detected encoding of the new content.
    pub encoding: Encoding,

    /// Diff algorithm requested by the caller.
    pub algorithm: Algorithm,
}
