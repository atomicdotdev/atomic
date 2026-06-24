//! The `sandbox` command for provisioning concurrent agent workspaces.
//!
//! A sandbox is a private, copy-on-write clone of the repository's working
//! tree. Several agents can each work in their own sandbox at once — isolated
//! build artifacts (`node_modules`, `target`), no collisions — while sharing
//! the single canonical graph. On filesystems that support reflinks (APFS,
//! Btrfs, XFS, ReFS) the clone shares blocks until written, so the disk cost is
//! the delta, not a full copy.
//!
//! The copy-on-write clone is performed in-process by the `atomic` binary
//! itself (via the `reflink-copy` crate) — no external `cp` is invoked.
//!
//! # Usage
//!
//! ```text
//! atomic sandbox create <NAME> [--dest <PATH>] [--view <VIEW>]
//! ```

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use atomic_repository::{Repository, SealOptions, StageOptions};

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Provision and manage concurrent agent sandboxes.
#[derive(Debug, clap::Args)]
#[command(name = "sandbox")]
pub struct Sandbox {
    #[command(subcommand)]
    pub command: SandboxCommands,
}

#[derive(Subcommand, Debug)]
pub enum SandboxCommands {
    /// Create a sandbox: a copy-on-write clone of the working tree.
    Create(Create),

    /// Stage a sandbox as a layered OCI image (shared base + thin delta).
    ///
    /// Built for the inner loop: the base layer is the shared view, the delta
    /// layer is only what changed. Ship the delta to CI / circuit-breaker
    /// workflows while the base is pulled once and cached.
    Stage(Stage),

    /// Seal a view as a flattened, self-contained OCI runtime image.
    ///
    /// Produces a single-layer deployable image of the full merged state —
    /// "run this exact version" anywhere an OCI runtime is available.
    Seal(Seal),
}

impl Command for Sandbox {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            SandboxCommands::Create(cmd) => cmd.run(),
            SandboxCommands::Stage(cmd) => cmd.run(),
            SandboxCommands::Seal(cmd) => cmd.run(),
        }
    }
}

/// Create a sandbox working tree for an agent.
///
/// Clones the current working tree (copy-on-write where supported) into a
/// private directory. The sandbox shares the canonical graph — it is not a
/// separate repository.
#[derive(Parser, Debug)]
pub struct Create {
    /// Name of the sandbox (used in the default destination path).
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Destination directory for the sandbox working tree.
    ///
    /// Defaults to `<repo-parent>/<repo-name>-sandboxes/<name>`.
    #[arg(long, value_name = "PATH")]
    pub dest: Option<PathBuf>,

    /// View the sandbox operates on. Defaults to the current view.
    ///
    /// Mutually exclusive with `--from`.
    #[arg(long, value_name = "VIEW", conflicts_with = "from")]
    pub view: Option<String>,

    /// Create a new draft view (named after the sandbox) from this view, and
    /// point the sandbox at it.
    ///
    /// With `--from dev`, the sandbox gets its own draft view forked from
    /// `dev`, so the agent's records land in that draft rather than a shared
    /// view — isolating each agent's history as well as its files.
    #[arg(long, value_name = "VIEW")]
    pub from: Option<String>,
}

impl Create {
    fn default_dest(repo_root: &std::path::Path, name: &str) -> PathBuf {
        let repo_name = repo_root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".to_string());
        let parent = repo_root.parent().unwrap_or(repo_root);
        parent.join(format!("{repo_name}-sandboxes")).join(name)
    }
}

impl Command for Create {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let mut repo = Repository::open(&root).map_err(CliError::Repository)?;

        let dest = self
            .dest
            .clone()
            .unwrap_or_else(|| Self::default_dest(&root, &self.name));

        if dest.exists() {
            return Err(CliError::InvalidArgument {
                message: format!("destination already exists: {}", dest.display()),
            });
        }

        // Resolve the view this sandbox records into:
        //   --from <v>  → create a new draft named after the sandbox, forked from <v>
        //   --view <v>  → use the existing view <v>
        //   (neither)   → the current view
        let (view, created_draft) = if let Some(from) = &self.from {
            repo.create_view_from(&self.name, from)
                .map_err(CliError::Repository)?;
            (self.name.clone(), true)
        } else {
            let v = self
                .view
                .clone()
                .unwrap_or_else(|| repo.current_view().to_string());
            (v, false)
        };

        let count = repo
            .provision_sandbox(&dest, &view)
            .map_err(CliError::Repository)?;

        println!("Sandbox '{}' created", self.name);
        println!("  Working tree: {}", dest.display());
        if created_draft {
            println!(
                "  View:         {view} (new draft from '{}')",
                self.from.as_deref().unwrap_or("")
            );
        } else {
            println!("  View:         {view}");
        }
        println!("  Files cloned: {count} (copy-on-write where supported)");
        println!(
            "  Graph:        shared (canonical {}/.atomic)",
            root.display()
        );

        Ok(())
    }
}

/// Stage a view as a layered OCI image (base + delta).
#[derive(Parser, Debug)]
pub struct Stage {
    /// The view to stage (the sandbox's draft view).
    #[arg(value_name = "VIEW")]
    pub view: String,

    /// The shared base view the delta is computed against.
    #[arg(long, value_name = "VIEW", default_value = "dev")]
    pub base: String,

    /// Output directory for the OCI image layout.
    #[arg(long, short = 'o', value_name = "DIR")]
    pub out: PathBuf,
}

impl Command for Stage {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let result = repo
            .stage(StageOptions {
                view: self.view.clone(),
                base_view: self.base.clone(),
                out: self.out.clone(),
            })
            .map_err(CliError::Repository)?;

        println!("Staged '{}' (delta over '{}')", self.view, self.base);
        println!("  Image:        {}", result.out.display());
        println!("  Manifest:     {}", result.manifest_digest);
        println!("  Base layer:   {}", result.base_diff_id);
        println!("  Delta files:  {}", result.delta_files);

        Ok(())
    }
}

/// Seal a view as a flattened, self-contained OCI runtime image.
#[derive(Parser, Debug)]
pub struct Seal {
    /// The view to seal.
    #[arg(value_name = "VIEW")]
    pub view: String,

    /// Output directory for the OCI image layout.
    #[arg(long, short = 'o', value_name = "DIR")]
    pub out: PathBuf,

    /// Image entrypoint argv (repeatable), e.g. `--entrypoint /app/run`.
    #[arg(long, value_name = "ARG")]
    pub entrypoint: Vec<String>,

    /// Image environment variable (repeatable), e.g. `--env PORT=8080`.
    #[arg(long, value_name = "KEY=VAL")]
    pub env: Vec<String>,
}

impl Command for Seal {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let result = repo
            .seal(SealOptions {
                view: self.view.clone(),
                out: self.out.clone(),
                entrypoint: self.entrypoint.clone(),
                env: self.env.clone(),
            })
            .map_err(CliError::Repository)?;

        println!("Sealed '{}'", self.view);
        println!("  Image:        {}", result.out.display());
        println!("  Manifest:     {}", result.manifest_digest);
        println!("  Files:        {}", result.files);

        Ok(())
    }
}
