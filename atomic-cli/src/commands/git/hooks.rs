//! Git hook installer for automatic Atomic sync.
//!
//! Installs `post-commit`, `post-merge`, and `post-rewrite` hooks that
//! call `atomic git import --incremental` after each git operation.
//! The import is idempotent — if a commit is already indexed in
//! GIT_SHA_INDEX, it's skipped.

use std::fs;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_warning};

const MARKER_BEGIN: &str = "# atomic:git:begin";
const MARKER_END: &str = "# atomic:git:end";

/// The hook script body inserted between markers.
/// Fails silently (|| true) so git operations never break.
const HOOK_BODY: &str = r#"atomic git import --incremental 2>/dev/null || true"#;

/// The three hooks we install.
const HOOK_NAMES: &[&str] = &["post-commit", "post-merge", "post-rewrite"];

/// Manage Git hooks for automatic Atomic sync.
#[derive(Debug, Args)]
pub struct Hooks {
    #[command(subcommand)]
    pub command: HookCommands,
}

#[derive(Debug, Subcommand)]
pub enum HookCommands {
    /// Install Git hooks for automatic sync.
    ///
    /// Adds `post-commit`, `post-merge`, and `post-rewrite` hooks that
    /// run `atomic git import --incremental` after each git operation.
    /// Existing hooks are preserved — Atomic appends between markers.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic git hooks install
    /// ```
    Install,

    /// Remove Atomic's Git hooks.
    ///
    /// Removes only the Atomic-managed sections (between markers).
    /// Other hook content is preserved.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic git hooks uninstall
    /// ```
    Uninstall,

    /// Show the current hook status.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic git hooks status
    /// ```
    Status,
}

impl Command for Hooks {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            HookCommands::Install => install_hooks(),
            HookCommands::Uninstall => uninstall_hooks(),
            HookCommands::Status => show_status(),
        }
    }
}

/// Find the .git/hooks directory.
fn find_hooks_dir() -> CliResult<PathBuf> {
    let git_repo = git2::Repository::discover(".").map_err(|_| CliError::GitError {
        message: "Not a git repository".to_string(),
    })?;
    let git_dir = git_repo.path(); // .git/
    let hooks_dir = git_dir.join("hooks");
    Ok(hooks_dir)
}

/// Install hooks into .git/hooks/.
fn install_hooks() -> CliResult<()> {
    let hooks_dir = find_hooks_dir()?;
    fs::create_dir_all(&hooks_dir).map_err(|e| CliError::GitError {
        message: format!("Failed to create hooks directory: {}", e),
    })?;

    let mut installed = 0;
    let mut skipped = 0;

    for hook_name in HOOK_NAMES {
        let hook_path = hooks_dir.join(hook_name);
        match install_single_hook(&hook_path, hook_name) {
            Ok(true) => installed += 1,
            Ok(false) => skipped += 1,
            Err(e) => {
                print_warning(&format!("Failed to install {}: {}", hook_name, e));
            }
        }
    }

    if installed > 0 {
        print_info(&format!(
            "Installed {} hook(s). {} already present.",
            installed, skipped
        ));
    } else if skipped > 0 {
        print_info("All hooks already installed.");
    }

    Ok(())
}

/// Install a single hook. Returns Ok(true) if newly installed, Ok(false) if already present.
fn install_single_hook(path: &Path, _name: &str) -> Result<bool, std::io::Error> {
    let existing = if path.exists() {
        fs::read_to_string(path)?
    } else {
        String::new()
    };

    // Already installed?
    if existing.contains(MARKER_BEGIN) {
        return Ok(false);
    }

    // Build the section to append
    let section = format!("\n{}\n{}\n{}\n", MARKER_BEGIN, HOOK_BODY, MARKER_END);

    let new_content = if existing.is_empty() {
        format!("#!/bin/sh\n{}", section)
    } else {
        format!("{}{}", existing.trim_end(), section)
    };

    fs::write(path, &new_content)?;

    // Make executable (unix only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms)?;
    }

    Ok(true)
}

/// Remove Atomic sections from all hooks.
fn uninstall_hooks() -> CliResult<()> {
    let hooks_dir = find_hooks_dir()?;
    let mut removed = 0;

    for hook_name in HOOK_NAMES {
        let hook_path = hooks_dir.join(hook_name);
        if !hook_path.exists() {
            continue;
        }
        match remove_atomic_section(&hook_path) {
            Ok(true) => removed += 1,
            Ok(false) => {}
            Err(e) => {
                print_warning(&format!("Failed to clean {}: {}", hook_name, e));
            }
        }
    }

    if removed > 0 {
        print_info(&format!("Removed Atomic hooks from {} file(s).", removed));
    } else {
        print_info("No Atomic hooks found.");
    }

    Ok(())
}

/// Remove the atomic:git:begin..end section from a hook file.
/// Returns Ok(true) if something was removed.
fn remove_atomic_section(path: &Path) -> Result<bool, std::io::Error> {
    let content = fs::read_to_string(path)?;
    if !content.contains(MARKER_BEGIN) {
        return Ok(false);
    }

    let mut result = String::new();
    let mut in_section = false;

    for line in content.lines() {
        if line.trim() == MARKER_BEGIN {
            in_section = true;
            continue;
        }
        if line.trim() == MARKER_END {
            in_section = false;
            continue;
        }
        if !in_section {
            result.push_str(line);
            result.push('\n');
        }
    }

    // Clean up: if only the shebang remains, delete the file
    let trimmed = result.trim();
    if trimmed.is_empty() || trimmed == "#!/bin/sh" {
        fs::remove_file(path)?;
    } else {
        fs::write(path, &result)?;
    }

    Ok(true)
}

/// Show hook installation status.
fn show_status() -> CliResult<()> {
    let hooks_dir = find_hooks_dir()?;

    for hook_name in HOOK_NAMES {
        let hook_path = hooks_dir.join(hook_name);
        let status = if !hook_path.exists() {
            "not installed"
        } else {
            let content = fs::read_to_string(&hook_path).unwrap_or_default();
            if content.contains(MARKER_BEGIN) {
                "installed"
            } else {
                "exists (no Atomic section)"
            }
        };
        print_info(&format!("  {}: {}", hook_name, status));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_install_new_hook() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("post-commit");
        assert!(install_single_hook(&path, "post-commit").unwrap());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("#!/bin/sh"));
        assert!(content.contains(MARKER_BEGIN));
        assert!(content.contains("atomic git import --incremental"));
        assert!(content.contains(MARKER_END));
    }

    #[test]
    fn test_install_existing_hook_appends() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("post-commit");
        fs::write(&path, "#!/bin/sh\necho 'existing'\n").unwrap();
        assert!(install_single_hook(&path, "post-commit").unwrap());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("echo 'existing'"));
        assert!(content.contains(MARKER_BEGIN));
    }

    #[test]
    fn test_install_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("post-commit");
        assert!(install_single_hook(&path, "post-commit").unwrap());
        // Second install should return false (already present)
        assert!(!install_single_hook(&path, "post-commit").unwrap());
    }

    #[test]
    fn test_uninstall_removes_section() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("post-commit");
        install_single_hook(&path, "post-commit").unwrap();
        assert!(remove_atomic_section(&path).unwrap());
        // File should be deleted (only had shebang + atomic section)
        assert!(!path.exists());
    }

    #[test]
    fn test_uninstall_preserves_other_content() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("post-commit");
        fs::write(&path, "#!/bin/sh\necho 'keep me'\n").unwrap();
        install_single_hook(&path, "post-commit").unwrap();
        remove_atomic_section(&path).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("echo 'keep me'"));
        assert!(!content.contains(MARKER_BEGIN));
    }

    #[test]
    fn test_uninstall_no_section() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("post-commit");
        fs::write(&path, "#!/bin/sh\necho 'no atomic'\n").unwrap();
        assert!(!remove_atomic_section(&path).unwrap());
    }
}
