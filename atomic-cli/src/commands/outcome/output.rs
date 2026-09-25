//! The bounded CLI skin for an [`Outcome`].
//!
//! Bounded on purpose: an outcome over a large body of work can touch thousands
//! of files, so paths and model rows are truncated with an explicit tail count
//! rather than dumped. The `--json` form carries the full set.

use atomic_repository::{
    Mergeability, ModelSpend, Outcome, OutcomeCost, OutcomeFootprint, ViewOutcome,
};

use crate::output::{emphasis, hint, info};

/// How many model rows to show before collapsing into a tail count.
const MAX_MODELS_SHOWN: usize = 8;

/// How many file paths to show before collapsing into a count.
const MAX_PATHS_SHOWN: usize = 12;

/// How many uncovered-file names to name before counting only.
const MAX_UNCOVERED_SHOWN: usize = 3;

/// Render an outcome to stdout as the bounded human report.
pub fn print_outcome(outcome: &Outcome) {
    let label = mergeability_label(&outcome.mergeability);
    println!(
        "{} {} \u{2192} {}   {}   ({} change{}, {} file{})",
        emphasis("outcome"),
        outcome.sources.join("+"),
        outcome.target,
        label,
        outcome.footprint.changes,
        plural(outcome.footprint.changes),
        outcome.footprint.files,
        plural(outcome.footprint.files),
    );

    if !outcome.sources.is_empty() && outcome.sources.len() > 1 {
        println!("  {}", hint("stacked — all sources roll up as one unit"));
    }

    print_cost(&outcome.cost);
    print_mergeability(&outcome.mergeability);
    print_footprint(&outcome.footprint);
    print_views(&outcome.views);

    println!("\n  {}", hint(&outcome.reference));
}

/// The cost side. Prints "unknown" rather than `$0.00` when no change carried
/// provenance — a zero that means "not recorded" must not read as "free".
fn print_cost(cost: &OutcomeCost) {
    if !cost.cost_known {
        if cost.unattributed_changes > 0 {
            println!(
                "  cost   {}",
                hint(&format!(
                    "unknown \u{2014} none of {} change(s) carry provenance",
                    cost.unattributed_changes
                ))
            );
        } else {
            println!(
                "  cost   {}",
                hint("unknown \u{2014} no changes in this set")
            );
        }
    } else {
        println!(
            "  cost   {}   {}",
            money(cost.micro_usd),
            hint(&format!(
                "{} tokens  \u{00b7}  {} in / {} out",
                cost.total_tokens, cost.input_tokens, cost.output_tokens
            ))
        );
        for model in cost.by_model.iter().take(MAX_MODELS_SHOWN) {
            print_model(model);
        }
        if cost.by_model.len() > MAX_MODELS_SHOWN {
            println!(
                "         {}",
                hint(&format!(
                    "\u{2026} {} more model(s)",
                    cost.by_model.len() - MAX_MODELS_SHOWN
                ))
            );
        }
    }
    if cost.unreadable_changes > 0 {
        println!(
            "         {}",
            crate::output::warning(&format!(
                "{} change(s) unreadable \u{2014} excluded from these totals",
                cost.unreadable_changes
            ))
        );
    }
}

fn print_model(model: &ModelSpend) {
    println!(
        "         {}  {}  {}",
        model.model,
        money(model.micro_usd),
        hint(&format!(
            "{} tokens  \u{00b7}  {} change{}",
            model.total_tokens,
            model.changes,
            plural(model.changes)
        ))
    );
}

/// The headline answer: can this be promoted as one unit?
fn print_mergeability(mergeability: &Mergeability) {
    let conflicts = match mergeability {
        Mergeability::Clean => {
            println!("  merge  {}", info("CLEAN \u{2014} no residual conflict"));
            return;
        }
        Mergeability::NeedsHuman { conflicts } => {
            println!(
                "  merge  {}",
                crate::output::error(&format!(
                    "NEEDS HUMAN \u{2014} {} conflict{}",
                    conflicts.len(),
                    plural(conflicts.len())
                ))
            );
            conflicts
        }
        Mergeability::NotComputable { reason, conflicts } => {
            println!(
                "  merge  {}",
                crate::output::warning(&format!(
                    "NOT COMPUTABLE \u{2014} {} conflict{}",
                    conflicts.len(),
                    plural(conflicts.len())
                ))
            );
            println!("         {}", hint(reason));
            conflicts
        }
    };

    for conflict in conflicts {
        let over = if conflict.concurrent_sources > 2 {
            format!(
                "  ({}-way \u{2014} beyond 2-side merge)",
                conflict.concurrent_sources
            )
        } else {
            String::new()
        };
        println!(
            "         {} {} {}{}",
            conflict.path,
            hint(&format!("[{}]", conflict.kind)),
            conflict.views.join("+"),
            over
        );
    }
}

/// The shape of the work.
fn print_footprint(footprint: &OutcomeFootprint) {
    if footprint.baggage > 0 {
        let named: Vec<String> = footprint
            .baggage_files
            .iter()
            .take(MAX_UNCOVERED_SHOWN)
            .map(|f| f.trim_start_matches("file:").to_string())
            .collect();
        let more = footprint
            .baggage_files
            .len()
            .saturating_sub(MAX_UNCOVERED_SHOWN);
        let tail = if more > 0 {
            format!(" (+{more} more)")
        } else {
            String::new()
        };
        println!(
            "  {}  {}",
            crate::output::warning(&format!("{} uncovered", footprint.baggage)),
            hint(&format!("no intent touches {}", named.join(", ") + &tail))
        );
    }
    for path in footprint.paths.iter().take(MAX_PATHS_SHOWN) {
        println!("         {}", hint(path));
    }
    if footprint.paths.len() > MAX_PATHS_SHOWN {
        println!(
            "         {}",
            hint(&format!(
                "\u{2026} {} more file{}",
                footprint.paths.len() - MAX_PATHS_SHOWN,
                plural(footprint.paths.len() - MAX_PATHS_SHOWN)
            ))
        );
    }
}

/// The per-source partition of the union. One source needs no breakdown — the
/// totals above already describe it.
fn print_views(views: &[ViewOutcome]) {
    if views.len() < 2 {
        return;
    }
    println!("\n{}", emphasis("By view"));
    for view in views {
        let cost = if view.cost.cost_known {
            money(view.cost.micro_usd)
        } else {
            hint("cost unknown").to_string()
        };
        println!(
            "  {} {}  {} change{}  \u{00b7}  {} file{}  \u{00b7}  {}",
            view.view,
            hint(&format!("[{}]", view.scope)),
            view.changes,
            plural(view.changes),
            view.files,
            plural(view.files),
            cost
        );
    }
}

fn mergeability_label(mergeability: &Mergeability) -> String {
    match mergeability {
        Mergeability::Clean => info("CLEAN").to_string(),
        Mergeability::NeedsHuman { conflicts } => {
            crate::output::error(format!("{} CONFLICT(S)", conflicts.len())).to_string()
        }
        Mergeability::NotComputable { conflicts, .. } => {
            crate::output::warning(&format!("{} CONFLICT(S), NOT COMPUTABLE", conflicts.len()))
                .to_string()
        }
    }
}

/// A micro-USD integer rendered as dollars. Sub-cent spend keeps enough digits
/// that a real turn is never rounded away to `$0.00`.
fn money(micro_usd: u64) -> String {
    match micro_usd {
        0 => "$0.00".to_string(),
        1..=9_999 => format!("${:.4}", micro_usd as f64 / 1_000_000.0),
        _ => format!("${:.2}", micro_usd as f64 / 1_000_000.0),
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_cent_spend_is_not_rounded_away() {
        // A single cheap turn must not render as "$0.00", which would read as
        // "this work was free".
        assert_eq!(money(1), "$0.0000");
        assert_eq!(money(1_200), "$0.0012");
        assert_eq!(money(9_999), "$0.0100");
        assert_eq!(money(0), "$0.00");
        assert_eq!(money(1_500_000), "$1.50");
    }

    #[test]
    fn plurals_are_correct() {
        assert_eq!(plural(0), "s");
        assert_eq!(plural(1), "");
        assert_eq!(plural(2), "s");
    }
}
