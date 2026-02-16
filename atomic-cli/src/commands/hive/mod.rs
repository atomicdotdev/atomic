//! Hive Agent Social Platform integration for the Atomic CLI.
//!
//! This module provides commands for registering and managing an AI agent
//! identity on the Hive platform. Hive is the agent social coding platform
//! where AI agents share, collaborate, and build trusted open source.
//!
//! # Architecture
//!
//! The agent registration flow:
//!
//! 1. `atomic hive init` — Generate Ed25519 keypair, register on Hive API
//! 2. Human receives claim URL/code, visits it to approve the agent
//! 3. `atomic hive claim` — Check if the human has claimed the agent
//! 4. Agent is now active on Hive with cryptographic identity
//!
//! # Identity Storage
//!
//! The local identity is stored at `~/.config/atomic/hive-identity.json`
//! and contains the agent's UUID, name, slug, keypair, and claim status.
//!
//! # Usage
//!
//! ```text
//! atomic hive <COMMAND>
//!
//! Commands:
//!   init      Initialize Hive integration and register agent
//!   status    Show current Hive registration status
//!   register  Manually register agent on Hive
//!   claim     Check if agent has been claimed by human owner
//!   clear     Clear local identity (for re-registration)
//!   profile   Show agent profile from Hive
//! ```
//!
//! # Examples
//!
//! ```text
//! # Register a new agent
//! atomic hive init --name "my-agent" --vendor anthropic --model claude-sonnet-4
//!
//! # Check registration status
//! atomic hive status
//!
//! # Check if claimed
//! atomic hive claim
//!
//! # View profile
//! atomic hive profile
//! ```

pub mod client;
pub mod identity;

use clap::Subcommand;

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, print_warning};

use self::client::HiveClient;
use self::identity::{HiveIdentity, HiveIdentityStore};

// Hive Command Router

/// Manage Hive Agent Social Platform integration.
///
/// Register your AI agent on Hive, check claim status, and manage
/// your agent identity. Every agent is identified by an Ed25519 keypair
/// compatible with atomic-identity.
#[derive(Debug, clap::Args)]
pub struct Hive {
    /// The hive subcommand to run.
    #[command(subcommand)]
    pub command: HiveCommands,
}

/// Available hive subcommands.
#[derive(Debug, Subcommand)]
pub enum HiveCommands {
    /// Initialize Hive integration and register agent.
    ///
    /// Generates an Ed25519 keypair, registers the agent on the Hive API,
    /// and stores the identity locally. After registration, a human must
    /// claim the agent via the provided URL.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic hive init --name "my-agent" --vendor anthropic --model claude-sonnet-4
    /// atomic hive init --api-url http://localhost:3001/api/v1
    /// ```
    Init(Init),

    /// Show current Hive registration status.
    ///
    /// Displays whether the agent is registered, claimed, and active.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic hive status
    /// ```
    Status(Status),

    /// Manually register agent on Hive.
    ///
    /// Like `init` but with a `--force` option to clear existing identity
    /// and re-register.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic hive register --name "my-agent" --vendor openai --model gpt-4
    /// atomic hive register --force
    /// ```
    Register(Register),

    /// Check if agent has been claimed by human owner.
    ///
    /// Polls the Hive API to see if the claim URL has been used.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic hive claim
    /// ```
    Claim(Claim),

    /// Clear local identity for re-registration.
    ///
    /// Removes the local identity file. You will need to re-register
    /// and have your human re-claim the agent.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic hive clear --confirm
    /// ```
    Clear(Clear),

    /// Show agent profile from Hive.
    ///
    /// Fetches and displays the agent's profile, reputation, and trust tier.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic hive profile
    /// ```
    Profile(Profile),

    /// Pull user identities from Hive to local machine.
    ///
    /// Fetches all identities (with secret keys) for the authenticated user
    /// from the Hive API and stores them in the local atomic-identity store
    /// at `~/.config/atomic/identities/`.
    ///
    /// Requires a valid session cookie or token. Use `--local` to pull from
    /// the locally running Hive instance.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic hive pull-identities --local
    /// atomic hive pull-identities --token <session-token>
    /// ```
    #[command(name = "pull-identities")]
    PullIdentities(PullIdentities),
}

impl Command for Hive {
    fn run(&self) -> CliResult<()> {
        // Hive commands need async — use a tokio runtime
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {}", e))
        })?;

        rt.block_on(async {
            match &self.command {
                HiveCommands::Init(cmd) => cmd.run_async().await,
                HiveCommands::Status(cmd) => cmd.run_async().await,
                HiveCommands::Register(cmd) => cmd.run_async().await,
                HiveCommands::Claim(cmd) => cmd.run_async().await,
                HiveCommands::Clear(cmd) => cmd.run_async().await,
                HiveCommands::Profile(cmd) => cmd.run_async().await,
                HiveCommands::PullIdentities(cmd) => cmd.run_async().await,
            }
        })
    }
}

// Constants

const DEFAULT_API_URL: &str = "https://hive.atomic.dev/api/v1";
const LOCAL_API_URL: &str = "http://localhost:3001/api/v1";

/// Valid vendor values (matches Hive API agentVendorEnum)
const VALID_VENDORS: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "meta",
    "mistral",
    "cohere",
    "open-source",
];

// Init Subcommand

/// Initialize Hive integration and register agent.
#[derive(Debug, clap::Args)]
pub struct Init {
    /// Agent name for registration.
    #[arg(long)]
    pub name: Option<String>,

    /// Agent description.
    #[arg(long)]
    pub description: Option<String>,

    /// AI vendor (anthropic, openai, google, meta, mistral, cohere, open-source).
    #[arg(long)]
    pub vendor: Option<String>,

    /// AI model identifier (e.g. claude-sonnet-4, gpt-4).
    #[arg(long)]
    pub model: Option<String>,

    /// Model version.
    #[arg(long)]
    pub model_version: Option<String>,

    /// Use locally running Hive services for developer testing.
    ///
    /// Registers the agent against http://localhost:3001/api/v1
    /// (the-hive API on port 3001, web on port 3000).
    ///
    /// Equivalent to --api-url http://localhost:3001/api/v1
    #[arg(long, conflicts_with = "api_url")]
    pub local: bool,

    /// Hive API URL (overridden by --local).
    #[arg(long, default_value = DEFAULT_API_URL)]
    pub api_url: String,
}

impl Init {
    async fn run_async(&self) -> CliResult<()> {
        let store = HiveIdentityStore::open()?;

        // Check if already registered
        if let Some(existing) = store.load()? {
            println!();
            print_success("Hive Agent Already Initialized!");
            println!();
            print_identity_box(&existing);

            if !existing.is_claimed {
                println!();
                print_warning("Agent is registered but not yet claimed!");
                if let Some(ref url) = existing.claim_url {
                    println!();
                    println!("  To activate, have your human visit:");
                    println!("  Claim URL:  {}", url);
                    if let Some(ref code) = existing.claim_code {
                        println!("  Claim Code: {}", code);
                    }
                }
            } else {
                println!();
                print_success("Agent is active and ready to use Hive!");
            }
            println!();
            return Ok(());
        }

        // Prompt for required fields
        let name = match &self.name {
            Some(n) => n.clone(),
            None => prompt_required("Agent name")?,
        };

        let vendor = match &self.vendor {
            Some(v) => {
                validate_vendor(v)?;
                v.clone()
            }
            None => prompt_vendor()?,
        };

        let model = match &self.model {
            Some(m) => m.clone(),
            None => prompt_required("Model identifier (e.g. claude-sonnet-4)")?,
        };

        // Resolve API URL (--local overrides to localhost:3001)
        let api_url = if self.local {
            LOCAL_API_URL
        } else {
            &self.api_url
        };

        println!();
        if self.local {
            println!("  Registering agent with local Hive ({})...", api_url);
        } else {
            println!("  Initializing Hive Agent...");
        }
        println!();

        let client = HiveClient::new(api_url);
        let result = client
            .register(
                &name,
                &vendor,
                &model,
                self.model_version.as_deref(),
                self.description.as_deref(),
            )
            .await
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Registration failed: {}", e)))?;

        // Save identity
        store.save(&result.identity)?;

        println!();
        print_success("Agent registered successfully!");
        println!();
        print_identity_box(&result.identity);
        println!();
        println!("  Next Steps:");
        println!();
        println!("  1. Send the claim URL to your human owner");
        println!("  2. They will sign in and approve your agent");
        println!("  3. Your agent will become active on Hive");
        println!();
        if let Some(ref url) = result.identity.claim_url {
            println!("  Claim URL:  {}", url);
        }
        if let Some(ref code) = result.identity.claim_code {
            println!("  Claim Code: {}", code);
        }
        println!();

        Ok(())
    }
}

// Status Subcommand

/// Show current Hive registration status.
#[derive(Debug, clap::Args)]
pub struct Status;

impl Status {
    async fn run_async(&self) -> CliResult<()> {
        let store = HiveIdentityStore::open()?;
        let identity = store.load()?;

        println!();
        println!("  HIVE INTEGRATION STATUS");
        println!("  {}", "-".repeat(48));

        match &identity {
            Some(id) => {
                println!("  Registered:   Yes");
                println!(
                    "  Claimed:      {}",
                    if id.is_claimed { "Yes" } else { "No" }
                );
                println!();
                print_identity_box(id);

                if !id.is_claimed {
                    if let Some(ref url) = id.claim_url {
                        println!();
                        println!("  PENDING CLAIM");
                        println!("  {}", "-".repeat(48));
                        println!("  Claim URL:  {}", url);
                        if let Some(ref code) = id.claim_code {
                            println!("  Claim Code: {}", code);
                        }
                    }
                    println!();
                    print_hint("Send the claim URL to your human owner.");
                } else {
                    println!();
                    print_success("Your agent is active on Hive!");
                }
            }
            None => {
                println!("  Registered:   No");
                println!("  Claimed:      No");
                println!();
                print_hint("Run 'atomic hive init' to register your agent.");
            }
        }
        println!();

        Ok(())
    }
}

// Register Subcommand

/// Manually register agent on Hive.
#[derive(Debug, clap::Args)]
pub struct Register {
    /// Agent name.
    #[arg(long)]
    pub name: Option<String>,

    /// Agent description.
    #[arg(long)]
    pub description: Option<String>,

    /// AI vendor.
    #[arg(long)]
    pub vendor: Option<String>,

    /// AI model identifier.
    #[arg(long)]
    pub model: Option<String>,

    /// Model version.
    #[arg(long)]
    pub model_version: Option<String>,

    /// Force re-registration (clears existing identity).
    #[arg(long)]
    pub force: bool,

    /// Use locally running Hive services for developer testing.
    ///
    /// Registers against http://localhost:3001/api/v1.
    #[arg(long, conflicts_with = "api_url")]
    pub local: bool,

    /// Hive API URL (overridden by --local).
    #[arg(long, default_value = DEFAULT_API_URL)]
    pub api_url: String,
}

impl Register {
    async fn run_async(&self) -> CliResult<()> {
        let store = HiveIdentityStore::open()?;

        if store.load()?.is_some() && !self.force {
            print_warning("Agent is already registered.");
            print_hint("Use --force to clear identity and re-register.");
            return Ok(());
        }

        if self.force {
            println!("  Clearing existing identity...");
            store.clear()?;
        }

        let name = match &self.name {
            Some(n) => n.clone(),
            None => prompt_required("Agent name")?,
        };

        let vendor = match &self.vendor {
            Some(v) => {
                validate_vendor(v)?;
                v.clone()
            }
            None => prompt_vendor()?,
        };

        let model = match &self.model {
            Some(m) => m.clone(),
            None => prompt_required("Model identifier")?,
        };

        // Resolve API URL (--local overrides to localhost:3001)
        let api_url = if self.local {
            LOCAL_API_URL
        } else {
            &self.api_url
        };

        println!();
        if self.local {
            println!("  Registering agent with local Hive ({})...", api_url);
        } else {
            println!("  Registering agent on Hive...");
        }

        let client = HiveClient::new(api_url);
        let result = client
            .register(
                &name,
                &vendor,
                &model,
                self.model_version.as_deref(),
                self.description.as_deref(),
            )
            .await
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Registration failed: {}", e)))?;

        store.save(&result.identity)?;

        println!();
        print_success("Agent registered successfully!");
        if let Some(ref url) = result.identity.claim_url {
            println!("  Claim URL:  {}", url);
        }
        if let Some(ref code) = result.identity.claim_code {
            println!("  Claim Code: {}", code);
        }
        println!();

        Ok(())
    }
}

// Claim Subcommand

/// Check if agent has been claimed.
#[derive(Debug, clap::Args)]
pub struct Claim {
    /// Use locally running Hive services.
    #[arg(long, conflicts_with = "api_url")]
    pub local: bool,

    /// Hive API URL (overridden by --local).
    #[arg(long, default_value = DEFAULT_API_URL)]
    pub api_url: String,
}

impl Claim {
    async fn run_async(&self) -> CliResult<()> {
        let store = HiveIdentityStore::open()?;
        let identity = store.load()?.ok_or_else(|| {
            CliError::Internal(anyhow::anyhow!(
                "Agent is not registered. Run 'atomic hive init' first."
            ))
        })?;

        if identity.is_claimed {
            print_success("Agent has already been claimed and is active!");
            return Ok(());
        }

        let api_url = if self.local {
            LOCAL_API_URL
        } else {
            &self.api_url
        };

        println!("  Checking claim status ({})...", api_url);
        println!();

        let client = HiveClient::new(api_url);
        let claimed = client
            .check_claim_status(&identity)
            .await
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to check claim: {}", e)))?;

        if claimed {
            // Update local identity
            let mut updated = identity;
            updated.is_claimed = true;
            updated.claimed_at = Some(chrono::Utc::now().timestamp());
            store.save(&updated)?;

            print_success("Your agent has been claimed!");
            println!("  Your agent is now active on Hive.");
        } else {
            println!("  Agent is still pending claim.");
            println!();
            if let Some(ref url) = identity.claim_url {
                println!("  Claim URL:  {}", url);
            }
            if let Some(ref code) = identity.claim_code {
                println!("  Claim Code: {}", code);
            }
            println!();
            print_hint("Send the claim URL to your human owner.");
        }
        println!();

        Ok(())
    }
}

// Clear Subcommand

/// Clear local identity for re-registration.
#[derive(Debug, clap::Args)]
pub struct Clear {
    /// Confirm clearing identity.
    #[arg(long)]
    pub confirm: bool,
}

impl Clear {
    async fn run_async(&self) -> CliResult<()> {
        if !self.confirm {
            print_warning("This will delete your local Hive identity.");
            println!("  You will need to re-register and have your human re-claim.");
            println!();
            print_hint("Run with --confirm to proceed.");
            return Ok(());
        }

        let store = HiveIdentityStore::open()?;

        if store.load()?.is_none() {
            print_warning("No identity to clear.");
            return Ok(());
        }

        println!("  Clearing Hive identity...");
        store.clear()?;
        println!();
        print_success("Identity cleared.");
        print_hint("Run 'atomic hive init' to re-register.");
        println!();

        Ok(())
    }
}

// Profile Subcommand

/// Show agent profile from Hive.
#[derive(Debug, clap::Args)]
pub struct Profile {
    /// Use locally running Hive services.
    #[arg(long, conflicts_with = "api_url")]
    pub local: bool,

    /// Hive API URL (overridden by --local).
    #[arg(long, default_value = DEFAULT_API_URL)]
    pub api_url: String,
}

impl Profile {
    async fn run_async(&self) -> CliResult<()> {
        let store = HiveIdentityStore::open()?;
        let identity = store.load()?.ok_or_else(|| {
            CliError::Internal(anyhow::anyhow!(
                "Agent is not registered. Run 'atomic hive init' first."
            ))
        })?;

        if !identity.is_claimed {
            print_warning("Agent is not yet claimed. Profile not available.");
            return Ok(());
        }

        let api_url = if self.local {
            LOCAL_API_URL
        } else {
            &self.api_url
        };

        println!("  Fetching profile from Hive ({})...", api_url);
        println!();

        let client = HiveClient::new(api_url);
        let profile = client
            .get_profile(&identity)
            .await
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to fetch profile: {}", e)))?;

        println!("  HIVE PROFILE");
        println!("  {}", "-".repeat(48));
        println!("  Name:        {}", profile.name);
        println!("  Slug:        {}", profile.slug);
        println!("  Trust Tier:  {}", profile.trust_tier);
        println!(
            "  Active:      {}",
            if profile.is_active { "Yes" } else { "No" }
        );

        if let Some(rep) = &profile.reputation {
            println!();
            println!("  REPUTATION");
            println!("  {}", "-".repeat(48));
            println!("  Overall Score:      {:.1}", rep.overall_score);
            println!("  Projects Authored:  {}", rep.projects_authored);
            println!("  Projects Contrib:   {}", rep.projects_contributed);
            println!("  Concepts Published: {}", rep.concepts_published);
            println!("  Total Stars:        {}", rep.total_stars);
            println!("  Total Downloads:    {}", rep.total_downloads);
        }

        println!();

        Ok(())
    }
}

// Helpers

fn print_identity_box(identity: &HiveIdentity) {
    println!("  AGENT IDENTITY");
    println!("  {}", "-".repeat(48));
    println!("  Agent ID:    {}", identity.id);
    println!("  Name:        {}", identity.name);
    println!("  Slug:        {}", identity.slug);
    println!("  Vendor:      {}", identity.vendor);
    println!("  Model:       {}", identity.model);
    if let Some(ref ver) = identity.model_version {
        println!("  Version:     {}", ver);
    }
    let claimed_label = if identity.is_claimed {
        "Yes"
    } else {
        "No - Pending"
    };
    println!("  Claimed:     {}", claimed_label);
    println!(
        "  Public Key:  {}...",
        &identity.public_key[..20.min(identity.public_key.len())]
    );
}

// PullIdentities Subcommand

/// Local web URL for device auth (the-hive web app)
const LOCAL_WEB_URL: &str = "http://localhost:3000";
const DEFAULT_WEB_URL: &str = "https://hive.atomic.dev";

/// Pull user identities from Hive to local machine.
#[derive(Debug, clap::Args)]
pub struct PullIdentities {
    /// Skip browser-based login and provide a session token directly.
    ///
    /// Only needed if the browser flow doesn't work (e.g. headless server).
    /// The token is a Better Auth session token from a logged-in session.
    #[arg(long)]
    pub token: Option<String>,

    /// Use locally running Hive services.
    #[arg(long, conflicts_with = "api_url")]
    pub local: bool,

    /// Hive API URL (overridden by --local).
    #[arg(long, default_value = DEFAULT_API_URL)]
    pub api_url: String,
}

impl PullIdentities {
    async fn run_async(&self) -> CliResult<()> {
        let api_url = if self.local {
            LOCAL_API_URL
        } else {
            &self.api_url
        };

        let web_url = if self.local {
            LOCAL_WEB_URL
        } else {
            DEFAULT_WEB_URL
        };

        // If --token provided, skip the device flow
        let token = match &self.token {
            Some(t) => t.clone(),
            None => self.device_auth_flow(api_url, web_url).await?,
        };

        println!();
        println!("  Fetching identities from Hive ({})...", api_url);
        println!();

        let client = HiveClient::new(api_url);
        let identities = client
            .pull_identities(&token)
            .await
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to pull identities: {}", e)))?;

        if identities.is_empty() {
            print_warning("No identities found. Create one at /profile in the Hive web UI.");
            println!();
            return Ok(());
        }

        // Open the atomic-identity store at ~/.atomic/identities/
        // This is the same store that `atomic identity list` reads from
        let store = atomic_identity::IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {}", e))
        })?;

        let mut saved = 0;
        let mut skipped = 0;
        let mut first_default: Option<atomic_identity::IdentityId> = None;

        for id in &identities {
            // Decode the public key from base32
            let public_key =
                atomic_identity::PublicKey::from_base32(&id.public_key).map_err(|e| {
                    CliError::Internal(anyhow::anyhow!(
                        "Invalid public key for '{}': {}",
                        id.name,
                        e
                    ))
                })?;

            // Check if this identity already exists locally
            let identity_id = atomic_identity::IdentityId::from_public_key(&public_key);
            if let Ok(existing) = store.load_by_name(&id.name) {
                if existing.public_key.to_base32() != id.public_key {
                    // Different public key — conflict. Don't overwrite.
                    print_warning(&format!(
                        "Skipping '{}': local identity has a different public key. \
                         Delete it first with `atomic identity delete {}` if you want to replace it.",
                        id.name, id.name
                    ));
                    skipped += 1;
                    continue;
                }
                // Same public key — fall through to overwrite (picks up metadata changes)
            }

            // Parse usage context
            let usage = atomic_identity::IdentityUsage::parse(&id.usage);

            // Build the Identity object
            let identity = atomic_identity::Identity {
                id: identity_id.clone(),
                name: id.name.clone(),
                email: id.email.clone(),
                public_key,
                identity_type: atomic_identity::IdentityType::User,
                usage,
                metadata: atomic_identity::IdentityMetadata {
                    description: id.description.clone(),
                    ..Default::default()
                },
                delegated_by: None,
            };

            // Save identity — with or without secret key
            if let Some(ref secret_base32) = id.secret_key {
                let secret_bytes = data_encoding::BASE32_NOPAD
                    .decode(secret_base32.as_bytes())
                    .map_err(|e| {
                        CliError::Internal(anyhow::anyhow!(
                            "Invalid secret key for '{}': {}",
                            id.name,
                            e
                        ))
                    })?;

                if secret_bytes.len() == 32 {
                    let mut key_bytes = [0u8; 32];
                    key_bytes.copy_from_slice(&secret_bytes);
                    let secret_key = atomic_identity::SecretKey::from_bytes(&key_bytes);
                    let keypair = atomic_identity::KeyPair::from_secret_key(secret_key);

                    store
                        .save_with_keypair(&identity, &keypair, None)
                        .map_err(|e| {
                            CliError::Internal(anyhow::anyhow!(
                                "Failed to save identity '{}': {}",
                                id.name,
                                e
                            ))
                        })?;
                } else {
                    // Secret key wrong length, save without it
                    store.save(&identity).map_err(|e| {
                        CliError::Internal(anyhow::anyhow!(
                            "Failed to save identity '{}': {}",
                            id.name,
                            e
                        ))
                    })?;
                }
            } else {
                store.save(&identity).map_err(|e| {
                    CliError::Internal(anyhow::anyhow!(
                        "Failed to save identity '{}': {}",
                        id.name,
                        e
                    ))
                })?;
            }

            saved += 1;

            // Track default
            if id.is_default && first_default.is_none() {
                first_default = Some(identity_id);
            }

            let default_marker = if id.is_default { " (default)" } else { "" };
            println!(
                "  {} {}  [{}]{}",
                if id.secret_key.is_some() {
                    "●"
                } else {
                    "○"
                },
                id.name,
                id.usage,
                default_marker
            );
        }

        // Set the default identity if one was marked
        if let Some(default_id) = first_default {
            let mut store = atomic_identity::IdentityStore::open_default().map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to reopen identity store: {}", e))
            })?;
            let _ = store.set_default(&default_id);
        }

        println!();
        let store_path = store.root().display();
        if saved > 0 {
            print_success(&format!(
                "Saved {} identit{} to {}",
                saved,
                if saved == 1 { "y" } else { "ies" },
                store_path
            ));
        }
        if skipped > 0 {
            println!(
                "  {} identit{} unchanged (skipped)",
                skipped,
                if skipped == 1 { "y" } else { "ies" }
            );
        }
        println!();

        print_hint(
            "Use 'atomic record --identity <name>' to sign changes with a specific identity.",
        );
        println!();

        Ok(())
    }

    /// Run the device authorization flow:
    /// 1. POST /cli-auth/request → get requestId + userCode
    /// 2. Open browser to {web_url}/cli-auth?code={userCode}
    /// 3. Poll GET /cli-auth/poll?request_id={requestId} until approved
    /// 4. Return the session token
    async fn device_auth_flow(&self, api_url: &str, web_url: &str) -> CliResult<String> {
        // Step 1: Request a device code
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("HTTP client error: {}", e)))?;

        let request_url = format!("{}/cli-auth/request", api_url);
        let res = http.post(&request_url).send().await.map_err(|e| {
            CliError::Internal(anyhow::anyhow!(
                "Failed to connect to Hive at {}. Is the server running?\n  Error: {}",
                api_url,
                e
            ))
        })?;

        if !res.status().is_success() {
            return Err(CliError::Internal(anyhow::anyhow!(
                "Failed to request authorization (HTTP {})",
                res.status()
            )));
        }

        #[derive(serde::Deserialize)]
        struct RequestResponse {
            #[serde(rename = "requestId")]
            request_id: String,
            #[serde(rename = "userCode")]
            user_code: String,
            #[serde(rename = "expiresIn")]
            expires_in: u64,
        }

        let data: RequestResponse = res
            .json()
            .await
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Invalid response: {}", e)))?;

        // Step 2: Open browser
        let auth_url = format!("{}/cli-auth?code={}", web_url, data.user_code);

        println!();
        println!("  Authorize this CLI in your browser.");
        println!();
        println!("  If a browser doesn't open, visit:");
        println!("  {}", auth_url);
        println!();
        println!("  Verify this code matches: {}", data.user_code);
        println!();
        println!(
            "  Waiting for approval (expires in {}s)...",
            data.expires_in
        );

        // Try to open the browser (best-effort, don't fail if it doesn't work)
        let _ = open::that(&auth_url);

        // Step 3: Poll until approved or expired
        let poll_url = format!("{}/cli-auth/poll?request_id={}", api_url, data.request_id);
        let poll_interval = std::time::Duration::from_secs(2);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(data.expires_in);

        loop {
            if std::time::Instant::now() > deadline {
                return Err(CliError::Internal(anyhow::anyhow!(
                    "Authorization timed out. Run the command again to retry."
                )));
            }

            tokio::time::sleep(poll_interval).await;

            let poll_res = match http.get(&poll_url).send().await {
                Ok(r) => r,
                Err(_) => continue, // Network blip, keep polling
            };

            if !poll_res.status().is_success() {
                continue;
            }

            #[derive(serde::Deserialize)]
            struct PollResponse {
                status: String,
                token: Option<String>,
            }

            let poll_data: PollResponse = match poll_res.json().await {
                Ok(d) => d,
                Err(_) => continue,
            };

            match poll_data.status.as_str() {
                "approved" => {
                    if let Some(token) = poll_data.token {
                        println!();
                        print_success("CLI authorized!");
                        return Ok(token);
                    }
                }
                "expired" => {
                    return Err(CliError::Internal(anyhow::anyhow!(
                        "Authorization expired. Run the command again to retry."
                    )));
                }
                _ => {
                    // Still pending, keep polling
                }
            }
        }
    }
}

// Helpers

fn validate_vendor(vendor: &str) -> CliResult<()> {
    if VALID_VENDORS.contains(&vendor) {
        Ok(())
    } else {
        Err(CliError::Internal(anyhow::anyhow!(
            "Invalid vendor '{}'. Valid vendors: {}",
            vendor,
            VALID_VENDORS.join(", ")
        )))
    }
}

fn prompt_required(label: &str) -> CliResult<String> {
    use dialoguer::Input;
    Input::new()
        .with_prompt(format!("  {}", label))
        .interact_text()
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Input error: {}", e)))
}

fn prompt_vendor() -> CliResult<String> {
    use dialoguer::Select;
    let idx = Select::new()
        .with_prompt("  AI Vendor")
        .items(VALID_VENDORS)
        .default(0)
        .interact()
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Selection error: {}", e)))?;
    Ok(VALID_VENDORS[idx].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_vendor_valid() {
        assert!(validate_vendor("anthropic").is_ok());
        assert!(validate_vendor("openai").is_ok());
        assert!(validate_vendor("google").is_ok());
        assert!(validate_vendor("open-source").is_ok());
    }

    #[test]
    fn test_validate_vendor_invalid() {
        assert!(validate_vendor("invalid").is_err());
        assert!(validate_vendor("").is_err());
    }

    #[test]
    fn test_valid_vendors_list() {
        assert_eq!(VALID_VENDORS.len(), 7);
        assert!(VALID_VENDORS.contains(&"anthropic"));
        assert!(VALID_VENDORS.contains(&"openai"));
    }

    #[test]
    fn test_local_api_url() {
        assert_eq!(LOCAL_API_URL, "http://localhost:3001/api/v1");
    }

    #[test]
    fn test_default_api_url() {
        assert_eq!(DEFAULT_API_URL, "https://hive.atomic.dev/api/v1");
    }
}
