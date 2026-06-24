//! The main Push command implementation.
//!
//! This module contains the `Push` struct and its `Command` implementation,
//! which orchestrates the push operation from the CLI.

use std::collections::HashSet;
use std::time::Duration;

use bytes::Bytes;
use clap::Parser;

use atomic_core::types::{Base32, Hash};
use atomic_remote::{HttpRemote, HttpRemoteConfig};
use atomic_repository::history::HistoryOptions;
use atomic_repository::Repository;

use crate::commands::{find_repository_root, format_hash, Command};
use crate::error::{CliError, CliResult};
use crate::output::{
    create_progress_bar, create_spinner, error, finish_error, finish_success, hash as style_hash,
    hint, print_blank, print_hint, print_success, print_warning, success, view as style_view,
};

use super::helpers::{
    calculate_push_delta, convert_remote_error, display_state_comparison, format_count,
    has_diverged, upload_change_smart,
};
use super::types::PushChange;

// Constants

/// Default remote name when none is specified.
pub const DEFAULT_REMOTE: &str = "origin";

/// Default request timeout in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

// Push Command

/// Push changes to a remote repository.
///
/// Uploads local changes to the specified remote, making them available
/// to other users. Changes are pushed in dependency order to ensure the
/// remote can apply them correctly.
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
/// on the remote. Use `--to-view` to push to a different remote view.
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
/// # Push to a different view
/// atomic push --to-view main
///
/// # Preview what would be pushed
/// atomic push --dry-run
///
/// # Force push (overwrite remote)
/// atomic push --force
/// ```
#[derive(Parser, Debug, Clone)]
#[command(name = "push")]
pub struct Push {
    /// Remote name or URL to push to.
    ///
    /// Can be a configured remote name (like "origin") or a full URL.
    /// If not specified, uses the default remote "origin".
    #[arg(default_value = DEFAULT_REMOTE)]
    pub remote: String,

    /// Remote view to push to.
    ///
    /// If not specified, uses the same name as the local view.
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

    /// Force push even if histories have diverged.
    ///
    /// Use with caution: this can overwrite remote changes.
    #[arg(short, long)]
    pub force: bool,

    /// Push all changes, not just those missing on the remote.
    ///
    /// Useful for ensuring all dependencies are present.
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
    /// assert_eq!(push.remote, "origin");
    /// assert!(!push.dry_run);
    /// ```
    pub fn new() -> Self {
        Self {
            remote: DEFAULT_REMOTE.to_string(),
            to_view: None,
            from_view: None,
            dry_run: false,
            force: false,
            all: false,
            insecure: false,
            timeout: DEFAULT_TIMEOUT_SECS,
        }
    }

    /// Builder: set the remote name or URL.
    pub fn with_remote(mut self, remote: impl Into<String>) -> Self {
        self.remote = remote.into();
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

    /// Resolve the remote URL.
    ///
    /// If the remote is a URL (contains "://"), returns it directly.
    /// Otherwise, looks up the named remote in the repository configuration.
    ///
    /// # Arguments
    ///
    /// * `repo` - The repository to look up configuration from
    ///
    /// # Returns
    ///
    /// The resolved remote URL.
    ///
    /// # Errors
    ///
    /// Returns `CliError::RemoteNotFound` if the named remote doesn't exist.
    fn resolve_remote_url(&self, repo: &Repository) -> CliResult<String> {
        // If it looks like a URL, use it directly
        if self.remote.contains("://") {
            return Ok(self.remote.clone());
        }

        // Look up named remote in repository configuration
        match repo.get_remote(&self.remote) {
            Ok(entry) => Ok(entry.url),
            Err(atomic_repository::RepositoryError::RemoteNotFound { .. }) => {
                Err(CliError::RemoteNotFound {
                    name: self.remote.clone(),
                })
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

    /// Get the remote view name to push to.
    ///
    /// Returns the explicitly specified view or the local view name.
    fn get_remote_view(&self, local_view: &str) -> String {
        self.to_view
            .clone()
            .unwrap_or_else(|| local_view.to_string())
    }

    /// Build the HTTP remote configuration.
    async fn build_remote_config(&self, remote_url: &str) -> HttpRemoteConfig {
        let config = HttpRemoteConfig::new()
            .with_timeout(Duration::from_secs(self.timeout))
            .danger_accept_invalid_certs(self.insecure);

        crate::commands::auth::attach_identity(config, remote_url).await
    }

    /// Display the dry run preview.
    fn display_dry_run(
        &self,
        remote_url: &str,
        remote_view: &str,
        to_upload: &[PushChange],
    ) -> CliResult<()> {
        if to_upload.is_empty() {
            print_success("Already up to date - nothing to push");
            return Ok(());
        }

        println!(
            "Would push {} to {} (view: {}):",
            format_count(to_upload.len(), "change"),
            self.remote,
            remote_view
        );
        print_blank();

        for change in to_upload {
            let hash_str = format_hash(&change.hash, false);
            let msg = change.message_or_default();
            println!("  {} {}", style_hash(&hash_str), msg);
        }

        print_blank();
        print_hint(&format!("Remote URL: {}", remote_url));

        Ok(())
    }

    /// Async implementation of the push command.
    async fn run_async(&self) -> CliResult<()> {
        // Find and open repository
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        // Resolve remote URL
        let remote_url = self.resolve_remote_url(&repo)?;

        // Determine views
        let local_view = self.get_local_view(&repo);
        let remote_view = self.get_remote_view(&local_view);
        let default_remote_view = "dev".to_string();

        // Print header
        println!(
            "Pushing to {} ({})",
            style_view(&self.remote),
            hint(&remote_url)
        );

        // Fail fast if we have no usable credentials for this remote. A push is
        // a write that always requires auth; without a credential the server's
        // negotiation endpoints return an ambiguous 404 (private projects are
        // deliberately masked), which surfaces as a misleading "view not found".
        // Catching the missing credential here gives the user an accurate,
        // actionable error instead.
        crate::commands::auth::check_push_credentials(&remote_url)?;

        // Connect to remote
        let spinner = create_spinner("Connecting to remote...");
        let config = self.build_remote_config(&remote_url).await;
        let remote = HttpRemote::with_config(&remote_url, config).map_err(|e| {
            finish_error(&spinner, "Failed to connect");
            convert_remote_error(e, &remote_url)
        })?;
        finish_success(&spinner, "Connected");

        // Query remote state
        let spinner = create_spinner("Querying remote state...");
        let remote_state = remote.get_state(&remote_view).await.map_err(|e| {
            finish_error(&spinner, "Failed to query state");
            convert_remote_error(e, &remote_url)
        })?;
        finish_success(&spinner, "Got remote state");

        // Get local history
        let spinner = create_spinner("Loading local history...");
        let local_entries = repo
            .log(HistoryOptions::default())
            .map_err(CliError::Repository)?;
        finish_success(
            &spinner,
            &format!("Loaded {} local changes", local_entries.len()),
        );

        // Get remote changelist for the target view
        let spinner = create_spinner("Fetching remote changelist...");
        let remote_entries = if !remote_state.is_empty() {
            // Remote has changes - fetch the full changelist from position 0
            remote.get_changelist(&remote_view, 0).await.map_err(|e| {
                finish_error(&spinner, "Failed to fetch changelist");
                convert_remote_error(e, &remote_url)
            })?
        } else {
            // Remote is empty (returned "-") - no changes to fetch
            Vec::new()
        };
        finish_success(
            &spinner,
            &format!("Got {} remote changes", remote_entries.len()),
        );

        // If the target view is empty/new and differs from the default,
        // check the default view to find a fork source. Views are perspectives
        // of the same graph — we can fork instead of re-uploading.
        let mut fork_source: Option<String> = None;
        let mut graph_hashes: HashSet<String> = HashSet::new();

        if remote_entries.is_empty() && remote_view != default_remote_view {
            let default_state = remote.get_state(&default_remote_view).await;
            if let Ok(ref state) = default_state {
                if !state.is_empty() {
                    if let Ok(default_entries) =
                        remote.get_changelist(&default_remote_view, 0).await
                    {
                        for entry in &default_entries {
                            graph_hashes.insert(entry.hash.clone());
                        }
                        if !default_entries.is_empty() {
                            fork_source = Some(default_remote_view.clone());
                        }
                    }
                }
            }
        }
        // Also include the target view's own entries
        for entry in &remote_entries {
            graph_hashes.insert(entry.hash.clone());
        }

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

        // Calculate what to push
        let to_upload = calculate_push_delta(
            &repo,
            &local_entries,
            &remote_entries,
            &graph_hashes,
            self.all,
        )?;

        // Handle dry run
        if self.dry_run {
            return self.display_dry_run(&remote_url, &remote_view, &to_upload);
        }

        // Check for nothing to push
        if to_upload.is_empty() {
            print_success("Already up to date");
            return Ok(());
        }

        // Check for diverged history (unless forcing)
        if !self.force && has_diverged(&local_entries, &remote_entries) {
            crate::output::print_error("Histories have diverged");
            print_blank();
            print_hint("The remote has changes that are not in your local history.");
            print_hint("Use 'atomic pull' to fetch remote changes, or");
            print_hint("Use 'atomic push --force' to overwrite remote (use with caution)");
            return Err(CliError::Conflict {
                description: "Remote has changes not present locally".to_string(),
            });
        }

        // Partition into changes that need uploading vs already in graph
        let new_changes: Vec<&PushChange> = to_upload.iter().filter(|c| c.needs_upload()).collect();
        let adopt_count = to_upload.iter().filter(|c| c.in_graph).count();

        // If there's a fork source and changes to adopt, fork the view first.
        // This is a single server-side operation that copies the changelog —
        // no data transfer, no per-change round trips. Views are perspectives.
        if let Some(ref source) = fork_source {
            if adopt_count > 0 {
                let spinner = create_spinner(&format!(
                    "Forking {} view from {}...",
                    style_view(&remote_view),
                    style_view(source)
                ));

                match remote.fork_view(&remote_view, source).await {
                    Ok(count) => {
                        finish_success(
                            &spinner,
                            &format!("Forked {} → {} ({} changes)", source, remote_view, count),
                        );
                    }
                    Err(e) => {
                        finish_error(&spinner, "Fork failed");
                        return Err(convert_remote_error(e, &remote_url));
                    }
                }
            }
        }

        // Now upload only the truly new changes (not in any remote view)
        if new_changes.is_empty() {
            // Everything was handled by the fork
            print_blank();
            print_success(&format!(
                "Push complete: {} view created on {}",
                remote_view, self.remote
            ));
            return Ok(());
        }

        // Upload new changes
        println!(
            "Uploading {}:",
            format_count(new_changes.len(), "new change")
        );
        print_blank();

        let progress = create_progress_bar(new_changes.len() as u64, "Pushing changes");

        let mut _total_bytes_sent: u64 = 0;
        let mut _total_bytes_saved: u64 = 0;
        let mut _delta_count: usize = 0;

        for (i, change) in new_changes.iter().enumerate() {
            let transfer = upload_change_smart(&remote, &repo, &change.hash, &remote_view).await;

            match transfer {
                Ok(result) => {
                    let msg = change.message_or_default();
                    let transfer_info = if result.used_delta {
                        _delta_count += 1;
                        format!(" [{}]", result)
                    } else {
                        String::new()
                    };
                    _total_bytes_sent += result.bytes_sent;
                    _total_bytes_saved += result.bytes_saved;

                    println!(
                        "  {} {} ({}/{}) {}{}",
                        success("✓"),
                        style_hash(&format_hash(&change.hash, false)),
                        i + 1,
                        new_changes.len(),
                        msg,
                        transfer_info,
                    );
                }
                Err(e) => {
                    let msg = change.message_or_default();
                    println!(
                        "  {} {} ({}/{}) {} - {}",
                        error("✗"),
                        style_hash(&format_hash(&change.hash, false)),
                        i + 1,
                        new_changes.len(),
                        msg,
                        e
                    );
                    return Err(e);
                }
            }

            progress.inc(1);
        }

        finish_success(&progress, &format!("Pushed {} changes", new_changes.len()));

        // Upload attestations that cover the pushed changes.
        // Attestations are graph-level audit nodes — they travel with
        // their dependencies but aren't part of any view's changelog.
        let mut attest_count = 0;
        {
            let all_pushed_hashes: Vec<Hash> = to_upload.iter().map(|c| c.hash).collect();

            // Collect unique attestations — the same attestation can cover
            // multiple changes, so searching per-change produces duplicates.
            let mut seen_attest: std::collections::HashSet<Hash> = std::collections::HashSet::new();
            let mut unique_attestations: Vec<(
                Hash,
                atomic_core::change::attestation::Attestation,
            )> = Vec::new();

            for pushed_hash in &all_pushed_hashes {
                let attestations = repo
                    .find_attestations_for_change(pushed_hash)
                    .unwrap_or_default();

                for (attest_hash, attestation) in attestations {
                    if !seen_attest.insert(attest_hash) {
                        continue; // Already collected this attestation
                    }

                    // Only upload if all covered changes have been pushed
                    let all_covered = attestation
                        .changes_covered
                        .iter()
                        .all(|h| all_pushed_hashes.contains(h));

                    if all_covered {
                        unique_attestations.push((attest_hash, attestation));
                    }
                }
            }

            for (attest_hash, attestation) in &unique_attestations {
                // Load and upload the raw attestation bytes
                let attest_data = match attestation.serialize() {
                    Ok(data) => Bytes::from(data),
                    Err(_) => continue,
                };

                match remote
                    .upload_attestation(&attest_hash.to_base32(), attest_data)
                    .await
                {
                    Ok(()) => {
                        attest_count += 1;
                        println!(
                            "  {} {} attestation ({}, {} covered)",
                            success("✓"),
                            style_hash(&format_hash(attest_hash, false)),
                            attestation.cost_display(),
                            attestation.change_count(),
                        );
                    }
                    Err(e) => {
                        print_warning(&format!(
                            "Failed to upload attestation {}: {}",
                            &attest_hash.to_base32()[..12],
                            e
                        ));
                    }
                }
            }
        }

        // Upload provenance graphs that explain the pushed changes.
        // Provenance graphs are causal decision DAGs — they travel with
        // their dependencies but aren't part of any view's changelog.
        let mut provenance_count = 0;
        {
            let all_pushed_hashes: Vec<Hash> = to_upload.iter().map(|c| c.hash).collect();

            // Collect unique provenance graphs — same dedup logic as attestations.
            let mut seen_prov: std::collections::HashSet<Hash> = std::collections::HashSet::new();
            let mut unique_graphs: Vec<(Hash, atomic_core::change::ProvenanceGraph)> = Vec::new();

            for pushed_hash in &all_pushed_hashes {
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
                // Only upload if all explained changes have been pushed
                let all_explained = graph
                    .changes_explained
                    .iter()
                    .all(|h| all_pushed_hashes.contains(h));

                if !all_explained {
                    continue;
                }

                // Load and upload the raw provenance bytes
                let prov_data = match graph.serialize() {
                    Ok(data) => Bytes::from(data),
                    Err(_) => continue,
                };

                match remote
                    .upload_provenance(&prov_hash.to_base32(), prov_data)
                    .await
                {
                    Ok(()) => {
                        provenance_count += 1;
                        println!(
                            "  {} {} provenance ({} nodes, {} changes)",
                            success("✓"),
                            style_hash(&format_hash(prov_hash, false)),
                            graph.node_count(),
                            graph.change_count(),
                        );
                    }
                    Err(e) => {
                        print_warning(&format!(
                            "Failed to upload provenance graph {}: {}",
                            &prov_hash.to_base32()[..12],
                            e
                        ));
                    }
                }
            }
        }

        // Upload tags for the pushed view.
        let mut tag_count = 0;
        {
            let tags = repo.list_tags_for_view(&remote_view).unwrap_or_default();

            for tag in &tags {
                let tag_hash = tag.content_hash();
                let tag_hash_str = tag_hash.to_base32();

                let tag_bytes = match atomic_repository::serialize_tag(tag) {
                    Ok(bytes) => Bytes::from(bytes),
                    Err(e) => {
                        print_warning(&format!("Failed to serialize tag '{}': {}", tag.name, e));
                        continue;
                    }
                };

                match remote
                    .upload_tag(&tag_hash_str, &remote_view, tag_bytes)
                    .await
                {
                    Ok(()) => {
                        tag_count += 1;
                        println!(
                            "  {} {} tag '{}' ({})",
                            success("\u{2713}"),
                            style_hash(&tag_hash_str[..12]),
                            tag.name,
                            tag.kind,
                        );
                    }
                    Err(e) => {
                        print_warning(&format!("Failed to upload tag '{}': {}", tag.name, e));
                    }
                }
            }
        }

        // Summary
        print_blank();
        let mut summary = format!(
            "Push complete: {} uploaded to {}",
            format_count(new_changes.len(), "change"),
            remote_view
        );
        if adopt_count > 0 {
            summary.push_str(&format!(
                " (forked {} from {})",
                format_count(adopt_count, "change"),
                fork_source.as_deref().unwrap_or("graph")
            ));
        }
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
    /// # Workflow
    ///
    /// 1. Find and open the local repository
    /// 2. Resolve the remote URL from configuration or argument
    /// 3. Determine local and remote view names
    /// 4. Connect to the remote server
    /// 5. Query remote state and changelist
    /// 6. Calculate which changes need to be pushed
    /// 7. If dry run, display preview and exit
    /// 8. Upload changes in dependency order
    /// 9. Upload any tagged states
    /// 10. Display summary
    ///
    /// # Errors
    ///
    /// Returns errors for:
    /// - Repository not found
    /// - Remote not configured
    /// - Network/connection failures
    /// - Authentication failures
    /// - Diverged histories (without --force)
    fn run(&self) -> CliResult<()> {
        // Create a tokio runtime for async operations
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

    // Push Command Construction Tests

    #[test]
    fn test_push_new() {
        let push = Push::new();
        assert_eq!(push.remote, "origin");
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
        assert_eq!(push.remote, "origin");
    }

    #[test]
    fn test_push_with_remote() {
        let push = Push::new().with_remote("upstream");
        assert_eq!(push.remote, "upstream");
    }

    #[test]
    fn test_push_with_remote_url() {
        let push = Push::new().with_remote("https://example.com/repo");
        assert_eq!(push.remote, "https://example.com/repo");
    }

    #[test]
    fn test_push_with_to_view() {
        let push = Push::new().with_to_view("main");
        assert_eq!(push.to_view, Some("main".to_string()));
    }

    #[test]
    fn test_push_with_from_view() {
        let push = Push::new().with_from_view("dev");
        assert_eq!(push.from_view, Some("dev".to_string()));
    }

    #[test]
    fn test_push_with_dry_run() {
        let push = Push::new().with_dry_run(true);
        assert!(push.dry_run);

        let push2 = push.with_dry_run(false);
        assert!(!push2.dry_run);
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
            .with_force(false)
            .with_all(true)
            .with_insecure(true)
            .with_timeout(120);

        assert_eq!(push.remote, "upstream");
        assert_eq!(push.to_view, Some("main".to_string()));
        assert_eq!(push.from_view, Some("feature".to_string()));
        assert!(push.dry_run);
        assert!(!push.force);
        assert!(push.all);
        assert!(push.insecure);
        assert_eq!(push.timeout, 120);
    }

    #[test]
    fn test_push_clone() {
        let push = Push::new().with_remote("test").with_dry_run(true);
        let cloned = push.clone();

        assert_eq!(push.remote, cloned.remote);
        assert_eq!(push.dry_run, cloned.dry_run);
    }

    #[test]
    fn test_push_debug() {
        let push = Push::new();
        let debug_str = format!("{:?}", push);

        assert!(debug_str.contains("Push"));
        assert!(debug_str.contains("origin"));
    }

    // Remote Channel Tests

    #[test]
    fn test_get_remote_view_explicit() {
        let push = Push::new().with_to_view("main");
        assert_eq!(push.get_remote_view("dev"), "main");
    }

    #[test]
    fn test_get_remote_view_default() {
        let push = Push::new();
        assert_eq!(push.get_remote_view("dev"), "dev");
    }

    // Remote Config Tests

    #[tokio::test]
    async fn test_build_remote_config_default() {
        let push = Push::new();
        let config = push
            .build_remote_config("http://test.localhost:8080/code")
            .await;

        assert_eq!(config.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert!(!config.danger_accept_invalid_certs);
    }

    #[tokio::test]
    async fn test_build_remote_config_custom_timeout() {
        let push = Push::new().with_timeout(120);
        let config = push
            .build_remote_config("http://test.localhost:8080/code")
            .await;

        assert_eq!(config.timeout, Duration::from_secs(120));
    }

    #[tokio::test]
    async fn test_build_remote_config_insecure() {
        let push = Push::new().with_insecure(true);
        let config = push
            .build_remote_config("http://test.localhost:8080/code")
            .await;

        assert!(config.danger_accept_invalid_certs);
    }

    // Constants Tests

    #[test]
    fn test_default_remote() {
        assert_eq!(DEFAULT_REMOTE, "origin");
    }

    #[test]
    fn test_default_timeout() {
        assert_eq!(DEFAULT_TIMEOUT_SECS, 30);
    }
}
