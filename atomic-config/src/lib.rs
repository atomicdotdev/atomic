//! Configuration management for Atomic VCS
//!
//! This crate handles loading, saving, and managing configuration for Atomic
//! repositories at three levels:
//!
//! 1. **System** - `/etc/atomic/config.toml` (Unix) or system-wide location
//! 2. **User** - `~/.atomic/config.toml` in the user's home directory
//! 3. **Repository** - `.atomic/config.toml` in the repository root
//!
//! Configuration is merged with later levels overriding earlier ones.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use thiserror::Error;

/// Configuration errors
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Failed to read configuration file: {path}")]
    ReadError {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Failed to parse configuration: {0}")]
    ParseError(#[from] toml::de::Error),

    #[error("Failed to serialize configuration: {0}")]
    SerializeError(#[from] toml::ser::Error),

    #[error("Failed to write configuration file: {path}")]
    WriteError {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Could not determine configuration directory")]
    NoConfigDir,
}

/// Author information for changes
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Author {
    /// Display name
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,

    /// Email address
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,

    /// Identity key reference
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
}

/// Configuration for remote atomic-storage server connection.
///
/// Set during `atomic identity register` and used by management commands
/// (workspace, project, org, team) to connect to the server.
///
/// ```toml
/// [server]
/// url = "https://atomic.storage"
/// default_org = "alice"
///
/// [server.default_workspaces]
/// alice = "personal"
/// acme = "backend"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerConfig {
    /// Base URL of the atomic-storage server.
    ///
    /// The management API constructs org-scoped URLs as
    /// `https://{default_org}.{domain}` from this base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// Default organization slug for management commands.
    ///
    /// Set automatically during registration (personal org = identity name).
    /// Can be switched with `atomic org set`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_org: Option<String>,

    /// Default workspace slug per organization.
    ///
    /// Workspaces are org-scoped, so the default is stored per org. When
    /// a management command needs a workspace and none is given on the
    /// CLI, the resolver looks up the current org in this map.
    ///
    /// Set with `atomic workspace set <slug> [--org <slug>]`.
    /// Uses `BTreeMap` so the serialized TOML is alphabetically ordered
    /// and produces stable diffs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub default_workspaces: BTreeMap<String, String>,

    /// Identity name to use when authenticating to this server.
    ///
    /// If set, management commands targeting this server use this identity
    /// instead of the global default. Set automatically during
    /// `atomic identity register` when `--identity` is specified.
    ///
    /// Example: `identity = "alice-staging"`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,

    /// Agent identity that recording hooks sign as against this server.
    ///
    /// Set by `atomic identity agent create`. Deliberately separate from
    /// `identity`, which stays the *human* the agent acts on behalf of:
    /// enrollment, renewal and revocation all authenticate as the human, while
    /// day-to-day recording and pushing authenticate as the agent. One field
    /// could not express "this machine holds both keys", which is the normal
    /// case on a developer laptop.
    ///
    /// Example: `agent_identity = "alice+claude"`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_identity: Option<String>,

    /// Whether the server is a single-tenant deployment.
    ///
    /// Single-tenant servers (reported by the registration response's
    /// `mode = "single"`) serve exactly one organization at their bare host:
    /// org-scoped URLs are NOT prefixed with the org slug — every path is
    /// already scoped to the single tenant server-side.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub single_tenant: bool,
}

impl ServerConfig {
    /// Whether the server has been configured (registration completed).
    pub fn is_configured(&self) -> bool {
        self.url.is_some() && self.default_org.is_some()
    }

    /// Build the org-scoped base URL.
    ///
    /// Given `url = "https://atomic.storage"` and `org = "alice"`,
    /// returns `"https://alice.atomic.storage"`.
    ///
    /// Given `url = "http://localhost:8080"` and `org = "alice"`,
    /// returns `"http://alice.localhost:8080"`.
    ///
    /// For single-tenant servers the org is already implied by the server
    /// itself, so the URL is returned unchanged (no `{org}.` prefix).
    pub fn org_base_url(&self, org_slug: &str) -> Option<String> {
        let url = self.url.as_ref()?;

        // Single-tenant: the bare host is already tenant-scoped server-side;
        // prefixing would produce bogus hosts like `org.org.org.example.com`.
        if self.single_tenant {
            return Some(url.trim_end_matches('/').to_string());
        }

        // Parse the URL to extract scheme, host, port
        let url_parsed = url::Url::parse(url).ok()?;
        let scheme = url_parsed.scheme();
        let host = url_parsed.host_str()?;
        let port = url_parsed.port();

        let base = if let Some(port) = port {
            format!("{}://{}.{}:{}", scheme, org_slug, host, port)
        } else {
            format!("{}://{}.{}", scheme, org_slug, host)
        };

        Some(base)
    }

    /// Get the org-scoped base URL using the default org.
    pub fn default_org_base_url(&self) -> Option<String> {
        let org = self.default_org.as_ref()?;
        self.org_base_url(org)
    }
}

/// Global configuration settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalConfig {
    /// Default author information
    #[serde(default)]
    pub author: Author,

    /// Default channel name for new repositories
    #[serde(default = "default_channel_name")]
    pub default_channel: String,

    /// Enable colored output
    #[serde(default)]
    pub colors: Option<ColorChoice>,

    /// Use pager for long output
    #[serde(default)]
    pub pager: Option<bool>,

    /// Global workspace configuration.
    ///
    /// Expose patterns defined here apply to ALL repositories.
    /// Repo-local `[workspace] expose` patterns are merged on top.
    #[serde(default)]
    pub workspace: WorkspaceConfig,

    /// Default remote server configuration for atomic-storage.
    ///
    /// This is the server used when no `--server` flag is given and no
    /// `default_server` points to a named server in `servers`.
    #[serde(default)]
    pub server: ServerConfig,

    /// Named server profiles (e.g. "staging", "prod").
    ///
    /// Each entry is a full `ServerConfig` with its own URL, default org,
    /// workspaces, and optional identity override.
    ///
    /// ```toml
    /// [servers.staging]
    /// url = "https://staging.atomic.storage"
    /// default_org = "alice"
    /// identity = "alice-staging"
    ///
    /// [servers.prod]
    /// url = "https://atomic.storage"
    /// default_org = "alice"
    /// identity = "alice-prod"
    /// ```
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub servers: BTreeMap<String, ServerConfig>,

    /// Name of the active server profile from `servers`.
    ///
    /// When set, management commands use `servers[default_server]` instead
    /// of `server`. Switch with `atomic server set <name>` or pass
    /// `--server <name>` per-command.
    ///
    /// When `None`, the legacy `[server]` block is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_server: Option<String>,
}

fn default_channel_name() -> String {
    "main".to_string()
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            author: Author::default(),
            default_channel: default_channel_name(),
            colors: None,
            pager: None,
            workspace: WorkspaceConfig::default(),
            server: ServerConfig::default(),
            servers: BTreeMap::new(),
            default_server: None,
        }
    }
}

impl GlobalConfig {
    /// Resolve the effective server config, honouring the optional name override.
    ///
    /// Resolution order:
    /// 1. `server_override` (from `--server <name>` CLI flag)
    /// 2. `default_server` (from `~/.atomic/config.toml`)
    /// 3. Legacy `server` block
    ///
    /// Returns `(server_config, Option<name>)` — the name is `Some` when a
    /// named profile was resolved, `None` for the legacy block.
    pub fn resolve_server<'a>(
        &'a self,
        server_override: Option<&'a str>,
    ) -> Result<(&'a ServerConfig, Option<&'a str>), String> {
        // 1. Explicit --server flag
        if let Some(name) = server_override {
            return self
                .servers
                .get(name)
                .map(|s| (s, Some(name)))
                .ok_or_else(|| {
                    format!(
                        "Server profile '{}' not found. \
                         Use 'atomic server list' to see available profiles, \
                         or 'atomic server add {0} <url>' to create it.",
                        name
                    )
                });
        }

        // 2. Configured default server name
        if let Some(ref name) = self.default_server {
            return self
                .servers
                .get(name.as_str())
                .map(|s| (s, Some(name.as_str())))
                .ok_or_else(|| {
                    format!(
                        "Default server profile '{}' not found in servers map. \
                         Run 'atomic server set <name>' to fix.",
                        name
                    )
                });
        }

        // 3. Legacy [server] block
        Ok((&self.server, None))
    }

    /// Resolve the effective server config mutably, honouring the optional
    /// name override.
    ///
    /// Mirrors the resolution order of [`resolve_server`](Self::resolve_server)
    /// so that writes (e.g. `atomic org set`, `atomic workspace set`) land on
    /// the same profile that reads resolve to:
    ///
    /// 1. `server_override` (from `--server <name>` CLI flag)
    /// 2. `default_server` (from `~/.atomic/config.toml`)
    /// 3. Legacy `server` block
    ///
    /// Returns the mutable profile plus its name (`Some` for a named profile,
    /// `None` for the legacy block).
    pub fn resolve_server_mut(
        &mut self,
        server_override: Option<&str>,
    ) -> Result<(&mut ServerConfig, Option<String>), String> {
        // Determine which profile name to target, if any. We resolve the name
        // first (immutable borrow) so the mutable borrow below is unambiguous.
        let name = if let Some(name) = server_override {
            Some(name.to_string())
        } else {
            self.default_server.clone()
        };

        match name {
            Some(name) => {
                let profile = self.servers.get_mut(&name).ok_or_else(|| {
                    format!(
                        "Server profile '{}' not found. \
                         Use 'atomic server list' to see available profiles, \
                         or 'atomic server add {0} <url>' to create it.",
                        name
                    )
                })?;
                Ok((profile, Some(name)))
            }
            None => Ok((&mut self.server, None)),
        }
    }
}

/// Color output preference
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ColorChoice {
    /// Automatically detect based on terminal
    #[default]
    Auto,
    /// Always use colors
    Always,
    /// Never use colors
    Never,
}

/// Execution environment of the current process, classified from the
/// standard CI/container markers (`CI`, `container`,
/// `KUBERNETES_SERVICE_HOST`, `/.dockerenv`).
///
/// RFC §11.2 rule 7: watchers and reactive daemons default off in CI and
/// containers because those environments have no persistent session to
/// watch and every command boundary already reconciles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Environment {
    /// A developer workstation or other persistent session.
    #[default]
    Desktop,
    /// A CI run (`CI` is set).
    Ci,
    /// A container or orchestrator run.
    Container,
}

impl Environment {
    /// Classify the current process environment from the standard markers.
    pub fn detect() -> Self {
        if std::env::var_os("CI").is_some() {
            return Self::Ci;
        }
        if std::env::var_os("container").is_some()
            || std::env::var_os("KUBERNETES_SERVICE_HOST").is_some()
            || std::path::Path::new("/.dockerenv").exists()
        {
            return Self::Container;
        }
        Self::Desktop
    }
}

/// Legacy-compatible `remotes` deserialization.
///
/// Accepts the current `[[remotes]]` sequence and the legacy
/// `[remotes.<name>]` map (0.17.x shape) interchangeably; the legacy map's
/// `default` flag is structural to 0.17.x and carries no meaning in the
/// current schema, so it is ignored here.
fn deserialize_remotes_compat<'de, D>(deserializer: D) -> Result<Vec<RemoteConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Array(entries) => entries
            .into_iter()
            .map(|entry| {
                serde_json::from_value::<RemoteConfig>(entry).map_err(serde::de::Error::custom)
            })
            .collect(),
        serde_json::Value::Object(map) => {
            let mut remotes = Vec::new();
            for (name, entry) in map {
                let url = entry
                    .get("url")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        serde::de::Error::custom(format!("legacy remote '{name}' has no url"))
                    })?;
                remotes.push(RemoteConfig {
                    name,
                    url: url.to_string(),
                    default_channel: entry
                        .get("default_channel")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                });
            }
            remotes.sort_by(|left, right| left.name.cmp(&right.name));
            Ok(remotes)
        }
        other => Err(serde::de::Error::custom(format!(
            "remotes must be a sequence or a legacy name-keyed map, got {other:?}"
        ))),
    }
}

/// Serialize `remotes` in the legacy `[remotes.<name>]` map shape so 0.17.x
/// binaries keep parsing the file.
fn serialize_remotes_compat<S>(remotes: &[RemoteConfig], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap as _;
    let mut map = serializer.serialize_map(Some(remotes.len()))?;
    for remote in remotes {
        let mut entry = serde_json::Map::new();
        entry.insert("url".into(), serde_json::Value::String(remote.url.clone()));
        entry.insert("default".into(), serde_json::Value::Bool(false));
        if let Some(channel) = &remote.default_channel {
            entry.insert(
                "default_channel".into(),
                serde_json::Value::String(channel.clone()),
            );
        }
        map.serialize_entry(&remote.name, &serde_json::Value::Object(entry))?;
    }
    map.end()
}

/// Repository-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepoConfig {
    /// Override author for this repository
    #[serde(default)]
    pub author: Option<Author>,

    /// Remote repositories
    ///
    /// Deserialized compatibly: both the current `[[remotes]]` sequence and
    /// the legacy `[remotes.<name>]` map written by 0.17.x are accepted.
    /// Serialized back in the legacy map shape so 0.17.x binaries keep
    /// reading the file (`default` is emitted as `false`, matching the
    /// shape 0.17.x itself writes).
    #[serde(
        default,
        deserialize_with = "deserialize_remotes_compat",
        serialize_with = "serialize_remotes_compat"
    )]
    pub remotes: Vec<RemoteConfig>,

    /// Workspace configuration for view switching behavior.
    #[serde(default)]
    pub workspace: WorkspaceConfig,

    /// Git interoperability configuration.
    #[serde(default)]
    pub git: GitConfig,

    /// Repository-byte content-filter policy.
    #[serde(default)]
    pub filters: ContentFilterConfig,
}

/// Git interoperability settings.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GitConfig {
    /// Candidate-path source used for Git-backed working copies.
    #[serde(default)]
    pub watch: GitWatch,

    /// Binding signer trust policy (RFC-ATOMIC-GIT-CAUSAL-BRIDGE §5.6).
    #[serde(default)]
    pub trust: GitTrustConfig,

    /// Export identity mapping (RFC-ATOMIC-GIT-CAUSAL-BRIDGE §5.5).
    #[serde(default)]
    pub identity: GitIdentityConfig,

    /// Bridge rollout gate (RFC-ATOMIC-GIT-CAUSAL-BRIDGE §13 Phase 13
    /// task 4, CB-13C). Default-off: the bridge is never enabled by
    /// configuration alone.
    #[serde(default)]
    pub bridge: GitBridgeConfig,
}

/// Bridge rollout gate (RFC §13 Phase 13 task 4, CB-13C).
///
/// # Decision recorded for CB-13C
///
/// The colocated bridge stays **opt-in only**. `enabled` defaults to
/// `false`, and no code path flips it: default enablement additionally
/// requires every named rollout gate (recovery, parity, security,
/// migration, performance) to be independently `Passed`, which per the
/// RFC §21 evidence matrix in the tracker they are not (several are
/// `Unknown`, and none carries owner approval). Until the owner approves
/// measured thresholds, [`GitBridgeConfig::default_enablement`] refuses.
///
/// ```toml
/// [git.bridge]
/// enabled = true   # explicit per-repository opt-in; default false
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct GitBridgeConfig {
    /// Explicit opt-in for bridge workflows on this repository. Defaults
    /// to `false`; the bridge is never enabled by configuration alone.
    #[serde(default)]
    pub enabled: bool,

    /// The optional metadata-only bridge watch daemon (RFC §11.2, CB-13D).
    /// Defaults to disabled; an absent `[git.bridge.watch]` section reads
    /// as fully disabled.
    #[serde(default)]
    pub watch: BridgeWatchConfig,
}

/// Configuration for the optional metadata-only bridge watch daemon
/// (RFC §11.2 rules 3–7).
///
/// The daemon is a latency accelerator only: correctness always comes from
/// command-boundary reconciliation, and a stopped or killed daemon must
/// never change the next command's outcome. It is therefore disabled unless
/// explicitly opted in per repository, over and above the bridge opt-in.
///
/// ```toml
/// [git.bridge.watch]
/// enabled = true  # explicit per-repository opt-in; default false
/// quiet_ms = 250  # minimum silence before a reactive reconcile (>= 250)
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BridgeWatchConfig {
    /// Explicit opt-in for the watch daemon on this repository. Defaults to
    /// `false`; the daemon is never started by configuration alone — not
    /// even in CI or containers, which simply receive the same disabled
    /// default everywhere else does.
    #[serde(default)]
    pub enabled: bool,
    /// Minimum silence window, in milliseconds, before a reactive
    /// reconcile may run (RFC §11.2 rule 3: at least 250 ms). Values below
    /// the floor are clamped up to [`BridgeWatchConfig::MIN_QUIET_MS`] by
    /// [`BridgeWatchConfig::effective_quiet_ms`]; the raw field is
    /// preserved for round-tripping.
    #[serde(default = "BridgeWatchConfig::default_quiet_ms")]
    pub quiet_ms: u64,
}

impl BridgeWatchConfig {
    /// The enforced lower bound for the reactive silence window
    /// (RFC §11.2 rule 3: at least 250 ms).
    pub const MIN_QUIET_MS: u64 = 250;

    const fn default_quiet_ms() -> u64 {
        Self::MIN_QUIET_MS
    }

    /// The silence window the daemon actually waits for: the configured
    /// value clamped up to the RFC floor. Never lower than 250 ms.
    pub fn effective_quiet_ms(&self) -> u64 {
        self.quiet_ms.max(Self::MIN_QUIET_MS)
    }

    /// Why the daemon must not start under the current configuration, if it
    /// must not. The daemon is opt-in only (RFC §11.2 rule 7): it is off
    /// unless `[git.bridge.watch] enabled = true` is explicitly recorded,
    /// which is also how CI and containers stay off — they get the same
    /// disabled default as everywhere else, and only an explicit
    /// per-repository opt-in starts the daemon there.
    pub fn startup_refusal(&self) -> Option<&'static str> {
        if self.enabled {
            return None;
        }
        Some(
            "the bridge watch daemon is disabled (set [git.bridge.watch] enabled = true to opt in; \
             command boundaries fully reconcile without it)",
        )
    }
}

impl Default for BridgeWatchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            quiet_ms: Self::default_quiet_ms(),
        }
    }
}

/// Measured rollout gates that must each be `Passed` before any default
/// enablement (RFC §21 evidence matrix; tracker "RFC §21 evidence matrix").
///
/// The states here are the recorded CB-13C decision surface: a gate is
/// flipped only when the tracker matrix cites measured evidence for it and
/// the owner approves the threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RolloutGateState {
    pub passed: bool,
    pub measured: bool,
}

/// The named rollout gates and their current recorded states.
pub const ROLLOUT_GATES: &[(&str, RolloutGateState)] = &[
    (
        "recovery",
        RolloutGateState {
            passed: false,
            measured: false,
        },
    ),
    (
        "parity",
        RolloutGateState {
            passed: false,
            measured: false,
        },
    ),
    (
        "security",
        RolloutGateState {
            passed: false,
            measured: false,
        },
    ),
    (
        "migration",
        RolloutGateState {
            passed: false,
            measured: false,
        },
    ),
    (
        "performance",
        RolloutGateState {
            passed: false,
            measured: false,
        },
    ),
];

/// Why default enablement is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DefaultEnablementRefusal {
    #[error("the bridge is opt-in only: set [git.bridge] enabled = true to opt in")]
    NotOptedIn,
    #[error("rollout gate '{gate}' is unmeasured; default enablement is blocked until the RFC §21 matrix records measured evidence and owner approval")]
    UnmeasuredGate { gate: &'static str },
    #[error("rollout gate '{gate}' has not passed its measured threshold; default enablement is blocked")]
    FailedGate { gate: &'static str },
}

impl GitBridgeConfig {
    /// Whether the bridge may be enabled *by default* (without an explicit
    /// user action naming this repository). Refuses while the repository has
    /// not opted in, or while any rollout gate is unmeasured or unpassed.
    pub fn default_enablement(&self) -> Result<(), DefaultEnablementRefusal> {
        if !self.enabled {
            return Err(DefaultEnablementRefusal::NotOptedIn);
        }
        for (gate, state) in ROLLOUT_GATES {
            if !state.measured {
                return Err(DefaultEnablementRefusal::UnmeasuredGate { gate });
            }
            if !state.passed {
                return Err(DefaultEnablementRefusal::FailedGate { gate });
            }
        }
        Ok(())
    }
}

/// Export identity mapping (RFC §5.5).
///
/// Maps Git emails to Atomic DIDs for exported commits. Mapped emails use
/// their configured DID; unmapped emails receive a deterministic
/// `did:atomic:git:<email-hash>` foreign identity flagged in provenance.
///
/// ```toml
/// [git.identity]
/// "lee@atomic.dev" = "did:atomic:U4NN…"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GitIdentityConfig {
    /// Email → DID mappings used when projecting commits to Git.
    #[serde(flatten)]
    pub mappings: std::collections::BTreeMap<String, String>,
}

impl GitIdentityConfig {
    /// Look up the DID for one email: configured mapping, else
    /// `None` (the caller synthesizes the deterministic foreign identity).
    pub fn did_for_email(&self, email: &str) -> Option<&str> {
        self.mappings.get(email).map(String::as_str)
    }
}

/// Binding signer trust policy.
///
/// # Decision for RFC 19 question 4 (CB-6A)
///
/// The default trust model is a **per-repository explicit allowlist**
/// (deny-by-default). The repository's own identity is trusted, plus every
/// DID listed under `[git.trust] signers` (configured collaborators).
/// Everything else is `Unknown` and yields explicitly **untrusted**
/// provenance/attestation claims — content correctness is still recomputed
/// independently of the signer (RFC §5.2), so an unknown signer can supply
/// correct content but can never satisfy a publication gate.
///
/// Web-of-trust via delegation certificates was considered and **deferred**:
/// it broadens who can satisfy gates without a repository-local decision, and
/// enabling it later requires an explicit policy decision (delegation
/// certificates and transitive trust evaluation), not a silent default. Until
/// such a decision lands, `revoked` signers stay revoked even when also
/// listed in `signers`, and revocation always wins.
///
/// ```toml
/// [git.trust]
/// signers = ["did:atomic:U4NN…"]   # configured collaborators
/// revoked = ["did:atomic:AAAA…"]   # explicit revocation; overrides signers
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitTrustConfig {
    /// Explicitly trusted signer DIDs (configured collaborators).
    #[serde(default)]
    pub signers: Vec<String>,

    /// Explicitly revoked signer DIDs. Takes precedence over `signers` and
    /// over the repository identity.
    #[serde(default)]
    pub revoked: Vec<String>,
}

/// Trust evaluation outcome for one binding signer.
///
/// Trust is independent of content validity: a signature proves who signed,
/// never that the content is correct (RFC §5.2/§12.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignerTrust {
    /// The signer is the repository identity or a configured collaborator.
    Trusted,
    /// The signer was explicitly revoked; overrides any other rule.
    Revoked,
    /// The signer is cryptographically valid but not configured; its
    /// provenance/attestation claims stay untrusted.
    Unknown,
}

impl GitTrustConfig {
    /// Evaluate one signer against this policy.
    ///
    /// `repository_identity` is the repository's own signer DID (the stated
    /// default trust root) when one is configured. `revoked` always wins:
    /// revocation is explicit and cannot be re-trusted by listing.
    pub fn evaluate(&self, signer: &str, repository_identity: Option<&str>) -> SignerTrust {
        if self.revoked.iter().any(|did| did == signer) {
            return SignerTrust::Revoked;
        }
        if repository_identity.is_some_and(|identity| identity == signer)
            || self.signers.iter().any(|did| did == signer)
        {
            return SignerTrust::Trusted;
        }
        SignerTrust::Unknown
    }
}

#[cfg(test)]
mod trust_evaluation_tests {
    use super::*;

    /// CB-12B follow-up AC-6 (RFC §19 Q4): trust evaluation is
    /// configured-policy-ONLY — unknown signers are untrusted, revocation
    /// wins over any allowlisting, the repository identity is trusted, and
    /// NO default (per-repo allowlist inference, web of trust, or
    /// content-acceptance inference) is granted. This test pins the exact
    /// semantics so no future change silently invents a Q4 default.
    #[test]
    fn git_trust_evaluation_is_configured_policy_only() {
        let repo_did = "did:atomic:REPO";
        let signer = GitTrustConfig {
            signers: vec!["did:atomic:COLLAB".to_string()],
            revoked: vec!["did:atomic:EVIL".to_string()],
        };

        // Unknown → untrusted: never inferred from anything else.
        assert_eq!(
            signer.evaluate("did:atomic:STRANGER", Some(repo_did)),
            SignerTrust::Unknown
        );
        assert_eq!(
            signer.evaluate("did:atomic:STRANGER", None),
            SignerTrust::Unknown
        );

        // Configured collaborator → trusted.
        assert_eq!(
            signer.evaluate("did:atomic:COLLAB", None),
            SignerTrust::Trusted
        );

        // The repository's own identity → trusted.
        assert_eq!(
            signer.evaluate(repo_did, Some(repo_did)),
            SignerTrust::Trusted
        );

        // Revocation WINS over allowlisting and over the repository identity.
        assert_eq!(
            signer.evaluate("did:atomic:EVIL", None),
            SignerTrust::Revoked
        );
        let revoked_repo = GitTrustConfig {
            signers: Vec::new(),
            revoked: vec![repo_did.to_string()],
        };
        assert_eq!(
            revoked_repo.evaluate(repo_did, Some(repo_did)),
            SignerTrust::Revoked
        );

        // The empty `[git.trust]` section has no allowlist — but the
        // repository's own identity remains its stated default trust root
        // (explicitly passed as repository_identity, a self-root, not a Q4
        // web-of-trust default). Strangers are still Unknown.
        let empty = GitTrustConfig::default();
        assert_eq!(
            empty.evaluate(repo_did, Some(repo_did)),
            SignerTrust::Trusted
        );
        assert_eq!(
            empty.evaluate("did:atomic:STRANGER", Some(repo_did)),
            SignerTrust::Unknown
        );
    }
}

/// Candidate-path source preference for Git-backed working copies.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GitWatch {
    /// Do not use an external change notification source.
    Off,
    /// Prefer builtin Git fsmonitor, then Watchman, then a full scan.
    #[default]
    Auto,
    /// Prefer Git's builtin fsmonitor daemon.
    Fsmonitor,
    /// Prefer Watchman.
    Watchman,
}

/// Bounds and external drivers used by repository-byte content filters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentFilterConfig {
    /// Maximum wall-clock time for one external clean or smudge command.
    #[serde(default = "default_filter_timeout_ms")]
    pub timeout_ms: u64,
    /// Maximum stdout or stderr retained from one external filter command.
    #[serde(default = "default_filter_output_bytes")]
    pub max_output_bytes: usize,
    /// Configured Git-style filter drivers, keyed by `.gitattributes` filter name.
    #[serde(default)]
    pub drivers: BTreeMap<String, ExternalFilterConfig>,
}

impl Default for ContentFilterConfig {
    fn default() -> Self {
        Self {
            timeout_ms: default_filter_timeout_ms(),
            max_output_bytes: default_filter_output_bytes(),
            drivers: BTreeMap::new(),
        }
    }
}

/// One bounded external clean/smudge driver.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExternalFilterConfig {
    /// Command receiving working bytes on stdin and producing repository bytes.
    pub clean: Option<String>,
    /// Command receiving repository bytes on stdin and producing working bytes.
    pub smudge: Option<String>,
    /// Whether an unavailable or failed command must abort the operation.
    #[serde(default)]
    pub required: bool,
}

const fn default_filter_timeout_ms() -> u64 {
    30_000
}

const fn default_filter_output_bytes() -> usize {
    64 * 1024 * 1024
}

/// Controls how ignored files are handled during view switches.
///
/// By default, all ignored files (`.atomicignore`) are shelved per-view
/// when switching — build artifacts get isolated so each view has its own
/// `node_modules/`, `target/`, etc. Paths listed in `expose` are the
/// exception: they persist across all views and are never shelved.
///
/// This keeps tool configs (`.opencode/`, `.vscode/`, `.idea/`) stable
/// while build artifacts are managed per-view automatically.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// Paths that persist across all views (never shelved).
    ///
    /// Everything in `.atomicignore` is shelved per-view on switch,
    /// EXCEPT paths matching these patterns — those are left alone.
    ///
    /// Example:
    /// ```toml
    /// [workspace]
    /// expose = [".opencode", ".vscode", ".idea"]
    /// ```
    #[serde(default)]
    pub expose: Vec<String>,
}

/// Remote repository configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// Name of the remote (e.g., "origin")
    pub name: String,

    /// URL of the remote repository
    pub url: String,

    /// Default channel to push/pull
    #[serde(default)]
    pub default_channel: Option<String>,
}

/// Get the global configuration directory.
///
/// Resolution order:
/// 1. The `ATOMIC_CONFIG_DIR` environment variable, if set — used verbatim as
///    the config directory (the one containing `config.toml`). This lets tests
///    isolate the global config on every platform, and lets users relocate it.
/// 2. `dirs::home_dir()/.atomic` (the default).
///
/// The variable is read at call time so it can be set and restored per test
/// (`dirs::home_dir()` on Windows resolves the profile known-folder, not
/// `HOME`, so an env override is the only reliable cross-platform isolation).
pub fn global_config_dir() -> Option<PathBuf> {
    global_config_dir_from(std::env::var_os("ATOMIC_CONFIG_DIR"))
}

/// Resolve the global config directory from an explicit override value.
///
/// Split out so the override precedence is unit-testable without mutating
/// process environment (which is racy and platform-sensitive).
fn global_config_dir_from(env_override: Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(dir) = env_override {
        return Some(PathBuf::from(dir));
    }
    dirs::home_dir().map(|p| p.join(".atomic"))
}

/// Get the global configuration file path
pub fn global_config_path() -> Option<PathBuf> {
    global_config_dir().map(|p| p.join("config.toml"))
}

impl GlobalConfig {
    /// Load global configuration from the default location
    pub fn load() -> Result<Self, ConfigError> {
        let path = global_config_path().ok_or(ConfigError::NoConfigDir)?;

        if !path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(&path).map_err(|e| ConfigError::ReadError {
            path: path.clone(),
            source: e,
        })?;

        Ok(toml::from_str(&content)?)
    }

    /// Save global configuration to the default location
    pub fn save(&self) -> Result<(), ConfigError> {
        let path = global_config_path().ok_or(ConfigError::NoConfigDir)?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ConfigError::WriteError {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, content).map_err(|e| ConfigError::WriteError { path, source: e })?;

        Ok(())
    }
}

impl RepoConfig {
    /// Load repository configuration from a specific path
    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(path).map_err(|e| ConfigError::ReadError {
            path: path.to_path_buf(),
            source: e,
        })?;

        Ok(toml::from_str(&content)?)
    }

    /// Save repository configuration to a specific path.
    ///
    /// Atomic replacement (write a same-directory temporary, fsync, rename
    /// over the target) — CB-13C F2: consent must never be persisted by a
    /// truncating in-place rewrite, so an interrupted save can never leave
    /// a half-written consent state behind.
    pub fn save(&self, path: &std::path::Path) -> Result<(), ConfigError> {
        let content = toml::to_string_pretty(self)?;
        let temporary = path.with_extension(format!("toml.tmp.{}", std::process::id()));
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|e| ConfigError::WriteError {
                    path: temporary.clone(),
                    source: e,
                })?;
            file.write_all(content.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|e| ConfigError::WriteError {
                    path: temporary.clone(),
                    source: e,
                })?;
        }
        let renamed = std::fs::rename(&temporary, path);
        if renamed.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        renamed.map_err(|e| ConfigError::WriteError {
            path: path.to_path_buf(),
            source: e,
        })?;
        Ok(())
    }

    /// Get a remote by name
    pub fn get_remote(&self, name: &str) -> Option<&RemoteConfig> {
        self.remotes.iter().find(|r| r.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_git_watch_defaults_to_auto() {
        assert_eq!(RepoConfig::default().git.watch, GitWatch::Auto);
        let parsed: RepoConfig = toml::from_str("").unwrap();
        assert_eq!(parsed.git.watch, GitWatch::Auto);
    }

    /// CB-13D (RFC §11.2): the watch daemon is opt-in only and its silence
    /// window is floored at 250 ms; an absent section reads as fully
    /// disabled and round-trips losslessly.
    #[test]
    fn bridge_watch_defaults_to_disabled_with_rfc_floor() {
        let parsed: RepoConfig = toml::from_str("").unwrap();
        assert!(!parsed.git.bridge.watch.enabled);
        assert_eq!(parsed.git.bridge.watch.quiet_ms, 250);
        assert_eq!(parsed.git.bridge.watch.effective_quiet_ms(), 250);
        assert!(parsed.git.bridge.watch.startup_refusal().is_some());

        let parsed: RepoConfig =
            toml::from_str("[git.bridge.watch]\nenabled = true\nquiet_ms = 40\n").unwrap();
        assert!(parsed.git.bridge.watch.enabled);
        assert_eq!(
            parsed.git.bridge.watch.effective_quiet_ms(),
            250,
            "the daemon never waits less than the RFC 250 ms silence floor"
        );
        assert!(parsed.git.bridge.watch.startup_refusal().is_none());

        let parsed: RepoConfig =
            toml::from_str("[git.bridge.watch]\nenabled = true\nquiet_ms = 900\n").unwrap();
        assert_eq!(parsed.git.bridge.watch.effective_quiet_ms(), 900);

        // Round-trip: a saved opt-in keeps its exact values.
        let config = RepoConfig {
            git: GitConfig {
                bridge: GitBridgeConfig {
                    enabled: true,
                    watch: BridgeWatchConfig {
                        enabled: true,
                        quiet_ms: 500,
                    },
                },
                ..GitConfig::default()
            },
            ..RepoConfig::default()
        };
        let encoded = toml::to_string_pretty(&config).unwrap();
        let decoded: RepoConfig = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.git.bridge.watch, config.git.bridge.watch);
    }

    #[test]
    fn git_identity_defaults_to_no_mappings() {
        let config: RepoConfig = toml::from_str("").unwrap();
        assert!(config.git.identity.mappings.is_empty());
        assert!(config
            .git
            .identity
            .did_for_email("lee@atomic.dev")
            .is_none());
    }

    /// RFC §5.5: `[git.identity]` maps Git emails to Atomic DIDs.
    #[test]
    fn git_identity_parses_email_to_did_mappings() {
        let config: RepoConfig =
            toml::from_str("[git.identity]\n\"lee@atomic.dev\" = \"did:atomic:U4NN\"\n").unwrap();
        assert_eq!(
            config.git.identity.did_for_email("lee@atomic.dev"),
            Some("did:atomic:U4NN")
        );
        assert!(config.git.identity.did_for_email("other@x.dev").is_none());
    }

    #[test]
    fn repo_git_watch_parses_all_modes() {
        for (value, expected) in [
            ("off", GitWatch::Off),
            ("auto", GitWatch::Auto),
            ("fsmonitor", GitWatch::Fsmonitor),
            ("watchman", GitWatch::Watchman),
        ] {
            let config: RepoConfig =
                toml::from_str(&format!("[git]\nwatch = \"{value}\"\n")).unwrap();
            assert_eq!(config.git.watch, expected);
        }
    }

    /// CB-13C rollout gate: the bridge is opt-in only and default-off.
    #[test]
    fn git_bridge_defaults_to_disabled() {
        let config: RepoConfig = toml::from_str("").unwrap();
        assert!(!config.git.bridge.enabled);
        assert_eq!(
            config.git.bridge.default_enablement(),
            Err(DefaultEnablementRefusal::NotOptedIn)
        );
    }

    /// Explicit opt-in parses, and default enablement is still refused
    /// while any rollout gate is unmeasured (RFC §21 matrix).
    #[test]
    fn git_bridge_opt_in_parses_but_default_enablement_stays_blocked_by_unmeasured_gates() {
        let config: RepoConfig = toml::from_str("[git.bridge]\nenabled = true\n").unwrap();
        assert!(config.git.bridge.enabled);
        match config.git.bridge.default_enablement() {
            Err(DefaultEnablementRefusal::UnmeasuredGate { gate }) => {
                assert_eq!(gate, "recovery", "the first unmeasured gate is named")
            }
            other => panic!("expected an unmeasured-gate refusal, got {other:?}"),
        }
    }

    /// Review R4: the recorded gate surface is exercised for what it
    /// actually is — every named gate is unmeasured, so default enablement
    /// refuses with the FIRST gate and can never report `FailedGate` while
    /// the constants are fixed. The helper is consumed by the CLI enable
    /// command and the consent-gated telemetry sink (`for_repository`);
    /// the live opt-in/rollback transition is covered by the CLI bridge
    /// test `enable_records_opt_in_and_disable_rolls_back`, which drives
    /// the real config file. This test asserts the real recorded states,
    /// not constructed booleans.
    #[test]
    fn default_enablement_refuses_with_the_real_unmeasured_gate_surface() {
        let config: RepoConfig = toml::from_str("[git.bridge]\nenabled = true\n").unwrap();
        assert!(config.git.bridge.enabled);

        // The recorded surface itself: every gate unmeasured and unpassed.
        for (gate, state) in ROLLOUT_GATES {
            assert!(!state.measured, "gate '{gate}' must stay unmeasured until the RFC §21 matrix records measured evidence and owner approval");
            assert!(
                !state.passed,
                "gate '{gate}' must stay unpassed without owner approval"
            );
        }
        // Opting in does not approve default rollout: the first gate is
        // named as unmeasured.
        match config.git.bridge.default_enablement() {
            Err(DefaultEnablementRefusal::UnmeasuredGate { gate }) => {
                assert_eq!(gate, "recovery")
            }
            other => panic!("expected an unmeasured-gate refusal, got {other:?}"),
        }

        // Rollback semantics on the type: opting back out refuses with
        // NotOptedIn regardless of any gate state.
        let rolled_back: RepoConfig = toml::from_str("").unwrap();
        assert_eq!(
            rolled_back.git.bridge.default_enablement(),
            Err(DefaultEnablementRefusal::NotOptedIn)
        );
        // The FailedGate branch is unreachable while every gate is
        // unmeasured — that is the recorded decision surface, and this
        // test pins it instead of simulating approval.
    }

    #[test]
    fn git_trust_defaults_to_empty_deny_by_default_policy() {
        let config: RepoConfig = toml::from_str("").unwrap();
        assert!(config.git.trust.signers.is_empty());
        assert!(config.git.trust.revoked.is_empty());
        // Deny-by-default: with no configuration at all, only the repository
        // identity is trusted.
        assert_eq!(
            config
                .git
                .trust
                .evaluate("did:atomic:AA", Some("did:atomic:AA")),
            SignerTrust::Trusted
        );
        assert_eq!(
            config
                .git
                .trust
                .evaluate("did:atomic:BB", Some("did:atomic:AA")),
            SignerTrust::Unknown
        );
        assert_eq!(
            config.git.trust.evaluate("did:atomic:BB", None),
            SignerTrust::Unknown
        );
    }

    #[test]
    fn git_trust_parses_signers_and_revoked() {
        let config: RepoConfig = toml::from_str(
            "[git.trust]\nsigners = [\"did:atomic:COLLAB\"]\nrevoked = [\"did:atomic:BANNED\"]\n",
        )
        .unwrap();
        assert_eq!(
            config.git.trust.evaluate("did:atomic:COLLAB", None),
            SignerTrust::Trusted
        );
        assert_eq!(
            config.git.trust.evaluate("did:atomic:BANNED", None),
            SignerTrust::Revoked
        );
        assert_eq!(
            config.git.trust.evaluate("did:atomic:OTHER", None),
            SignerTrust::Unknown
        );
    }

    #[test]
    fn git_trust_revocation_overrides_signers_and_repository_identity() {
        let config = GitTrustConfig {
            signers: vec!["did:atomic:BOTH".to_string()],
            revoked: vec!["did:atomic:BOTH".to_string()],
        };
        assert_eq!(
            config.evaluate("did:atomic:BOTH", None),
            SignerTrust::Revoked,
            "revoked must win over the signers allowlist"
        );
        let revoked_identity = GitTrustConfig {
            signers: Vec::new(),
            revoked: vec!["did:atomic:REPO".to_string()],
        };
        assert_eq!(
            revoked_identity.evaluate("did:atomic:REPO", Some("did:atomic:REPO")),
            SignerTrust::Revoked,
            "an explicitly revoked repository identity stays revoked"
        );
    }

    #[test]
    fn git_trust_reloads_from_repository_config_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        let before = RepoConfig::load(&path).unwrap();
        assert_eq!(
            before.git.trust.evaluate("did:atomic:X", None),
            SignerTrust::Unknown
        );

        std::fs::write(&path, "[git.trust]\nsigners = [\"did:atomic:X\"]\n").unwrap();
        let after = RepoConfig::load(&path).unwrap();
        assert_eq!(
            before.git.trust.evaluate("did:atomic:X", None),
            SignerTrust::Unknown,
            "the previously loaded policy snapshot is unchanged"
        );
        assert_eq!(
            after.git.trust.evaluate("did:atomic:X", None),
            SignerTrust::Trusted,
            "a configuration reload picks up newly trusted signers"
        );
    }

    #[test]
    fn test_author_serialization() {
        let author = Author {
            name: "Test User".to_string(),
            email: Some("test@example.com".to_string()),
            identity: None,
        };

        let toml_str = toml::to_string(&author).unwrap();
        let parsed: Author = toml::from_str(&toml_str).unwrap();
        assert_eq!(author, parsed);
    }

    #[test]
    fn test_global_config_default() {
        let config = GlobalConfig::default();
        assert_eq!(config.default_channel, "main");
    }

    #[test]
    fn test_server_config_default() {
        let config = ServerConfig::default();
        assert!(!config.is_configured());
        assert!(config.url.is_none());
        assert!(config.default_org.is_none());
        assert!(config.org_base_url("alice").is_none());
        assert!(config.default_org_base_url().is_none());
    }

    #[test]
    fn test_server_config_is_configured() {
        let config = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: Some("alice".to_string()),
            default_workspaces: BTreeMap::new(),
            identity: None,
            agent_identity: None,
            single_tenant: false,
        };
        assert!(config.is_configured());

        // Missing org → not configured
        let partial = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: None,
            default_workspaces: BTreeMap::new(),
            identity: None,
            agent_identity: None,
            single_tenant: false,
        };
        assert!(!partial.is_configured());

        // Missing url → not configured
        let partial = ServerConfig {
            url: None,
            default_org: Some("alice".to_string()),
            default_workspaces: BTreeMap::new(),
            identity: None,
            agent_identity: None,
            single_tenant: false,
        };
        assert!(!partial.is_configured());
    }

    #[test]
    fn test_server_config_org_base_url() {
        let config = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: Some("alice".to_string()),
            default_workspaces: BTreeMap::new(),
            identity: None,
            agent_identity: None,
            single_tenant: false,
        };
        assert_eq!(
            config.org_base_url("alice"),
            Some("https://alice.atomic.storage".to_string())
        );
        assert_eq!(
            config.org_base_url("acme-corp"),
            Some("https://acme-corp.atomic.storage".to_string())
        );
    }

    #[test]
    fn test_server_config_org_base_url_with_port() {
        let config = ServerConfig {
            url: Some("http://localhost:8080".to_string()),
            default_org: None,
            default_workspaces: BTreeMap::new(),
            identity: None,
            agent_identity: None,
            single_tenant: false,
        };
        assert_eq!(
            config.org_base_url("alice"),
            Some("http://alice.localhost:8080".to_string())
        );
    }

    #[test]
    fn test_server_config_default_org_base_url() {
        let config = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: Some("alice".to_string()),
            default_workspaces: BTreeMap::new(),
            identity: None,
            agent_identity: None,
            single_tenant: false,
        };
        assert_eq!(
            config.default_org_base_url(),
            Some("https://alice.atomic.storage".to_string())
        );

        // No default org → None
        let config = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: None,
            default_workspaces: BTreeMap::new(),
            identity: None,
            agent_identity: None,
            single_tenant: false,
        };
        assert!(config.default_org_base_url().is_none());
    }

    #[test]
    fn test_server_config_serialization_roundtrip() {
        let config = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: Some("alice".to_string()),
            default_workspaces: BTreeMap::new(),
            identity: None,
            agent_identity: None,
            single_tenant: false,
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(toml_str.contains("url = \"https://atomic.storage\""));
        assert!(toml_str.contains("default_org = \"alice\""));

        let parsed: ServerConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.url, config.url);
        assert_eq!(parsed.default_org, config.default_org);
    }

    #[test]
    fn test_global_config_with_server() {
        let config = GlobalConfig {
            author: Author {
                name: "Test User".to_string(),
                email: Some("test@example.com".to_string()),
                identity: None,
            },
            server: ServerConfig {
                url: Some("https://atomic.storage".to_string()),
                default_org: Some("alice".to_string()),
                default_workspaces: BTreeMap::new(),
                identity: None,
                agent_identity: None,
                single_tenant: false,
            },
            ..GlobalConfig::default()
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(toml_str.contains("[server]"));
        assert!(toml_str.contains("url = \"https://atomic.storage\""));

        let parsed: GlobalConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            parsed.server.url,
            Some("https://atomic.storage".to_string())
        );
        assert_eq!(parsed.server.default_org, Some("alice".to_string()));
        assert!(parsed.server.is_configured());
    }

    #[test]
    fn test_default_workspaces_skipped_when_empty() {
        let config = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: Some("alice".to_string()),
            default_workspaces: BTreeMap::new(),
            identity: None,
            agent_identity: None,
            single_tenant: false,
        };
        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(!toml_str.contains("default_workspaces"));
    }

    #[test]
    fn test_default_workspaces_roundtrip() {
        let mut workspaces = BTreeMap::new();
        workspaces.insert("alice".to_string(), "personal".to_string());
        workspaces.insert("acme".to_string(), "backend".to_string());

        let config = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: Some("alice".to_string()),
            default_workspaces: workspaces,
            identity: None,
            agent_identity: None,
            single_tenant: false,
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(toml_str.contains("[default_workspaces]"));

        // BTreeMap → alphabetical, so "acme" appears before "alice"
        let acme_pos = toml_str.find("acme = \"backend\"").unwrap();
        let alice_pos = toml_str.find("alice = \"personal\"").unwrap();
        assert!(acme_pos < alice_pos);

        let parsed: ServerConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            parsed.default_workspaces.get("alice"),
            Some(&"personal".to_string())
        );
        assert_eq!(
            parsed.default_workspaces.get("acme"),
            Some(&"backend".to_string())
        );
    }

    #[test]
    fn test_resolve_server_mut_targets_named_profile() {
        // default_server points at a named profile → mutation must land there,
        // not on the legacy [server] block.
        let mut config = GlobalConfig {
            default_server: Some("prod".to_string()),
            ..GlobalConfig::default()
        };
        config.servers.insert(
            "prod".to_string(),
            ServerConfig {
                url: Some("https://atomic.storage".to_string()),
                default_org: None,
                default_workspaces: BTreeMap::new(),
                identity: Some("continuouslee".to_string()),
                agent_identity: None,
                single_tenant: false,
            },
        );

        let (server, name) = config.resolve_server_mut(None).unwrap();
        assert_eq!(name.as_deref(), Some("prod"));
        server.default_org = Some("atomic".to_string());

        assert_eq!(
            config.servers["prod"].default_org.as_deref(),
            Some("atomic")
        );
        assert!(config.server.default_org.is_none());
    }

    #[test]
    fn test_resolve_server_mut_falls_back_to_legacy_block() {
        // No default_server and no override → legacy [server] block.
        let mut config = GlobalConfig::default();

        let (server, name) = config.resolve_server_mut(None).unwrap();
        assert!(name.is_none());
        server.default_org = Some("alice".to_string());

        assert_eq!(config.server.default_org.as_deref(), Some("alice"));
    }

    #[test]
    fn test_resolve_server_mut_override_wins() {
        let mut config = GlobalConfig {
            default_server: Some("prod".to_string()),
            ..GlobalConfig::default()
        };
        config
            .servers
            .insert("prod".to_string(), ServerConfig::default());
        config
            .servers
            .insert("staging".to_string(), ServerConfig::default());

        let (server, name) = config.resolve_server_mut(Some("staging")).unwrap();
        assert_eq!(name.as_deref(), Some("staging"));
        server.default_org = Some("staging-org".to_string());

        assert_eq!(
            config.servers["staging"].default_org.as_deref(),
            Some("staging-org")
        );
        assert!(config.servers["prod"].default_org.is_none());
    }

    #[test]
    fn test_resolve_server_mut_missing_profile_errors() {
        let mut config = GlobalConfig {
            default_server: Some("ghost".to_string()),
            ..GlobalConfig::default()
        };
        assert!(config.resolve_server_mut(None).is_err());
    }

    #[test]
    fn test_default_workspaces_backward_compatibility() {
        // Old configs without default_workspaces should still parse.
        let toml_str = r#"
url = "https://atomic.storage"
default_org = "alice"
"#;
        let parsed: ServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.default_org.as_deref(), Some("alice"));
        assert!(parsed.default_workspaces.is_empty());
    }

    #[test]
    fn test_global_config_backward_compatibility_without_server() {
        // A TOML string without [server] should still parse correctly
        let toml_str = r#"
default_channel = "main"

[author]
name = "Test User"
email = "test@example.com"
"#;

        let parsed: GlobalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.default_channel, "main");
        assert_eq!(parsed.author.name, "Test User");
        // server should be default (not configured)
        assert!(!parsed.server.is_configured());
        assert!(parsed.server.url.is_none());
        assert!(parsed.server.default_org.is_none());
    }

    #[test]
    fn test_org_base_url_single_tenant_not_prefixed() {
        let config = ServerConfig {
            url: Some("https://storage.acme.com".to_string()),
            default_org: Some("acme".to_string()),
            default_workspaces: BTreeMap::new(),
            identity: None,
            agent_identity: None,
            single_tenant: true,
        };
        // Single-tenant: the bare host is already tenant-scoped — no org prefix.
        assert_eq!(
            config.org_base_url("acme").as_deref(),
            Some("https://storage.acme.com")
        );
        // Any org slug returns the bare host unchanged.
        assert_eq!(
            config.org_base_url("anything-else").as_deref(),
            Some("https://storage.acme.com")
        );
        assert_eq!(
            config.default_org_base_url().as_deref(),
            Some("https://storage.acme.com")
        );
    }

    #[test]
    fn test_org_base_url_single_tenant_strips_trailing_slash() {
        let config = ServerConfig {
            url: Some("https://storage.acme.com/".to_string()),
            default_org: Some("acme".to_string()),
            default_workspaces: BTreeMap::new(),
            identity: None,
            agent_identity: None,
            single_tenant: true,
        };
        assert_eq!(
            config.org_base_url("acme").as_deref(),
            Some("https://storage.acme.com")
        );
    }

    #[test]
    fn test_single_tenant_defaults_false_for_legacy_configs() {
        // Configs written before the field existed must deserialize as
        // multi-tenant (org-prefixed) — no migration needed.
        let legacy = r#"url = "https://atomic.storage"
default_org = "alice"
"#;
        let config: ServerConfig = toml::from_str(legacy).unwrap();
        assert!(!config.single_tenant);
        assert_eq!(
            config.org_base_url("alice").as_deref(),
            Some("https://alice.atomic.storage")
        );
    }

    #[test]
    fn global_config_dir_uses_env_override_verbatim() {
        // With the override set, the directory is used exactly as given.
        let override_dir = std::ffi::OsString::from("/some/test/config/dir");
        assert_eq!(
            global_config_dir_from(Some(override_dir)),
            Some(PathBuf::from("/some/test/config/dir"))
        );
    }

    #[test]
    fn global_config_dir_falls_back_to_home_when_unset() {
        // Without the override, it falls back to <home>/.atomic (matching the
        // pre-existing behavior). We only assert the `.atomic` suffix so the
        // test is host-independent.
        if let Some(dir) = global_config_dir_from(None) {
            assert!(
                dir.ends_with(".atomic"),
                "expected <home>/.atomic, got {dir:?}"
            );
        }
    }
}
