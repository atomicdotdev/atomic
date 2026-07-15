//! `atomic vault intent` — manage vault intents (tasks).
//!
//! DEPRECATED: this command family is superseded by the canonical
//! `atomic intent` commands (`atomic intent new/show/list/validate/
//! attest/verify`), which scaffold the full acceptance-criteria / why /
//! tasks template. `atomic vault intent create` only produces a bare,
//! title-only intent. These commands still work but emit a deprecation
//! warning; prefer `atomic intent` for new work.
//!
//! Intents represent planned work items that can be linked to goals.
//! They track status, priority, assignee, and related goals.
//!
//! # Usage
//!
//! ```text
//! atomic vault intent <COMMAND>
//!
//! Commands:
//!   create  Create a new intent
//!   list    List intents
//!   show    Show an intent's content
//!   update  Update an intent's fields
//!   delete  Delete an unstarted backlog intent
//!   link    Link a goal to an intent
//! ```
//!
//! # Examples
//!
//! ```text
//! # Create an intent
//! atomic vault intent create --title "Fix authentication bug"
//!
//! # Create with priority and assignee
//! atomic vault intent create --title "Add OAuth" -p high --assignee alice
//!
//! # List all intents
//! atomic vault intent list
//!
//! # List only in-progress intents
//! atomic vault intent list -s in-progress
//!
//! # Show intent details
//! atomic vault intent show PIMO-1
//!
//! # Update intent status
//! atomic vault intent update PIMO-1 --status in-progress
//!
//! # Replace the intent body inline
//! atomic vault intent update PIMO-1 --body "# Title\n\n## Problem\n..."
//!
//! # Replace the intent body from a file
//! cat plan.md | atomic vault intent update PIMO-1 --body-stdin
//!
//! # Delete an accidental draft intent
//! atomic vault intent delete PIMO-1
//! atomic vault intent delete PIMO-1 --force
//!
//! # Link a goal to an intent
//! atomic vault intent link PIMO-1 --goal swift-meadow-a3f2
//! ```

use clap::{Parser, Subcommand};

use atomic_agent::turn::session::SessionStore;
use atomic_repository::{IntentCreateOptions, IntentUpdateOptions, Repository};

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Resolve the active agent session for the current view.
///
/// Scans `.atomic/sessions/` for a session whose `view_name` matches
/// the repository's current view. Returns `(session_id, turn_count)`
/// if found.
fn resolve_active_session(repo: &Repository) -> Option<(String, u32)> {
    let sessions_dir = repo.dot_dir().join("sessions");
    let store = SessionStore::new(&sessions_dir).ok()?;
    let current_view = repo.current_view();
    let active = store.find_active().ok()?;
    active
        .into_iter()
        .find(|s| s.view_name == current_view)
        .map(|s| (s.session_id.clone(), s.turn_count))
}

// Intent Subcommands

/// Subcommands for intent management.
#[derive(Subcommand, Debug)]
pub enum IntentCommands {
    /// [DEPRECATED: use `atomic intent new`] Create a new intent.
    ///
    /// Deprecated: `atomic vault intent create` produces a bare,
    /// title-only intent. Use `atomic intent new`, which scaffolds the
    /// full acceptance-criteria / why / tasks template.
    ///
    /// Creates an intent (task) with a title and optional metadata.
    /// Intents can be linked to goals to track which work addresses them.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault intent create --title "Fix auth"
    /// atomic vault intent create --title "Add OAuth" -p high --assignee alice
    /// ```
    Create(IntentCreate),

    /// [DEPRECATED: use `atomic intent list`] List intents.
    ///
    /// Shows intents filtered by status. Defaults to showing all intents.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault intent list
    /// atomic vault intent list -s in-progress
    /// ```
    List(IntentList),

    /// [DEPRECATED: use `atomic intent show`] Show an intent's content.
    ///
    /// Displays the full content of an intent by its ID.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault intent show PIMO-1
    /// atomic vault intent show 1
    /// ```
    Show(IntentShow),

    /// [DEPRECATED: use the `atomic intent` family] Update an intent's fields.
    ///
    /// Modifies the status, assignee, priority, title, or Markdown body
    /// of an existing intent. The body can be set inline with `--body`
    /// or piped via `--body-stdin`.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault intent update PIMO-1 --status in-progress
    /// atomic vault intent update PIMO-1 --assignee bob --priority critical
    /// atomic vault intent update PIMO-1 --body "# Fix auth\n\n## Problem\n..."
    /// cat plan.md | atomic vault intent update PIMO-1 --body-stdin
    /// ```
    Update(IntentUpdate),

    /// [DEPRECATED: use the `atomic intent` family] Delete an unstarted backlog intent.
    ///
    /// Removes an accidental draft intent from the vault. Only backlog
    /// intents with no linked goals can be deleted. Use --force to skip the
    /// confirmation prompt.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault intent delete PIMO-1
    /// atomic vault intent delete PIMO-1 --force
    /// ```
    Delete(IntentDelete),

    /// [DEPRECATED: use the `atomic intent` family] Link a goal to an intent.
    ///
    /// Associates a goal with an intent so that the work done in the goal
    /// is tracked against the intent.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault intent link PIMO-1 --goal swift-meadow-a3f2
    /// ```
    Link(IntentLink),
}

// Intent Command

/// [DEPRECATED: use `atomic intent`] Manage vault intents (tasks).
///
/// Deprecated in favor of the canonical `atomic intent` family
/// (`atomic intent new/show/list/validate/attest/verify`), which
/// scaffolds the full acceptance-criteria / why / tasks template.
///
/// Intents represent planned work items that can be linked to goals.
/// They track status, priority, assignee, and related goals.
#[derive(Debug, clap::Args)]
#[command(name = "intent")]
pub struct Intent {
    #[command(subcommand)]
    pub command: IntentCommands,
}

impl Command for Intent {
    fn run(&self) -> CliResult<()> {
        // Emit a one-time deprecation notice steering users to the
        // canonical `atomic intent` family (Recording the Why), which
        // scaffolds the full acceptance-criteria / why / tasks template.
        let msg = match &self.command {
            IntentCommands::Create(_) => {
                "`atomic vault intent create` is deprecated; use `atomic intent new` instead \
                 (it scaffolds acceptance criteria / why / tasks)."
            }
            IntentCommands::List(_) => {
                "`atomic vault intent list` is deprecated; use `atomic intent list` instead."
            }
            IntentCommands::Show(_) => {
                "`atomic vault intent show` is deprecated; use `atomic intent show` instead."
            }
            IntentCommands::Update(_) => {
                "`atomic vault intent update` is deprecated; use the `atomic intent` family instead."
            }
            IntentCommands::Delete(_) => {
                "`atomic vault intent delete` is deprecated; use the `atomic intent` family instead."
            }
            IntentCommands::Link(_) => {
                "`atomic vault intent link` is deprecated; use the `atomic intent` family instead."
            }
        };
        eprintln!("warning: {msg}");

        match &self.command {
            IntentCommands::Create(cmd) => cmd.run(),
            IntentCommands::List(cmd) => cmd.run(),
            IntentCommands::Show(cmd) => cmd.run(),
            IntentCommands::Update(cmd) => cmd.run(),
            IntentCommands::Delete(cmd) => cmd.run(),
            IntentCommands::Link(cmd) => cmd.run(),
        }
    }
}

// Create Subcommand

/// Create a new intent.
#[derive(Parser, Debug)]
#[command(name = "create")]
pub struct IntentCreate {
    /// Title of the intent.
    #[arg(long)]
    pub title: String,

    /// Priority (low, medium, high, critical).
    #[arg(long, short = 'p')]
    pub priority: Option<String>,

    /// Assignee name.
    #[arg(long)]
    pub assignee: Option<String>,

    /// Labels (comma-separated).
    #[arg(long)]
    pub labels: Option<String>,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for IntentCreate {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let labels: Vec<String> = self
            .labels
            .as_deref()
            .map(|l| l.split(',').map(|s| s.trim().to_string()).collect())
            .unwrap_or_default();

        // Resolve session/turn from the active agent session (if any).
        let (session_id, turn_id) = resolve_active_session(&repo)
            .map(|(sid, tc)| (Some(sid), Some(tc + 1))) // next turn
            .unwrap_or((None, None));

        let result = repo
            .vault_intent_create(IntentCreateOptions {
                title: self.title.clone(),
                priority: self.priority.clone(),
                assignee: self.assignee.clone(),
                labels,
                session_id,
                turn_id,
            })
            .map_err(CliError::Repository)?;

        if self.json {
            let json = serde_json::json!({
                "id": result.id,
                "intent_dir": result.intent_dir,
                "intent_file": result.intent_file,
                "view_name": result.view_name,
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else {
            println!("Created intent: {}", result.id);
            println!("  file: .vault/{}", result.intent_file);
        }

        Ok(())
    }
}

// List Subcommand

/// List intents.
#[derive(Parser, Debug)]
#[command(name = "list")]
pub struct IntentList {
    /// Filter by status (backlog, planned, in-progress, review, done, all).
    #[arg(long, short = 's')]
    pub status: Option<String>,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for IntentList {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let filter = self.status.as_deref();
        let intents = repo
            .vault_intent_list(filter)
            .map_err(CliError::Repository)?;

        if self.json {
            let json: Vec<serde_json::Value> = intents
                .iter()
                .map(|i| {
                    serde_json::json!({
                        "id": i.id,
                        "title": i.title,
                        "status": i.status,
                        "priority": i.priority,
                        "assignee": i.assignee,
                        "goals": i.goals,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else {
            if intents.is_empty() {
                println!("No intents found.");
                return Ok(());
            }
            for i in &intents {
                let assignee = i.assignee.as_deref().unwrap_or("—");
                println!(
                    "  {:10} {:8} {:8} {:16} {}",
                    i.id, i.status, i.priority, assignee, i.title
                );
            }
        }

        Ok(())
    }
}

// Show Subcommand

/// Show an intent's content.
#[derive(Parser, Debug)]
#[command(name = "show")]
pub struct IntentShow {
    /// Intent ID (e.g., "PIMO-1" or just "1").
    pub id: String,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for IntentShow {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let entry = repo
            .vault_intent_show(&self.id)
            .map_err(CliError::Repository)?;

        if self.json {
            let json = serde_json::json!({
                "id": self.id,
                "content": String::from_utf8_lossy(&entry.content_bytes),
                "frontmatter": serde_json::from_str::<serde_json::Value>(
                    &entry.frontmatter_json
                ).unwrap_or_default(),
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else {
            print!("{}", String::from_utf8_lossy(&entry.content_bytes));
        }

        Ok(())
    }
}

// Update Subcommand

/// Update an intent's fields.
#[derive(Parser, Debug)]
#[command(name = "update")]
pub struct IntentUpdate {
    /// Intent ID.
    pub id: String,

    /// New status.
    #[arg(long)]
    pub status: Option<String>,

    /// New assignee.
    #[arg(long)]
    pub assignee: Option<String>,

    /// New priority.
    #[arg(long)]
    pub priority: Option<String>,

    /// New title.
    #[arg(long)]
    pub title: Option<String>,

    /// New Markdown body content, provided inline.
    #[arg(long, conflicts_with = "body_stdin")]
    pub body: Option<String>,

    /// Read the new Markdown body content from standard input.
    #[arg(long)]
    pub body_stdin: bool,

    /// Rewrite the body even if the intent has started or is linked to a goal.
    #[arg(long, short = 'f')]
    pub force: bool,
}

impl IntentUpdate {
    fn resolve_body(&self) -> CliResult<Option<String>> {
        if let Some(ref body) = self.body {
            return Ok(Some(body.clone()));
        }
        if self.body_stdin {
            use std::io::Read;
            let mut content = String::new();
            std::io::stdin()
                .read_to_string(&mut content)
                .map_err(CliError::Io)?;
            return Ok(Some(content));
        }
        Ok(None)
    }
}

impl Command for IntentUpdate {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let content = self.resolve_body()?;
        if self.status.is_none()
            && self.assignee.is_none()
            && self.priority.is_none()
            && self.title.is_none()
            && content.is_none()
        {
            return Err(CliError::InvalidArgument {
                message: "Nothing to update. Provide at least one of \
                    --status, --assignee, --priority, --title, --body, \
                    or --body-stdin."
                    .to_string(),
            });
        }

        let body_updated = content.is_some();
        let info = repo
            .vault_intent_update(
                &self.id,
                IntentUpdateOptions {
                    status: self.status.clone(),
                    assignee: self.assignee.clone(),
                    priority: self.priority.clone(),
                    title: self.title.clone(),
                    content,
                    force: self.force,
                },
            )
            .map_err(CliError::Repository)?;

        println!("Updated intent: {}", info.id);
        println!("  status: {}", info.status);
        println!("  priority: {}", info.priority);
        if let Some(ref assignee) = info.assignee {
            println!("  assignee: {}", assignee);
        }
        if body_updated {
            println!("  body: updated");
        }

        Ok(())
    }
}

// Delete Subcommand

/// Delete an unstarted backlog intent.
///
/// Removes an intent that has not left backlog and has no linked goals. Use this
/// for accidental drafts only; started work should be closed or superseded
/// instead of deleted. Without `--force`, prompts for confirmation.
#[derive(Parser, Debug)]
#[command(name = "delete")]
pub struct IntentDelete {
    /// Intent ID.
    pub id: String,

    /// Skip the confirmation prompt.
    #[arg(long, short = 'f')]
    pub force: bool,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for IntentDelete {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        if !self.force {
            let prompt = format!(
                "Discard intent '{}'? This removes the unstarted draft from the vault.",
                self.id
            );
            let confirmed = dialoguer::Confirm::new()
                .with_prompt(&prompt)
                .default(false)
                .interact()
                .map_err(|e| CliError::Internal(anyhow::anyhow!("Prompt failed: {}", e)))?;

            if !confirmed {
                return Err(CliError::Cancelled);
            }
        }

        let result = repo
            .vault_intent_delete(&self.id)
            .map_err(CliError::Repository)?;

        if self.json {
            let json = serde_json::json!({
                "id": result.id,
                "intent_file": result.intent_file,
                "deleted": true,
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else {
            println!("Deleted intent: {}", result.id);
            println!("  file: .vault/{}", result.intent_file);
        }

        Ok(())
    }
}

// Link Subcommand

/// Link a goal to an intent.
#[derive(Parser, Debug)]
#[command(name = "link")]
pub struct IntentLink {
    /// Intent ID.
    pub id: String,

    /// Goal name to link.
    #[arg(long)]
    pub goal: String,
}

impl Command for IntentLink {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        repo.vault_intent_link(&self.id, &self.goal)
            .map_err(CliError::Repository)?;
        println!("Linked goal '{}' to intent '{}'", self.goal, self.id);

        Ok(())
    }
}
