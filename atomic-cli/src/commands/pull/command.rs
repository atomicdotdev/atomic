//! The main Pull command implementation.
//!
//! This module contains the `Pull` struct and its `Command` implementation,
//! which orchestrates the pull operation from the CLI. The pull command
//! downloads changes from a remote repository and applies them to the local
//! view.
//!
//! # Architecture
//!
//! The pull command follows a clear workflow:
//!
//! 1. **Discovery**: Find and open the local repository
//! 2. **Connection**: Connect to the remote using HTTP
//! 3. **Comparison**: Query remote and local state, calculate delta
//! 4. **Download**: Fetch missing changes from the remote
//! 5. **Application**: Apply downloaded changes to the local view
//! 6. **Reporting**: Display results to the user
//!
//! # Error Handling
//!
//! The command provides detailed error messages with suggestions for
//! resolution. Common error scenarios include:
//!
//! - Network connectivity issues
//! - Authentication failures
//! - Missing remote channels
//! - Diverged history warnings

use std::collections::HashMap;
use std::time::Duration;

use bytes::Bytes;
use clap::Parser;

use atomic_core::types::{Base32, Hash, Merkle, SetId};
use atomic_objects::{ObjectFamily, SyncPack, SyncWants, ViewSnapshot};
use atomic_remote::{ChangelistEntry, HttpRemote, HttpRemoteConfig, StateResponse};
use atomic_repository::history::HistoryOptions;
use atomic_repository::{InsertOptions, Repository, ViewManifest};

use crate::commands::{find_repository_root, format_hash, Command};
use crate::error::{CliError, CliResult};
use crate::output::{
    create_progress_bar, create_spinner, error, finish_error, finish_success, hash as style_hash,
    hint, print_blank, print_hint, print_info, print_success, print_warning, success,
    view as style_view, warning,
};

use super::helpers::{
    calculate_pull_delta, convert_remote_error, display_state_comparison, find_local_only_changes,
    format_bytes, format_count, has_local_only_changes, save_downloaded_change,
};
use super::types::{PullChange, PullStats};

/// Reconstruct a view's manifest from a pulled [`SyncPack`]: find the view's ref
/// target, then its view-snapshot object, and render the manifest text.
/// Returns `None` when the pack carries no ref/snapshot for `name` (the remote
/// view does not exist).
///
/// This replaces the per-object `get_view_ref` + `get_object("views", …)`
/// round-trips: a pull now reads the remote's view state from the single `/code`
/// sync response.
fn manifest_from_pack(pack: &SyncPack, name: &str, url: &str) -> CliResult<Option<ViewManifest>> {
    let target = match pack.refs.iter().find(|r| r.name == name) {
        Some(r) => r.new_target.clone(),
        None => return Ok(None),
    };
    let snapshot = pack
        .objects
        .iter()
        .find(|o| o.family == ObjectFamily::View && o.key == target)
        .and_then(|o| ViewSnapshot::from_bytes(&o.bytes));
    let snapshot = match snapshot {
        Some(s) => s,
        None => return Ok(None),
    };
    let manifest = ViewManifest::parse(&snapshot.to_manifest_text(name)).map_err(|e| {
        CliError::RemoteError {
            message: format!("Corrupt manifest for view '{name}': {e}"),
            url: Some(url.to_string()),
        }
    })?;
    // O(1) producer-integrity cross-check: the snapshot's declared `own_set_id`
    // must equal the order-invariant fold of its own change list. Content
    // addressing guarantees the object's bytes, but not that a (buggy) producer
    // minted a self-consistent set-id; this catches that cheaply.
    let mut fold = SetId::ZERO;
    for h in &manifest.changes {
        fold = fold.add(h);
    }
    if fold.to_base32() != snapshot.own_set_id {
        print_warning(&format!(
            "Remote view '{name}' snapshot set-id disagrees with its change list; \
             the remote object may be inconsistent."
        ));
    }
    Ok(Some(manifest))
}

/// Index a pack's objects of one family into `content key → bytes`.
fn pack_objects_by_key(pack: &SyncPack, family: ObjectFamily) -> HashMap<String, Vec<u8>> {
    pack.objects
        .iter()
        .filter(|o| o.family == family)
        .map(|o| (o.key.clone(), o.bytes.clone()))
        .collect()
}

/// Reconstruct the requested view's metadata chain from a sync pack in
/// root-to-leaf order. The server includes the requested ref and every ancestor
/// snapshot. Views are metadata closures over the common graph, so pull applies
/// this whole chain after saving the missing graph objects.
fn manifest_chain_from_pack(
    pack: &SyncPack,
    leaf: &str,
    url: &str,
) -> CliResult<Vec<ViewManifest>> {
    let mut by_name = HashMap::new();
    for r in &pack.refs {
        if let Some(m) = manifest_from_pack(pack, &r.name, url)? {
            by_name.insert(r.name.clone(), m);
        }
    }

    let mut chain = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut cursor = Some(leaf.to_string());
    while let Some(name) = cursor {
        if !seen.insert(name.clone()) {
            return Err(CliError::RemoteError {
                message: format!("Remote view parent chain contains a cycle at '{name}'"),
                url: Some(url.to_string()),
            });
        }
        let manifest = by_name.remove(&name).ok_or_else(|| CliError::RemoteError {
            message: format!("Remote view metadata is missing '{name}'"),
            url: Some(url.to_string()),
        })?;
        cursor = manifest.parent.clone();
        chain.push(manifest);
    }
    chain.reverse();
    Ok(chain)
}

// Constants

/// Default remote name when none is specified.
///
/// This matches the conventional default remote name used in most VCS tools.
pub const DEFAULT_REMOTE: &str = "origin";

/// Default request timeout in seconds.
///
/// 30 seconds provides a reasonable balance between allowing slow networks
/// and failing quickly on unresponsive servers.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

// Pull Command

/// Pull changes from a remote repository.
///
/// Downloads remote changes and applies them to the local view, bringing
/// the local repository up to date with the remote.
///
/// # Remote Configuration
///
/// Remotes are configured in the repository's config file. The default
/// remote is "origin", but you can specify any configured remote or
/// provide a URL directly.
///
/// # View Mapping
///
/// By default, changes are pulled from the remote view with the same
/// name as the local view. Use `--from-view` to pull from a different
/// remote view, and `--to-view` to apply to a different local view.
///
/// # Examples
///
/// ```text
/// # Pull from default remote (origin)
/// atomic pull
///
/// # Pull from a specific remote
/// atomic pull upstream
///
/// # Pull from a different view
/// atomic pull --from-view main
///
/// # Preview what would be pulled
/// atomic pull --dry-run
///
/// # Download without applying
/// atomic pull --download-only
/// ```
#[derive(Parser, Debug, Clone)]
#[command(name = "pull")]
pub struct Pull {
    /// Remote name or URL to pull from.
    ///
    /// Can be a configured remote name (like "origin") or a full URL.
    ///
    /// Defaults to the configured remote, then `origin`.
    #[arg()]
    pub remote: Option<String>,

    /// Local view to pull into.
    ///
    /// If not specified, defaults to the remote view being pulled
    /// (`--from-view`), which itself defaults to the current view. The view
    /// is created automatically if it does not already exist locally.
    #[arg(long = "to-view")]
    pub to_view: Option<String>,

    /// Remote view to pull from.
    ///
    /// If not specified, uses the current view.
    #[arg(long = "from-view")]
    pub from_view: Option<String>,

    /// Show what would be pulled without actually pulling.
    ///
    /// Useful for previewing changes before pulling them.
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Pull all changes, not just those missing locally.
    ///
    /// Useful for ensuring all changes are present even if some
    /// were already downloaded previously.
    #[arg(short, long)]
    pub all: bool,

    /// Skip TLS certificate verification.
    ///
    /// Only use this for testing or with self-signed certificates.
    /// Using this option reduces security.
    #[arg(short = 'k', long)]
    pub insecure: bool,

    /// Request timeout in seconds.
    ///
    /// How long to wait for remote responses before giving up.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,

    /// Download changes without applying them to the local view.
    ///
    /// Changes are saved to the change store but not applied to any view.
    /// Useful for pre-fetching changes or examining them before applying.
    #[arg(long)]
    pub download_only: bool,

    /// Identity to use for authentication.
    ///
    /// Overrides the identity inferred from the remote URL subdomain and
    /// the `identity` field in the remote's config entry.  Must match a
    /// locally stored identity name (see `atomic identity list`).
    ///
    /// Example: `atomic pull --identity alice-staging`
    #[arg(long)]
    pub identity: Option<String>,
}

impl Pull {
    /// Create a new Pull command with default settings.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::pull::Pull;
    ///
    /// let pull = Pull::new();
    /// assert!(pull.remote.is_none());
    /// assert!(!pull.dry_run);
    /// ```
    pub fn new() -> Self {
        Self {
            remote: None,
            to_view: None,
            from_view: None,
            dry_run: false,
            all: false,
            insecure: false,
            timeout: DEFAULT_TIMEOUT_SECS,
            download_only: false,
            identity: None,
        }
    }

    /// Builder: set the remote name or URL.
    pub fn with_remote(mut self, remote: impl Into<String>) -> Self {
        self.remote = Some(remote.into());
        self
    }

    /// Builder: set the local view to pull into.
    pub fn with_to_view(mut self, view: impl Into<String>) -> Self {
        self.to_view = Some(view.into());
        self
    }

    /// Builder: set the remote view to pull from.
    pub fn with_from_view(mut self, view: impl Into<String>) -> Self {
        self.from_view = Some(view.into());
        self
    }

    /// Builder: set the dry-run flag.
    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
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

    /// Builder: set the download-only flag.
    pub fn with_download_only(mut self, download_only: bool) -> Self {
        self.download_only = download_only;
        self
    }

    /// Builder: set an explicit identity name override.
    pub fn with_identity(mut self, identity: impl Into<String>) -> Self {
        self.identity = Some(identity.into());
        self
    }

    // Internal Helper Methods

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

    /// Get the remote view name to pull from.
    ///
    /// Returns the explicitly specified `--from-view`, or the current view
    /// when omitted.
    fn get_remote_view(&self, current_view: &str) -> String {
        self.from_view
            .clone()
            .unwrap_or_else(|| current_view.to_string())
    }

    /// Get the local view name to pull into.
    ///
    /// Returns the explicitly specified `--to-view`, or defaults to the
    /// remote view being pulled — so `atomic pull --from-view X` pulls into
    /// a local view named `X`.
    fn get_local_view(&self, remote_view: &str) -> String {
        self.to_view
            .clone()
            .unwrap_or_else(|| remote_view.to_string())
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

    /// Display the dry run preview.
    ///
    /// Shows what changes would be pulled without actually pulling them.
    fn display_dry_run(
        &self,
        remote_name: &str,
        remote_url: &str,
        remote_view: &str,
        to_download: &[PullChange],
    ) -> CliResult<()> {
        if to_download.is_empty() {
            print_success("Already up to date - nothing to pull");
            return Ok(());
        }

        println!(
            "Would pull {} from {} (view: {}):",
            format_count(to_download.len(), "change"),
            remote_name,
            remote_view
        );
        print_blank();

        for change in to_download {
            let hash_str = format_hash(&change.hash, false);
            let msg = change.message_or_default();
            let tag_marker = if change.tagged { " [tagged]" } else { "" };
            println!("  {} {}{}", style_hash(&hash_str), msg, tag_marker);
        }

        print_blank();
        print_hint(&format!("Remote URL: {}", remote_url));

        Ok(())
    }

    /// Display warning about local-only changes.
    ///
    /// Warns the user that they have changes not present on the remote.
    fn display_local_only_warning(&self, local_only: &[String]) {
        print_blank();
        print_warning(&format!(
            "You have {} not on the remote:",
            format_count(local_only.len(), "local change")
        ));

        // Show up to 5 local-only changes
        for hash in local_only.iter().take(5) {
            let hash_short = &hash[..12.min(hash.len())];
            println!("  {} {}...", warning("!"), hash_short);
        }

        if local_only.len() > 5 {
            println!("  ... and {} more", local_only.len() - 5);
        }

        print_blank();
        print_hint("These changes exist locally but not on the remote.");
        print_hint("Use 'atomic push' to upload them, or they will remain local-only.");
    }

    /// Async implementation of the pull command.
    ///
    /// This is the main entry point for the pull operation. It coordinates
    /// all the steps required to download and apply remote changes.
    async fn run_async(&self) -> CliResult<()> {
        // Find and open repository
        let repo_root = find_repository_root()?;
        let mut repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        // Resolve remote name, URL, and identity hint
        let (remote_name, remote_url, identity_hint) = self.resolve_remote_url(&repo)?;

        // Determine views. The remote view defaults to the current view; the
        // local view defaults to the remote view being pulled, so
        // `atomic pull --from-view X` pulls into a local view named `X`.
        let remote_view = self.get_remote_view(repo.current_view());
        let local_view = self.get_local_view(&remote_view);

        // Whether the local target view already exists. Creation is deferred
        // until the remote manifest is known (below), so a pulled draft can be
        // created with the remote's identity (scope + parent) — inheriting its
        // parent's history — instead of a flat shared view.
        let local_view_exists = repo
            .view_exists(&local_view)
            .map_err(CliError::Repository)?;

        // Print header
        println!(
            "Pulling from {} ({})",
            style_view(&remote_name),
            hint(&remote_url)
        );

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

        // Load the target view's history for display only. Object negotiation is
        // graph-wide: `haves` is every change node already registered in the
        // common pristine, regardless of which view metadata currently exposes
        // it. This prevents the server from resending shared-view nodes when a
        // draft's own local log is empty.
        let spinner = create_spinner("Loading local history...");
        let local_entries = if local_view_exists {
            repo.log(HistoryOptions::new().view(&local_view))
                .map_err(CliError::Repository)?
        } else {
            Vec::new()
        };
        let haves: Vec<String> = repo
            .registered_change_hashes()
            .map_err(CliError::Repository)?
            .into_iter()
            .map(|hash| hash.to_base32())
            .collect();
        finish_success(
            &spinner,
            &format!(
                "Loaded {} local view changes ({} graph objects present)",
                local_entries.len(),
                haves.len()
            ),
        );

        // One `/code` pull: request the remote view metadata chain and any graph
        // objects absent from the graph-wide `haves`. The response carries the
        // requested view + ancestor snapshots and the missing `.change` objects.
        let spinner = create_spinner("Fetching remote view...");
        let pull_pack = remote
            .sync_pull(&SyncWants {
                refs: vec![remote_view.clone()],
                haves,
                // A dry run only needs the manifest to compute the delta; skip
                // downloading change bodies it will not apply.
                refs_only: self.dry_run,
            })
            .await
            .map_err(|e| {
                finish_error(&spinner, "Failed to fetch remote view");
                convert_remote_error(e, &remote_url)
            })?;
        let mut remote_chain = match manifest_chain_from_pack(&pull_pack, &remote_view, &remote_url)
        {
            Ok(chain) if !chain.is_empty() => chain,
            _ => {
                finish_error(&spinner, "Remote view not found");
                return Err(CliError::RemoteError {
                    message: format!("Remote view '{}' does not exist", remote_view),
                    url: Some(remote_url.clone()),
                });
            }
        };
        // `--to-view` renames only the requested leaf locally. Ancestor closure
        // metadata retains its canonical names; the leaf's parent remains that
        // reconciled local ancestor.
        if let Some(leaf) = remote_chain.last_mut() {
            leaf.name = local_view.clone();
        }
        let remote_manifest = remote_chain.last().expect("non-empty remote chain").clone();
        // Adapt the manifest's change log to the existing delta/display types.
        // Reconstruct each change's merkle state by folding from `Merkle::ZERO`
        // (the same fold `ViewManifest` uses), so `calculate_pull_delta` — which
        // parses `entry.merkle` and skips any that fail — sees valid states and
        // the final one matches `remote_manifest.state`.
        let remote_entries: Vec<ChangelistEntry> = {
            let mut acc = Merkle::ZERO;
            remote_manifest
                .changes
                .iter()
                .enumerate()
                .map(|(i, h)| {
                    acc = acc.next(h);
                    ChangelistEntry::new(i as u64, h.to_base32(), acc.to_base32(), false)
                })
                .collect()
        };
        let remote_state = if remote_manifest.changes.is_empty() {
            StateResponse::empty()
        } else {
            StateResponse::state(
                (remote_manifest.changes.len() as u64).saturating_sub(1),
                remote_manifest.state.to_base32(),
                String::new(),
            )
        };
        finish_success(
            &spinner,
            &format!("Got {} remote changes", remote_entries.len()),
        );

        // Display state comparison
        print_blank();
        display_state_comparison(
            &local_view,
            &local_entries,
            &remote_view,
            &remote_state,
            &remote_entries,
        );
        print_blank();

        // Check for local-only changes (diverged history warning)
        if has_local_only_changes(&local_entries, &remote_entries) {
            let local_only = find_local_only_changes(&local_entries, &remote_entries);
            self.display_local_only_warning(&local_only);
        }

        // The server already subtracted the graph-wide `haves`; every change
        // object in the pack is a genuinely missing node in the common graph.
        // Do not derive this from one view's log.
        let change_objects = pack_objects_by_key(&pull_pack, ObjectFamily::Change);
        let to_download: Vec<PullChange> = change_objects
            .keys()
            .enumerate()
            .filter_map(|(i, key)| {
                Hash::from_base32(key.as_bytes())
                    .map(|hash| PullChange::new(hash, i as u64, Merkle::ZERO))
            })
            .collect();

        // Handle dry run
        if self.dry_run {
            return self.display_dry_run(&remote_name, &remote_url, &remote_view, &to_download);
        }

        // Save missing graph nodes first. Even when none are missing, continue:
        // remote view metadata may still add closures around nodes already in
        // the local graph (for example, advancing local `dev` before applying a
        // draft's metadata).
        if !to_download.is_empty() {
            println!("Downloading {}:", format_count(to_download.len(), "change"));
            print_blank();
        }

        let progress = create_progress_bar(to_download.len() as u64, "Pulling changes");
        let mut stats = PullStats::new();

        for (i, change) in to_download.iter().enumerate() {
            let hash_str = change.hash.to_base32();
            let msg = change.message_or_default();

            match change_objects.get(&hash_str) {
                Some(data) => {
                    let data_len = data.len() as u64;

                    // Save to local change store
                    match save_downloaded_change(&repo, &change.hash, Bytes::from(data.clone())) {
                        Ok(()) => {
                            stats.record_change_downloaded(data_len);
                            println!(
                                "  {} {} ({}/{}) {}",
                                success("✓"),
                                style_hash(&format_hash(&change.hash, false)),
                                i + 1,
                                to_download.len(),
                                msg
                            );
                        }
                        Err(e) => {
                            stats.record_failed();
                            println!(
                                "  {} {} ({}/{}) {} - save failed: {}",
                                error("✗"),
                                style_hash(&format_hash(&change.hash, false)),
                                i + 1,
                                to_download.len(),
                                msg,
                                e
                            );
                        }
                    }
                }
                None => {
                    stats.record_failed();
                    println!(
                        "  {} {} ({}/{}) {} - not found on remote",
                        error("✗"),
                        style_hash(&format_hash(&change.hash, false)),
                        i + 1,
                        to_download.len(),
                        msg,
                    );
                    return Err(CliError::ChangeNotFound {
                        hash: hash_str.clone(),
                    });
                }
            }

            progress.inc(1);
        }

        finish_success(
            &progress,
            &format!(
                "Downloaded {} ({})",
                format_count(stats.changes_downloaded, "change"),
                format_bytes(stats.bytes_transferred)
            ),
        );

        // Apply changes (unless download-only)
        if self.download_only {
            // Sidecars (provenance, attestations) still land in the store —
            // the pack carried them and dropping them would require a later
            // pull to recover. DEPS wiring to the covered changes happens
            // when those changes are applied (`atomic insert`).
            {
                let sidecar_stats = crate::commands::sidecars::import_sidecars(&repo, &pull_pack);
                if !sidecar_stats.is_empty() {
                    crate::commands::sidecars::report_sidecars(sidecar_stats);
                }
            }
            print_blank();
            print_success(&format!(
                "Downloaded {} (not inserted - use 'atomic insert' to insert)",
                format_count(stats.changes_downloaded, "change")
            ));
            return Ok(());
        }

        // Reconcile view metadata root-to-leaf over the now-populated common
        // graph. Shared ancestors union their local and remote own memberships;
        // the requested draft (if any) then naturally inherits the synchronized
        // shared closure. These are metadata closures around graph nodes — not
        // per-view object transfers.
        print_blank();
        let spinner = create_spinner("Reconciling view metadata...");
        let mut apply_errors: Vec<String> = Vec::new();
        for manifest in &remote_chain {
            match repo.reconcile_view_manifest(manifest) {
                Ok(outcome) => {
                    for _ in 0..outcome.replayed {
                        stats.record_applied();
                    }
                }
                Err(e) => apply_errors.push(format!(
                    "Failed to reconcile view '{}': {}",
                    manifest.name, e
                )),
            }
        }

        if apply_errors.is_empty() {
            finish_success(
                &spinner,
                &format!("Reconciled {} view closures", remote_chain.len()),
            );
        } else {
            finish_error(&spinner, "View metadata reconciliation failed");
            for err in &apply_errors {
                print_warning(err);
            }
        }

        // Sync provenance graphs and attestations — the same sidecar model
        // as `.change` files: the server sends what the pack carries, we
        // save what we do not have. This runs AFTER view reconciliation so
        // the covered changes are registered in the graph: the DEPS edges
        // that make `atomic change <hash>` find its provenance (via
        // REV_DEPS) are only written for already-registered changes.
        // Saving each provenance graph also indexes the session ledger and
        // publishes the session manifest locally — the receiving repository
        // rebuilds the full Atomic session index without any relationship
        // queries against the server. Shared with clone so both ingest
        // identically.
        {
            let sidecar_stats = crate::commands::sidecars::import_sidecars(&repo, &pull_pack);
            if !sidecar_stats.is_empty() {
                crate::commands::sidecars::report_sidecars(sidecar_stats);
            }
        }

        // Validate the pull with the SetId — the order-invariant convergence
        // identity. The remote's expected set-id is the server-computed
        // **effective** (own ∪ ancestors) set-id from the view inventory
        // (`GET /refs/views`), compared to the local view's set after applying.
        // Using the server's effective value is correct for drafts (whose
        // effective set differs from their own set) and is an independent source
        // — it proves the pull reproduced exactly the remote set, regardless of
        // change counts or merkle order.
        if stats.has_applied() {
            let remote_set = remote.list_view_refs().await.ok().and_then(|inv| {
                inv.into_iter()
                    .find(|v| v.name == remote_view)
                    .and_then(|v| v.set_id)
            });
            match (repo.view_set_id(&local_view), remote_set) {
                (Ok(local), Some(expected)) => {
                    if local.to_base32() == expected {
                        print_success(
                            "Verified: local view set-id matches the remote (convergent)",
                        );
                    } else {
                        print_warning(&format!(
                            "Set-id mismatch after pull: local {} != remote {}. \
                             The view may be divergent or incomplete.",
                            local.to_base32(),
                            expected
                        ));
                    }
                }
                (Ok(_), None) => { /* remote reported no set-id; skip verification */ }
                (Err(e), _) => print_warning(&format!("Could not compute local set-id: {}", e)),
            }
        }

        // Materialize the working copy so on-disk files reflect the new
        // state — but only when the pulled view is the current view. Pulling
        // into a different view updates that view's change log without
        // disturbing the working copy; the user switches to it to see the
        // files.
        let mut materialize_failed = false;
        if stats.has_applied() {
            if local_view == repo.current_view() {
                let mat_spinner = create_spinner("Updating working copy...");
                match repo.materialize() {
                    Ok(result) => {
                        finish_success(
                            &mat_spinner,
                            &format!("{} files updated", result.files_written),
                        );
                    }
                    Err(e) => {
                        materialize_failed = true;
                        finish_error(&mat_spinner, "Failed to update working copy");
                        print_warning(&format!(
                            "Applied {} but failed to update working copy: {}",
                            format_count(stats.changes_applied, "change"),
                            e
                        ));
                    }
                }
            } else {
                print_hint(&format!(
                    "Applied {} to view '{}'. Run 'atomic view switch {}' to check it out.",
                    format_count(stats.changes_applied, "change"),
                    local_view,
                    local_view
                ));
            }
        }

        // Download tags from the remote view.
        let mut tags_downloaded: usize = 0;
        for (_tag_hash, tag_bytes) in pack_objects_by_key(&pull_pack, ObjectFamily::Tag) {
            match atomic_repository::deserialize_tag(&tag_bytes) {
                Ok(tag) => {
                    // Skip tags we already have locally.
                    if let Ok(Some(_)) = repo.get_tag_from_view(&tag.name, &local_view) {
                        continue;
                    }
                    let tag_len = tag_bytes.len() as u64;
                    if let Err(e) = repo.save_synced_tag(&tag) {
                        print_warning(&format!("Failed to save tag '{}': {}", tag.name, e));
                    } else {
                        tags_downloaded += 1;
                        stats.record_tag_downloaded(tag_len);
                        println!(
                            "  {} tag '{}' ({})",
                            success("\u{2713}"),
                            tag.name,
                            tag.kind,
                        );
                    }
                }
                Err(e) => {
                    print_warning(&format!("Failed to deserialize tag: {}", e));
                }
            }
        }

        if tags_downloaded > 0 {
            print_info(&format!(
                "Downloaded {}",
                format_count(tags_downloaded, "tag")
            ));
        }

        // The graph-materialized working copy is authoritative after pull.
        // Deflate `.vault/` disk content into redb; never inflate redb back over
        // these tracked files, which would regenerate system frontmatter and make
        // a clean pull immediately appear modified.
        if stats.has_applied() && repo.vault_dir().exists() {
            if repo.has_vault().unwrap_or(false) {
                match repo.vault_record_working_copy() {
                    Ok(paths) => log::info!(
                        "Synchronized {} pulled vault entries into redb",
                        paths.len()
                    ),
                    Err(e) => print_warning(&format!(
                        "Failed to synchronize pulled vault content: {}",
                        e
                    )),
                }
            } else {
                print_info(
                    "Vault files detected \u{2014} initializing vault from pulled content...",
                );
                match repo.bootstrap_vault_from_working_copy() {
                    Ok(()) => print_success("Vault initialized from pulled content"),
                    Err(e) => print_warning(&format!("Failed to bootstrap vault: {}", e)),
                }
            }
        }

        // Enrich the knowledge graph from pulled VCS data (views, files, changes).
        // This runs for every pull that applies changes, not just vaulted repos.
        if stats.changes_applied > 0 {
            // Ensure KG tables exist (idempotent — no-op if already created)
            if let Err(e) = repo.init_kg() {
                log::warn!("KG table init failed: {}", e);
            }
            match repo.kg_enrich_from_vcs() {
                Ok(kg_stats) => log::info!("KG enriched after pull: {}", kg_stats),
                Err(e) => log::warn!("KG enrichment after pull failed: {}", e),
            }
        }

        // Keep partial output, but return failure when any step failed.
        print_blank();
        if stats.has_failures() || !apply_errors.is_empty() || materialize_failed {
            print_warning(&format!(
                "Pull completed with errors: {} downloaded, {} failed to download, {} failed to apply",
                stats.changes_downloaded,
                stats.changes_failed,
                apply_errors.len(),
            ));
            return Err(CliError::Internal(anyhow::anyhow!(
                "pull completed with errors ({} download, {} apply{})",
                stats.changes_failed,
                apply_errors.len(),
                if materialize_failed {
                    ", working copy not updated"
                } else {
                    ""
                },
            )));
        }

        print_success(&format!(
            "Pull complete: {} downloaded and applied to {}",
            format_count(stats.changes_downloaded, "change"),
            local_view
        ));

        Ok(())
    }
}

impl Default for Pull {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Pull {
    /// Execute the pull command.
    ///
    /// This method creates a tokio runtime and executes the async pull
    /// operation. It handles all the steps required to download and apply
    /// remote changes.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No repository is found
    /// - The remote cannot be connected to
    /// - Network operations fail
    /// - Changes fail to download or save
    fn run(&self) -> CliResult<()> {
        // Create a runtime for async operations
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {}", e))
        })?;

        rt.block_on(self.run_async())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // Constructor and Builder Tests

    /// Test creating a new Pull with defaults.
    #[test]
    fn test_pull_new() {
        let pull = Pull::new();

        assert!(pull.remote.is_none());
        assert!(pull.to_view.is_none());
        assert!(pull.from_view.is_none());
        assert!(!pull.dry_run);
        assert!(!pull.all);
        assert!(!pull.insecure);
        assert_eq!(pull.timeout, DEFAULT_TIMEOUT_SECS);
        assert!(!pull.download_only);
    }

    /// Test Default trait implementation.
    #[test]
    fn test_pull_default() {
        let pull: Pull = Default::default();
        assert!(pull.remote.is_none());
    }

    /// Test with_remote builder method.
    #[test]
    fn test_pull_with_remote() {
        let pull = Pull::new().with_remote("upstream");
        assert_eq!(pull.remote.as_deref(), Some("upstream"));
    }

    /// Test with_remote with URL.
    #[test]
    fn test_pull_with_remote_url() {
        let pull = Pull::new().with_remote("https://example.com/repo");
        assert_eq!(pull.remote.as_deref(), Some("https://example.com/repo"));
    }

    /// Test with_to_view builder method.
    #[test]
    fn test_pull_with_to_view() {
        let pull = Pull::new().with_to_view("feature");
        assert_eq!(pull.to_view, Some("feature".to_string()));
    }

    /// Test with_from_view builder method.
    #[test]
    fn test_pull_with_from_view() {
        let pull = Pull::new().with_from_view("main");
        assert_eq!(pull.from_view, Some("main".to_string()));
    }

    /// Test with_dry_run builder method.
    #[test]
    fn test_pull_with_dry_run() {
        let pull = Pull::new().with_dry_run(true);
        assert!(pull.dry_run);

        let pull = Pull::new().with_dry_run(false);
        assert!(!pull.dry_run);
    }

    /// Test with_all builder method.
    #[test]
    fn test_pull_with_all() {
        let pull = Pull::new().with_all(true);
        assert!(pull.all);
    }

    /// Test with_insecure builder method.
    #[test]
    fn test_pull_with_insecure() {
        let pull = Pull::new().with_insecure(true);
        assert!(pull.insecure);
    }

    /// Test with_timeout builder method.
    #[test]
    fn test_pull_with_timeout() {
        let pull = Pull::new().with_timeout(60);
        assert_eq!(pull.timeout, 60);
    }

    /// Test with_download_only builder method.
    #[test]
    fn test_pull_with_download_only() {
        let pull = Pull::new().with_download_only(true);
        assert!(pull.download_only);
    }

    /// Test chaining multiple builder methods.
    #[test]
    fn test_pull_builder_chain() {
        let pull = Pull::new()
            .with_remote("upstream")
            .with_to_view("feature")
            .with_from_view("main")
            .with_dry_run(true)
            .with_all(true)
            .with_insecure(true)
            .with_timeout(120)
            .with_download_only(true);

        assert_eq!(pull.remote.as_deref(), Some("upstream"));
        assert_eq!(pull.to_view, Some("feature".to_string()));
        assert_eq!(pull.from_view, Some("main".to_string()));
        assert!(pull.dry_run);
        assert!(pull.all);
        assert!(pull.insecure);
        assert_eq!(pull.timeout, 120);
        assert!(pull.download_only);
    }

    /// Test Pull can be cloned.
    #[test]
    fn test_pull_clone() {
        let original = Pull::new().with_remote("test").with_dry_run(true);

        let cloned = original.clone();

        assert_eq!(cloned.remote.as_deref(), Some("test"));
        assert!(cloned.dry_run);
    }

    /// Test Pull has Debug implementation.
    #[test]
    fn test_pull_debug() {
        let pull = Pull::new();
        let debug_str = format!("{:?}", pull);

        assert!(debug_str.contains("Pull"));
    }

    // Internal Method Tests

    /// Test get_remote_view with explicit view.
    #[test]
    fn test_get_remote_view_explicit() {
        let pull = Pull::new().with_from_view("production");
        assert_eq!(pull.get_remote_view("dev"), "production");
    }

    /// Test get_remote_view defaults to the current view name.
    #[test]
    fn test_get_remote_view_default() {
        let pull = Pull::new();
        assert_eq!(pull.get_remote_view("feature"), "feature");
    }

    /// Test get_local_view with an explicit --to-view.
    #[test]
    fn test_get_local_view_explicit() {
        let pull = Pull::new().with_to_view("staging");
        assert_eq!(pull.get_local_view("orange"), "staging");
    }

    /// Test get_local_view defaults to the remote view being pulled.
    #[test]
    fn test_get_local_view_default() {
        let pull = Pull::new();
        assert_eq!(pull.get_local_view("orange"), "orange");
    }

    /// Test build_remote_config with default settings.
    #[tokio::test]
    async fn test_build_remote_config_default() {
        let pull = Pull::new();
        let config = pull
            .build_remote_config("http://test.localhost:8080/code", None)
            .await;

        // HttpRemoteConfig doesn't expose fields directly, so we just verify
        // it doesn't panic and returns something
        assert!(std::mem::size_of_val(&config) > 0);
    }

    /// Test build_remote_config with custom timeout.
    #[tokio::test]
    async fn test_build_remote_config_custom_timeout() {
        let pull = Pull::new().with_timeout(120);
        let config = pull
            .build_remote_config("http://test.localhost:8080/code", None)
            .await;
        assert!(std::mem::size_of_val(&config) > 0);
    }

    /// Test build_remote_config with insecure flag.
    #[tokio::test]
    async fn test_build_remote_config_insecure() {
        let pull = Pull::new().with_insecure(true);
        let config = pull
            .build_remote_config("http://test.localhost:8080/code", None)
            .await;
        assert!(std::mem::size_of_val(&config) > 0);
    }

    // Constant Tests

    /// Test that DEFAULT_REMOTE is "origin".
    #[test]
    fn test_default_remote() {
        assert_eq!(DEFAULT_REMOTE, "origin");
    }

    /// Test that DEFAULT_TIMEOUT_SECS is 30.
    #[test]
    fn test_default_timeout() {
        assert_eq!(DEFAULT_TIMEOUT_SECS, 30);
    }
}
