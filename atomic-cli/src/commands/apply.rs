#![allow(dead_code)]
//! Apply command implementation.
//!
//! The `apply` command applies changes to stacks, supporting:
//! - Applying a single change by hash
//! - Applying changes from one stack to another
//! - Applying changes up to a specific tag
//! - Cherry-picking specific changes

use clap::{Args, Subcommand};

use atomic_core::types::{Base32, Hash};
use atomic_repository::{ApplyOptions, CrossStackApplyOptions, CrossStackApplyOutcome, Repository};

use crate::commands::{format_hash, require_repository};
use crate::error::{CliError, CliResult};
use crate::output;

// =============================================================================
// Apply Command
// =============================================================================

/// Apply changes to a stack.
///
/// This command applies changes to the repository graph. It supports several
/// modes of operation:
///
/// - Apply a single change by hash
/// - Apply all changes from one stack to another
/// - Apply changes up to a specific tag
/// - Cherry-pick specific changes
#[derive(Debug, Args)]
pub struct Apply {
    #[command(subcommand)]
    command: Option<ApplySubcommand>,

    /// Hash of the change to apply (when not using subcommands).
    #[arg(value_name = "CHANGE")]
    change: Option<String>,

    /// Stack to apply the change to (default: current stack).
    #[arg(short, long)]
    stack: Option<String>,

    /// Apply dependencies automatically.
    #[arg(long, default_value = "true")]
    deps: bool,

    /// Allow conflicts during apply.
    #[arg(long)]
    allow_conflicts: bool,

    /// Repository path.
    #[arg(short = 'R', long)]
    repository: Option<String>,
}

/// Apply subcommands for cross-stack operations.
#[derive(Debug, Subcommand)]
pub enum ApplySubcommand {
    /// Apply changes from one stack to another.
    #[command(name = "from-stack")]
    FromStack(FromStackArgs),

    /// Apply changes up to a specific tag.
    #[command(name = "tag")]
    Tag(TagArgs),

    /// Cherry-pick specific changes.
    #[command(name = "pick")]
    Pick(PickArgs),

    /// Show what would be applied (dry run).
    #[command(name = "preview")]
    Preview(PreviewArgs),
}

/// Arguments for applying from one stack to another.
#[derive(Debug, Args)]
pub struct FromStackArgs {
    /// Source stack to copy changes from.
    #[arg(value_name = "SOURCE")]
    from_stack: String,

    /// Target stack to apply changes to (default: current stack).
    #[arg(short, long)]
    to_stack: Option<String>,

    /// Apply dependencies automatically.
    #[arg(long, default_value = "true")]
    deps: bool,

    /// Allow conflicts during apply.
    #[arg(long)]
    allow_conflicts: bool,

    /// Perform a dry run (don't actually apply).
    #[arg(long)]
    dry_run: bool,
}

/// Arguments for applying up to a tag.
#[derive(Debug, Args)]
pub struct TagArgs {
    /// Name of the tag to apply up to.
    #[arg(value_name = "TAG")]
    tag_name: String,

    /// Source stack containing the tag.
    #[arg(short, long)]
    from_stack: Option<String>,

    /// Target stack to apply changes to (default: current stack).
    #[arg(short, long)]
    to_stack: Option<String>,

    /// Apply dependencies automatically.
    #[arg(long, default_value = "true")]
    deps: bool,

    /// Allow conflicts during apply.
    #[arg(long)]
    allow_conflicts: bool,

    /// Perform a dry run (don't actually apply).
    #[arg(long)]
    dry_run: bool,
}

/// Arguments for cherry-picking specific changes.
#[derive(Debug, Args)]
pub struct PickArgs {
    /// Hashes of changes to cherry-pick.
    #[arg(value_name = "CHANGES", required = true)]
    changes: Vec<String>,

    /// Target stack to apply changes to (default: current stack).
    #[arg(short, long)]
    to_stack: Option<String>,

    /// Apply dependencies automatically.
    #[arg(long, default_value = "true")]
    deps: bool,

    /// Allow conflicts during apply.
    #[arg(long)]
    allow_conflicts: bool,
}

/// Arguments for previewing what would be applied.
#[derive(Debug, Args)]
pub struct PreviewArgs {
    /// Source stack to preview changes from.
    #[arg(value_name = "SOURCE")]
    from_stack: String,

    /// Target stack (default: current stack).
    #[arg(short, long)]
    to_stack: Option<String>,

    /// Optional tag to limit preview up to.
    #[arg(long)]
    up_to_tag: Option<String>,
}

// =============================================================================
// Command Implementation
// =============================================================================

impl crate::commands::Command for Apply {
    fn run(&self) -> CliResult<()> {
        let repo_path = self.repository.as_ref().map(std::path::Path::new);
        let repo = require_repository(repo_path)?;

        match &self.command {
            Some(ApplySubcommand::FromStack(args)) => run_from_stack(&repo, args),
            Some(ApplySubcommand::Tag(args)) => run_tag(&repo, args),
            Some(ApplySubcommand::Pick(args)) => run_pick(&repo, args),
            Some(ApplySubcommand::Preview(args)) => run_preview(&repo, args),
            None => {
                // Apply a single change
                if let Some(ref change_str) = self.change {
                    run_single_apply(&repo, change_str, self)
                } else {
                    Err(CliError::InvalidArgument {
                        message: "Missing CHANGE argument. Provide a change hash or use a subcommand (from-stack, tag, pick)".to_string(),
                    })
                }
            }
        }
    }
}

// =============================================================================
// Subcommand Implementations
// =============================================================================

/// Apply a single change by hash.
fn run_single_apply(repo: &Repository, change_str: &str, args: &Apply) -> CliResult<()> {
    let hash = parse_change_hash(repo, change_str)?;
    let is_current_stack =
        args.stack.is_none() || args.stack.as_deref() == Some(repo.current_stack());

    let options = ApplyOptions::default()
        .apply_deps(args.deps)
        .allow_conflict(args.allow_conflicts);

    let options = if let Some(ref stack) = args.stack {
        options.stack(stack)
    } else {
        options
    };

    output::print_info(&format!("Applying change {}...", format_hash(&hash, true)));

    let outcome = if args.deps {
        repo.apply_change_rec(&hash, options)
    } else {
        repo.apply_change(&hash, options)
    }
    .map_err(|e| CliError::Conflict {
        description: e.to_string(),
    })?;

    print_apply_outcome(
        &outcome.stats.applied_hashes,
        outcome.new_state,
        outcome.has_conflicts,
    );

    // Update working copy if we applied to the current stack (like Git merge)
    if is_current_stack && !outcome.stats.applied_hashes.is_empty() {
        let output_result = repo.output_working_copy().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to update working copy: {}", e))
        })?;
        output::print_success(&format!(
            "{} files updated, {} directories",
            output_result.files_written, output_result.directories_created
        ));
    }

    Ok(())
}

/// Apply changes from one stack to another.
fn run_from_stack(repo: &Repository, args: &FromStackArgs) -> CliResult<()> {
    let to_stack = args
        .to_stack
        .clone()
        .unwrap_or_else(|| repo.current_stack().to_string());
    let is_current_stack = to_stack == repo.current_stack();

    output::print_info(&format!(
        "Applying changes from '{}' to '{}'...",
        args.from_stack, to_stack
    ));

    let options = CrossStackApplyOptions::new(&args.from_stack, &to_stack)
        .with_dependencies(args.deps)
        .allow_conflicts(args.allow_conflicts)
        .dry_run(args.dry_run);

    let outcome = repo
        .apply_from_stack(options)
        .map_err(|e| CliError::Conflict {
            description: e.to_string(),
        })?;

    print_cross_stack_outcome(&outcome, args.dry_run);

    // Update working copy if we applied to the current stack (like Git merge)
    if is_current_stack && !args.dry_run && outcome.changes_applied > 0 {
        let output_result = repo.output_working_copy().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to update working copy: {}", e))
        })?;
        output::print_success(&format!(
            "{} files updated, {} directories",
            output_result.files_written, output_result.directories_created
        ));
    }

    Ok(())
}

/// Apply changes up to a specific tag.
fn run_tag(repo: &Repository, args: &TagArgs) -> CliResult<()> {
    let from_stack = args
        .from_stack
        .clone()
        .unwrap_or_else(|| repo.current_stack().to_string());
    let to_stack = args
        .to_stack
        .clone()
        .unwrap_or_else(|| repo.current_stack().to_string());
    let is_current_stack = to_stack == repo.current_stack();

    output::print_info(&format!(
        "Applying changes up to tag '{}' from '{}' to '{}'...",
        args.tag_name, from_stack, to_stack
    ));

    let options = CrossStackApplyOptions::new(&from_stack, &to_stack)
        .up_to_tag(&args.tag_name)
        .with_dependencies(args.deps)
        .allow_conflicts(args.allow_conflicts)
        .dry_run(args.dry_run);

    let outcome = repo
        .apply_from_stack(options)
        .map_err(|e| CliError::Conflict {
            description: e.to_string(),
        })?;

    print_cross_stack_outcome(&outcome, args.dry_run);

    // Update working copy if we applied to the current stack (like Git merge)
    if is_current_stack && !args.dry_run && outcome.changes_applied > 0 {
        let output_result = repo.output_working_copy().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to update working copy: {}", e))
        })?;
        output::print_success(&format!(
            "{} files updated, {} directories",
            output_result.files_written, output_result.directories_created
        ));
    }

    Ok(())
}

/// Cherry-pick specific changes.
fn run_pick(repo: &Repository, args: &PickArgs) -> CliResult<()> {
    let to_stack = args
        .to_stack
        .clone()
        .unwrap_or_else(|| repo.current_stack().to_string());
    let is_current_stack = to_stack == repo.current_stack();

    // Parse all change hashes
    let mut hashes = Vec::new();
    for change_str in &args.changes {
        let hash = parse_change_hash(repo, change_str)?;
        hashes.push(hash);
    }

    output::print_info(&format!(
        "Cherry-picking {} change(s) to '{}'...",
        hashes.len(),
        to_stack
    ));

    let outcome = repo
        .cherry_pick(&hashes, "", Some(&to_stack))
        .map_err(|e| CliError::Conflict {
            description: e.to_string(),
        })?;

    print_cross_stack_outcome(&outcome, false);

    // Update working copy if we applied to the current stack (like Git merge)
    if is_current_stack && outcome.changes_applied > 0 {
        let output_result = repo.output_working_copy().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to update working copy: {}", e))
        })?;
        output::print_success(&format!(
            "{} files updated, {} directories",
            output_result.files_written, output_result.directories_created
        ));
    }

    Ok(())
}

/// Preview what would be applied.
fn run_preview(repo: &Repository, args: &PreviewArgs) -> CliResult<()> {
    let to_stack = args
        .to_stack
        .clone()
        .unwrap_or_else(|| repo.current_stack().to_string());

    output::print_section("Apply Preview");
    println!();
    println!("  Source stack: {}", args.from_stack);
    println!("  Target stack: {}", to_stack);
    if let Some(ref tag) = args.up_to_tag {
        println!("  Up to tag:    {}", tag);
    }
    println!();

    // Get the changes that would be applied
    let missing = if let Some(ref tag_name) = args.up_to_tag {
        // Get changes up to tag, then filter to missing
        let options = CrossStackApplyOptions::new(&args.from_stack, &to_stack)
            .up_to_tag(tag_name)
            .dry_run(true);

        let outcome = repo
            .apply_from_stack(options)
            .map_err(|e| CliError::Conflict {
                description: e.to_string(),
            })?;

        outcome.applied_hashes
    } else {
        repo.get_missing_changes_between(&args.from_stack, Some(&to_stack))
            .map_err(|e| CliError::Conflict {
                description: e.to_string(),
            })?
    };

    if missing.is_empty() {
        output::print_success("No changes to apply - target stack is up to date.");
    } else {
        println!("Changes that would be applied ({}):", missing.len());
        println!();
        for (i, hash) in missing.iter().enumerate() {
            // Try to load change header for more info
            if let Ok(change) = repo.load_change(hash) {
                let message = &change.hashed.header.message;
                let short_msg = if message.len() > 50 {
                    format!("{}...", &message[..47])
                } else {
                    message.to_string()
                };
                println!("  {}. {} {}", i + 1, format_hash(hash, true), short_msg);
            } else {
                println!("  {}. {}", i + 1, format_hash(hash, true));
            }
        }
        println!();
        output::print_info(&format!(
            "Run 'atomic apply from-stack {}' to apply these changes.",
            args.from_stack
        ));
    }

    Ok(())
}

// =============================================================================
// Helper Functions
// =============================================================================

/// Parse a change hash from a string (full or abbreviated).
fn parse_change_hash(repo: &Repository, hash_str: &str) -> CliResult<Hash> {
    // Try to parse as full hash first
    if let Some(hash) = Hash::from_base32(hash_str.as_bytes()) {
        return Ok(hash);
    }

    // Try to find by prefix
    if hash_str.len() >= 2 {
        // Look through recent changes to find a match
        match repo.find_change_by_prefix(hash_str) {
            Ok(Some(hash)) => return Ok(hash),
            Ok(None) => {}
            Err(_e) => {
                // Check if it's an ambiguous hash error
                return Err(CliError::AmbiguousHash {
                    hash: hash_str.to_string(),
                });
            }
        }
    }

    Err(CliError::ChangeNotFound {
        hash: hash_str.to_string(),
    })
}

/// Print the outcome of a single apply operation.
fn print_apply_outcome(
    applied: &[Hash],
    new_state: atomic_core::types::Merkle,
    has_conflicts: bool,
) {
    println!();

    if applied.is_empty() {
        output::print_warning("No changes applied.");
        return;
    }

    output::print_success(&format!("Applied {} change(s)", applied.len()));

    println!("  New state: {}", new_state.to_base32());

    if has_conflicts {
        println!();
        output::print_warning("Conflicts detected. Run 'atomic status' to see details.");
    }
}

/// Print the outcome of a cross-stack apply operation.
fn print_cross_stack_outcome(outcome: &CrossStackApplyOutcome, dry_run: bool) {
    println!();

    if dry_run {
        if outcome.applied_hashes.is_empty() {
            output::print_info("Dry run: No changes would be applied.");
        } else {
            output::print_info(&format!(
                "Dry run: {} change(s) would be applied",
                outcome.applied_hashes.len()
            ));

            println!();
            println!("Changes:");
            for hash in &outcome.applied_hashes {
                println!("  {}", format_hash(hash, true));
            }
        }
        return;
    }

    if outcome.changes_applied == 0 {
        output::print_success("No changes to apply - already up to date.");
    } else {
        output::print_success(&format!("Applied {} change(s)", outcome.changes_applied));
        println!("  New state: {}", outcome.new_state.to_base32());

        if !outcome.skipped_hashes.is_empty() {
            println!(
                "  Skipped:   {} (already applied)",
                outcome.skipped_hashes.len()
            );
        }
    }

    if outcome.has_conflicts {
        println!();
        output::print_warning("Conflicts detected. Run 'atomic status' to see details.");
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_subcommand_variants() {
        // Just verify the enums compile correctly
        let _ = ApplySubcommand::FromStack(FromStackArgs {
            from_stack: "feature".to_string(),
            to_stack: Some("main".to_string()),
            deps: true,
            allow_conflicts: false,
            dry_run: false,
        });

        let _ = ApplySubcommand::Tag(TagArgs {
            tag_name: "v1.0.0".to_string(),
            from_stack: Some("feature".to_string()),
            to_stack: Some("main".to_string()),
            deps: true,
            allow_conflicts: false,
            dry_run: false,
        });

        let _ = ApplySubcommand::Pick(PickArgs {
            changes: vec!["abc123".to_string()],
            to_stack: None,
            deps: true,
            allow_conflicts: false,
        });

        let _ = ApplySubcommand::Preview(PreviewArgs {
            from_stack: "feature".to_string(),
            to_stack: None,
            up_to_tag: None,
        });
    }

    #[test]
    fn test_from_stack_args_defaults() {
        let args = FromStackArgs {
            from_stack: "feature".to_string(),
            to_stack: None,
            deps: true,
            allow_conflicts: false,
            dry_run: false,
        };

        assert_eq!(args.from_stack, "feature");
        assert!(args.to_stack.is_none());
        assert!(args.deps);
        assert!(!args.allow_conflicts);
        assert!(!args.dry_run);
    }

    #[test]
    fn test_tag_args_with_all_options() {
        let args = TagArgs {
            tag_name: "v1.0.0".to_string(),
            from_stack: Some("feature".to_string()),
            to_stack: Some("main".to_string()),
            deps: false,
            allow_conflicts: true,
            dry_run: true,
        };

        assert_eq!(args.tag_name, "v1.0.0");
        assert_eq!(args.from_stack, Some("feature".to_string()));
        assert_eq!(args.to_stack, Some("main".to_string()));
        assert!(!args.deps);
        assert!(args.allow_conflicts);
        assert!(args.dry_run);
    }

    #[test]
    fn test_pick_args_multiple_changes() {
        let args = PickArgs {
            changes: vec![
                "abc123".to_string(),
                "def456".to_string(),
                "ghi789".to_string(),
            ],
            to_stack: Some("main".to_string()),
            deps: true,
            allow_conflicts: false,
        };

        assert_eq!(args.changes.len(), 3);
        assert_eq!(args.to_stack, Some("main".to_string()));
    }

    #[test]
    fn test_preview_args_minimal() {
        let args = PreviewArgs {
            from_stack: "feature".to_string(),
            to_stack: None,
            up_to_tag: None,
        };

        assert_eq!(args.from_stack, "feature");
        assert!(args.to_stack.is_none());
        assert!(args.up_to_tag.is_none());
    }

    #[test]
    fn test_preview_args_with_tag() {
        let args = PreviewArgs {
            from_stack: "feature".to_string(),
            to_stack: Some("main".to_string()),
            up_to_tag: Some("v1.0.0".to_string()),
        };

        assert_eq!(args.up_to_tag, Some("v1.0.0".to_string()));
    }
}
