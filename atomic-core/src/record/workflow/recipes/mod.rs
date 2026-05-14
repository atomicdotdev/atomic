//! Record-side recipes for translating file modifications into CRDT ops.
//!
//! Different change shapes call for different operation streams.  A
//! small targeted line edit, a code-extraction refactor that moves
//! lines across the file, and a cross-view merge each want a different
//! mapping from `(old, new)` content to `BranchOp::*` operations.
//!
//! Rather than handle all of those cases inline in a single function,
//! each shape is a named **recipe** with its own scoring and op-building
//! logic.  Recording a file picks the highest-scoring recipe for the
//! input and delegates op generation to it.
//!
//! # Architecture
//!
//! ```text
//! ┌────────────────────────────────────────────────────────────────────┐
//! │  record_modified_file                                              │
//! │       │                                                            │
//! │       ▼                                                            │
//! │  RecipeContext { path, old, new, existing_branches, … }            │
//! │       │                                                            │
//! │       ▼                                                            │
//! │  detector::detect_recipe(&ctx) ──► picks one Recipe                │
//! │       │                                                            │
//! │       ▼                                                            │
//! │  Recipe::build_ops(&ctx) ──► (FileOps, CrdtBuildStats)             │
//! └────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Registered recipes
//!
//! - [`Recipe::InPlaceEdit`] — baseline.  Line-level diff with no move
//!   detection.  Suits in-place changes (modify a line, add/remove lines
//!   adjacent to existing content).  Always applies.
//!
//! Planned (see tasks #31, #32, and beyond):
//!
//! - `ExtractMove` — detects code moves via content-hash matching and
//!   emits `BranchOp::Reparent` instead of `Delete+Insert` for moved
//!   lines.  Preserves blame and identity across refactors.
//! - `CrossViewMerge` — view-filter-aware op generation.
//! - `GitImportWithMoves` — drives off git's diff plus rename detection.
//! - `WhitespaceCleanup` — token-level rather than line-level matching.
//!
//! # Adding a new recipe
//!
//! 1. Add a variant to [`Recipe`].
//! 2. Create a new file `<recipe_name>.rs` with `build_ops(ctx) -> (FileOps, CrdtBuildStats)`.
//! 3. Wire it into [`Recipe::build_ops`].
//! 4. Implement scoring in [`detector::detect_recipe`].
//! 5. Add a canonical test for the recipe (see the recipe-corpus
//!    convention in `atomic-repository/tests`).

mod content_hash;
mod context;
mod detector;
pub mod diff_op_rules;
mod extract_move;
mod in_place_edit;

pub use content_hash::{hash_line, LineHashIndex};
pub use context::RecipeContext;
pub use detector::detect_recipe;

use crate::change::FileOps;
use crate::record::workflow::crdt::CrdtBuildStats;

/// The set of recipes we can route a file modification through.
///
/// Each variant has a corresponding submodule.  See the module-level
/// docs for how recipes are scored and how to add new ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recipe {
    /// Baseline line-level edit.  Always applies as a fallback.
    InPlaceEdit,
    /// Move-aware: content-hash matching identifies relocated lines and
    /// emits `BranchOp::Reparent` instead of `Delete + Insert`.
    /// Applies when ≥30% of new lines match old lines at different
    /// positions (extract function, code reorder, refactors).
    ExtractMove,
}

impl Recipe {
    /// Score every registered recipe and pick the winner.
    ///
    /// Convenience: equivalent to [`detect_recipe`].
    #[inline]
    pub fn detect(ctx: &RecipeContext<'_>) -> Self {
        detect_recipe(ctx)
    }

    /// Build CRDT ops for `ctx` using this recipe.
    pub fn build_ops(self, ctx: &RecipeContext<'_>) -> (FileOps, CrdtBuildStats) {
        match self {
            Recipe::InPlaceEdit => in_place_edit::build_ops(ctx),
            Recipe::ExtractMove => extract_move::build_ops(ctx),
        }
    }
}

