//! The main Push command implementation.
//!
//! This module contains the `Push` struct and its `Command` implementation,
//! which orchestrates the push operation from the CLI.
//!
//! Push is **identity-preserving**: it syncs the target view's full parent
//! chain (root → leaf) via view manifests instead of flattening a draft into
//! a shared remote view. Each view's scope, parent, and exact change log
//! survive the transfer.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use clap::Parser;

use atomic_core::types::{Base32, Hash, SetId};
use atomic_objects::{
    ObjectFamily, ObjectRecord, RefRecord, SyncPack, SyncWants, ViewScopeLabel, ViewSnapshot,
};
use atomic_remote::{HttpRemote, HttpRemoteConfig};
use atomic_repository::{Repository, ViewManifest};

use crate::commands::{find_repository_root, format_hash, Command};
use crate::error::{CliError, CliResult};
use crate::output::{
    create_progress_bar, create_spinner, finish_error, finish_success, hash as style_hash, hint,
    print_blank, print_hint, print_success, print_warning, success, view as style_view,
};

use super::helpers::{
    build_view_chain, convert_remote_error, display_manifest_divergence, format_count,
    load_change_data, load_change_message, parse_remote_manifest, plan_view_sync, ChainError,
    ViewSyncConflict, ViewSyncPlan,
};

/// The remote ref advertisement, fetched once per push via a single `/code`
/// sync call: each remote view's current snapshot object key (the CAS `prev`)
/// and its manifest reconstructed from that snapshot.
struct RemoteAdvertisement {
    /// remote view name → current view-snapshot object key.
    ref_targets: HashMap<String, String>,
    /// remote view name → manifest reconstructed from that snapshot.
    manifests: HashMap<String, ViewManifest>,
}

/// Parse a ref-advertisement [`SyncPack`] (from `sync_pull` with `refs_only`)
/// into per-view snapshot keys and manifests.
///
/// This replaces the legacy per-view `get_view_ref` + `get_object("views", …)`
/// round-trips: push negotiation now reads the remote's view state from a single
/// `/code` advertisement, uniform across local (`FsStore`) and geo (`S3Store`).
fn parse_advertisement(adv: &SyncPack, url: &str) -> CliResult<RemoteAdvertisement> {
    let mut snapshots: HashMap<String, ViewSnapshot> = HashMap::new();
    for obj in &adv.objects {
        if obj.family == ObjectFamily::View {
            if let Some(s) = ViewSnapshot::from_bytes(&obj.bytes) {
                snapshots.insert(obj.key.clone(), s);
            }
        }
    }
    let mut ref_targets = HashMap::new();
    let mut manifests = HashMap::new();
    for r in &adv.refs {
        ref_targets.insert(r.name.clone(), r.new_target.clone());
        match snapshots.get(&r.new_target) {
            Some(s) => {
                let manifest = parse_remote_manifest(&r.name, &s.to_manifest_text(&r.name), url)?;
                manifests.insert(r.name.clone(), manifest);
            }
            None => {
                // Ref points at a snapshot the advertisement did not carry;
                // treat the view as absent (a re-push heals it).
            }
        }
    }
    Ok(RemoteAdvertisement {
        ref_targets,
        manifests,
    })
}

/// Mint the content-addressed `ViewSnapshot` for a view from its local manifest
/// and the remote's current snapshot key (`prev`, for the CAS-on-`prev` chain).
///
/// The `own_set_id` fold matches the server's `from_manifest` bridge exactly, so
/// client and server mint byte-identical objects with identical content keys.
fn mint_view_snapshot(manifest: &ViewManifest, prev: Option<String>) -> ViewSnapshot {
    let own_changes: Vec<String> = manifest.changes.iter().map(|h| h.to_base32()).collect();
    let mut acc = SetId::ZERO;
    for h in &manifest.changes {
        acc = acc.add(h);
    }
    let own_set_id = acc.to_base32();
    let merkle_state = if manifest.changes.is_empty() {
        None
    } else {
        Some(manifest.state.to_base32())
    };
    let scope = if manifest.scope.is_draft() {
        ViewScopeLabel::Draft
    } else {
        ViewScopeLabel::Shared
    };
    ViewSnapshot::new(
        scope,
        manifest.parent.clone(),
        prev.into_iter().collect(),
        own_changes,
        own_set_id,
        merkle_state,
    )
}

// Constants

/// Default remote name when none is specified.
pub const DEFAULT_REMOTE: &str = "origin";

/// Default request timeout in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

// Push Command

/// Push changes to a remote repository.
///
/// Syncs the local view — and every ancestor in its parent chain — to the
/// remote via view manifests. Each view is transferred with its identity
/// intact: a draft stays a draft, parented on the same view, with the same
/// ordered change log. Change files are stored content-first (idempotent),
/// then the view's manifest is declared, so a view is never half-created.
///
/// # Remote Configuration
///
/// Remotes are configured in the repository's config file. The default
/// remote is "origin", but you can specify any configured remote or
/// provide a URL directly.
///
/// # View Mapping
///
/// By default, the local view is pushed to a view with the same name
/// on the remote. Use `--to-view` to declare the leaf view under a
/// different name on the remote; ancestors keep their own names.
///
/// # Examples
///
/// ```text
/// # Push to default remote (origin)
/// atomic push
///
/// # Push to a specific remote
/// atomic push upstream
///
/// # Push the leaf view under a different remote name
/// atomic push --to-view main
///
/// # Preview what would be pushed
/// atomic push --dry-run
///
/// # Attempt the push even if histories have diverged (server decides)
/// atomic push --force
/// ```
#[derive(Parser, Debug, Clone)]
#[command(name = "push")]
pub struct Push {
    /// Remote name or URL to push to.
    ///
    /// Can be a configured remote name (like "origin") or a full URL.
    ///
    /// Defaults to the configured remote, then `origin`.
    #[arg()]
    pub remote: Option<String>,

    /// Remote view name to declare the leaf view under.
    ///
    /// If not specified, uses the same name as the local view. Ancestor
    /// views in the parent chain always keep their own names.
    #[arg(long = "to-view")]
    pub to_view: Option<String>,

    /// Local view to push from.
    ///
    /// If not specified, uses the current view.
    #[arg(long = "from-view")]
    pub from_view: Option<String>,

    /// Show what would be pushed without actually pushing.
    ///
    /// Useful for previewing changes before pushing them.
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Attempt the push even if histories have diverged.
    ///
    /// Stores every local change the remote view lacks and declares the
    /// manifest anyway. The server remains authoritative: manifests only
    /// fast-forward, so a genuinely diverged remote will still reject the
    /// declare. Identity mismatches (scope/parent) are never forced.
    #[arg(short, long)]
    pub force: bool,

    /// Store all changes in the view's log, not just those missing on the remote.
    ///
    /// Useful for repairing a remote that is missing change files. Storing
    /// is content-addressed and idempotent.
    #[arg(short, long)]
    pub all: bool,

    /// Skip TLS certificate verification.
    ///
    /// Only use this for testing or with self-signed certificates.
    #[arg(short = 'k', long)]
    pub insecure: bool,

    /// Request timeout in seconds.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,

    /// Identity to use for authentication.
    ///
    /// Overrides the identity inferred from the remote URL subdomain and
    /// the `identity` field in the remote's config entry.  Must match a
    /// locally stored identity name (see `atomic identity list`).
    ///
    /// Example: `atomic push --identity alice-staging`
    #[arg(long)]
    pub identity: Option<String>,
}

/// Per-view sync work computed before any writes happen.
///
/// Built once per view in the chain; drives the dry-run preview and the
/// actual store/declare phase.
struct ViewSync {
    /// Name of the view on the remote (leaf may be renamed via `--to-view`).
    remote_name: String,

    /// The local manifest to declare (name already rewritten for the leaf).
    manifest: ViewManifest,

    /// The plan: what to store, whether to declare.
    plan: ViewSyncPlan,

    /// Changes that actually need storing (plan suffix minus hashes already
    /// known to be on the remote from earlier views in this push).
    to_store: Vec<Hash>,
}

impl Push {
    /// Create a new Push command with default settings.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::push::Push;
    ///
    /// let push = Push::new();
    /// assert!(push.remote.is_none());
    /// assert!(!push.dry_run);
    /// ```
    pub fn new() -> Self {
        Self {
            remote: None,
            to_view: None,
            from_view: None,
            dry_run: false,
            force: false,
            all: false,
            insecure: false,
            timeout: DEFAULT_TIMEOUT_SECS,
            identity: None,
        }
    }

    /// Builder: set the remote name or URL.
    pub fn with_remote(mut self, remote: impl Into<String>) -> Self {
        self.remote = Some(remote.into());
        self
    }

    /// Builder: set the remote view to push to.
    pub fn with_to_view(mut self, view: impl Into<String>) -> Self {
        self.to_view = Some(view.into());
        self
    }

    /// Builder: set the local view to push from.
    pub fn with_from_view(mut self, view: impl Into<String>) -> Self {
        self.from_view = Some(view.into());
        self
    }

    /// Use the explicit remote first, then the repository default.
    fn resolve_remote_name(&self, repo: &Repository) -> String {
        match &self.remote {
            Some(name) => name.clone(),
            None => repo
                .get_default_remote()
                .map(|(name, _entry)| name)
                .unwrap_or_else(|_| DEFAULT_REMOTE.to_string()),
        }
    }

    /// Builder: set the dry-run flag.
    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    /// Builder: set the force flag.
    pub fn with_force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    /// Builder: set the --all flag.
    pub fn with_all(mut self, all: bool) -> Self {
        self.all = all;
        self
    }

    /// Builder: set the insecure flag.
    pub fn with_insecure(mut self, insecure: bool) -> Self {
        self.insecure = insecure;
        self
    }

    /// Builder: set the timeout in seconds.
    pub fn with_timeout(mut self, timeout: u64) -> Self {
        self.timeout = timeout;
        self
    }

    /// Builder: set an explicit identity name override.
    pub fn with_identity(mut self, identity: impl Into<String>) -> Self {
        self.identity = Some(identity.into());
        self
    }

    /// Resolve the remote and optional identity override.
    fn resolve_remote_url(&self, repo: &Repository) -> CliResult<(String, String, Option<String>)> {
        let remote_name = self.resolve_remote_name(repo);

        // If it looks like a URL, use it directly
        if remote_name.contains("://") {
            return Ok((remote_name.clone(), remote_name, self.identity.clone()));
        }

        // Look up named remote in repository configuration
        match repo.get_remote(&remote_name) {
            Ok(entry) => Ok((remote_name, entry.url, self.identity.clone())),
            Err(atomic_repository::RepositoryError::RemoteNotFound { .. }) => {
                Err(CliError::RemoteNotFound { name: remote_name })
            }
            Err(e) => Err(CliError::Repository(e)),
        }
    }

    /// Get the local view name to push from.
    ///
    /// Returns the explicitly specified view or the repository's current view.
    fn get_local_view(&self, repo: &Repository) -> String {
        self.from_view
            .clone()
            .unwrap_or_else(|| repo.current_view().to_string())
    }

    /// Get the remote view name to declare the leaf view under.
    ///
    /// Returns the explicitly specified view or the local view name.
    fn get_remote_view(&self, local_view: &str) -> String {
        self.to_view
            .clone()
            .unwrap_or_else(|| local_view.to_string())
    }

    /// Build the HTTP remote configuration.
    ///
    /// `identity_hint` — an explicit identity name to use (from `--identity`).
    /// When `None`, identity is inferred from the remote URL and the global
    /// config's server bindings.
    async fn build_remote_config(
        &self,
        remote_url: &str,
        identity_hint: Option<&str>,
    ) -> HttpRemoteConfig {
        let config = HttpRemoteConfig::new()
            .with_timeout(Duration::from_secs(self.timeout))
            .danger_accept_invalid_certs(self.insecure);

        crate::commands::auth::attach_identity(config, remote_url, identity_hint).await
    }

    /// Display the dry run preview: per-view manifest sync plans.
    fn display_dry_run(
        &self,
        repo: &Repository,
        remote_name: &str,
        remote_url: &str,
        syncs: &[ViewSync],
    ) -> CliResult<()> {
        let active: Vec<&ViewSync> = syncs.iter().filter(|s| !s.plan.is_noop()).collect();

        if active.is_empty() {
            print_success("Already up to date - nothing to push");
            return Ok(());
        }

        println!(
            "Would sync {} to {}:",
            format_count(active.len(), "view"),
            remote_name
        );
        print_blank();

        for sync in &active {
            let scope = sync.manifest.scope.to_string();
            let parent = sync
                .manifest
                .parent
                .as_deref()
                .map(|p| format!(", parent {}", p))
                .unwrap_or_default();
            println!(
                "  {} [{}{}]: {} to store, declare manifest ({} in log)",
                style_view(&sync.remote_name),
                scope,
                parent,
                format_count(sync.to_store.len(), "change"),
                format_count(sync.manifest.changes.len(), "change"),
            );
            for hash in &sync.to_store {
                let msg =
                    load_change_message(repo, hash).unwrap_or_else(|| "(no message)".to_string());
                println!("    {} {}", style_hash(&format_hash(hash, false)), msg);
            }
        }

        print_blank();
        print_hint(&format!("Remote URL: {}", remote_url));

        Ok(())
    }

    /// Report a divergence conflict and build the error to return.
    fn divergence_error(
        &self,
        local_view: &str,
        local: &ViewManifest,
        remote: Option<&ViewManifest>,
        conflict: &ViewSyncConflict,
    ) -> CliError {
        match conflict {
            ViewSyncConflict::Diverged { .. } => {
                crate::output::print_error(&format!(
                    "View '{}' has diverged from the remote",
                    local_view
                ));
                print_blank();
                if let Some(remote) = remote {
                    display_manifest_divergence(local_view, local, remote);
                    print_blank();
                }
                print_hint("The remote view's log is not a prefix of your local log.");
                print_hint("Use 'atomic pull' to fetch remote changes, or");
                print_hint("Use 'atomic push --force' to attempt the push anyway (server decides)");
                CliError::Conflict {
                    description: format!("View '{}': {}", local_view, conflict),
                }
            }
            ViewSyncConflict::IdentityMismatch { .. } => {
                crate::output::print_error(&format!(
                    "View '{}' exists on the remote with a different identity",
                    local_view
                ));
                print_blank();
                print_hint(&format!("{}", conflict));
                print_hint("A view's scope and parent are fixed at creation and cannot be");
                print_hint("changed by a push. Rename the local view or push to a different");
                print_hint("remote view with '--to-view'.");
                CliError::Conflict {
                    description: format!("View '{}': {}", local_view, conflict),
                }
            }
        }
    }

    /// Async implementation of the push command.
    async fn run_async(&self) -> CliResult<()> {
        // Find and open repository
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        // Resolve remote name, URL, and identity hint
        let (remote_name, remote_url, identity_hint) = self.resolve_remote_url(&repo)?;

        // Determine views: the leaf we push, and its name on the remote.
        let local_view = self.get_local_view(&repo);
        let remote_view = self.get_remote_view(&local_view);

        // Print header
        println!(
            "Pushing to {} ({})",
            style_view(&remote_name),
            hint(&remote_url)
        );

        // Fail fast if we have no usable credentials for this remote. A push is
        // a write that always requires auth; without a credential the server's
        // negotiation endpoints return an ambiguous 404 (private projects are
        // deliberately masked), which surfaces as a misleading "view not found".
        // Catching the missing credential here gives the user an accurate,
        // actionable error instead.
        crate::commands::auth::check_push_credentials(&remote_url, identity_hint.as_deref())?;

        // Build the local ancestor chain, root → leaf. A draft's parent must
        // exist on the remote before the draft's manifest can reference it,
        // so the whole chain is synced in order.
        let chain = build_view_chain(&local_view, |name| {
            repo.get_view_info(name).map(|info| info.parent_name)
        })
        .map_err(|e| match e {
            ChainError::Cycle { view } => CliError::InvalidRepository {
                reason: format!("view parent chain contains a cycle at '{}'", view),
            },
            ChainError::Lookup(err) => CliError::Repository(err),
        })?;

        // Connect to remote
        let spinner = create_spinner("Connecting to remote...");
        let config = self
            .build_remote_config(&remote_url, identity_hint.as_deref())
            .await;
        let remote = HttpRemote::with_config(&remote_url, config).map_err(|e| {
            finish_error(&spinner, "Failed to connect");
            convert_remote_error(e, &remote_url)
        })?;
        finish_success(&spinner, "Connected");

        // Fetch remote view metadata only. Push does not import remote graph
        // nodes or mutate local view metadata: doing so would change the current
        // view's effective closure without materializing its working copy. The
        // server performs the local∪remote set union when publishing refs.
        let remote_names: Vec<String> = chain
            .iter()
            .map(|name| {
                if name == &local_view {
                    remote_view.clone()
                } else {
                    name.clone()
                }
            })
            .collect();
        let spinner = create_spinner("Fetching remote view metadata...");
        let adv_pack = remote
            .sync_pull(&SyncWants::advertise(remote_names.clone()))
            .await
            .map_err(|e| {
                finish_error(&spinner, "Failed to fetch remote metadata");
                convert_remote_error(e, &remote_url)
            })?;
        let adv = parse_advertisement(&adv_pack, &remote_url)?;
        finish_success(&spinner, "Fetched remote view metadata");

        // Plan phase: for each view in the chain, export the local manifest,
        // fetch the remote's, and compute the fast-forward suffix. Hashes
        // known to be on the remote (remote prefixes, or stored for an
        // earlier view in this push) are skipped when storing — a draft's
        // inherited prefix was already synced with its parent.
        let mut known_on_remote: HashSet<Hash> = HashSet::new();
        let mut syncs: Vec<ViewSync> = Vec::with_capacity(chain.len());

        for name in &chain {
            let is_leaf = name == &local_view;
            let remote_view_name = if is_leaf {
                remote_view.clone()
            } else {
                name.clone()
            };

            // Export the local view identity. For a renamed leaf
            // (`--to-view`) the manifest is declared under the remote name.
            let mut manifest = repo.view_manifest(name).map_err(CliError::Repository)?;
            if manifest.name != remote_view_name {
                manifest.name = remote_view_name.clone();
            }

            // The remote's manifest for this view comes from the single ref
            // advertisement fetched up front (no per-view round-trip).
            let remote_manifest = adv.manifests.get(&remote_view_name).cloned();
            println!(
                "  {} Remote {} {}",
                success("✓"),
                style_view(&remote_view_name),
                match &remote_manifest {
                    Some(m) => format!("has {}", format_count(m.changes.len(), "change")),
                    None => "does not exist yet".to_string(),
                }
            );

            // Everything the remote view already logs is known-present.
            if let Some(rm) = &remote_manifest {
                known_on_remote.extend(rm.changes.iter().copied());
            }

            let plan = plan_view_sync(&manifest, remote_manifest.as_ref(), self.force, is_leaf)
                .map_err(|conflict| {
                    self.divergence_error(name, &manifest, remote_manifest.as_ref(), &conflict)
                })?;

            if plan.forced {
                print_warning(&format!(
                    "View '{}' has diverged; pushing anyway (--force). The server may still reject it.",
                    remote_view_name
                ));
            }

            // With --all, re-store the full log (content-addressed and
            // idempotent) to repair a remote missing change files. Otherwise
            // store only the suffix, minus what's already known present.
            let candidates: &[Hash] = if self.all {
                &manifest.changes
            } else {
                &plan.suffix
            };
            let to_store: Vec<Hash> = candidates
                .iter()
                .filter(|h| self.all || !known_on_remote.contains(h))
                .copied()
                .collect();

            syncs.push(ViewSync {
                remote_name: remote_view_name,
                manifest,
                plan,
                to_store,
            });
        }

        // Handle dry run
        if self.dry_run {
            return self.display_dry_run(&repo, &remote_name, &remote_url, &syncs);
        }

        // Check for nothing to push
        if syncs.iter().all(|s| s.plan.is_noop()) {
            print_success("Already up to date");
            return Ok(());
        }

        // Sync phase: store change files, then declare each manifest,
        // root → leaf so a draft's parent always exists before the draft.
        let mut total_stored = 0usize;
        let mut views_declared = 0usize;
        let mut stored_hashes: Vec<Hash> = Vec::new();
        // One SyncPack for the whole push: objects (changes, view snapshots,
        // attestations, provenance, tags) + ref CAS moves, sent via `/code`.
        let mut pack = SyncPack::empty();

        for sync in &syncs {
            if sync.plan.is_noop() {
                if sync.plan.shrink {
                    // Ancestor view shrank locally but its remote copy is larger.
                    // We leave the remote intact (it already satisfies the view
                    // we're pushing) rather than implicitly rewriting shared
                    // history; the user can rewrite it deliberately.
                    print_warning(&format!(
                        "View '{}' is larger on the remote than locally; leaving it unchanged. \
                         Push '{}' directly with --force to rewrite it to your smaller set.",
                        sync.remote_name, sync.remote_name
                    ));
                } else {
                    println!(
                        "  {} {} already up to date",
                        success("✓"),
                        style_view(&sync.remote_name)
                    );
                }
                continue;
            }

            // Store the change files the remote lacks.
            if !sync.to_store.is_empty() {
                println!(
                    "Syncing view {} ({} new):",
                    style_view(&sync.remote_name),
                    format_count(sync.to_store.len(), "change")
                );

                let progress = create_progress_bar(sync.to_store.len() as u64, "Storing changes");

                for (i, hash) in sync.to_store.iter().enumerate() {
                    // Loaded fully into memory for now; a streamed `?store`
                    // for large changes is a known follow-up.
                    let data = load_change_data(&repo, hash)?;
                    let msg = load_change_message(&repo, hash)
                        .unwrap_or_else(|| "(no message)".to_string());

                    pack.objects.push(ObjectRecord::new(
                        ObjectFamily::Change,
                        hash.to_base32(),
                        data.to_vec(),
                    ));
                    println!(
                        "  {} {} ({}/{}) {}",
                        success("✓"),
                        style_hash(&format_hash(hash, false)),
                        i + 1,
                        sync.to_store.len(),
                        msg,
                    );
                    progress.inc(1);
                }

                finish_success(
                    &progress,
                    &format!("Stored {}", format_count(sync.to_store.len(), "change")),
                );

                total_stored += sync.to_store.len();
                stored_hashes.extend(sync.to_store.iter().copied());
            }

            // Mint the content-addressed snapshot and queue its object plus a
            // ref CAS move (`prev` = the remote's current tip from the
            // advertisement). The store and ancestry-gated CAS happen
            // server-side in the single `/code` push below.
            let prev = adv.ref_targets.get(&sync.remote_name).cloned();
            let snapshot = mint_view_snapshot(&sync.manifest, prev.clone());
            let snap_key = snapshot.content_key();
            pack.objects.push(ObjectRecord::new(
                ObjectFamily::View,
                snap_key.clone(),
                snapshot.to_canonical_bytes(),
            ));
            pack.refs.push(RefRecord {
                name: sync.remote_name.clone(),
                expect_old: prev,
                new_target: snap_key,
            });
            views_declared += 1;
            println!(
                "  {} Declared {} [{}] ({} in log)",
                success("✓"),
                style_view(&sync.remote_name),
                sync.manifest.scope,
                format_count(sync.manifest.changes.len(), "change"),
            );
        }

        // Sidecars may travel only when every covered change is in the closure
        // being published (the union of this view chain's own memberships), not
        // merely somewhere in the local common graph.
        let all_synced: HashSet<Hash> = syncs
            .iter()
            .flat_map(|sync| sync.manifest.changes.iter().copied())
            .collect();

        // Upload attestations that cover the pushed changes.
        // Attestations are graph-level audit nodes — they travel with
        // their dependencies but aren't part of any view's changelog.
        let mut attest_count = 0;
        {
            // Collect unique attestations — the same attestation can cover
            // multiple changes, so searching per-change produces duplicates.
            let mut seen_attest: HashSet<Hash> = HashSet::new();
            let mut unique_attestations: Vec<(
                Hash,
                atomic_core::change::attestation::Attestation,
            )> = Vec::new();

            for pushed_hash in &stored_hashes {
                let attestations = repo
                    .find_attestations_for_change(pushed_hash)
                    .unwrap_or_default();

                for (attest_hash, attestation) in attestations {
                    if !seen_attest.insert(attest_hash) {
                        continue; // Already collected this attestation
                    }

                    // Only upload if all covered changes are on the remote
                    let all_covered = attestation
                        .changes_covered
                        .iter()
                        .all(|h| all_synced.contains(h));

                    if all_covered {
                        unique_attestations.push((attest_hash, attestation));
                    }
                }
            }

            for (attest_hash, attestation) in &unique_attestations {
                // Load the raw attestation bytes and queue them.
                let attest_data = match attestation.serialize() {
                    Ok(data) => data,
                    Err(_) => continue,
                };

                pack.objects.push(ObjectRecord::new(
                    ObjectFamily::Attest,
                    attest_hash.to_base32(),
                    attest_data,
                ));
                attest_count += 1;
                println!(
                    "  {} {} attestation ({}, {} covered)",
                    success("✓"),
                    style_hash(&format_hash(attest_hash, false)),
                    attestation.cost_display(),
                    attestation.change_count(),
                );
            }
        }

        // Upload provenance graphs that explain the pushed changes, via the
        // RESTful provenance object family: `HEAD /provenance/{key}` to skip
        // what the server already holds, `PUT /provenance/{key}` to store.
        let mut provenance_count = 0;
        {
            // Collect unique provenance graphs — same dedup logic as attestations.
            let mut seen_prov: HashSet<Hash> = HashSet::new();
            let mut unique_graphs: Vec<(Hash, atomic_core::change::ProvenanceGraph)> = Vec::new();

            for pushed_hash in &stored_hashes {
                let graphs = repo
                    .find_provenance_for_change(pushed_hash)
                    .unwrap_or_default();

                for (prov_hash, graph) in graphs {
                    if seen_prov.insert(prov_hash) {
                        unique_graphs.push((prov_hash, graph));
                    }
                }
            }

            for (prov_hash, graph) in &unique_graphs {
                // Only upload if all explained changes are on the remote
                let all_explained = graph
                    .changes_explained
                    .iter()
                    .all(|h| all_synced.contains(h));

                if !all_explained {
                    continue;
                }

                let prov_key = prov_hash.to_base32();

                // Load the raw provenance bytes and queue them. The server
                // dedupes idempotently, so there is no per-object presence probe.
                let prov_data = match graph.serialize() {
                    Ok(data) => data,
                    Err(_) => continue,
                };

                pack.objects.push(ObjectRecord::new(
                    ObjectFamily::Provenance,
                    prov_key,
                    prov_data,
                ));
                provenance_count += 1;
                println!(
                    "  {} {} provenance ({} nodes, {} changes)",
                    success("✓"),
                    style_hash(&format_hash(prov_hash, false)),
                    graph.node_count(),
                    graph.change_count(),
                );
            }
        }

        // Upload tags for the pushed leaf view. Tags live locally under the
        // local view name but are declared remotely under the (possibly
        // renamed) remote view name.
        let mut tag_count = 0;
        {
            let tags = repo.list_tags_for_view(&local_view).unwrap_or_default();

            for tag in &tags {
                let tag_hash = tag.content_hash();
                let tag_hash_str = tag_hash.to_base32();

                let tag_bytes = match atomic_repository::serialize_tag(tag) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        print_warning(&format!("Failed to serialize tag '{}': {}", tag.name, e));
                        continue;
                    }
                };

                pack.objects.push(ObjectRecord::new(
                    ObjectFamily::Tag,
                    tag_hash_str.clone(),
                    tag_bytes,
                ));
                tag_count += 1;
                println!(
                    "  {} {} tag '{}' ({})",
                    success("\u{2713}"),
                    style_hash(&tag_hash_str[..12]),
                    tag.name,
                    tag.kind,
                );
            }
        }

        // Send everything in one `/code` push: objects stored + refs CAS-moved.
        if !pack.is_empty() {
            let spinner = create_spinner("Pushing to remote...");
            remote.sync_push(&pack).await.map_err(|e| {
                finish_error(&spinner, "Push failed");
                convert_remote_error(e, &remote_url)
            })?;

            // Re-read final metadata only. A concurrent writer may have caused
            // the server to publish a larger union, which is valid. Push verifies
            // that every locally proposed patch is present remotely; it does not
            // import remote-only nodes or mutate the local closures.
            let final_pack = remote
                .sync_pull(&SyncWants::advertise(remote_names.clone()))
                .await
                .map_err(|e| {
                    finish_error(&spinner, "Push landed, but remote verification failed");
                    convert_remote_error(e, &remote_url)
                })?;
            let final_adv = parse_advertisement(&final_pack, &remote_url)?;
            for sync in &syncs {
                let final_manifest =
                    final_adv.manifests.get(&sync.remote_name).ok_or_else(|| {
                        CliError::RemoteError {
                            message: format!(
                                "Remote did not advertise pushed view '{}'",
                                sync.remote_name
                            ),
                            url: Some(remote_url.clone()),
                        }
                    })?;
                let final_set: HashSet<Hash> = final_manifest.changes.iter().copied().collect();
                if let Some(missing) = sync
                    .manifest
                    .changes
                    .iter()
                    .find(|hash| !final_set.contains(hash))
                {
                    finish_error(&spinner, "Push landed, but remote union is incomplete");
                    return Err(CliError::Conflict {
                        description: format!(
                            "Remote view '{}' does not contain proposed patch {}",
                            sync.remote_name,
                            missing.to_base32()
                        ),
                    });
                }
            }
            finish_success(
                &spinner,
                "Push complete; remote union contains proposed patches",
            );
        }

        // Summary
        print_blank();
        let mut summary = format!(
            "Push complete: {} synced ({} stored) to {}",
            format_count(views_declared, "view"),
            format_count(total_stored, "change"),
            remote_name
        );
        if attest_count > 0 {
            summary.push_str(&format!(
                ", {} synced",
                format_count(attest_count, "attestation")
            ));
        }
        if provenance_count > 0 {
            summary.push_str(&format!(
                ", {} synced",
                format_count(provenance_count, "provenance graph")
            ));
        }
        if tag_count > 0 {
            summary.push_str(&format!(", {} synced", format_count(tag_count, "tag")));
        }
        print_success(&summary);

        Ok(())
    }
}

impl Default for Push {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Push {
    /// Execute the push command.
    ///
    /// Syncs the current (or specified) view's parent chain to the remote
    /// via view manifests, then uploads attestations, provenance graphs,
    /// and tags that travel with the pushed changes.
    ///
    /// # Errors
    ///
    /// - `CliError::RepositoryNotFound` - Not in a repository
    /// - `CliError::RemoteNotFound` - Remote doesn't exist
    /// - `CliError::AuthenticationFailed` - Auth failure
    /// - `CliError::Conflict` - Histories diverged or view identity mismatch
    /// - `CliError::RemoteError` - Network/server error (including servers
    ///   that predate view-manifest support)
    fn run(&self) -> CliResult<()> {
        // Create async runtime for HTTP operations
        let runtime = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {}", e))
        })?;

        runtime.block_on(self.run_async())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // Constructor Tests

    #[test]
    fn test_push_new() {
        let push = Push::new();
        assert!(push.remote.is_none());
        assert!(push.to_view.is_none());
        assert!(push.from_view.is_none());
        assert!(!push.dry_run);
        assert!(!push.force);
        assert!(!push.all);
        assert!(!push.insecure);
        assert_eq!(push.timeout, DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn test_push_default() {
        let push = Push::default();
        assert!(push.remote.is_none());
    }

    // Builder Tests

    #[test]
    fn test_push_with_remote() {
        let push = Push::new().with_remote("upstream");
        assert_eq!(push.remote, Some("upstream".to_string()));
    }

    #[test]
    fn test_push_with_remote_url() {
        let push = Push::new().with_remote("https://api.example.com/repo");
        assert_eq!(
            push.remote,
            Some("https://api.example.com/repo".to_string())
        );
    }

    #[test]
    fn test_push_with_to_view() {
        let push = Push::new().with_to_view("production");
        assert_eq!(push.to_view, Some("production".to_string()));
    }

    #[test]
    fn test_push_with_from_view() {
        let push = Push::new().with_from_view("feature");
        assert_eq!(push.from_view, Some("feature".to_string()));
    }

    #[test]
    fn test_push_with_dry_run() {
        let push = Push::new().with_dry_run(true);
        assert!(push.dry_run);

        let push = Push::new().with_dry_run(false);
        assert!(!push.dry_run);
    }

    #[test]
    fn test_push_with_force() {
        let push = Push::new().with_force(true);
        assert!(push.force);
    }

    #[test]
    fn test_push_with_all() {
        let push = Push::new().with_all(true);
        assert!(push.all);
    }

    #[test]
    fn test_push_with_insecure() {
        let push = Push::new().with_insecure(true);
        assert!(push.insecure);
    }

    #[test]
    fn test_push_with_timeout() {
        let push = Push::new().with_timeout(60);
        assert_eq!(push.timeout, 60);
    }

    #[test]
    fn test_push_builder_chain() {
        let push = Push::new()
            .with_remote("upstream")
            .with_to_view("main")
            .with_from_view("feature")
            .with_dry_run(true)
            .with_force(true)
            .with_all(true)
            .with_insecure(true)
            .with_timeout(120);

        assert_eq!(push.remote, Some("upstream".to_string()));
        assert_eq!(push.to_view, Some("main".to_string()));
        assert_eq!(push.from_view, Some("feature".to_string()));
        assert!(push.dry_run);
        assert!(push.force);
        assert!(push.all);
        assert!(push.insecure);
        assert_eq!(push.timeout, 120);
    }

    #[test]
    fn test_push_clone() {
        let push = Push::new().with_remote("origin").with_dry_run(true);
        let cloned = push.clone();

        assert_eq!(cloned.remote, push.remote);
        assert_eq!(cloned.dry_run, push.dry_run);
    }

    #[test]
    fn test_push_debug() {
        let push = Push::new().with_remote("origin");
        let debug_str = format!("{:?}", push);

        assert!(debug_str.contains("Push"));
        assert!(debug_str.contains("origin"));
    }

    // View Resolution Tests

    #[test]
    fn test_get_remote_view_explicit() {
        let push = Push::new().with_to_view("production");
        assert_eq!(push.get_remote_view("local"), "production");
    }

    #[test]
    fn test_get_remote_view_default() {
        let push = Push::new();
        assert_eq!(push.get_remote_view("local"), "local");
    }

    // Remote Config Tests

    #[tokio::test]
    async fn test_build_remote_config_default() {
        let push = Push::new();
        let config = push
            .build_remote_config("https://api.example.com/none", None)
            .await;

        assert_eq!(config.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert!(!config.danger_accept_invalid_certs);
    }

    #[tokio::test]
    async fn test_build_remote_config_custom_timeout() {
        let push = Push::new().with_timeout(60);
        let config = push
            .build_remote_config("https://api.example.com/none", None)
            .await;

        assert_eq!(config.timeout, Duration::from_secs(60));
    }

    #[tokio::test]
    async fn test_build_remote_config_insecure() {
        let push = Push::new().with_insecure(true);
        let config = push
            .build_remote_config("https://api.example.com/none", None)
            .await;

        assert!(config.danger_accept_invalid_certs);
    }

    // Constant Tests

    #[test]
    fn test_default_remote() {
        assert_eq!(DEFAULT_REMOTE, "origin");
    }

    #[test]
    fn test_default_timeout() {
        assert_eq!(DEFAULT_TIMEOUT_SECS, 30);
    }
}
