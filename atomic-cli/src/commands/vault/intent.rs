//! `atomic vault intent` — manage vault intents (tasks).
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
//! # Link a goal to an intent
//! atomic vault intent link PIMO-1 --goal swift-meadow-a3f2
//! ```

use clap::{Parser, Subcommand};

use atomic_repository::{IntentCreateOptions, IntentUpdateOptions, Repository};

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

// Intent Subcommands

/// Subcommands for intent management.
#[derive(Subcommand, Debug)]
pub enum IntentCommands {
    /// Create a new intent.
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

    /// List intents.
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

    /// Show an intent's content.
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

    /// Update an intent's fields.
    ///
    /// Modifies the status, assignee, priority, or title of an existing intent.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault intent update PIMO-1 --status in-progress
    /// atomic vault intent update PIMO-1 --assignee bob --priority critical
    /// ```
    Update(IntentUpdate),

    /// Link a goal to an intent.
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

/// Manage vault intents (tasks).
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
        match &self.command {
            IntentCommands::Create(cmd) => cmd.run(),
            IntentCommands::List(cmd) => cmd.run(),
            IntentCommands::Show(cmd) => cmd.run(),
            IntentCommands::Update(cmd) => cmd.run(),
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

        let result = repo
            .vault_intent_create(IntentCreateOptions {
                title: self.title.clone(),
                priority: self.priority.clone(),
                assignee: self.assignee.clone(),
                labels,
            })
            .map_err(CliError::Repository)?;

        if self.json {
            let json = serde_json::json!({
                "id": result.id,
                "intent_dir": result.intent_dir,
                "intent_file": result.intent_file,
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
}

impl Command for IntentUpdate {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let info = repo
            .vault_intent_update(
                &self.id,
                IntentUpdateOptions {
                    status: self.status.clone(),
                    assignee: self.assignee.clone(),
                    priority: self.priority.clone(),
                    title: self.title.clone(),
                },
            )
            .map_err(CliError::Repository)?;

        println!("Updated intent: {}", info.id);
        println!("  status: {}", info.status);
        println!("  priority: {}", info.priority);
        if let Some(ref assignee) = info.assignee {
            println!("  assignee: {}", assignee);
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
