//! `InPlaceEdit` recipe — the default for line-level edits without moves.
//!
//! Today this is a thin shim over the existing
//! [`build_crdt_ops_for_modified_file`](super::super::record::crdt::build_crdt_ops_for_modified_file)
//! function.  The shim lets us route through the dispatcher without
//! relocating ~600 lines of code in the same change.
//!
//! Future cleanup: move the implementation here verbatim and delete the
//! old function once all callers go through the dispatcher.

use super::RecipeContext;
use crate::change::FileOps;
use crate::record::workflow::crdt::CrdtBuildStats;

/// Build CRDT ops using the in-place line-edit strategy.
///
/// Delegates to the existing implementation.  Score from the detector is
/// always at least 1, so this recipe is the fallback when no other
/// recipe claims a higher score.
pub fn build_ops(ctx: &RecipeContext<'_>) -> (FileOps, CrdtBuildStats) {
    super::super::record::crdt::build_crdt_ops_for_modified_file(
        ctx.path,
        ctx.old_content,
        ctx.new_content,
        ctx.encoding,
        ctx.algorithm,
        ctx.existing_branches,
    )
}
