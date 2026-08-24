//! The main Clone command implementation.
//!
//! This module contains the `Clone` struct and its `Command` implementation,
//! which orchestrates the clone operation from the CLI. The clone command
//! creates a new local repository by downloading all changes from a remote
//! repository.
//!
//! # Architecture
//!
//! The clone command follows a clear workflow:
//!
//! 1. **Parse URL**: Extract remote URL and infer repository name
//! 2. **Validate Target**: Ensure target directory doesn't exist
//! 3. **Create with Guard**: Create directory with cleanup on error
//! 4. **Initialize Repository**: Set up empty local repository
//! 5. **Connect to Remote**: Establish HTTP connection
//! 6. **Fetch View Manifests**: Get the requested view's manifest and walk
//!    its parent chain to the root
//! 7. **Download Changes**: Fetch the missing union of the chain's changes
//! 8. **Apply Manifests**: Apply root→leaf, preserving each view's
//!    scope and parent (unless download-only)
//! 9. **Configure Remote**: Save "origin" remote configuration
//! 10. **Report Results**: Display summary to user
//!
//! View reconstruction is manifest-based: each view arrives as a
//! [`ViewManifest`] carrying its full identity (name, scope, parent, ordered
//! change log, merkle state), so a draft cloned back is still a draft with
//! its parent intact. Servers that predate `?view-manifest` support cannot
//! be cloned from — that is a hard error, not a lossy fallback.
//!
//! # Error Handling
//!
//! The command provides detailed error messages with suggestions for
//! resolution. If the clone fails partway through, the `CleanupGuard`
//! ensures the partially created directory is removed.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use clap::Parser;
use git2::Repository as GitRepository;

use atomic_core::pristine::ViewScope;
use atomic_core::types::{Base32, Hash, SetId};
use atomic_objects::{ObjectFamily, SyncPack, SyncWants, ViewSnapshot};
use atomic_remote::{HttpRemote, HttpRemoteConfig, RemoteError};
use atomic_repository::{ManifestApplyOutcome, Repository, ViewManifest};

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{
    create_progress_bar, create_spinner, error, finish_error, finish_success, hash as style_hash,
    hint, print_blank, print_hint, print_info, print_success, print_warning, success,
    view as style_view,
};

use super::helpers::{
    change_union, classify_inventory, convert_remote_error, format_bytes, format_count,
    infer_repo_name, manifest_apply_order, parse_remote_manifest, resolve_target_path,
    save_downloaded_change, validate_target_path, CleanupGuard, InventoryOutcome,
};
use super::types::{ClonePhase, CloneProgress, CloneStats};

// Constants

/// Default view to clone when none is specified.
///
/// This is "dev" to match atomic-api convention.
pub const DEFAULT_VIEW: &str = "dev";

/// Default request timeout in seconds.
///
/// 30 seconds provides a reasonable balance between allowing slow networks
/// and failing quickly on unresponsive servers.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Temporary parking view used while deleting an init scaffold view.
///
/// `Repository::init` pre-creates a shared root view and leaves it current.
/// When the remote's manifest for that view declares a different identity
/// (draft, or a parent), the empty scaffold must be deleted and recreated —
/// but the current view cannot be deleted, so clone briefly parks on this
/// view and removes it again before finishing.
const SCAFFOLD_PARK_VIEW: &str = "atomic-clone-scaffold-park";

// Clone Command

/// Clone a remote repository.
///
/// Creates a new local repository by downloading all changes from the
/// specified remote repository. The repository is initialized with the
/// remote configured as "origin".
///
/// # Remote URL Format
///
/// The URL can be in various formats:
///
/// - `https://example.com/org/project/code` - Standard atomic-api URL
/// - `https://example.com/tenant/t/portfolio/p/project/pr/code` - Full path
/// - `https://github.com/user/repo.git` - Git-style URL (for compatibility)
///
/// # Target Directory
///
/// By default, the repository name is inferred from the URL and used as
/// the target directory name. You can override this by providing an
/// explicit path.
///
/// # Examples
///
/// ```text
/// # Clone to inferred directory name
/// atomic clone https://example.com/org/project/code
///
/// # Clone to specific directory
/// atomic clone https://example.com/org/project/code my-project
///
/// # Clone specific view
/// atomic clone https://example.com/org/project/code --view dev
///
/// # Clone without applying changes
/// atomic clone https://example.com/org/project/code --download-only
/// ```
#[derive(Parser, Debug, Clone)]
#[command(name = "clone")]
pub struct Clone {
    /// URL of the repository to clone.
    ///
    /// Can be any valid atomic-api URL or compatible format.
    pub url: String,

    /// Directory to clone into.
    ///
    /// If not specified, the repository name is inferred from the URL.
    pub path: Option<String>,

    /// View to clone.
    ///
    /// If not specified, clones the "dev" view (atomic-api default).
    #[arg(long, default_value = DEFAULT_VIEW)]
    pub view: String,

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
    /// Useful for examining changes before applying.
    #[arg(long)]
    pub download_only: bool,

    /// Bootstrap Atomic in an existing Git checkout without materializing
    /// Atomic content over Git's working tree.
    #[arg(long, requires = "path", conflicts_with = "download_only")]
    pub into_existing: bool,

    /// Also clone every other view the remote exposes, not just `--view`.
    ///
    /// Each additional view is created locally and populated from the
    /// remote. Changes are content-addressed and shared across views, so
    /// this mostly adds view references without re-downloading. Requires
    /// server support for the view-inventory endpoint; older servers are
    /// treated as single-view.
    #[arg(long, conflicts_with = "download_only")]
    pub all_views: bool,
}

fn validate_existing_git_checkout(path: &Path) -> CliResult<()> {
    if !path.is_dir() {
        return Err(CliError::InvalidPath {
            path: path.to_path_buf(),
            source: None,
        });
    }

    let git_repo = GitRepository::discover(path).map_err(|_| CliError::InvalidArgument {
        message: format!(
            "--into-existing requires an existing Git checkout: {}",
            path.display()
        ),
    })?;

    let workdir = git_repo
        .workdir()
        .ok_or_else(|| CliError::InvalidArgument {
            message: format!(
                "--into-existing requires a non-bare Git checkout: {}",
                path.display()
            ),
        })?;

    let target = std::fs::canonicalize(path).map_err(|e| CliError::InvalidPath {
        path: path.to_path_buf(),
        source: Some(e),
    })?;
    let workdir = std::fs::canonicalize(workdir).map_err(|e| CliError::InvalidPath {
        path: workdir.to_path_buf(),
        source: Some(e),
    })?;
    if target != workdir {
        return Err(CliError::InvalidArgument {
            message: format!(
                "--into-existing must target the Git worktree root: {}",
                path.display()
            ),
        });
    }

    if path.join(".atomic").exists() {
        return Err(CliError::RepositoryExists {
            path: path.join(".atomic"),
        });
    }

    Ok(())
}

impl Clone {
    /// Create a new Clone command with the given URL.
    ///
    /// # Arguments
    ///
    /// * `url` - The remote URL to clone from
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::Clone;
    ///
    /// let clone = Clone::new("https://example.com/repo".to_string());
    /// assert_eq!(clone.url, "https://example.com/repo");
    /// assert_eq!(clone.view, "dev");
    /// ```
    pub fn new(url: String) -> Self {
        Self {
            url,
            path: None,
            view: DEFAULT_VIEW.to_string(),
            insecure: false,
            timeout: DEFAULT_TIMEOUT_SECS,
            download_only: false,
            into_existing: false,
            all_views: false,
        }
    }

    /// Builder: set the target path.
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Builder: set the view name.
    pub fn with_view(mut self, view: impl Into<String>) -> Self {
        self.view = view.into();
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

    /// Builder: bootstrap an existing Git checkout.
    pub fn with_into_existing(mut self, into_existing: bool) -> Self {
        self.into_existing = into_existing;
        self
    }

    /// Builder: clone all remote views, not just the primary one.
    pub fn with_all_views(mut self, all_views: bool) -> Self {
        self.all_views = all_views;
        self
    }

    // Internal Helper Methods

    /// Build the HTTP remote configuration.
    ///
    /// Creates an `HttpRemoteConfig` with the timeout and security settings
    /// specified by the user.
    async fn build_remote_config(&self) -> HttpRemoteConfig {
        let config = HttpRemoteConfig::new()
            .with_timeout(Duration::from_secs(self.timeout))
            .danger_accept_invalid_certs(self.insecure);

        // Clone uses the URL directly — identity inferred from subdomain only.
        crate::commands::auth::attach_identity(config, &self.url, None).await
    }

    /// Get the display name for the repository.
    ///
    /// Returns the inferred name from URL or the provided path.
    fn get_display_name(&self) -> String {
        self.path
            .clone()
            .or_else(|| infer_repo_name(&self.url))
            .unwrap_or_else(|| "repo".to_string())
    }

    /// Convert a manifest-fetch failure into a CLI error.
    ///
    /// A protocol error means the server predates `?view-manifest` support.
    /// That is fatal: without manifests a clone cannot preserve view
    /// identity (scope and parent), so there is no fallback to the old flat
    /// changelist path.
    fn manifest_fetch_error(&self, err: RemoteError) -> CliError {
        if matches!(err, RemoteError::ProtocolError { .. }) {
            CliError::RemoteError {
                message: "This server is too old for identity-preserving clone: it does not \
                          support view manifests (?view-manifest). Upgrade the server."
                    .to_string(),
                url: Some(self.url.clone()),
            }
        } else {
            convert_remote_error(err, &self.url)
        }
    }

    /// Fetch and validate one view's manifest from the remote.
    ///
    /// Returns `Ok(None)` if the remote does not have the view.
    /// Reconstruct one view's manifest from a pulled [`SyncPack`]: find its ref
    /// target, then its view-snapshot object, and render the manifest text.
    /// Returns `Ok(None)` when the pack carries no ref/snapshot for `view`.
    ///
    /// This replaces the per-object `get_view_ref` + `get_object("views", …)`
    /// round-trips: clone reads every view's state from the single `/code` pull.
    fn view_manifest_from_pack(
        &self,
        pack: &SyncPack,
        view: &str,
    ) -> CliResult<Option<ViewManifest>> {
        let target = match pack.refs.iter().find(|r| r.name == view) {
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
        let manifest = parse_remote_manifest(view, &snapshot.to_manifest_text(view), &self.url)?;
        // O(1) producer-integrity cross-check: the snapshot's declared
        // `own_set_id` must equal the order-invariant fold of its own change
        // list (content addressing guarantees the bytes, not a self-consistent
        // producer).
        let mut fold = SetId::ZERO;
        for h in &manifest.changes {
            fold = fold.add(h);
        }
        if fold.to_base32() != snapshot.own_set_id {
            print_warning(&format!(
                "Remote view '{view}' snapshot set-id disagrees with its change list; \
                 the remote object may be inconsistent."
            ));
        }
        Ok(Some(manifest))
    }

    /// Verify convergence with the order-invariant `SetId`: each applied view's
    /// local **effective** set-id must equal the remote's, taken from the server
    /// view inventory (`GET /refs/views`), which the server folds over each
    /// view's effective (own ∪ ancestors) change set. Comparing against the
    /// server-computed value — an independent source — catches a dropped or
    /// corrupt change that counts/merkle-order alone would miss, and is correct
    /// for drafts (whose effective set differs from their own set).
    async fn verify_convergence(
        &self,
        repo: &Repository,
        remote: &HttpRemote,
        views: &HashSet<String>,
    ) {
        let inventory = match remote.list_view_refs().await {
            Ok(inv) => inv,
            // Inventory unavailable — skip verification rather than fail the clone.
            Err(_) => return,
        };
        let remote_set: HashMap<String, String> = inventory
            .into_iter()
            .filter_map(|v| v.set_id.map(|s| (v.name, s)))
            .collect();

        let mut verified = 0usize;
        let mut mismatched = 0usize;
        for view in views {
            let Some(expected) = remote_set.get(view) else {
                continue; // remote reported no set-id for this view
            };
            match repo.view_set_id(view) {
                Ok(local) if &local.to_base32() == expected => verified += 1,
                Ok(local) => {
                    mismatched += 1;
                    print_warning(&format!(
                        "Set-id mismatch for view '{}': local {} != remote {}. \
                         The clone may be incomplete or divergent.",
                        view,
                        local.to_base32(),
                        expected
                    ));
                }
                Err(e) => print_warning(&format!(
                    "Could not compute local set-id for view '{}': {}",
                    view, e
                )),
            }
        }
        if mismatched == 0 && verified > 0 {
            print_success("Verified: cloned view set-ids match the remote (convergent)");
        }
    }

    /// Index a pack's change objects into `base32 hash → bytes` for local save.
    fn change_objects_from_pack(pack: &SyncPack) -> HashMap<String, Vec<u8>> {
        pack.objects
            .iter()
            .filter(|o| o.family == ObjectFamily::Change)
            .map(|o| (o.key.clone(), o.bytes.clone()))
            .collect()
    }

    /// Fetch the requested view's manifest and walk its parent chain up to
    /// the root, returning the manifests in root→leaf apply order.
    ///
    /// Returns `Ok(None)` when the requested view does not exist on the
    /// remote. A declared parent that is missing remotely, or a parent chain
    /// that loops, means the remote's view metadata is corrupted and is a
    /// hard error.
    fn fetch_manifest_chain(&self, pack: &SyncPack) -> CliResult<Option<Vec<ViewManifest>>> {
        let Some(leaf) = self.view_manifest_from_pack(pack, &self.view)? else {
            return Ok(None);
        };

        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(leaf.name.clone());
        let mut next_parent = leaf.parent.clone();
        let mut chain = vec![leaf];

        while let Some(parent) = next_parent {
            if !visited.insert(parent.clone()) {
                return Err(CliError::RemoteError {
                    message: format!(
                        "View parent chain loops at '{}' — the remote's view metadata is corrupted",
                        parent
                    ),
                    url: Some(self.url.clone()),
                });
            }
            let child_name = chain
                .last()
                .map(|m| m.name.clone())
                .unwrap_or_else(|| self.view.clone());
            let manifest = self
                .view_manifest_from_pack(pack, &parent)?
                .ok_or_else(|| CliError::RemoteError {
                    message: format!(
                        "View '{}' declares parent '{}', but the remote has no such view",
                        child_name, parent
                    ),
                    url: Some(self.url.clone()),
                })?;
            next_parent = manifest.parent.clone();
            chain.push(manifest);
        }

        // Collected leaf→root; parents must be applied first.
        chain.reverse();
        Ok(Some(chain))
    }

    /// Download every change in `missing` and save it to the change store.
    ///
    /// Stops on the first download failure to maintain consistency (the
    /// manifest apply would fail on the missing change anyway).
    fn download_missing_changes(
        &self,
        repo: &Repository,
        change_objects: &HashMap<String, Vec<u8>>,
        missing: &[Hash],
        stats: &mut CloneStats,
        progress: &mut CloneProgress,
    ) -> CliResult<()> {
        if missing.is_empty() {
            return Ok(());
        }

        print_blank();
        println!("Downloading {}:", format_count(missing.len(), "change"));
        print_blank();

        let progress_bar = create_progress_bar(missing.len() as u64, "Downloading changes");

        for (i, hash) in missing.iter().enumerate() {
            let hash_str = hash.to_base32();
            let hash_display = &hash_str[..12.min(hash_str.len())];

            match change_objects.get(&hash_str) {
                Some(data) => {
                    let data_len = data.len() as u64;

                    // Save to local change store
                    match save_downloaded_change(repo, hash, Bytes::from(data.clone())) {
                        Ok(()) => {
                            stats.record_change_downloaded(data_len);
                            progress.record_downloaded();
                            println!(
                                "  {} {}... ({}/{})",
                                success("✓"),
                                style_hash(hash_display),
                                i + 1,
                                missing.len()
                            );
                        }
                        Err(e) => {
                            stats.record_failed();
                            println!(
                                "  {} {}... ({}/{}) save failed: {}",
                                error("✗"),
                                hash_display,
                                i + 1,
                                missing.len(),
                                e
                            );
                        }
                    }
                }
                None => {
                    stats.record_failed();
                    println!(
                        "  {} {}... ({}/{}) not found on remote",
                        error("✗"),
                        hash_display,
                        i + 1,
                        missing.len(),
                    );
                    return Err(CliError::ChangeNotFound {
                        hash: hash_str.clone(),
                    });
                }
            }

            progress_bar.inc(1);
        }

        finish_success(
            &progress_bar,
            &format!(
                "Downloaded {} ({})",
                format_count(stats.changes_downloaded, "change"),
                format_bytes(stats.bytes_transferred)
            ),
        );

        Ok(())
    }

    /// Remove an empty locally-created view whose identity conflicts with
    /// the manifest about to be applied.
    ///
    /// `Repository::init` pre-creates a shared root view (e.g. "dev"). If
    /// the remote's view of the same name is a draft or has a parent,
    /// `apply_view_manifest` would report an identity mismatch even though
    /// the local view is just an empty scaffold. Deleting the scaffold lets
    /// the manifest recreate the view with its declared identity.
    ///
    /// Views with changes are left alone: a genuine divergence must surface
    /// as an apply error, never a silent delete.
    fn reconcile_scaffold_view(
        &self,
        repo: &mut Repository,
        manifest: &ViewManifest,
    ) -> CliResult<()> {
        if !repo
            .view_exists(&manifest.name)
            .map_err(CliError::Repository)?
        {
            return Ok(());
        }
        let info = repo
            .get_view_info(&manifest.name)
            .map_err(CliError::Repository)?;
        if info.scope == manifest.scope && info.parent_name == manifest.parent {
            // Identity matches; apply_view_manifest handles the log.
            return Ok(());
        }
        if info.change_count > 0 {
            // Not a scaffold; let apply_view_manifest report the mismatch.
            return Ok(());
        }

        // The scaffold may be the current view (init leaves it current),
        // and the current view cannot be deleted — park on a temporary
        // root view first. It is removed again before clone finishes.
        if repo.current_view() == manifest.name {
            if !repo
                .view_exists(SCAFFOLD_PARK_VIEW)
                .map_err(CliError::Repository)?
            {
                repo.create_shared_view(SCAFFOLD_PARK_VIEW)
                    .map_err(CliError::Repository)?;
            }
            repo.align_to_view(SCAFFOLD_PARK_VIEW)
                .map_err(CliError::Repository)?;
        }

        // Shared views cannot be deleted directly; demote first.
        if info.scope.is_shared() {
            repo.set_view_scope(&manifest.name, ViewScope::Draft)
                .map_err(CliError::Repository)?;
        }
        repo.delete_view(&manifest.name)
            .map_err(CliError::Repository)?;
        Ok(())
    }

    /// Remove the temporary parking view, if scaffold reconciliation
    /// created it. Best-effort: a leftover parking view is cosmetic.
    fn remove_park_view(&self, repo: &mut Repository) {
        if let Ok(true) = repo.view_exists(SCAFFOLD_PARK_VIEW) {
            let _ = repo.set_view_scope(SCAFFOLD_PARK_VIEW, ViewScope::Draft);
            if let Err(e) = repo.delete_view(SCAFFOLD_PARK_VIEW) {
                log::warn!(
                    "could not remove temporary view '{}': {}",
                    SCAFFOLD_PARK_VIEW,
                    e
                );
            }
        }
    }

    /// Apply manifests in root→leaf order, reconciling scaffold views first.
    ///
    /// Returns one outcome per manifest, in apply order.
    fn apply_manifests(
        &self,
        repo: &mut Repository,
        manifests: &[ViewManifest],
    ) -> CliResult<Vec<ManifestApplyOutcome>> {
        let mut outcomes = Vec::with_capacity(manifests.len());
        for manifest in manifests {
            self.reconcile_scaffold_view(repo, manifest)?;
            let outcome = repo.apply_view_manifest(manifest).map_err(|e| {
                CliError::Internal(anyhow::anyhow!(
                    "apply manifest for view '{}': {}",
                    manifest.name,
                    e
                ))
            })?;
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }

    /// Clone every remote view not already applied by the primary chain.
    ///
    /// Each view arrives as a manifest and is applied with the same
    /// primitive as the primary view, so scope and parent are preserved.
    /// Manifests are ordered parents-before-children; changes are
    /// content-addressed and deduplicated against everything already
    /// downloaded. Returns the number of additional views cloned.
    fn clone_additional_views(
        &self,
        repo: &mut Repository,
        pack: &SyncPack,
        change_objects: &HashMap<String, Vec<u8>>,
        stats: &mut CloneStats,
        applied: &mut HashSet<String>,
    ) -> CliResult<usize> {
        // With `--all-views` the primary pull requested every view, so the pack
        // already carries all view refs + snapshots. Derive the inventory from
        // it — no separate `GET /refs/views` round-trip.
        let names: Vec<String> = pack.refs.iter().map(|r| r.name.clone()).collect();
        let others = match classify_inventory(&names, applied) {
            InventoryOutcome::Views(others) => others,
            InventoryOutcome::NothingNew => return Ok(0),
            InventoryOutcome::Unsupported => {
                return Err(CliError::RemoteError {
                    message: "--all-views requires the view inventory (?views), but this \
                              server does not support it (empty inventory). Upgrade the \
                              server, or clone a single view without --all-views."
                        .to_string(),
                    url: Some(self.url.clone()),
                })
            }
        };

        print_blank();
        println!(
            "Cloning {} additional {}:",
            others.len(),
            if others.len() == 1 { "view" } else { "views" }
        );

        // Fetch manifests for every remaining view.
        let mut manifests: Vec<ViewManifest> = Vec::with_capacity(others.len());
        for name in &others {
            match self.view_manifest_from_pack(pack, name) {
                Ok(Some(m)) => manifests.push(m),
                Ok(None) => {
                    print_warning(&format!(
                        "View '{}' vanished from the remote — skipped",
                        name
                    ));
                }
                Err(e) => {
                    print_warning(&format!(
                        "Failed to fetch manifest for view '{}': {}",
                        name, e
                    ));
                }
            }
        }

        // Parents must apply before children; anything unorderable (parent
        // cycle or a parent that exists nowhere) is reported and skipped.
        let (order, stuck) = manifest_apply_order(&manifests, applied);
        for &i in &stuck {
            print_warning(&format!(
                "Cannot clone view '{}': parent '{}' is not available (cycle or missing view)",
                manifests[i].name,
                manifests[i].parent.as_deref().unwrap_or("-"),
            ));
        }

        let ordered: Vec<ViewManifest> = order.iter().map(|&i| manifests[i].clone()).collect();

        // Download the union of changes these views need, minus everything
        // the primary chain already fetched (content-addressed dedupe).
        let missing: Vec<Hash> = change_union(&ordered)
            .into_iter()
            .filter(|h| !repo.has_change(h))
            .collect();
        let mut progress = CloneProgress::new(missing.len());
        progress.phase = ClonePhase::Downloading;
        self.download_missing_changes(repo, change_objects, &missing, stats, &mut progress)?;

        let mut cloned = 0usize;
        for manifest in &ordered {
            self.reconcile_scaffold_view(repo, manifest)?;
            match repo.apply_view_manifest(manifest) {
                Ok(outcome) => {
                    cloned += 1;
                    applied.insert(manifest.name.clone());
                    println!(
                        "  {} {} ({})",
                        success("\u{2713}"),
                        style_view(&manifest.name),
                        format_count(outcome.already_present + outcome.replayed, "change")
                    );
                }
                Err(e) => {
                    print_warning(&format!("Failed to clone view '{}': {}", manifest.name, e));
                }
            }
        }

        Ok(cloned)
    }

    /// Print a hint listing the other views available on the remote.
    ///
    /// Views already cloned (the requested view and its ancestor chain) are
    /// not hinted. Best-effort: silent if the server has no inventory
    /// endpoint or nothing else to clone.
    async fn hint_other_views(&self, remote: &HttpRemote, applied: &HashSet<String>) {
        // A cheap ref advertisement (refs only, no change bodies) lists every
        // view on the remote.
        let Ok(adv) = remote.sync_pull(&SyncWants::advertise(vec![])).await else {
            return;
        };
        let others: Vec<String> = adv
            .refs
            .into_iter()
            .map(|r| r.name)
            .filter(|name| name != &self.view && !applied.contains(name))
            .collect();
        if others.is_empty() {
            return;
        }

        print_blank();
        print_hint(&format!(
            "This remote has {} other {}: {}",
            others.len(),
            if others.len() == 1 { "view" } else { "views" },
            others.join(", ")
        ));
        print_hint(
            "Clone them too with 'atomic clone --all-views', or list them with 'atomic view list --remote'.",
        );
    }

    /// Async implementation of the clone command.
    ///
    /// This is the main entry point for the clone operation. It coordinates
    /// all the steps required to create a local copy of a remote repository.
    async fn run_async(&self) -> CliResult<()> {
        // Resolve target path
        let target_path = resolve_target_path(&self.url, self.path.clone());
        let display_name = self.get_display_name();

        // Print header
        println!(
            "Cloning from {} into {}...",
            hint(&self.url),
            style_view(&display_name)
        );
        print_blank();

        let guard = if self.into_existing {
            validate_existing_git_checkout(&target_path)?;
            None
        } else {
            validate_target_path(&target_path)?;
            std::fs::create_dir_all(&target_path).map_err(|e| CliError::InvalidPath {
                path: target_path.clone(),
                source: Some(e),
            })?;
            Some(CleanupGuard::new(target_path.clone()))
        };

        // Initialize repository
        let spinner = create_spinner("Initializing repository...");
        let mut repo = Repository::init(&target_path).map_err(|e| {
            finish_error(&spinner, "Failed to initialize");
            CliError::Repository(e)
        })?;
        finish_success(&spinner, "Repository initialized");

        // The requested view is NOT pre-created here: its manifest declares
        // its identity (scope + parent), and `apply_view_manifest` creates it
        // accordingly. Pre-creating it as a shared root would clash with a
        // remote draft or child view.

        // Connect to remote
        let spinner = create_spinner("Connecting to remote...");
        let config = self.build_remote_config().await;
        let remote = HttpRemote::with_config(&self.url, config).map_err(|e| {
            finish_error(&spinner, "Failed to connect");
            convert_remote_error(e, &self.url)
        })?;
        finish_success(&spinner, "Connected");

        // One `/code` pull for the whole clone: request the view (every view
        // with `--all-views`) and take the response — view snapshots + change
        // bodies. A fresh clone holds nothing, so it declares no `haves`.
        let spinner = create_spinner("Fetching from remote...");
        let wants = if self.all_views {
            SyncWants::all(Vec::new())
        } else {
            SyncWants {
                refs: vec![self.view.clone()],
                haves: Vec::new(),
                refs_only: false,
            }
        };
        let pack = match remote.sync_pull(&wants).await {
            Ok(p) => p,
            Err(e) => {
                finish_error(&spinner, "Failed to fetch from remote");
                return Err(convert_remote_error(e, &self.url));
            }
        };
        let change_objects = Self::change_objects_from_pack(&pack);
        finish_success(&spinner, "Fetched from remote");

        // Reconstruct the requested view's manifest chain (view → ... → root)
        // from the pulled snapshots. The manifests carry each view's identity,
        // so clone reconstructs scope and parent exactly.
        let spinner = create_spinner("Reading view manifests...");
        let manifests = match self.fetch_manifest_chain(&pack) {
            Ok(m) => m,
            Err(e) => {
                finish_error(&spinner, "Failed to read view manifests");
                return Err(e);
            }
        };

        let Some(manifests) = manifests else {
            // The remote doesn't have this view: create it empty locally,
            // matching the empty-view clone behavior.
            finish_success(
                &spinner,
                &format!("View '{}' not found on remote — starting empty", self.view),
            );

            if !repo.view_exists(&self.view).map_err(CliError::Repository)? {
                repo.create_shared_view(&self.view)
                    .map_err(CliError::Repository)?;
            }
            repo.align_to_view(&self.view)
                .map_err(CliError::Repository)?;

            // Configure remote as "origin" even for empty repositories
            let spinner = create_spinner("Configuring remote...");
            if let Err(e) = repo.add_remote_default("origin", &self.url) {
                finish_error(&spinner, "Failed to configure remote");
                print_warning(&format!("Could not save remote configuration: {}", e));
                print_hint(
                    "You can manually add the remote later with 'atomic remote add origin <url>'",
                );
            } else {
                finish_success(&spinner, "Remote 'origin' configured");
            }

            print_blank();
            print_success(&format!(
                "Clone complete: empty repository created at {}",
                target_path.display()
            ));

            // Disable cleanup guard - clone succeeded
            if let Some(guard) = guard {
                guard.disable();
            }
            return Ok(());
        };

        let leaf = manifests
            .last()
            .expect("manifest chain contains at least the requested view");
        let leaf_state = if leaf.changes.is_empty() {
            "(empty)".to_string()
        } else {
            let full = leaf.state.to_base32();
            format!("{}...", &full[..12.min(full.len())])
        };
        finish_success(
            &spinner,
            &format!(
                "View '{}' at {} ({} in chain)",
                self.view,
                leaf_state,
                format_count(manifests.len(), "view"),
            ),
        );

        // The union of changes across the chain, deduplicated: changes
        // shared between views (a draft's inherited prefix) download once.
        let missing: Vec<Hash> = change_union(&manifests)
            .into_iter()
            .filter(|h| !repo.has_change(h))
            .collect();

        // Initialize progress tracking
        let mut stats = CloneStats::new();
        let mut progress = CloneProgress::new(missing.len());
        progress.phase = ClonePhase::Downloading;

        // Save the missing changes from the pulled pack.
        self.download_missing_changes(&repo, &change_objects, &missing, &mut stats, &mut progress)?;

        // Apply changes (unless download-only)
        if self.download_only {
            // Keep the pre-manifest behavior: the requested view exists and
            // is current, but nothing is applied to it. (The manifest is not
            // applied, so the downloaded changes stay in the change store.)
            if !repo.view_exists(&self.view).map_err(CliError::Repository)? {
                repo.create_shared_view(&self.view)
                    .map_err(CliError::Repository)?;
            }
            repo.align_to_view(&self.view)
                .map_err(CliError::Repository)?;

            // Configure remote as "origin" even for download-only mode
            let spinner = create_spinner("Configuring remote...");
            if let Err(e) = repo.add_remote_default("origin", &self.url) {
                finish_error(&spinner, "Failed to configure remote");
                print_warning(&format!("Could not save remote configuration: {}", e));
                print_hint(
                    "You can manually add the remote later with 'atomic remote add origin <url>'",
                );
            } else {
                finish_success(&spinner, "Remote 'origin' configured");
            }

            progress.phase = ClonePhase::Complete;
            print_blank();
            print_success(&format!(
                "Clone complete: {} downloaded to {} (not applied)",
                format_count(stats.changes_downloaded, "change"),
                target_path.display()
            ));
            print_hint("Use 'atomic insert' to insert the downloaded changes");

            // Disable cleanup guard - clone succeeded
            if let Some(guard) = guard {
                guard.disable();
            }
            return Ok(());
        }

        // Apply the manifest chain root→leaf: each apply creates the view
        // with its declared scope/parent (parents exist first) and replays
        // its log — metadata-only for changes already in the graph.
        let mut apply_errors = Vec::new();
        let mut applied_views: HashSet<String> = HashSet::new();
        {
            print_blank();
            progress.phase = ClonePhase::Applying;
            let spinner = create_spinner("Applying view manifests...");

            match self.apply_manifests(&mut repo, &manifests) {
                Ok(outcomes) => {
                    for outcome in &outcomes {
                        applied_views.insert(outcome.view.clone());
                    }
                    // The requested view's log length (inherited + own).
                    if let Some(leaf_outcome) = outcomes.last() {
                        for _ in 0..(leaf_outcome.already_present + leaf_outcome.replayed) {
                            stats.record_applied();
                            progress.record_applied();
                        }
                    }
                }
                Err(e) => apply_errors.push(e.to_string()),
            }

            // Make the requested view current.
            if apply_errors.is_empty() {
                if let Err(e) = repo.align_to_view(&self.view) {
                    apply_errors.push(format!("align to view '{}': {}", self.view, e));
                }
            }

            if apply_errors.is_empty() {
                if self.into_existing {
                    print_info("Preserving the existing Git working copy.");
                } else {
                    // Output the working copy — reconstruct files from the graph
                    match repo.materialize() {
                        Ok(output) => {
                            log::info!(
                                "Output working copy: {} files, {} dirs",
                                output.files_written,
                                output.directories_created
                            );
                        }
                        Err(e) => {
                            apply_errors.push(format!("output working copy: {}", e));
                        }
                    }
                }
            }

            if apply_errors.is_empty() {
                finish_success(
                    &spinner,
                    &format!(
                        "Applied {} to {}",
                        format_count(stats.changes_applied, "change"),
                        self.view
                    ),
                );
            } else {
                finish_error(&spinner, "Failed to apply view manifests");
                for err in &apply_errors {
                    print_warning(err);
                }
            }
        }

        // Configure remote as "origin"
        progress.phase = ClonePhase::ConfiguringRemote;
        let spinner = create_spinner("Configuring remote...");

        // Save the remote URL as "origin" in the repository configuration
        if let Err(e) = repo.add_remote_default("origin", &self.url) {
            finish_error(&spinner, "Failed to configure remote");
            print_warning(&format!("Could not save remote configuration: {}", e));
            print_hint(
                "You can manually add the remote later with 'atomic remote add origin <url>'",
            );
        } else {
            finish_success(&spinner, "Remote 'origin' configured");
        }

        // Clone additional views (--all-views), or at least tell the user
        // that other views exist on the remote. View manifests carry each
        // view's scope and parent, so a draft's parent relationship is
        // recoverable and preserved — cloned views keep their identity.
        if !self.download_only && apply_errors.is_empty() {
            if self.all_views {
                match self.clone_additional_views(
                    &mut repo,
                    &pack,
                    &change_objects,
                    &mut stats,
                    &mut applied_views,
                ) {
                    Ok(0) => {}
                    Ok(n) => print_success(&format!(
                        "Cloned {} additional {}",
                        n,
                        if n == 1 { "view" } else { "views" }
                    )),
                    // --all-views was an explicit request: failing to honor
                    // it fails the clone rather than degrading silently.
                    Err(e) => return Err(e),
                }
            } else {
                self.hint_other_views(&remote, &applied_views).await;
            }
        }

        // Validate convergence with the order-invariant SetId across every
        // applied view (chain + any `--all-views` extras), against the
        // server-computed effective set-ids.
        if !self.download_only && apply_errors.is_empty() {
            self.verify_convergence(&repo, &remote, &applied_views)
                .await;
        }

        // Drop the temporary parking view, if scaffold reconciliation
        // needed one while recreating an init-created view.
        self.remove_park_view(&mut repo);

        // Build content search index and enrich KG
        if stats.has_applied() {
            let spinner = create_spinner("Building search index...");

            // Bootstrap vault if .vault/ files were cloned but redb tables don't exist.
            // This happens when the source repo had a vault initialized.
            let vault_on_disk = repo.vault_dir().exists();
            let vault_in_db = repo.has_vault().unwrap_or(false);
            if vault_on_disk && !vault_in_db {
                log::info!("Vault files detected — bootstrapping vault from cloned content");
                match repo.bootstrap_vault_from_working_copy() {
                    Ok(()) => log::info!("Vault bootstrapped from cloned content"),
                    Err(e) => log::warn!("Failed to bootstrap vault: {}", e),
                }
            }

            // KG enrichment — runs for every clone, not just vaulted repos.
            // Ensure KG tables exist (idempotent — no-op if already created)
            if let Err(e) = repo.init_kg() {
                log::warn!("KG table init failed: {}", e);
            }
            match repo.kg_enrich_from_vcs() {
                Ok(kg_stats) => log::info!("KG enriched: {}", kg_stats),
                Err(e) => log::warn!("KG enrichment failed: {}", e),
            }

            // Content search index (syntext)
            match atomic_repository::build_content_index(&target_path) {
                Ok(()) => finish_success(&spinner, "Search index built"),
                Err(e) => {
                    finish_error(&spinner, "Search index failed");
                    log::warn!("Content index build failed: {}", e);
                }
            }
        }

        progress.phase = ClonePhase::Complete;

        // Keep partial output, but return failure when any step failed.
        print_blank();
        if stats.has_failures() || !apply_errors.is_empty() {
            print_warning(&format!(
                "Clone completed with errors: {} downloaded, {} failed to download, {} failed to apply",
                stats.changes_downloaded,
                stats.changes_failed,
                apply_errors.len(),
            ));

            if self.into_existing {
                print_hint(
                    "Next: run 'atomic git import --incremental' to import the Git checkout.",
                );
            }

            // Preserve the partial clone before returning the error.
            if let Some(guard) = guard {
                guard.disable();
            }
            return Err(CliError::Internal(anyhow::anyhow!(
                "clone completed with errors ({} download, {} apply)",
                stats.changes_failed,
                apply_errors.len(),
            )));
        }

        print_success(&format!(
            "Clone complete: {} downloaded into {}/",
            format_count(stats.changes_downloaded, "change"),
            target_path.display()
        ));

        if self.into_existing {
            print_hint("Next: run 'atomic git import --incremental' to import the Git checkout.");
        }

        // Disable cleanup guard - clone succeeded
        if let Some(guard) = guard {
            guard.disable();
        }

        Ok(())
    }
}

impl Default for Clone {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl Command for Clone {
    /// Execute the clone command.
    ///
    /// This method creates a tokio runtime and executes the async clone
    /// operation. It handles all the steps required to create a local copy
    /// of a remote repository.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The target directory already exists
    /// - The remote cannot be connected to
    /// - Network operations fail
    /// - Changes fail to download or save
    fn run(&self) -> CliResult<()> {
        // Validate URL is not empty
        if self.url.is_empty() {
            return Err(CliError::InvalidArgument {
                message: "Repository URL is required".to_string(),
            });
        }

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

    /// Test creating a new Clone with URL.
    #[test]
    fn test_clone_new() {
        let clone = Clone::new("https://example.com/repo".to_string());

        assert_eq!(clone.url, "https://example.com/repo");
        assert!(clone.path.is_none());
        assert_eq!(clone.view, DEFAULT_VIEW);
        assert!(!clone.insecure);
        assert_eq!(clone.timeout, DEFAULT_TIMEOUT_SECS);
        assert!(!clone.download_only);
        assert!(!clone.into_existing);
        assert!(!clone.all_views);
    }

    /// Test Default trait implementation.
    #[test]
    fn test_clone_default() {
        let clone: Clone = Default::default();
        assert!(clone.url.is_empty());
        assert_eq!(clone.view, DEFAULT_VIEW);
    }

    /// Test with_path builder method.
    #[test]
    fn test_clone_with_path() {
        let clone = Clone::new("https://example.com/repo".to_string()).with_path("my-project");
        assert_eq!(clone.path, Some("my-project".to_string()));
    }

    /// Test with_view builder method.
    #[test]
    fn test_clone_with_view() {
        let clone = Clone::new("https://example.com/repo".to_string()).with_view("dev");
        assert_eq!(clone.view, "dev");
    }

    /// Test with_insecure builder method.
    #[test]
    fn test_clone_with_insecure() {
        let clone = Clone::new("https://example.com/repo".to_string()).with_insecure(true);
        assert!(clone.insecure);
    }

    /// Test with_timeout builder method.
    #[test]
    fn test_clone_with_timeout() {
        let clone = Clone::new("https://example.com/repo".to_string()).with_timeout(60);
        assert_eq!(clone.timeout, 60);
    }

    /// Test with_download_only builder method.
    #[test]
    fn test_clone_with_download_only() {
        let clone = Clone::new("https://example.com/repo".to_string()).with_download_only(true);
        assert!(clone.download_only);
    }

    #[test]
    fn test_clone_with_into_existing() {
        let clone = Clone::new("https://example.com/repo".to_string()).with_into_existing(true);
        assert!(clone.into_existing);
    }

    /// Test with_all_views builder method.
    #[test]
    fn test_clone_with_all_views() {
        let clone = Clone::new("https://example.com/repo".to_string()).with_all_views(true);
        assert!(clone.all_views);
    }

    /// Test chaining multiple builder methods.
    #[test]
    fn test_clone_builder_chain() {
        let clone = Clone::new("https://example.com/repo".to_string())
            .with_path("my-project")
            .with_view("dev")
            .with_insecure(true)
            .with_timeout(120)
            .with_download_only(true)
            .with_into_existing(true)
            .with_all_views(true);

        assert_eq!(clone.url, "https://example.com/repo");
        assert_eq!(clone.path, Some("my-project".to_string()));
        assert_eq!(clone.view, "dev");
        assert!(clone.insecure);
        assert_eq!(clone.timeout, 120);
        assert!(clone.download_only);
        assert!(clone.into_existing);
        assert!(clone.all_views);
    }

    /// Test Clone can be cloned (the trait, not the command).
    #[test]
    fn test_clone_clone() {
        let original = Clone::new("https://example.com/repo".to_string())
            .with_path("test")
            .with_insecure(true);

        let cloned = original.clone();

        assert_eq!(cloned.url, "https://example.com/repo");
        assert_eq!(cloned.path, Some("test".to_string()));
        assert!(cloned.insecure);
    }

    /// Test Clone has Debug implementation.
    #[test]
    fn test_clone_debug() {
        let clone = Clone::new("https://example.com/repo".to_string());
        let debug_str = format!("{:?}", clone);

        assert!(debug_str.contains("Clone"));
        assert!(debug_str.contains("https://example.com/repo"));
    }

    // Internal Method Tests

    /// Test build_remote_config with default settings.
    #[tokio::test]
    async fn test_build_remote_config_default() {
        let clone = Clone::new("https://example.com/repo".to_string());
        let config = clone.build_remote_config().await;

        // HttpRemoteConfig doesn't expose fields directly, so we just verify
        // it doesn't panic and returns something
        assert!(std::mem::size_of_val(&config) > 0);
    }

    /// Test build_remote_config with custom timeout.
    #[tokio::test]
    async fn test_build_remote_config_custom_timeout() {
        let clone = Clone::new("https://example.com/repo".to_string()).with_timeout(120);
        let config = clone.build_remote_config().await;
        assert!(std::mem::size_of_val(&config) > 0);
    }

    /// Test build_remote_config with insecure flag.
    #[tokio::test]
    async fn test_build_remote_config_insecure() {
        let clone = Clone::new("https://example.com/repo".to_string()).with_insecure(true);
        let config = clone.build_remote_config().await;
        assert!(std::mem::size_of_val(&config) > 0);
    }

    /// Test get_display_name with explicit path.
    #[test]
    fn test_get_display_name_explicit() {
        let clone = Clone::new("https://example.com/repo".to_string()).with_path("my-project");
        assert_eq!(clone.get_display_name(), "my-project");
    }

    /// Test get_display_name with inferred name.
    #[test]
    fn test_get_display_name_inferred() {
        let clone = Clone::new("https://example.com/org/project/code".to_string());
        assert_eq!(clone.get_display_name(), "project");
    }

    /// Test get_display_name fallback.
    #[test]
    fn test_get_display_name_fallback() {
        let clone = Clone::new(String::new());
        assert_eq!(clone.get_display_name(), "repo");
    }

    #[test]
    fn test_validate_existing_git_checkout_accepts_git_worktree() {
        let dir = tempfile::tempdir().unwrap();
        GitRepository::init(dir.path()).unwrap();

        assert!(validate_existing_git_checkout(dir.path()).is_ok());
    }

    #[test]
    fn test_validate_existing_git_checkout_rejects_non_git_directory() {
        let dir = tempfile::tempdir().unwrap();

        assert!(validate_existing_git_checkout(dir.path()).is_err());
    }

    #[test]
    fn test_validate_existing_git_checkout_rejects_git_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        GitRepository::init(dir.path()).unwrap();
        let child = dir.path().join("child");
        std::fs::create_dir(&child).unwrap();

        assert!(validate_existing_git_checkout(&child).is_err());
    }

    #[test]
    fn test_validate_existing_git_checkout_rejects_atomic_repository() {
        let dir = tempfile::tempdir().unwrap();
        GitRepository::init(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join(".atomic")).unwrap();

        assert!(matches!(
            validate_existing_git_checkout(dir.path()),
            Err(CliError::RepositoryExists { .. })
        ));
    }

    // Constant Tests

    /// Test that DEFAULT_VIEW is "dev" (atomic-api convention).
    #[test]
    fn test_default_view() {
        assert_eq!(DEFAULT_VIEW, "dev");
    }

    /// Test that DEFAULT_TIMEOUT_SECS is 30.
    #[test]
    fn test_default_timeout() {
        assert_eq!(DEFAULT_TIMEOUT_SECS, 30);
    }

    // Command Validation Tests

    /// Test run with empty URL returns error.
    #[test]
    fn test_run_empty_url() {
        let clone = Clone::new(String::new());
        let result = clone.run();

        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::InvalidArgument { message } => {
                assert!(message.contains("URL"));
            }
            _ => panic!("Expected InvalidArgument error"),
        }
    }

    // Manifest Error Mapping Tests

    /// A protocol error (server predates ?view-manifest) is a hard error
    /// telling the user to upgrade the server — clone never falls back to
    /// the identity-losing flat path.
    #[test]
    fn test_manifest_fetch_error_old_server() {
        let clone = Clone::new("https://example.com/repo".to_string());
        let err = clone.manifest_fetch_error(RemoteError::protocol(
            "server does not support view manifests (?view-manifest)",
        ));

        match err {
            CliError::RemoteError { message, url } => {
                assert!(message.contains("too old"));
                assert!(message.contains("Upgrade the server"));
                assert_eq!(url.as_deref(), Some("https://example.com/repo"));
            }
            other => panic!("Expected RemoteError, got {:?}", other),
        }
    }

    /// Non-protocol failures pass through the standard remote-error mapping.
    #[test]
    fn test_manifest_fetch_error_other_errors_pass_through() {
        let clone = Clone::new("https://example.com/repo".to_string());
        let err = clone.manifest_fetch_error(RemoteError::ViewNotFound {
            view: "dev".to_string(),
        });

        match err {
            CliError::RemoteError { message, .. } => {
                assert!(message.contains("View 'dev' not found"));
                assert!(!message.contains("too old"));
            }
            other => panic!("Expected RemoteError, got {:?}", other),
        }
    }
}
