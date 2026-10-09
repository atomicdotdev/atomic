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
//! atomic sandbox open <VIEW> [--acting-as <DID>] [--ttl <SECS>] [--capability <CAP>]... [--dest <DIR>]
//! atomic sandbox renew <VIEW> [--ttl <SECS>]
//! atomic sandbox close <VIEW>
//! atomic sandbox materialize        # inside a remote sandbox
//! ```
//!
//! A **remote** sandbox is a directory holding only a pointer
//! (`.atomic-sandbox`: repository, view, token — no address) for a machine
//! that reaches the repository through whatever serves its daemon socket.
//! `open` mints its grant on the host and prints (or writes) the pointer;
//! `materialize`, run inside it, writes the view's tree and its cache.

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

    /// Grant a remote sandbox one view, and print its pointer.
    ///
    /// The pointer (repository, view, token) goes in an empty directory as
    /// `.atomic-sandbox`; `atomic sandbox materialize` there writes the view.
    Open(Open),

    /// Extend a remote sandbox's grant.
    Renew(Renew),

    /// Revoke a remote sandbox's grant now.
    Close(Close),

    /// Inside a remote sandbox: write its view's tree and make its cache.
    Materialize(Materialize),
}

impl Command for Sandbox {
    fn run(&self) -> CliResult<()> {
        // Route every form through the service layer: the handler runs the
        // same domain calls (view creation, copy-on-write provisioning,
        // OCI stage/seal) and the CLI renders the local reports.
        if let Some((root, _)) = crate::remote_sandbox::current() {
            return match &self.command {
                SandboxCommands::Materialize(cmd) => cmd.run(),
                _ => Err(crate::remote_sandbox::refusal(
                    &root,
                    "only `atomic sandbox materialize` runs inside a remote sandbox",
                )),
            };
        }
        let routed = match &self.command {
            SandboxCommands::Open(cmd) => return cmd.run(),
            SandboxCommands::Renew(cmd) => return cmd.run(),
            SandboxCommands::Close(cmd) => return cmd.run(),
            SandboxCommands::Materialize(cmd) => return cmd.run(),
            SandboxCommands::Create(cmd) => crate::commands::rpc::sandbox_create(cmd)?,
            SandboxCommands::Stage(cmd) => crate::commands::rpc::sandbox_stage(cmd)?,
            SandboxCommands::Seal(cmd) => crate::commands::rpc::sandbox_seal(cmd)?,
        };
        if routed {
            return Ok(());
        }
        match &self.command {
            SandboxCommands::Create(cmd) => cmd.run(),
            SandboxCommands::Stage(cmd) => cmd.run(),
            SandboxCommands::Seal(cmd) => cmd.run(),
            SandboxCommands::Open(_)
            | SandboxCommands::Renew(_)
            | SandboxCommands::Close(_)
            | SandboxCommands::Materialize(_) => unreachable!("handled above"),
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

// ---------------------------------------------------------------------------
// remote sandboxes
// ---------------------------------------------------------------------------

/// What a remote sandbox's grant carries when `--capability` is not given:
/// enough to read and land its view's work and its agent's provenance.
const DEFAULT_CAPABILITIES: &[&str] = &[
    "sandbox.read",
    "sandbox.submit",
    "provenance.read",
    "provenance.write",
    "agent.checkpoint",
    "vault.read",
    "vault.write",
];

fn request_meta() -> Option<atomic_client::proto::RequestMeta> {
    Some(atomic_client::proto::RequestMeta {
        request_id: uuid::Uuid::new_v4().to_string(),
        observed_at: None,
    })
}

fn view_ref(view: &str) -> atomic_client::proto::ViewRef {
    libatomic::daemon::sandbox_wire::view_ref(view)
}

fn expiry(stamp: Option<&prost_types::Timestamp>) -> String {
    stamp
        .and_then(|t| chrono::DateTime::from_timestamp(t.seconds, 0))
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| "-".to_string())
}

/// Grant a remote sandbox one view, and print its pointer.
#[derive(Parser, Debug)]
pub struct Open {
    /// The view the sandbox reaches.
    #[arg(value_name = "VIEW")]
    pub view: String,

    /// The identity the sandbox's work is attributed to (an agent's DID).
    #[arg(long, value_name = "DID")]
    pub acting_as: Option<String>,

    /// How long the grant lasts, in seconds (1..31536000; renew it with
    /// `atomic sandbox renew`).
    #[arg(long, value_name = "SECS", default_value_t = 7200)]
    pub ttl: u64,

    /// A capability to grant (repeatable). Defaults to reading and
    /// submitting the view, its provenance, and its vault.
    #[arg(long = "capability", value_name = "CAP")]
    pub capabilities: Vec<String>,

    /// Write the pointer into this directory (created if needed, mode 0600)
    /// instead of printing it.
    #[arg(long, value_name = "DIR")]
    pub dest: Option<PathBuf>,
}

impl Command for Open {
    fn run(&self) -> CliResult<()> {
        let session = crate::service::Service::open_admin()?;
        let capabilities = if self.capabilities.is_empty() {
            DEFAULT_CAPABILITIES.iter().map(|c| c.to_string()).collect()
        } else {
            self.capabilities.clone()
        };
        let opened = session
            .open_sandbox(atomic_client::proto::OpenSandboxRequest {
                repository: Some(session.reference.clone()),
                meta: request_meta(),
                view: self.view.clone(),
                acting_as_did: self.acting_as.clone(),
                ttl_secs: Some(self.ttl),
                capabilities,
                target: Some(view_ref(&self.view)),
            })?
            .opened
            .ok_or_else(|| CliError::Internal(anyhow::anyhow!("OpenSandbox returned nothing")))?;
        let pointer = crate::remote_sandbox::pointer_from(&opened)?;
        match &self.dest {
            Some(dest) => {
                std::fs::create_dir_all(dest).map_err(CliError::Io)?;
                let path = atomic_repository::write_remote_sandbox_pointer(dest, &pointer)
                    .map_err(CliError::Repository)?;
                println!("Remote sandbox for view '{}' opened", pointer.view);
                println!("  Pointer:      {}", path.display());
                println!("  Expires:      {}", expiry(opened.expires_at.as_ref()));
                println!("  Next:         run `atomic sandbox materialize` there");
            }
            None => println!(
                "{}",
                serde_json::to_string_pretty(&pointer).map_err(|e| CliError::Internal(e.into()))?
            ),
        }
        Ok(())
    }
}

/// Extend a remote sandbox's grant.
#[derive(Parser, Debug)]
pub struct Renew {
    /// The sandbox's view.
    #[arg(value_name = "VIEW")]
    pub view: String,

    /// New lifetime from now, in seconds.
    #[arg(long, value_name = "SECS", default_value_t = 7200)]
    pub ttl: u64,
}

impl Command for Renew {
    fn run(&self) -> CliResult<()> {
        let session = crate::service::Service::open_admin()?;
        let renewed = session.renew_sandbox(atomic_client::proto::RenewSandboxRequest {
            repository: Some(session.reference.clone()),
            meta: request_meta(),
            view: self.view.clone(),
            ttl_secs: self.ttl,
            target: Some(view_ref(&self.view)),
        })?;
        println!(
            "Sandbox grant for '{}' now expires {}",
            self.view,
            expiry(renewed.expires_at.as_ref())
        );
        Ok(())
    }
}

/// Revoke a remote sandbox's grant now.
#[derive(Parser, Debug)]
pub struct Close {
    /// The sandbox's view.
    #[arg(value_name = "VIEW")]
    pub view: String,
}

impl Command for Close {
    fn run(&self) -> CliResult<()> {
        let session = crate::service::Service::open_admin()?;
        let closed = session.close_sandbox(atomic_client::proto::CloseSandboxRequest {
            repository: Some(session.reference.clone()),
            meta: request_meta(),
            view: self.view.clone(),
            target: Some(view_ref(&self.view)),
        })?;
        if closed.revoked {
            println!("Sandbox grant for '{}' revoked", self.view);
        } else {
            println!("No live sandbox grant for '{}'", self.view);
        }
        Ok(())
    }
}

/// Inside a remote sandbox: write its view's tree and make its cache.
#[derive(Parser, Debug)]
pub struct Materialize {}

impl Command for Materialize {
    fn run(&self) -> CliResult<()> {
        let Some((root, _)) = crate::remote_sandbox::current() else {
            return Err(CliError::InvalidArgument {
                message: "not in a remote sandbox: no remote `.atomic-sandbox` pointer here or \
                          above (make one with `atomic sandbox open <view> --dest <dir>` on the \
                          host)"
                    .to_string(),
            });
        };
        let (entries, view) = crate::remote_sandbox::materialize(&root)?;
        println!(
            "Materialized {entries} entries of view '{view}' into {}",
            root.display()
        );
        Ok(())
    }
}
