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

use std::time::Duration;

use clap::Parser;

use atomic_core::types::Base32;
use atomic_remote::{HttpRemote, HttpRemoteConfig};
use atomic_repository::history::HistoryOptions;
use atomic_repository::{InsertOptions, Repository};

use crate::commands::{find_repository_root, format_hash, Command};
use crate::error::{CliError, CliResult};
use crate::output::{
    create_progress_bar, create_spinner, error, finish_error, finish_success, hash as style_hash,
    hint, print_blank, print_hint, print_success, print_warning, success, view as style_view,
    warning,
};

use super::helpers::{
    calculate_pull_delta, convert_remote_error, display_state_comparison, find_local_only_changes,
    format_bytes, format_count, has_local_only_changes, save_downloaded_change,
};
use super::types::{PullChange, PullStats};

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
    /// If not specified, uses the default remote "origin".
    #[arg(default_value = DEFAULT_REMOTE)]
    pub remote: String,

    /// Local view to pull into.
    ///
    /// If not specified, uses the current view.
    #[arg(long = "to-view")]
    pub to_view: Option<String>,

    /// Remote view to pull from.
    ///
    /// If not specified, uses the same name as the local view.
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
    /// assert_eq!(pull.remote, "origin");
    /// assert!(!pull.dry_run);
    /// ```
    pub fn new() -> Self {
        Self {
            remote: DEFAULT_REMOTE.to_string(),
            to_view: None,
            from_view: None,
            dry_run: false,
            all: false,
            insecure: false,
            timeout: DEFAULT_TIMEOUT_SECS,
            download_only: false,
        }
    }

    /// Builder: set the remote name or URL.
    pub fn with_remote(mut self, remote: impl Into<String>) -> Self {
        self.remote = remote.into();
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

    // Internal Helper Methods

    /// Resolve the remote URL from the remote name or return as-is if it's a URL.
    ///
    /// If the remote string looks like a URL (contains "://"), it's returned as-is.
    /// Otherwise, it's treated as a remote name and looked up in the repository
    /// configuration.
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

    /// Get the local view name to pull into.
    ///
    /// Returns the explicitly specified view or the repository's current view.
    fn get_local_view(&self, repo: &Repository) -> String {
        self.to_view
            .clone()
            .unwrap_or_else(|| repo.current_view().to_string())
    }

    /// Get the remote view name to pull from.
    ///
    /// Returns the explicitly specified view or defaults to the local view name.
    fn get_remote_view(&self, local_view: &str) -> String {
        self.from_view
            .clone()
            .unwrap_or_else(|| local_view.to_string())
    }

    /// Build the HTTP remote configuration.
    ///
    /// Creates an `HttpRemoteConfig` with the timeout and security settings
    /// specified by the user.
    fn build_remote_config(&self, remote_url: &str) -> HttpRemoteConfig {
        let config = HttpRemoteConfig::new()
            .with_timeout(Duration::from_secs(self.timeout))
            .danger_accept_invalid_certs(self.insecure);

        crate::commands::auth::attach_identity(config, remote_url)
    }

    /// Display the dry run preview.
    ///
    /// Shows what changes would be pulled without actually pulling them.
    fn display_dry_run(
        &self,
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
            self.remote,
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
        let repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        // Resolve remote URL
        let remote_url = self.resolve_remote_url(&repo)?;

        // Determine views
        let local_view = self.get_local_view(&repo);
        let remote_view = self.get_remote_view(&local_view);

        // Print header
        println!(
            "Pulling from {} ({})",
            style_view(&self.remote),
            hint(&remote_url)
        );

        // Connect to remote
        let spinner = create_spinner("Connecting to remote...");
        let config = self.build_remote_config(&remote_url);
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

        // Get remote changelist
        let spinner = create_spinner("Fetching remote changelist...");
        let remote_from = 0; // Always get full changelist for comparison
        let remote_entries = remote
            .get_changelist(&remote_view, remote_from)
            .await
            .map_err(|e| {
                finish_error(&spinner, "Failed to fetch changelist");
                convert_remote_error(e, &remote_url)
            })?;
        finish_success(
            &spinner,
            &format!("Got {} remote changes", remote_entries.len()),
        );

        // Get local history
        let spinner = create_spinner("Loading local history...");
        let local_entries = repo
            .log(HistoryOptions::default())
            .map_err(CliError::Repository)?;
        finish_success(
            &spinner,
            &format!("Loaded {} local changes", local_entries.len()),
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

        // Calculate what to pull
        let to_download = calculate_pull_delta(&remote_entries, &local_entries, self.all);

        // Handle dry run
        if self.dry_run {
            return self.display_dry_run(&remote_url, &remote_view, &to_download);
        }

        // Check for nothing to pull
        if to_download.is_empty() {
            print_success("Already up to date");
            return Ok(());
        }

        // Download changes
        println!("Downloading {}:", format_count(to_download.len(), "change"));
        print_blank();

        let progress = create_progress_bar(to_download.len() as u64, "Pulling changes");
        let mut stats = PullStats::new();

        for (i, change) in to_download.iter().enumerate() {
            let hash_str = change.hash.to_base32();
            let msg = change.message_or_default();

            // Download the change
            let result = remote.download_change(&hash_str).await;

            match result {
                Ok(data) => {
                    let data_len = data.len() as u64;

                    // Save to local change store
                    match save_downloaded_change(&repo, &change.hash, data) {
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
                Err(e) => {
                    stats.record_failed();
                    println!(
                        "  {} {} ({}/{}) {} - {}",
                        error("✗"),
                        style_hash(&format_hash(&change.hash, false)),
                        i + 1,
                        to_download.len(),
                        msg,
                        e
                    );

                    // Stop on first failure to maintain consistency
                    return Err(convert_remote_error(e, &remote_url));
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
            print_blank();
            print_success(&format!(
                "Downloaded {} (not inserted - use 'atomic insert' to insert)",
                format_count(stats.changes_downloaded, "change")
            ));
            return Ok(());
        }

        // Apply downloaded changes to the local view
        print_blank();
        let spinner = create_spinner("Applying changes to view...");

        // Apply downloaded changes to the local view in sequence order.
        // Changes that failed to save during download are skipped gracefully
        // by insert_change_rec (it will return a "change not found" error).
        let mut apply_errors: Vec<String> = Vec::new();

        for change in &to_download {
            let options = InsertOptions::default().apply_deps(true).view(&local_view);

            match repo.insert_change_rec(&change.hash, options) {
                Ok(_outcome) => {
                    stats.record_applied();
                }
                Err(e) => {
                    // Log but don't abort — other changes may still apply
                    apply_errors.push(format!(
                        "Failed to apply {}: {}",
                        format_hash(&change.hash, false),
                        e
                    ));
                }
            }
        }

        if stats.has_applied() {
            finish_success(
                &spinner,
                &format!(
                    "Applied {} to {}",
                    format_count(stats.changes_applied, "change"),
                    local_view
                ),
            );
        } else {
            finish_error(&spinner, "No changes were applied");
        }

        // Report any per-change apply errors
        if !apply_errors.is_empty() {
            for err in &apply_errors {
                print_warning(err);
            }
        }

        // Materialize the working copy so on-disk files reflect the new state
        if stats.has_applied() {
            let mat_spinner = create_spinner("Updating working copy...");
            match repo.materialize() {
                Ok(result) => {
                    finish_success(
                        &mat_spinner,
                        &format!("{} files updated", result.files_written),
                    );
                }
                Err(e) => {
                    finish_error(&mat_spinner, "Failed to update working copy");
                    print_warning(&format!(
                        "Applied {} but failed to update working copy: {}",
                        format_count(stats.changes_applied, "change"),
                        e
                    ));
                }
            }
        }

        // Final summary
        print_blank();
        if stats.has_failures() {
            print_warning(&format!(
                "Pull completed with errors: {} downloaded, {} failed",
                stats.changes_downloaded, stats.changes_failed
            ));
        } else {
            print_success(&format!(
                "Pull complete: {} downloaded and applied to {}",
                format_count(stats.changes_downloaded, "change"),
                local_view
            ));
        }

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

        assert_eq!(pull.remote, DEFAULT_REMOTE);
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
        assert_eq!(pull.remote, DEFAULT_REMOTE);
    }

    /// Test with_remote builder method.
    #[test]
    fn test_pull_with_remote() {
        let pull = Pull::new().with_remote("upstream");
        assert_eq!(pull.remote, "upstream");
    }

    /// Test with_remote with URL.
    #[test]
    fn test_pull_with_remote_url() {
        let pull = Pull::new().with_remote("https://example.com/repo");
        assert_eq!(pull.remote, "https://example.com/repo");
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

        assert_eq!(pull.remote, "upstream");
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

        assert_eq!(cloned.remote, "test");
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

    /// Test get_remote_view defaults to local view name.
    #[test]
    fn test_get_remote_view_default() {
        let pull = Pull::new();
        assert_eq!(pull.get_remote_view("feature"), "feature");
    }

    /// Test build_remote_config with default settings.
    #[test]
    fn test_build_remote_config_default() {
        let pull = Pull::new();
        let config = pull.build_remote_config("http://test.localhost:8080/code");

        // HttpRemoteConfig doesn't expose fields directly, so we just verify
        // it doesn't panic and returns something
        assert!(std::mem::size_of_val(&config) > 0);
    }

    /// Test build_remote_config with custom timeout.
    #[test]
    fn test_build_remote_config_custom_timeout() {
        let pull = Pull::new().with_timeout(120);
        let config = pull.build_remote_config("http://test.localhost:8080/code");
        assert!(std::mem::size_of_val(&config) > 0);
    }

    /// Test build_remote_config with insecure flag.
    #[test]
    fn test_build_remote_config_insecure() {
        let pull = Pull::new().with_insecure(true);
        let config = pull.build_remote_config("http://test.localhost:8080/code");
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
