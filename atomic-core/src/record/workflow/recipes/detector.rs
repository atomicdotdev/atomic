//! Rules-based recipe selection.
//!
//! Instead of fuzzy similarity scoring, the detector is a small policy
//! engine: each [`Rule`] pairs a **named predicate** with a target
//! recipe.  Rules are evaluated in declaration order and the first one
//! whose predicate matches wins.  When no rule matches, the detector
//! falls back to [`Recipe::InPlaceEdit`].
//!
//! # Why rules, not scoring?
//!
//! Scoring blurs the answer to "why did this recipe fire?" — the
//! recipe that won by 0.31 vs 0.29 is indistinguishable from a tight
//! call.  A rule with a concrete predicate is auditable: every match
//! has a named cause that we can log, test, and reason about.
//!
//! # Adding a rule
//!
//! 1. Write a predicate function in [`predicates`].  Keep it cheap —
//!    every modified file is evaluated against every rule.
//! 2. Append a [`Rule`] entry to [`RULES`] with the predicate, the
//!    target recipe, and a descriptive name.
//! 3. Add a unit test that builds a synthetic `RecipeContext` matching
//!    your predicate and asserts `detect_recipe` returns the right
//!    recipe.

use super::{Recipe, RecipeContext};

pub mod predicates;

/// One named rule in the policy engine.
///
/// Rules are pure: the predicate is a function pointer (no captured
/// state), and the recipe is a fixed mapping.  This keeps the rule
/// table `const`-friendly and the dispatch trivially testable.
#[derive(Debug, Clone, Copy)]
pub struct Rule {
    /// Human-readable identifier surfaced in logs and tests.
    pub name: &'static str,
    /// Returns `true` when this rule applies to the context.
    pub predicate: fn(&RecipeContext<'_>) -> bool,
    /// The recipe to run when the predicate matches.
    pub recipe: Recipe,
}

/// The ordered rule table.
///
/// Earlier rules take precedence.  Add more-specific rules near the
/// top; the default fallback is implicit (`InPlaceEdit` runs when no
/// rule matches).
///
/// # Why empty by default?
///
/// Heuristic-driven move detection has a recurring failure mode: a
/// pure insertion of N lines above existing content looks identical
/// (at the line-hash level) to a "block of lines moved earlier" —
/// every line below the insertion point matches old content at a
/// shifted position.  Without an explicit signal that *some lines
/// moved up while others moved down*, scoring confuses the two and
/// over-selects `ExtractMove`.
///
/// Rather than chase calibration cliffs, we ship the rules engine
/// with no auto-firing rules and add them only when a concrete
/// high-confidence trigger is identified:
///
///   - **Git import**: the importer provides `diff_lines` with `+`/`-`
///     classification and per-file rename detection.  A future rule
///     would test for that signal and route to a dedicated git recipe.
///   - **Explicit caller intent**: a recording option that *names* the
///     recipe to use (e.g., for tools that know they're extracting a
///     function).
///   - **Block predicate refinement**: a predicate that distinguishes
///     "pure shift" (constant delta across the suffix) from "real
///     relocation" (delta varies / some lines move up, others down).
///
/// Each new rule must come with a unit test pinning its match
/// condition, and a non-regression test confirming `InPlaceEdit`-suited
/// inputs still fall through.
#[allow(dead_code)]
pub const RULES: &[Rule] = &[];

// Predicates are exported for future use and explicitly retained
// even when no rule references them — they're the building blocks
// for rules we'll add as the system matures.
#[allow(dead_code)]
fn _retain_predicate_for_future_rules() {
    let _ = predicates::has_large_relocated_block;
}

/// Pick the best recipe for `ctx`.
///
/// Walks the `RULES` table top-to-bottom; the first rule whose predicate
/// matches selects its recipe.  Falls back to [`Recipe::InPlaceEdit`]
/// when nothing matches.
///
/// `log::trace!` is emitted for the match (or fallback) so production
/// builds can introspect which rule fired without rebuilding.
pub fn detect_recipe(ctx: &RecipeContext<'_>) -> Recipe {
    for rule in RULES {
        if (rule.predicate)(ctx) {
            log::trace!(
                "recipes::detect_recipe: path={:?} matched rule={:?} → {:?}",
                ctx.path,
                rule.name,
                rule.recipe
            );
            return rule.recipe;
        }
    }
    log::trace!(
        "recipes::detect_recipe: path={:?} no rule matched → InPlaceEdit",
        ctx.path
    );
    Recipe::InPlaceEdit
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::Encoding;
    use crate::diff::Algorithm;

    fn ctx<'a>(
        old: &'a [u8],
        new: &'a [u8],
        existing: Option<&'a [crate::crdt::BranchId]>,
    ) -> RecipeContext<'a> {
        RecipeContext {
            path: "test.txt",
            old_content: old,
            new_content: new,
            existing_branches: existing,
            existing_trunk_id: None,
            encoding: Encoding::Utf8,
            algorithm: Algorithm::Myers,
        }
    }

    #[test]
    fn empty_existing_branches_falls_back_to_in_place_edit() {
        // With no CRDT state, move-detection rules can't fire — must
        // fall back to InPlaceEdit.
        let recipe = detect_recipe(&ctx(b"a\n", b"b\n", None));
        assert_eq!(recipe, Recipe::InPlaceEdit);
    }

    #[test]
    fn no_relocation_means_in_place_edit() {
        // Modifying one line in the middle: not a move pattern.
        use crate::crdt::BranchId;
        use crate::types::NodeId;
        let branches = [
            BranchId::new(NodeId::new(1), 0),
            BranchId::new(NodeId::new(1), 1),
            BranchId::new(NodeId::new(1), 2),
        ];
        let recipe = detect_recipe(&ctx(
            b"alpha\nbeta\ngamma\n",
            b"alpha\nBETA\ngamma\n",
            Some(&branches),
        ));
        assert_eq!(recipe, Recipe::InPlaceEdit);
    }
}
