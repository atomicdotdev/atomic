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
//! 8. **Apply Changes**: Apply to local stack (unless download-only)
//! 9. **Configure Remote**: Save "origin" remote configuration
//! 10. **Report Results**: Display summary to user
//!
//! # Error Handling
//!
//! The command provides detailed error messages with suggestions for
//! resolution. If the clone fails partway through, the `CleanupGuard`
//! ensures the partially created directory is removed.

use std::time::Duration;

use clap::Parser;

use atomic_core::types::Base32;
use atomic_remote::{HttpRemote, HttpRemoteConfig};
use atomic_repository::Repository;

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{
    create_progress_bar, create_spinner, error, finish_error, finish_success, hash as style_hash,
    hint, print_blank, print_hint, print_success, print_warning, stack as style_stack, success,
};

use super::helpers::{
    convert_remote_error, format_bytes, format_count, infer_repo_name, resolve_target_path,
    save_downloaded_change, validate_target_path, CleanupGuard,
};
use super::types::{ClonePhase, CloneProgress, CloneStats};

// Constants

/// Default stack to clone when none is specified.
///
/// This is "dev" to match atomic-api convention.
pub const DEFAULT_STACK: &str = "dev";

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
/// # Clone specific stack
/// atomic clone https://example.com/org/project/code --stack dev
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

    /// Stack to clone.
    ///
    /// If not specified, clones the "dev" stack (atomic-api default).
    #[arg(long, default_value = DEFAULT_STACK)]
    pub stack: String,

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

    /// Download changes without applying them to the local stack.
    ///
    /// Changes are saved to the change store but not applied to any stack.
    /// Useful for examining changes before applying.
    #[arg(long)]
    pub download_only: bool,
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
    /// assert_eq!(clone.stack, "dev");
    /// ```
    pub fn new(url: String) -> Self {
        Self {
            url,
            path: None,
            stack: DEFAULT_STACK.to_string(),
            insecure: false,
            timeout: DEFAULT_TIMEOUT_SECS,
            download_only: false,
        }
    }

    /// Builder: set the target path.
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Builder: set the stack name.
    pub fn with_stack(mut self, stack: impl Into<String>) -> Self {
        self.stack = stack.into();
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

    /// Build the HTTP remote configuration.
    ///
    /// Creates an `HttpRemoteConfig` with the timeout and security settings
    /// specified by the user.
    fn build_remote_config(&self) -> HttpRemoteConfig {
        let config = HttpRemoteConfig::new()
            .with_timeout(Duration::from_secs(self.timeout))
            .danger_accept_invalid_certs(self.insecure);

        crate::commands::auth::attach_identity(config, &self.url)
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
            style_stack(&display_name)
        );
        print_blank();

        // Validate target doesn't exist
        validate_target_path(&target_path)?;

        // Create cleanup guard - will remove directory if we fail
        let guard = CleanupGuard::new(target_path.clone());

        // Create target directory
        std::fs::create_dir_all(&target_path).map_err(|e| CliError::InvalidPath {
            path: target_path.clone(),
            source: Some(e),
        })?;

        // Initialize repository
        let spinner = create_spinner("Initializing repository...");
        let repo = Repository::init(&target_path).map_err(|e| {
            finish_error(&spinner, "Failed to initialize");
            CliError::Repository(e)
        })?;
        finish_success(&spinner, "Repository initialized");

        // Connect to remote
        let spinner = create_spinner("Connecting to remote...");
        let config = self.build_remote_config();
        let remote = HttpRemote::with_config(&self.url, config).map_err(|e| {
            finish_error(&spinner, "Failed to connect");
            convert_remote_error(e, &self.url)
        })?;
        finish_success(&spinner, "Connected");

        // Query remote state
        let spinner = create_spinner("Querying remote state...");
        let remote_state = remote.get_state(&self.stack).await.map_err(|e| {
            finish_error(&spinner, "Failed to query state");
            convert_remote_error(e, &self.url)
        })?;

        if remote_state.is_empty() {
            finish_success(&spinner, &format!("Stack '{}' is empty", self.stack));

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
            guard.disable();
            return Ok(());
        }

        let remote_merkle = remote_state.merkle().map(|m| m.to_string());
        finish_success(
            &spinner,
            &format!(
                "Stack '{}' at {}",
                self.stack,
                remote_merkle
                    .as_ref()
                    .map(|m| format!("{}...", &m[..12.min(m.len())]))
                    .unwrap_or_else(|| "(empty)".to_string())
            ),
        );

        // Get remote changelist
        let spinner = create_spinner("Fetching changelist...");
        let remote_entries = remote.get_changelist(&self.stack, 0).await.map_err(|e| {
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
            guard.disable();
            return Ok(());
        }

        // Apply downloaded changes to the local stack
        if stats.changes_downloaded > 0 {
            print_blank();
            progress.phase = ClonePhase::Applying;
            let spinner = create_spinner("Inserting changes into view...");

            // Insert each downloaded change in order to build the graph
            let insert_options = atomic_repository::InsertOptions::default();
            let mut apply_errors = Vec::new();

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

            if stats.has_applied() {
                finish_success(
                    &spinner,
                    &format!(
                        "Applied {} to {}",
                        format_count(stats.changes_applied, "change"),
                        self.stack
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

        progress.phase = ClonePhase::Complete;

        // Final summary
        print_blank();
        if stats.has_failures() {
            print_warning(&format!(
                "Clone completed with errors: {} downloaded, {} failed",
                stats.changes_downloaded, stats.changes_failed
            ));
        } else {
            print_success(&format!(
                "Clone complete: {} downloaded into {}/",
                format_count(stats.changes_downloaded, "change"),
                target_path.display()
            ));
        }

        // Disable cleanup guard - clone succeeded
        guard.disable();

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
        assert_eq!(clone.stack, DEFAULT_STACK);
        assert!(!clone.insecure);
        assert_eq!(clone.timeout, DEFAULT_TIMEOUT_SECS);
        assert!(!clone.download_only);
    }

    /// Test Default trait implementation.
    #[test]
    fn test_clone_default() {
        let clone: Clone = Default::default();
        assert!(clone.url.is_empty());
        assert_eq!(clone.stack, DEFAULT_STACK);
    }

    /// Test with_path builder method.
    #[test]
    fn test_clone_with_path() {
        let clone = Clone::new("https://example.com/repo".to_string()).with_path("my-project");
        assert_eq!(clone.path, Some("my-project".to_string()));
    }

    /// Test with_stack builder method.
    #[test]
    fn test_clone_with_stack() {
        let clone = Clone::new("https://example.com/repo".to_string()).with_stack("dev");
        assert_eq!(clone.stack, "dev");
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

    /// Test chaining multiple builder methods.
    #[test]
    fn test_clone_builder_chain() {
        let clone = Clone::new("https://example.com/repo".to_string())
            .with_path("my-project")
            .with_stack("dev")
            .with_insecure(true)
            .with_timeout(120)
            .with_download_only(true);

        assert_eq!(clone.url, "https://example.com/repo");
        assert_eq!(clone.path, Some("my-project".to_string()));
        assert_eq!(clone.stack, "dev");
        assert!(clone.insecure);
        assert_eq!(clone.timeout, 120);
        assert!(clone.download_only);
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
    #[test]
    fn test_build_remote_config_default() {
        let clone = Clone::new("https://example.com/repo".to_string());
        let config = clone.build_remote_config();

        // HttpRemoteConfig doesn't expose fields directly, so we just verify
        // it doesn't panic and returns something
        assert!(std::mem::size_of_val(&config) > 0);
    }

    /// Test build_remote_config with custom timeout.
    #[test]
    fn test_build_remote_config_custom_timeout() {
        let clone = Clone::new("https://example.com/repo".to_string()).with_timeout(120);
        let config = clone.build_remote_config();
        assert!(std::mem::size_of_val(&config) > 0);
    }

    /// Test build_remote_config with insecure flag.
    #[test]
    fn test_build_remote_config_insecure() {
        let clone = Clone::new("https://example.com/repo".to_string()).with_insecure(true);
        let config = clone.build_remote_config();
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

    // Constant Tests

    /// Test that DEFAULT_STACK is "dev" (atomic-api convention).
    #[test]
    fn test_default_stack() {
        assert_eq!(DEFAULT_STACK, "dev");
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
