//! `atomic vault goal` — manage vault goals.
//!
//! Goals represent individual development interactions (e.g., an AI agent
//! turn, a pairing session, or a focused work block). Each goal tracks
//! its developer, linked intent, model, and accumulated tool results.
//!
//! # Usage
//!
//! ```text
//! atomic vault goal <COMMAND>
//!
//! Commands:
//!   start   Start a new goal
//!   stop    Stop the current goal
//!   resume  Resume a suspended goal
//!   list    List goals
//!   show    Show a goal's content
//! ```
//!
//! # Examples
//!
//! ```text
//! # Start a new goal
//! atomic vault goal start --developer alice --model claude-4
//!
//! # Start linked to an intent
//! atomic vault goal start --intent PIMO-42
//!
//! # Stop the active goal
//! atomic vault goal stop
//!
//! # Stop and promote for team review
//! atomic vault goal stop --promote
//!
//! # Resume a suspended goal
//! atomic vault goal resume swift-meadow-a3f2
//!
//! # List all goals
//! atomic vault goal list
//!
//! # List only active goals
//! atomic vault goal list --status active
//!
//! # Show goal content
//! atomic vault goal show swift-meadow-a3f2 --json
//! ```

use clap::{Parser, Subcommand};

use atomic_repository::{GoalStartOptions, GoalStopOptions, Repository};

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

// Goal Subcommands

/// Subcommands for vault goal management.
#[derive(Subcommand, Debug)]
pub enum GoalCommands {
    /// Start a new goal.
    ///
    /// Creates a new vault goal with optional metadata. If no name is
    /// provided, one is generated from the current timestamp.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault goal start
    /// atomic vault goal start --developer alice --model claude-4
    /// atomic vault goal start --intent PIMO-42
    /// ```
    Start(GoalStart),

    /// Stop the current goal.
    ///
    /// Stops an active goal, optionally promoting it for team
    /// consumption or discarding it entirely.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault goal stop
    /// atomic vault goal stop --promote
    /// atomic vault goal stop my-goal --discard
    /// ```
    Stop(GoalStop),

    /// Resume a suspended goal.
    ///
    /// Reactivates a previously suspended goal so work can continue.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault goal resume swift-meadow-a3f2
    /// ```
    Resume(GoalResume),

    /// List goals.
    ///
    /// Shows goals filtered by status. Defaults to showing all goals.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault goal list
    /// atomic vault goal list --status active
    /// atomic vault goal list --json
    /// ```
    List(GoalList),

    /// Show a goal's content.
    ///
    /// Prints the full content of a goal's vault entry.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault goal show swift-meadow-a3f2
    /// atomic vault goal show swift-meadow-a3f2 --json
    /// ```
    Show(GoalShow),
}

// Goal Command (dispatch)

/// Manage vault goals.
///
/// Goals track individual development interactions — AI agent turns,
/// pairing sessions, or focused work blocks.
#[derive(Debug, clap::Args)]
pub struct Goal {
    #[command(subcommand)]
    pub command: GoalCommands,
}

impl Command for Goal {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            GoalCommands::Start(cmd) => cmd.run(),
            GoalCommands::Stop(cmd) => cmd.run(),
            GoalCommands::Resume(cmd) => cmd.run(),
            GoalCommands::List(cmd) => cmd.run(),
            GoalCommands::Show(cmd) => cmd.run(),
        }
    }
}

// goal start

/// Start a new vault goal.
#[derive(Parser, Debug)]
pub struct GoalStart {
    /// Override the generated goal name.
    #[arg(long)]
    pub name: Option<String>,

    /// Developer name.
    #[arg(long)]
    pub developer: Option<String>,

    /// Link to an intent ID.
    #[arg(long)]
    pub intent: Option<String>,

    /// AI model being used.
    #[arg(long)]
    pub model: Option<String>,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for GoalStart {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let result = repo
            .vault_goal_start(GoalStartOptions {
                name: self.name.clone(),
                developer: self.developer.clone(),
                intent: self.intent.clone(),
                model: self.model.clone(),
            })
            .map_err(CliError::Repository)?;

        if self.json {
            let json = serde_json::json!({
                "name": result.name,
                "goal_dir": result.goal_dir,
                "goal_file": result.goal_file,
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else {
            println!("Started goal: {}", result.name);
            println!("  directory: .vault/{}", result.goal_dir);
        }

        Ok(())
    }
}

// goal stop

/// Stop the current vault goal.
#[derive(Parser, Debug)]
pub struct GoalStop {
    /// Goal name to stop (defaults to most recent active goal).
    pub goal: Option<String>,

    /// Mark as completed (promoted for team consumption).
    #[arg(long)]
    pub promote: bool,

    /// Delete the goal entirely.
    #[arg(long)]
    pub discard: bool,
}

impl Command for GoalStop {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        // Find the goal to stop
        let goal_name = match &self.goal {
            Some(name) => name.clone(),
            None => {
                // Find the most recent active goal
                let goals = repo
                    .vault_goal_list(Some("active"))
                    .map_err(CliError::Repository)?;
                goals
                    .first()
                    .map(|s| s.name.clone())
                    .ok_or_else(|| CliError::InvalidArgument {
                        message: "No active goal found. Specify a goal name.".to_string(),
                    })?
            }
        };

        let result = repo
            .vault_goal_stop(
                &goal_name,
                GoalStopOptions {
                    promote: self.promote,
                    discard: self.discard,
                },
            )
            .map_err(CliError::Repository)?;

        match result.status.as_str() {
            "discarded" => println!(
                "Discarded goal: {} ({} files removed)",
                result.name, result.paths_removed
            ),
            "completed" => println!("Completed goal: {}", result.name),
            status => println!("Suspended goal: {} (status: {})", result.name, status),
        }

        Ok(())
    }
}

// goal resume

/// Resume a suspended vault goal.
#[derive(Parser, Debug)]
pub struct GoalResume {
    /// Goal name to resume.
    pub goal: String,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for GoalResume {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let info = repo
            .vault_goal_resume(&self.goal)
            .map_err(CliError::Repository)?;

        if self.json {
            let json = serde_json::json!({
                "name": info.name,
                "developer": info.developer,
                "status": info.status,
                "intent": info.intent,
                "started_at": info.started_at,
                "turns": info.turns,
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else {
            println!("Resumed goal: {}", info.name);
            if !info.developer.is_empty() {
                println!("  developer: {}", info.developer);
            }
            if let Some(ref intent) = info.intent {
                println!("  intent: {}", intent);
            }
        }

        Ok(())
    }
}

// goal list

/// List vault goals.
#[derive(Parser, Debug)]
pub struct GoalList {
    /// Filter by status (active, completed, suspended, all).
    #[arg(long, short = 's', default_value = "all")]
    pub status: String,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for GoalList {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let filter = if self.status == "all" {
            Some("all")
        } else {
            Some(self.status.as_str())
        };
        let goals = repo.vault_goal_list(filter).map_err(CliError::Repository)?;

        if self.json {
            let json: Vec<serde_json::Value> = goals
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "developer": s.developer,
                        "status": s.status,
                        "intent": s.intent,
                        "started_at": s.started_at,
                        "turns": s.turns,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else {
            if goals.is_empty() {
                println!("No goals found.");
                return Ok(());
            }
            for s in &goals {
                let intent_str = s.intent.as_deref().unwrap_or("");
                println!(
                    "  {:12} {:24} {:10} {}",
                    s.status, s.name, s.developer, intent_str
                );
            }
        }

        Ok(())
    }
}

// goal show

/// Show a vault goal's content.
#[derive(Parser, Debug)]
pub struct GoalShow {
    /// Goal name.
    pub goal: String,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for GoalShow {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let entry = repo
            .vault_goal_show(&self.goal)
            .map_err(CliError::Repository)?;

        if self.json {
            let json = serde_json::json!({
                "name": self.goal,
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
