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
//! 6. **Query Remote State**: Get channel state and changelist
//! 7. **Download Changes**: Fetch all changes in order
//! 8. **Apply Changes**: Apply to local view (unless download-only)
//! 9. **Configure Remote**: Save "origin" remote configuration
//! 10. **Report Results**: Display summary to user
//!
//! # Error Handling
//!
//! The command provides detailed error messages with suggestions for
//! resolution. If the clone fails partway through, the `CleanupGuard`
//! ensures the partially created directory is removed.

use std::path::Path;
use std::time::Duration;

use clap::Parser;
use git2::Repository as GitRepository;

use atomic_core::types::Base32;
use atomic_remote::{HttpRemote, HttpRemoteConfig};
use atomic_repository::Repository;

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{
    create_progress_bar, create_spinner, error, finish_error, finish_success, hash as style_hash,
    hint, print_blank, print_hint, print_info, print_success, print_warning, success,
    view as style_view,
};

use super::helpers::{
    convert_remote_error, format_bytes, format_count, infer_repo_name, resolve_target_path,
    save_downloaded_change, validate_target_path, CleanupGuard,
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

        // Clone must insert remote changes into the requested local view,
        // rather than whichever default view repository initialization chose.
        if !repo.view_exists(&self.view).map_err(CliError::Repository)? {
            repo.create_shared_view(&self.view)
                .map_err(CliError::Repository)?;
        }
        repo.align_to_view(&self.view)
            .map_err(CliError::Repository)?;

        // Connect to remote
        let spinner = create_spinner("Connecting to remote...");
        let config = self.build_remote_config().await;
        let remote = HttpRemote::with_config(&self.url, config).map_err(|e| {
            finish_error(&spinner, "Failed to connect");
            convert_remote_error(e, &self.url)
        })?;
        finish_success(&spinner, "Connected");

        // Query remote state
        let spinner = create_spinner("Querying remote state...");
        let remote_state = remote.get_state(&self.view).await.map_err(|e| {
            finish_error(&spinner, "Failed to query state");
            convert_remote_error(e, &self.url)
        })?;

        if remote_state.is_empty() {
            finish_success(&spinner, &format!("View '{}' is empty", self.view));

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
        }

        let remote_merkle = remote_state.merkle().map(|m| m.to_string());
        finish_success(
            &spinner,
            &format!(
                "View '{}' at {}",
                self.view,
                remote_merkle
                    .as_ref()
                    .map(|m| format!("{}...", &m[..12.min(m.len())]))
                    .unwrap_or_else(|| "(empty)".to_string())
            ),
        );

        // Get remote changelist
        let spinner = create_spinner("Fetching changelist...");
        let remote_entries = remote.get_changelist(&self.view, 0).await.map_err(|e| {
            finish_error(&spinner, "Failed to fetch changelist");
            convert_remote_error(e, &self.url)
        })?;
        finish_success(
            &spinner,
            &format!("Found {}", format_count(remote_entries.len(), "change")),
        );

        // Initialize progress tracking
        let mut stats = CloneStats::new();
        let mut progress = CloneProgress::new(remote_entries.len());
        progress.phase = ClonePhase::Downloading;

        // Download changes
        if !remote_entries.is_empty() {
            print_blank();
            println!(
                "Downloading {}:",
                format_count(remote_entries.len(), "change")
            );
            print_blank();

            let progress_bar =
                create_progress_bar(remote_entries.len() as u64, "Downloading changes");

            for (i, entry) in remote_entries.iter().enumerate() {
                let hash_str = &entry.hash;
                let hash_display = &hash_str[..12.min(hash_str.len())];

                // Download the change
                let result = remote.download_change(hash_str).await;

                match result {
                    Ok(data) => {
                        let data_len = data.len() as u64;

                        // Parse and verify the hash
                        let expected_hash =
                            match atomic_core::types::Hash::from_base32(hash_str.as_bytes()) {
                                Some(h) => h,
                                None => {
                                    stats.record_failed();
                                    println!(
                                        "  {} {}... ({}/{}) invalid hash format",
                                        error("✗"),
                                        hash_display,
                                        i + 1,
                                        remote_entries.len()
                                    );
                                    continue;
                                }
                            };

                        // Save to local change store
                        match save_downloaded_change(&repo, &expected_hash, data) {
                            Ok(()) => {
                                stats.record_change_downloaded(data_len);
                                progress.record_downloaded();
                                println!(
                                    "  {} {}... ({}/{})",
                                    success("✓"),
                                    style_hash(hash_display),
                                    i + 1,
                                    remote_entries.len()
                                );
                            }
                            Err(e) => {
                                stats.record_failed();
                                println!(
                                    "  {} {}... ({}/{}) save failed: {}",
                                    error("✗"),
                                    hash_display,
                                    i + 1,
                                    remote_entries.len(),
                                    e
                                );
                            }
                        }
                    }
                    Err(e) => {
                        stats.record_failed();
                        println!(
                            "  {} {}... ({}/{}) download failed: {}",
                            error("✗"),
                            hash_display,
                            i + 1,
                            remote_entries.len(),
                            e
                        );

                        // Stop on first failure to maintain consistency
                        return Err(convert_remote_error(e, &self.url));
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
        }

        // Apply changes (unless download-only)
        if self.download_only {
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

        // Apply downloaded changes to the local view
        let mut apply_errors = Vec::new();
        if stats.changes_downloaded > 0 {
            print_blank();
            progress.phase = ClonePhase::Applying;
            let spinner = create_spinner("Inserting changes into view...");

            // Insert each downloaded change in order to build the graph
            let insert_options = atomic_repository::InsertOptions::default();

            for entry in &remote_entries {
                let hash = match atomic_core::types::Hash::from_base32(entry.hash.as_bytes()) {
                    Some(h) => h,
                    None => continue,
                };

                match repo.insert_change(&hash, insert_options.clone()) {
                    Ok(_outcome) => {
                        stats.record_applied();
                        progress.record_applied();
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        // "already applied" is fine — skip it
                        if msg.contains("already applied") || msg.contains("AlreadyApplied") {
                            stats.record_applied();
                            progress.record_applied();
                        } else {
                            apply_errors.push(format!(
                                "{}...: {}",
                                &entry.hash[..12.min(entry.hash.len())],
                                msg
                            ));
                        }
                    }
                }
            }

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

            if stats.has_applied() {
                finish_success(
                    &spinner,
                    &format!(
                        "Applied {} to {}",
                        format_count(stats.changes_applied, "change"),
                        self.view
                    ),
                );
            } else {
                finish_error(&spinner, "No changes were applied");
            }

            if !apply_errors.is_empty() {
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

        // Final summary. Download and apply failures were already printed
        // above — exit non-zero so scripts don't mistake a partial clone for
        // success (the partially-cloned repository is kept on disk).
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

            // Keep the partial clone: disable the cleanup guard before failing.
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

    /// Test chaining multiple builder methods.
    #[test]
    fn test_clone_builder_chain() {
        let clone = Clone::new("https://example.com/repo".to_string())
            .with_path("my-project")
            .with_view("dev")
            .with_insecure(true)
            .with_timeout(120)
            .with_download_only(true)
            .with_into_existing(true);

        assert_eq!(clone.url, "https://example.com/repo");
        assert_eq!(clone.path, Some("my-project".to_string()));
        assert_eq!(clone.view, "dev");
        assert!(clone.insecure);
        assert_eq!(clone.timeout, 120);
        assert!(clone.download_only);
        assert!(clone.into_existing);
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
}
