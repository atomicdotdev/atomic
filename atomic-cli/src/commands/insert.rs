//! Insert command implementation.
//!
//! The `insert` command inserts changes into views, supporting:
//! - Inserting a single change by hash
//! - Inserting changes from one view to another
//! - Inserting changes up to a specific tag
//! - Cherry-picking specific changes

use clap::{Args, Subcommand};
use clap_complete::engine::ArgValueCompleter;

use atomic_core::types::{Base32, Hash};
use atomic_repository::{
    CrossViewInsertOptions, CrossViewInsertOutcome, InsertOptions, Repository,
};

use crate::commands::complete::{complete_change_hashes, complete_view_names};
use crate::commands::{format_hash, require_repository};
use crate::error::{CliError, CliResult};
use crate::output;

// Insert Command

/// Insert changes into a view.
///
/// This command inserts changes into the repository graph. It supports several
/// modes of operation:
///
/// - **Bare `atomic insert`** (no arguments): promote the current view's
///   changes into its parent view. This is the "I'm done with this draft,
///   land it" gesture.
/// - Insert a single change by hash
/// - Insert all changes from one view to another (`from-view`)
/// - Insert changes up to a specific tag (`tag`)
/// - Cherry-pick specific changes (`pick`)
#[derive(Debug, Args)]
pub struct Insert {
    #[command(subcommand)]
    command: Option<InsertSubcommand>,

    /// Hash of the change to insert (when not using subcommands).
    ///
    /// Omit entirely to promote the current view's changes into its parent
    /// view (see the command-level docs).
    #[arg(value_name = "CHANGE", add = ArgValueCompleter::new(complete_change_hashes))]
    change: Option<String>,

    /// Target view.
    ///
    /// For `atomic insert <hash>` this is the view the change is inserted
    /// into (default: current view). For the bare `atomic insert` promotion
    /// it overrides the target (default: the current view's parent).
    #[arg(long, visible_alias = "to", add = ArgValueCompleter::new(complete_view_names))]
    view: Option<String>,

    /// Insert dependencies automatically.
    #[arg(long, default_value = "true")]
    deps: bool,

    /// Allow conflicts during insert.
    #[arg(long)]
    allow_conflicts: bool,

    /// Preview the promotion without inserting anything.
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Skip the confirmation prompt when promoting between two shared views.
    #[arg(long)]
    confirm: bool,

    /// Repository path.
    #[arg(short = 'R', long)]
    repository: Option<String>,
}

/// Insert subcommands for cross-view operations.
#[derive(Debug, Subcommand)]
pub enum InsertSubcommand {
    /// Insert all changes from another view.
    ///
    /// `from-view` is kept as an alias for backward compatibility.
    #[command(name = "view", alias = "from-view")]
    View(ViewArgs),

    /// Insert changes up to a specific tag.
    #[command(name = "tag")]
    Tag(TagArgs),

    /// Insert specific change(s) by hash.
    ///
    /// `pick` is kept as an alias for backward compatibility.
    #[command(name = "change", alias = "pick")]
    Change(ChangeArgs),

    /// Show what would be inserted (dry run).
    #[command(name = "preview")]
    Preview(PreviewArgs),
}

/// Arguments for inserting all changes from another view.
#[derive(Debug, Args)]
pub struct ViewArgs {
    /// Source view to copy changes from.
    #[arg(value_name = "SOURCE", add = ArgValueCompleter::new(complete_view_names))]
    from_view: String,

    /// Target view to insert changes into (default: current view).
    #[arg(long, visible_alias = "to", add = ArgValueCompleter::new(complete_view_names))]
    to_view: Option<String>,

    /// Insert dependencies automatically.
    #[arg(long, default_value = "true")]
    deps: bool,

    /// Allow conflicts during insert.
    #[arg(long)]
    allow_conflicts: bool,

    /// Perform a dry run (don't actually insert).
    #[arg(short = 'n', long)]
    dry_run: bool,
}

/// Arguments for inserting up to a tag.
#[derive(Debug, Args)]
pub struct TagArgs {
    /// Name of the tag to insert up to.
    #[arg(value_name = "TAG")]
    tag_name: String,

    /// Source view containing the tag.
    #[arg(long, add = ArgValueCompleter::new(complete_view_names))]
    from_view: Option<String>,

    /// Target view to insert changes into (default: current view).
    #[arg(long, visible_alias = "to", add = ArgValueCompleter::new(complete_view_names))]
    to_view: Option<String>,

    /// Insert dependencies automatically.
    #[arg(long, default_value = "true")]
    deps: bool,

    /// Allow conflicts during insert.
    #[arg(long)]
    allow_conflicts: bool,

    /// Perform a dry run (don't actually insert).
    #[arg(short = 'n', long)]
    dry_run: bool,
}

/// Arguments for inserting specific change(s) by hash.
#[derive(Debug, Args)]
pub struct ChangeArgs {
    /// Hashes of changes to insert.
    #[arg(value_name = "CHANGES", required = true, add = ArgValueCompleter::new(complete_change_hashes))]
    changes: Vec<String>,

    /// Target view to insert changes into (default: current view).
    #[arg(long, visible_alias = "to", add = ArgValueCompleter::new(complete_view_names))]
    to_view: Option<String>,

    /// Insert dependencies automatically.
    #[arg(long, default_value = "true")]
    deps: bool,

    /// Allow conflicts during insert.
    #[arg(long)]
    allow_conflicts: bool,
}

/// Arguments for previewing what would be inserted.
#[derive(Debug, Args)]
pub struct PreviewArgs {
    /// Source view to preview changes from.
    #[arg(value_name = "SOURCE", add = ArgValueCompleter::new(complete_view_names))]
    from_view: String,

    /// Target view (default: current view).
    #[arg(long, visible_alias = "to", add = ArgValueCompleter::new(complete_view_names))]
    to_view: Option<String>,

    /// Optional tag to limit preview up to.
    #[arg(long)]
    up_to_tag: Option<String>,
}

// Command Implementation

impl crate::commands::Command for Insert {
    fn run(&self) -> CliResult<()> {
        let repo_path = self.repository.as_ref().map(std::path::Path::new);
        let repo = require_repository(repo_path)?;

        match &self.command {
            Some(InsertSubcommand::View(args)) => run_view_insert(&repo, args),
            Some(InsertSubcommand::Tag(args)) => run_tag(&repo, args),
            Some(InsertSubcommand::Change(args)) => run_change_insert(&repo, args),
            Some(InsertSubcommand::Preview(args)) => run_preview(&repo, args),
            None => {
                if let Some(ref change_str) = self.change {
                    // Insert a single change into the current (or --view) view.
                    run_single_insert(&repo, change_str, self)
                } else {
                    // No change and no subcommand: promote the current view's
                    // changes into its parent view.
                    run_promote_to_parent(&repo, self)
                }
            }
        }
    }
}

// Subcommand Implementations

/// Insert a single change by hash.
fn run_single_insert(repo: &Repository, change_str: &str, args: &Insert) -> CliResult<()> {
    let hash = parse_change_hash(repo, change_str)?;
    let is_current_view = args.view.is_none() || args.view.as_deref() == Some(repo.current_view());

    let options = InsertOptions::default()
        .apply_deps(args.deps)
        .allow_conflict(args.allow_conflicts);

    let options = if let Some(ref view) = args.view {
        options.view(view)
    } else {
        options
    };

    output::print_info(&format!("Inserting change {}...", format_hash(&hash, true)));

    let outcome = if args.deps {
        repo.insert_change_rec(&hash, options)
    } else {
        repo.insert_change(&hash, options)
    }
    .map_err(|e| CliError::Conflict {
        description: e.to_string(),
    })?;

    print_insert_outcome(
        &outcome.stats.applied_hashes,
        outcome.new_state,
        outcome.has_conflicts,
    );

    // Update working copy if we inserted into the current view
    if is_current_view && !outcome.stats.applied_hashes.is_empty() {
        let output_result = repo.materialize().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to update working copy: {}", e))
        })?;
        output::print_success(&format!(
            "{} files updated, {} directories",
            output_result.files_written, output_result.directories_created
        ));
        print_conflict_summary(repo);
    }

    Ok(())
}

/// Promote the current view's changes into its parent view.
///
/// This is the behavior of a bare `atomic insert` (no change hash and no
/// subcommand). The source is always the current view; the target defaults to
/// the current view's parent but can be overridden with `--to`/`--view`.
///
/// The working copy is intentionally NOT rematerialized: the target view is
/// not checked out, so the current view's on-disk state is unchanged.
fn run_promote_to_parent(repo: &Repository, args: &Insert) -> CliResult<()> {
    let source = repo.current_view().to_string();
    let source_info = repo
        .get_view_info(&source)
        .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?;

    // Resolve the target: explicit --to/--view, else the current view's parent.
    let target = match args.view.as_deref() {
        Some(v) => v.to_string(),
        None => source_info
            .parent_name
            .clone()
            .ok_or_else(|| CliError::InvalidArgument {
                message: format!(
                    "'{source}' is a root view — there is no parent to insert into.\n  \
                     Use 'atomic insert from-view <source>' or pass --to <view> \
                     to choose a target."
                ),
            })?,
    };

    if target == source {
        return Err(CliError::InvalidArgument {
            message: format!(
                "Source and target are the same view ('{source}'). \
                 Pass --to <view> to insert somewhere else."
            ),
        });
    }

    // Figure out what would move so we can show a count and short-circuit.
    let missing = repo
        .get_missing_changes_between(&source, Some(&target))
        .map_err(|e| CliError::Conflict {
            description: e.to_string(),
        })?;

    if missing.is_empty() {
        output::print_success(&format!(
            "Already even with '{target}' — nothing to insert."
        ));
        return Ok(());
    }

    output::print_info(&format!(
        "Inserting {} change(s): {source} → {target}",
        missing.len()
    ));

    // Dry run: list the changes and stop before mutating.
    if args.dry_run {
        println!();
        for (i, hash) in missing.iter().enumerate() {
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
        output::print_info("Dry run: no changes inserted. Re-run without --dry-run to insert.");
        return Ok(());
    }

    // Guard: promoting between two shared views is a bigger deal than landing
    // a draft, so require confirmation unless --confirm was passed.
    let target_info = repo
        .get_view_info(&target)
        .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?;
    let both_shared = source_info.scope.is_shared() && target_info.scope.is_shared();
    if both_shared && !args.confirm {
        let prompt = format!(
            "Insert {} change(s) from shared view '{source}' into shared view '{target}'?",
            missing.len()
        );
        let confirmed = dialoguer::Confirm::new()
            .with_prompt(&prompt)
            .default(false)
            .interact()
            .map_err(|_| CliError::InvalidArgument {
                message: "Refusing to insert between two shared views without \
                          confirmation. Re-run with --confirm to proceed \
                          non-interactively."
                    .to_string(),
            })?;
        if !confirmed {
            output::print_info("Aborted.");
            return Ok(());
        }
    }

    let options = CrossViewInsertOptions::new(&source, &target)
        .with_dependencies(args.deps)
        .allow_conflicts(args.allow_conflicts);

    let outcome = repo
        .insert_from_view(options)
        .map_err(|e| CliError::Conflict {
            description: e.to_string(),
        })?;

    print_cross_view_outcome(&outcome, false);

    Ok(())
}

/// Insert all changes from another view (the `insert view` subcommand).
fn run_view_insert(repo: &Repository, args: &ViewArgs) -> CliResult<()> {
    let to_view = args
        .to_view
        .clone()
        .unwrap_or_else(|| repo.current_view().to_string());
    let is_current_view = to_view == repo.current_view();

    output::print_info(&format!(
        "Inserting changes from '{}' to '{}'...",
        args.from_view, to_view
    ));

    let options = CrossViewInsertOptions::new(&args.from_view, &to_view)
        .with_dependencies(args.deps)
        .allow_conflicts(args.allow_conflicts)
        .dry_run(args.dry_run);

    let outcome = repo
        .insert_from_view(options)
        .map_err(|e| CliError::Conflict {
            description: e.to_string(),
        })?;

    print_cross_view_outcome(&outcome, args.dry_run);

    // Update working copy if we inserted into the current view.
    // Collect affected file paths from the inserted changes and only
    // materialize those, avoiding a full rematerialization of the
    // entire working copy.
    if is_current_view && !args.dry_run && outcome.changes_applied > 0 {
        let spinner = output::create_spinner("Materializing files for view...");

        let mut affected_paths = std::collections::HashSet::new();
        for hash in &outcome.applied_hashes {
            if let Ok(change) = repo.load_change(hash) {
                for op in change.hunks() {
                    if let Some(p) = op.path() {
                        affected_paths.insert(p.to_string());
                    }
                }
            }
        }

        let output_result = if affected_paths.is_empty() {
            // No path info available (e.g. AddRoot-only changes) —
            // fall back to full materialize.
            repo.materialize().map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to update working copy: {}", e))
            })?
        } else {
            repo.materialize_paths(affected_paths).map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to update working copy: {}", e))
            })?
        };

        output::finish_success(
            &spinner,
            &format!(
                "{} files updated, {} directories",
                output_result.files_written, output_result.directories_created
            ),
        );
        print_conflict_summary(repo);
    }

    Ok(())
}

/// Insert changes up to a specific tag.
fn run_tag(repo: &Repository, args: &TagArgs) -> CliResult<()> {
    let from_view = args
        .from_view
        .clone()
        .unwrap_or_else(|| repo.current_view().to_string());
    let to_view = args
        .to_view
        .clone()
        .unwrap_or_else(|| repo.current_view().to_string());
    let is_current_view = to_view == repo.current_view();

    output::print_info(&format!(
        "Inserting changes up to tag '{}' from '{}' to '{}'...",
        args.tag_name, from_view, to_view
    ));

    let options = CrossViewInsertOptions::new(&from_view, &to_view)
        .up_to_tag(&args.tag_name)
        .with_dependencies(args.deps)
        .allow_conflicts(args.allow_conflicts)
        .dry_run(args.dry_run);

    let outcome = repo
        .insert_from_view(options)
        .map_err(|e| CliError::Conflict {
            description: e.to_string(),
        })?;

    print_cross_view_outcome(&outcome, args.dry_run);

    // Update working copy if we inserted into the current view.
    if is_current_view && !args.dry_run && outcome.changes_applied > 0 {
        let spinner = output::create_spinner("Materializing files for view...");

        let mut affected_paths = std::collections::HashSet::new();
        for hash in &outcome.applied_hashes {
            if let Ok(change) = repo.load_change(hash) {
                for op in change.hunks() {
                    if let Some(p) = op.path() {
                        affected_paths.insert(p.to_string());
                    }
                }
            }
        }

        let output_result = if affected_paths.is_empty() {
            repo.materialize().map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to update working copy: {}", e))
            })?
        } else {
            repo.materialize_paths(affected_paths).map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to update working copy: {}", e))
            })?
        };

        output::finish_success(
            &spinner,
            &format!(
                "{} files updated, {} directories",
                output_result.files_written, output_result.directories_created
            ),
        );
        print_conflict_summary(repo);
    }

    Ok(())
}

/// Insert specific change(s) by hash (the `insert change` subcommand).
fn run_change_insert(repo: &Repository, args: &ChangeArgs) -> CliResult<()> {
    let to_view = args
        .to_view
        .clone()
        .unwrap_or_else(|| repo.current_view().to_string());
    let is_current_view = to_view == repo.current_view();

    // Parse all change hashes
    let mut hashes = Vec::new();
    for change_str in &args.changes {
        let hash = parse_change_hash(repo, change_str)?;
        hashes.push(hash);
    }

    output::print_info(&format!(
        "Cherry-picking {} change(s) to '{}'...",
        hashes.len(),
        to_view
    ));

    let outcome =
        repo.cherry_pick(&hashes, "", Some(&to_view))
            .map_err(|e| CliError::Conflict {
                description: e.to_string(),
            })?;

    print_cross_view_outcome(&outcome, false);

    // Update working copy if we inserted into the current view
    if is_current_view && outcome.changes_applied > 0 {
        let output_result = repo.materialize().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to update working copy: {}", e))
        })?;
        output::print_success(&format!(
            "{} files updated, {} directories",
            output_result.files_written, output_result.directories_created
        ));
        print_conflict_summary(repo);
    }

    Ok(())
}

/// Preview what would be inserted.
fn run_preview(repo: &Repository, args: &PreviewArgs) -> CliResult<()> {
    let to_view = args
        .to_view
        .clone()
        .unwrap_or_else(|| repo.current_view().to_string());

    output::print_section("Insert Preview");
    println!();
    println!("  Source view: {}", args.from_view);
    println!("  Target view: {}", to_view);
    if let Some(ref tag) = args.up_to_tag {
        println!("  Up to tag:   {}", tag);
    }
    println!();

    // Get the changes that would be inserted
    let missing = if let Some(ref tag_name) = args.up_to_tag {
        // Get changes up to tag, then filter to missing
        let options = CrossViewInsertOptions::new(&args.from_view, &to_view)
            .up_to_tag(tag_name)
            .dry_run(true);

        let outcome = repo
            .insert_from_view(options)
            .map_err(|e| CliError::Conflict {
                description: e.to_string(),
            })?;

        outcome.applied_hashes
    } else {
        repo.get_missing_changes_between(&args.from_view, Some(&to_view))
            .map_err(|e| CliError::Conflict {
                description: e.to_string(),
            })?
    };

    if missing.is_empty() {
        output::print_success("No changes to insert - target view is up to date.");
    } else {
        println!("Changes that would be inserted ({}):", missing.len());
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
            "Run 'atomic insert view {}' to insert these changes.",
            args.from_view
        ));
    }

    Ok(())
}

// Helper Functions

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

/// Print the outcome of a single insert operation.
fn print_insert_outcome(
    applied: &[Hash],
    new_state: atomic_core::types::Merkle,
    has_conflicts: bool,
) {
    println!();

    if applied.is_empty() {
        output::print_warning("No changes inserted.");
        return;
    }

    output::print_success(&format!("Inserted {} change(s)", applied.len()));

    println!("  New state: {}", new_state.to_base32());

    if has_conflicts {
        println!();
        output::print_warning("Conflicts detected. Run 'atomic conflicts' to see details.");
    }
}

/// After materializing the current view, list any conflicted files inline so
/// the user does not have to run a second command to find them.
fn print_conflict_summary(repo: &Repository) {
    let conflicts = match repo.list_conflicts() {
        Ok(c) => c,
        Err(_) => return,
    };
    if conflicts.is_empty() {
        return;
    }
    println!();
    let file_word = if conflicts.len() == 1 {
        "file"
    } else {
        "files"
    };
    output::print_warning(&format!(
        "{} conflicted {} — resolve markers, then record:",
        conflicts.len(),
        file_word
    ));
    for (path, records) in &conflicts {
        let where_ = records
            .first()
            .and_then(|c| c.line)
            .map(|l| format!(" (line {})", l))
            .unwrap_or_default();
        println!("    {}{}", path, where_);
    }
    output::print_hint("See 'atomic conflicts' for details.");
}

/// Print the outcome of a cross-view insert operation.
fn print_cross_view_outcome(outcome: &CrossViewInsertOutcome, dry_run: bool) {
    println!();

    if dry_run {
        if outcome.applied_hashes.is_empty() {
            output::print_info("Dry run: No changes would be inserted.");
        } else {
            output::print_info(&format!(
                "Dry run: {} change(s) would be inserted",
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
        output::print_success("No changes to insert - already up to date.");
    } else {
        output::print_success(&format!("Inserted {} change(s)", outcome.changes_applied));
        println!("  New state: {}", outcome.new_state.to_base32());

        if !outcome.skipped_hashes.is_empty() {
            println!(
                "  Skipped:   {} (already inserted)",
                outcome.skipped_hashes.len()
            );
        }
    }

    if outcome.has_conflicts {
        println!();
        output::print_warning("Conflicts detected. Run 'atomic conflicts' to see details.");
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_subcommand_variants() {
        // Just verify the enums compile correctly
        let _ = InsertSubcommand::View(ViewArgs {
            from_view: "feature".to_string(),
            to_view: Some("main".to_string()),
            deps: true,
            allow_conflicts: false,
            dry_run: false,
        });

        let _ = InsertSubcommand::Tag(TagArgs {
            tag_name: "v1.0.0".to_string(),
            from_view: Some("feature".to_string()),
            to_view: Some("main".to_string()),
            deps: true,
            allow_conflicts: false,
            dry_run: false,
        });

        let _ = InsertSubcommand::Change(ChangeArgs {
            changes: vec!["abc123".to_string()],
            to_view: None,
            deps: true,
            allow_conflicts: false,
        });

        let _ = InsertSubcommand::Preview(PreviewArgs {
            from_view: "feature".to_string(),
            to_view: None,
            up_to_tag: None,
        });
    }

    #[test]
    fn test_view_args_defaults() {
        let args = ViewArgs {
            from_view: "feature".to_string(),
            to_view: None,
            deps: true,
            allow_conflicts: false,
            dry_run: false,
        };

        assert_eq!(args.from_view, "feature");
        assert!(args.to_view.is_none());
        assert!(args.deps);
        assert!(!args.allow_conflicts);
        assert!(!args.dry_run);
    }

    #[test]
    fn test_tag_args_with_all_options() {
        let args = TagArgs {
            tag_name: "v1.0.0".to_string(),
            from_view: Some("feature".to_string()),
            to_view: Some("main".to_string()),
            deps: false,
            allow_conflicts: true,
            dry_run: true,
        };

        assert_eq!(args.tag_name, "v1.0.0");
        assert_eq!(args.from_view, Some("feature".to_string()));
        assert_eq!(args.to_view, Some("main".to_string()));
        assert!(!args.deps);
        assert!(args.allow_conflicts);
        assert!(args.dry_run);
    }

    #[test]
    fn test_change_args_multiple_changes() {
        let args = ChangeArgs {
            changes: vec![
                "abc123".to_string(),
                "def456".to_string(),
                "ghi789".to_string(),
            ],
            to_view: Some("main".to_string()),
            deps: true,
            allow_conflicts: false,
        };

        assert_eq!(args.changes.len(), 3);
        assert_eq!(args.to_view, Some("main".to_string()));
    }

    #[test]
    fn test_preview_args_minimal() {
        let args = PreviewArgs {
            from_view: "feature".to_string(),
            to_view: None,
            up_to_tag: None,
        };

        assert_eq!(args.from_view, "feature");
        assert!(args.to_view.is_none());
        assert!(args.up_to_tag.is_none());
    }

    #[test]
    fn test_preview_args_with_tag() {
        let args = PreviewArgs {
            from_view: "feature".to_string(),
            to_view: Some("main".to_string()),
            up_to_tag: Some("v1.0.0".to_string()),
        };

        assert_eq!(args.up_to_tag, Some("v1.0.0".to_string()));
    }
}
