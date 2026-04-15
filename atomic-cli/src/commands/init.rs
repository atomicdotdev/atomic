//! The `init` command for initializing a new Atomic repository.
//!
//! This module implements the `atomic init` command, which creates a new
//! Atomic repository in the specified directory (or current directory by
//! default). The initialization process sets up the `.atomic` directory
//! structure and creates an initial view.
//!
//! # Usage
//!
//! ```text
//! atomic init [OPTIONS] [PATH]
//!
//! Arguments:
//!   [PATH]  Path to initialize (defaults to current directory)
//!
//! Options:
//!   -s, --view <NAME>   Name of the initial view (defaults to "dev")
//!   -k, --kind <KIND>   Project kind for .atomicignore template
//!   -h, --help          Print help information
//! ```
//!
//! # Examples
//!
//! Initialize in the current directory:
//! ```text
//! $ atomic init
//! Initialized empty Atomic repository in /home/user/project/.atomic
//! Created view: dev
//!
//! Next steps:
//!   atomic add <files>      Add files to track
//!   atomic record -m "..."  Record your first change
//! ```
//!
//! Initialize with a custom view name:
//! ```text
//! $ atomic init --view main
//! Initialized empty Atomic repository in /home/user/project/.atomic
//! Created view: main
//! ```
//!
//! Initialize a Rust project (creates appropriate .atomicignore):
//! ```text
//! $ atomic init --kind rust
//! Initialized empty Atomic repository in /home/user/project/.atomic
//! Created view: dev
//! Created .atomicignore for rust project
//! ```

use std::path::{Path, PathBuf};

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_next_steps, print_success};

// Constants

/// Default view name for new repositories.
pub const DEFAULT_VIEW_NAME: &str = "dev";

// Project Kind Templates

/// Get the .atomicignore content for a given project kind.
///
/// This function returns appropriate ignore patterns for common project
/// types, helping users start with sensible defaults.
///
/// # Arguments
///
/// * `kind` - The project kind (e.g., "rust", "python", "node")
///
/// # Returns
///
/// The content for the .atomicignore file, or `None` if the kind is unknown.
///
/// # Supported Kinds
///
/// - `rust`: Ignores `target/`, `Cargo.lock` (for libraries)
/// - `python`: Ignores `__pycache__/`, `.venv/`, `*.pyc`
/// - `node` / `javascript` / `typescript`: Ignores `node_modules/`, `dist/`
/// - `go`: Ignores `bin/`, `*.exe`
/// - `java`: Ignores `target/`, `*.class`, `*.jar`
/// - `c` / `cpp`: Ignores `*.o`, `*.a`, `*.so`, `build/`
pub fn get_ignore_template(kind: &str) -> Option<&'static str> {
    match kind.to_lowercase().as_str() {
        "rust" => Some(
            r#"# Rust
target/
**/*.rs.bk
Cargo.lock

# IDE
.idea/
.vscode/
*.swp
*.swo
*~

# OS
.DS_Store
Thumbs.db
"#,
        ),
        "python" => Some(
            r#"# Python
__pycache__/
*.py[cod]
*$py.class
*.so
.Python
build/
dist/
*.egg-info/
.eggs/
.venv/
venv/
ENV/
.pytest_cache/
.mypy_cache/
.coverage
htmlcov/

# IDE
.idea/
.vscode/
*.swp
*~

# OS
.DS_Store
Thumbs.db
"#,
        ),
        "node" | "javascript" | "typescript" | "js" | "ts" => Some(
            r#"# Node.js
node_modules/
npm-debug.log*
yarn-debug.log*
yarn-error.log*
.npm
.yarn
dist/
build/
coverage/
.env
.env.local
.env.*.local

# IDE
.idea/
.vscode/
*.swp
*~

# OS
.DS_Store
Thumbs.db
"#,
        ),
        "go" | "golang" => Some(
            r#"# Go
bin/
pkg/
*.exe
*.exe~
*.dll
*.so
*.dylib
*.test
*.out
vendor/

# IDE
.idea/
.vscode/
*.swp
*~

# OS
.DS_Store
Thumbs.db
"#,
        ),
        "java" | "kotlin" => Some(
            r#"# Java/Kotlin
target/
*.class
*.jar
*.war
*.ear
*.logs
*.log
.gradle/
build/
out/

# IDE
.idea/
*.iml
.vscode/
*.swp
*~

# OS
.DS_Store
Thumbs.db
"#,
        ),
        "c" | "cpp" | "c++" => Some(
            r#"# C/C++
*.o
*.a
*.so
*.dylib
*.dll
*.exe
*.out
build/
cmake-build-*/
CMakeFiles/
CMakeCache.txt
Makefile
*.make

# IDE
.idea/
.vscode/
*.swp
*~
compile_commands.json

# OS
.DS_Store
Thumbs.db
"#,
        ),
        _ => None,
    }
}

/// Get a list of supported project kinds for help text.
pub fn supported_kinds() -> &'static [&'static str] {
    &[
        "rust",
        "python",
        "node",
        "javascript",
        "typescript",
        "go",
        "java",
        "kotlin",
        "c",
        "cpp",
    ]
}

// Init Command

/// Initialize a new Atomic repository.
///
/// Creates the `.atomic` directory structure and sets up an initial view.
/// If the directory doesn't exist, it will be created. If a repository
/// already exists at the path, an error is returned.
///
/// # Fields
///
/// * `path` - The directory to initialize (defaults to current directory)
/// * `view` - Name of the initial view (defaults to "dev")
/// * `kind` - Project kind for generating .atomicignore
#[derive(Parser, Debug)]
#[command(about = "Initialize a new Atomic repository")]
pub struct Init {
    /// Path to initialize (defaults to current directory).
    ///
    /// If the directory doesn't exist, it will be created.
    /// If a repository already exists at this path, an error is returned.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Name of the initial view (defaults to "dev").
    ///
    /// Atomic uses views instead of branches. The initial view is where
    /// you'll record your first changes. You can create more views later
    /// with `atomic view create`.
    #[arg(long, short = 's', default_value = DEFAULT_VIEW_NAME)]
    pub view: String,

    /// Project kind for .atomicignore template.
    ///
    /// If specified, creates a .atomicignore file with appropriate patterns
    /// for the given project type. Supported kinds: rust, python, node,
    /// javascript, typescript, go, java, kotlin, c, cpp.
    #[arg(long, short = 'k')]
    pub kind: Option<String>,

    /// Initialize with a vault (shared project knowledge store).
    ///
    /// Enabled by default. Creates `.vault/` with subdirectories for goals,
    /// memory, skills, intents, and scratch notes. The vault stores project
    /// knowledge as versioned markdown files with cryptographic provenance.
    ///
    /// Use `--no-vault` to skip vault initialization.
    #[arg(long, default_value_t = true, action = clap::ArgAction::SetTrue)]
    pub vault: bool,

    /// Skip vault initialization.
    ///
    /// Creates a repository without the `.vault/` knowledge store.
    /// Useful for minimal repos that don't need goals, intents, or memory.
    #[arg(long, default_value_t = false)]
    pub no_vault: bool,
}

impl Init {
    /// Create a new Init command with default values.
    ///
    /// This is useful for testing or programmatic repository creation.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let init = Init::new();
    /// init.run()?;
    /// ```
    pub fn new() -> Self {
        Self {
            path: PathBuf::from("."),
            view: DEFAULT_VIEW_NAME.to_string(),
            kind: None,
            vault: true,
            no_vault: false,
        }
    }

    /// Create a new Init command targeting a specific path.
    pub fn at_path<P: Into<PathBuf>>(path: P) -> Self {
        Self {
            path: path.into(),
            view: DEFAULT_VIEW_NAME.to_string(),
            kind: None,
            vault: true,
            no_vault: false,
        }
    }

    /// Builder: set the initial view name.
    pub fn with_view(mut self, view: impl Into<String>) -> Self {
        self.view = view.into();
        self
    }

    /// Builder: set the project kind for .atomicignore template.
    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }

    /// Builder: enable vault initialization.
    pub fn with_vault(mut self) -> Self {
        self.vault = true;
        self.no_vault = false;
        self
    }

    /// Builder: disable vault initialization.
    pub fn without_vault(mut self) -> Self {
        self.vault = false;
        self.no_vault = true;
        self
    }

    /// Resolve the target path to an absolute path.
    ///
    /// If the path is relative, it's resolved relative to the current
    /// working directory.
    fn resolve_path(&self) -> CliResult<PathBuf> {
        // On Windows, Path::is_absolute() returns false for Unix-style "/foo"
        // paths (they're drive-relative). Treat a leading '/' as absolute on
        // all platforms so cross-platform tests and user input behave the same.
        let looks_absolute =
            self.path.is_absolute() || self.path.to_string_lossy().starts_with('/');
        let path = if looks_absolute {
            self.path.clone()
        } else {
            let cwd =
                std::env::current_dir().map_err(|e| CliError::invalid_path(&self.path, Some(e)))?;
            cwd.join(&self.path)
        };

        // Canonicalize if the path exists, otherwise just normalize it
        if path.exists() {
            path.canonicalize()
                .map_err(|e| CliError::invalid_path(&self.path, Some(e)))
        } else {
            Ok(path)
        }
    }

    /// Create the .atomicignore file if a kind is specified.
    fn create_ignore_file(&self, repo_path: &Path) -> CliResult<bool> {
        let Some(kind) = &self.kind else {
            return Ok(false);
        };

        let Some(template) = get_ignore_template(kind) else {
            // Unknown kind - warn but don't fail
            print_hint(&format!(
                "Unknown project kind '{}'. Supported kinds: {}",
                kind,
                supported_kinds().join(", ")
            ));
            return Ok(false);
        };

        let ignore_path = repo_path.join(".atomicignore");

        // Don't overwrite existing ignore file
        if ignore_path.exists() {
            print_hint(".atomicignore already exists, skipping");
            return Ok(false);
        }

        std::fs::write(&ignore_path, template).map_err(CliError::Io)?;

        Ok(true)
    }

    /// Validate the view name.
    fn validate_view_name(&self) -> CliResult<()> {
        let name = &self.view;

        if name.is_empty() {
            return Err(CliError::InvalidArgument {
                message: "View name cannot be empty".to_string(),
            });
        }

        if name.contains('/') || name.contains('\\') {
            return Err(CliError::InvalidArgument {
                message: "View name cannot contain path separators".to_string(),
            });
        }

        if name.starts_with('.') {
            return Err(CliError::InvalidArgument {
                message: "View name cannot start with a dot".to_string(),
            });
        }

        if name.contains(char::is_whitespace) {
            return Err(CliError::InvalidArgument {
                message: "View name cannot contain whitespace".to_string(),
            });
        }

        Ok(())
    }
}

impl Default for Init {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Init {
    /// Execute the init command.
    ///
    /// This method:
    /// 1. Resolves and validates the target path
    /// 2. Validates the view name
    /// 3. Creates the directory if it doesn't exist
    /// 4. Initializes the repository
    /// 5. Creates the initial view
    /// 6. Optionally creates a .atomicignore file
    /// 7. Prints success message and next steps
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path cannot be resolved
    /// - A repository already exists at the path
    /// - The view name is invalid
    /// - The repository cannot be created
    fn run(&self) -> CliResult<()> {
        // Validate inputs
        self.validate_view_name()?;

        // Resolve the target path
        let target_path = self.resolve_path()?;

        // Create the directory if it doesn't exist
        if !target_path.exists() {
            std::fs::create_dir_all(&target_path)
                .map_err(|e| CliError::invalid_path(&target_path, Some(e)))?;
        }

        // Check if a repository already exists
        let dot_dir = target_path.join(".atomic");
        if dot_dir.exists() {
            return Err(CliError::repository_exists(&target_path));
        }

        // ── Step 1: Create .atomic/ and the pristine database ────────
        let mut repo = Repository::init(&target_path).map_err(|e| match e {
            atomic_repository::RepositoryError::AlreadyExists { .. } => {
                CliError::repository_exists(&target_path)
            }
            other => CliError::Repository(other),
        })?;

        // Create the initial view if it's different from the default
        if self.view != atomic_repository::DEFAULT_VIEW {
            repo.create_view(&self.view).map_err(CliError::Repository)?;
            repo.set_current_view(&self.view)
                .map_err(CliError::Repository)?;
        }

        print_success(&format!(
            "Initialized empty Atomic repository in {}",
            dot_dir.display()
        ));
        println!("Created view: {}", self.view);

        // ── Step 2: Create .atomicignore → add → record ─────────────
        //
        // The .atomicignore MUST contain ".atomic" so the VCS internals
        // are never tracked. Create it even without --kind (with a minimal
        // default), then add and record it as the first change.
        {
            let ignore_path = target_path.join(".atomicignore");
            if !ignore_path.exists() {
                if let Some(ref kind) = self.kind {
                    // Use the language-specific template
                    if let Some(template) = get_ignore_template(kind) {
                        std::fs::write(&ignore_path, template).map_err(CliError::Io)?;
                        println!("Created .atomicignore for {} project", kind);
                    } else {
                        // Unknown kind — write minimal default
                        std::fs::write(&ignore_path, ".atomic\n.git\n").map_err(CliError::Io)?;
                    }
                } else {
                    // No --kind specified — write minimal default
                    std::fs::write(&ignore_path, ".atomic\n.git\n").map_err(CliError::Io)?;
                }
            }

            // Add and record .atomicignore
            let _ = repo.add(
                ".atomicignore",
                atomic_repository::TrackingOptions::default(),
            );
            let header = atomic_core::change::ChangeHeader::new("Initialize repository");
            match repo.record(header, atomic_repository::RecordOptions::default()) {
                Ok(_) => {}
                Err(atomic_repository::RecordError::NothingToRecord) => {}
                Err(e) => log::warn!("Failed to record .atomicignore: {}", e),
            }
        }

        // ── Step 3: Create .vault/ → install defaults → add → record ─
        if self.vault && !self.no_vault {
            // Create vault tables + directory structure + default files
            repo.init_vault().map_err(CliError::Repository)?;

            println!("Initialized vault at {}", repo.vault_dir().display());

            // Add all vault files to tracking
            let vault_dir = repo.vault_dir();
            if vault_dir.exists() {
                add_vault_files_recursive(&repo, &vault_dir)?;
            }

            // Record vault files as their own change
            let header = atomic_core::change::ChangeHeader::new("Initialize vault");
            match repo.record(header, atomic_repository::RecordOptions::default()) {
                Ok(outcome) => {
                    println!(
                        "Recorded vault defaults ({} files)",
                        outcome.stats().files_recorded
                    );
                }
                Err(atomic_repository::RecordError::NothingToRecord) => {}
                Err(e) => log::warn!("Failed to record vault files: {}", e),
            }
        }

        // ── Status should be clean at this point ─────────────────────

        // Print next steps
        print_next_steps(&[
            ("atomic add <files>", "Add files to track"),
            ("atomic record -m \"...\"", "Record your first change"),
            ("atomic status", "See what's changed"),
        ]);

        Ok(())
    }
}

/// Recursively add all files under a directory to Atomic tracking.
fn add_vault_files_recursive(repo: &Repository, dir: &std::path::Path) -> CliResult<()> {
    let entries = std::fs::read_dir(dir).map_err(CliError::Io)?;
    for entry in entries {
        let entry = entry.map_err(CliError::Io)?;
        let path = entry.path();
        if path.is_dir() {
            add_vault_files_recursive(repo, &path)?;
        } else if path.is_file() {
            if let Ok(rel) = path.strip_prefix(repo.root()) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                let _ = repo.add(&rel_str, atomic_repository::TrackingOptions::default());
            }
        }
    }
    Ok(())
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    // -------------------------------------------------------------------------
    // Init Construction Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_init_new() {
        let init = Init::new();
        assert_eq!(init.path, PathBuf::from("."));
        assert_eq!(init.view, DEFAULT_VIEW_NAME);
        assert!(init.kind.is_none());
        assert!(init.vault);
        assert!(!init.no_vault);
    }

    #[test]
    fn test_init_default() {
        let init = Init::default();
        assert_eq!(init.path, PathBuf::from("."));
        assert_eq!(init.view, DEFAULT_VIEW_NAME);
    }

    #[test]
    fn test_init_at_path() {
        let init = Init::at_path("/some/path");
        assert_eq!(init.path, PathBuf::from("/some/path"));
        assert_eq!(init.view, DEFAULT_VIEW_NAME);
    }

    #[test]
    fn test_init_with_view() {
        let init = Init::new().with_view("main");
        assert_eq!(init.view, "main");
    }

    #[test]
    fn test_init_with_kind() {
        let init = Init::new().with_kind("rust");
        assert_eq!(init.kind, Some("rust".to_string()));
    }

    #[test]
    fn test_init_builder_chain() {
        let init = Init::at_path("/project")
            .with_view("main")
            .with_kind("python");

        assert_eq!(init.path, PathBuf::from("/project"));
        assert_eq!(init.view, "main");
        assert_eq!(init.kind, Some("python".to_string()));
    }

    #[test]
    fn test_init_with_vault_flag() {
        let init = Init::new().with_vault();
        assert!(init.vault);
        assert!(!init.no_vault);
    }

    #[test]
    fn test_init_vault_is_default() {
        let init = Init::new();
        assert!(init.vault);
        assert!(!init.no_vault);
    }

    #[test]
    fn test_init_without_vault() {
        let init = Init::new().without_vault();
        assert!(!init.vault);
        assert!(init.no_vault);
    }

    // -------------------------------------------------------------------------
    // View Name Validation Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_validate_view_name_valid() {
        let init = Init::new().with_view("main");
        assert!(init.validate_view_name().is_ok());

        let init = Init::new().with_view("feature-branch");
        assert!(init.validate_view_name().is_ok());

        let init = Init::new().with_view("v1.0.0");
        assert!(init.validate_view_name().is_ok());
    }

    #[test]
    fn test_validate_view_name_empty() {
        let init = Init::new().with_view("");
        let result = init.validate_view_name();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument { .. }));
    }

    #[test]
    fn test_validate_view_name_with_slash() {
        let init = Init::new().with_view("feature/branch");
        let result = init.validate_view_name();
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_view_name_with_backslash() {
        let init = Init::new().with_view("feature\\branch");
        let result = init.validate_view_name();
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_view_name_starts_with_dot() {
        let init = Init::new().with_view(".hidden");
        let result = init.validate_view_name();
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_view_name_with_whitespace() {
        let init = Init::new().with_view("my view");
        let result = init.validate_view_name();
        assert!(result.is_err());

        let init = Init::new().with_view("my\tview");
        let result = init.validate_view_name();
        assert!(result.is_err());
    }

    // -------------------------------------------------------------------------
    // Ignore Template Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_get_ignore_template_rust() {
        let template = get_ignore_template("rust");
        assert!(template.is_some());
        let content = template.unwrap();
        assert!(content.contains("target/"));
        assert!(content.contains("Cargo.lock"));
    }

    #[test]
    fn test_get_ignore_template_python() {
        let template = get_ignore_template("python");
        assert!(template.is_some());
        let content = template.unwrap();
        assert!(content.contains("__pycache__/"));
        assert!(content.contains(".venv/"));
    }

    #[test]
    fn test_get_ignore_template_node() {
        let template = get_ignore_template("node");
        assert!(template.is_some());
        let content = template.unwrap();
        assert!(content.contains("node_modules/"));
    }

    #[test]
    fn test_get_ignore_template_javascript() {
        let template = get_ignore_template("javascript");
        assert!(template.is_some());
    }

    #[test]
    fn test_get_ignore_template_typescript() {
        let template = get_ignore_template("typescript");
        assert!(template.is_some());
    }

    #[test]
    fn test_get_ignore_template_go() {
        let template = get_ignore_template("go");
        assert!(template.is_some());
        let content = template.unwrap();
        assert!(content.contains("bin/"));
    }

    #[test]
    fn test_get_ignore_template_java() {
        let template = get_ignore_template("java");
        assert!(template.is_some());
        let content = template.unwrap();
        assert!(content.contains("*.class"));
    }

    #[test]
    fn test_get_ignore_template_c() {
        let template = get_ignore_template("c");
        assert!(template.is_some());
        let content = template.unwrap();
        assert!(content.contains("*.o"));
    }

    #[test]
    fn test_get_ignore_template_cpp() {
        let template = get_ignore_template("cpp");
        assert!(template.is_some());
    }

    #[test]
    fn test_get_ignore_template_unknown() {
        let template = get_ignore_template("unknown");
        assert!(template.is_none());
    }

    #[test]
    fn test_get_ignore_template_case_insensitive() {
        assert!(get_ignore_template("RUST").is_some());
        assert!(get_ignore_template("Python").is_some());
        assert!(get_ignore_template("NODE").is_some());
    }

    // -------------------------------------------------------------------------
    // Supported Kinds Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_supported_kinds() {
        let kinds = supported_kinds();
        assert!(kinds.contains(&"rust"));
        assert!(kinds.contains(&"python"));
        assert!(kinds.contains(&"node"));
        assert!(kinds.contains(&"go"));
    }

    // -------------------------------------------------------------------------
    // Integration Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_init_creates_repository() {
        let temp = TempDir::new().unwrap();
        let init = Init::at_path(temp.path());

        let result = init.run();
        if let Err(ref e) = result {
            eprintln!("Init failed: {:?}", e);
        }
        assert!(result.is_ok(), "Init should succeed: {:?}", result.err());

        // Verify .atomic directory was created
        assert!(temp.path().join(".atomic").is_dir());
    }

    #[test]
    fn test_init_creates_directory_if_missing() {
        let temp = TempDir::new().unwrap();
        let new_dir = temp.path().join("new_project");
        let init = Init::at_path(&new_dir);

        let result = init.run();
        if let Err(ref e) = result {
            eprintln!("Init failed: {:?}", e);
        }
        assert!(result.is_ok(), "Init should succeed: {:?}", result.err());

        // Verify directory and .atomic were created
        assert!(new_dir.is_dir());
        assert!(new_dir.join(".atomic").is_dir());
    }

    #[test]
    fn test_init_fails_if_repository_exists() {
        let temp = TempDir::new().unwrap();

        // First init should succeed
        let init1 = Init::at_path(temp.path());
        let result1 = init1.run();
        if let Err(ref e) = result1 {
            eprintln!("First init failed: {:?}", e);
        }
        assert!(
            result1.is_ok(),
            "First init should succeed: {:?}",
            result1.err()
        );

        // Second init should fail
        let init2 = Init::at_path(temp.path());
        let result = init2.run();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            CliError::RepositoryExists { .. }
        ));
    }

    #[test]
    fn test_init_with_custom_view() {
        let temp = TempDir::new().unwrap();
        let init = Init::at_path(temp.path()).with_view("main");

        let result = init.run();
        assert!(result.is_ok());

        // Verify view was created (by checking current_stack file)
        let current_stack_path = temp.path().join(".atomic").join("current_stack");
        if current_stack_path.exists() {
            let content = std::fs::read_to_string(current_stack_path).unwrap();
            assert!(content.contains("main"));
        }
    }

    #[test]
    fn test_init_with_kind_creates_ignore_file() {
        let temp = TempDir::new().unwrap();
        let init = Init::at_path(temp.path()).with_kind("rust");

        let result = init.run();
        if let Err(ref e) = result {
            eprintln!("Init with kind failed: {:?}", e);
        }
        assert!(result.is_ok(), "Init should succeed: {:?}", result.err());

        // Verify .atomicignore was created
        let ignore_path = temp.path().join(".atomicignore");
        assert!(ignore_path.exists());

        let content = std::fs::read_to_string(ignore_path).unwrap();
        assert!(content.contains("target/"));
    }

    #[test]
    fn test_init_does_not_overwrite_existing_ignore() {
        let temp = TempDir::new().unwrap();

        // Create an existing .atomicignore
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, "# Custom ignore\n").unwrap();

        let init = Init::at_path(temp.path()).with_kind("rust");
        let result = init.run();
        if let Err(ref e) = result {
            eprintln!("Init with existing ignore failed: {:?}", e);
        }
        assert!(result.is_ok(), "Init should succeed: {:?}", result.err());

        // Verify original content is preserved
        let content = std::fs::read_to_string(ignore_path).unwrap();
        assert!(content.contains("# Custom ignore"));
        assert!(!content.contains("target/"));
    }

    #[test]
    fn test_init_with_invalid_view_name_fails() {
        let temp = TempDir::new().unwrap();
        let init = Init::at_path(temp.path()).with_view("");

        let result = init.run();
        assert!(result.is_err());

        // Verify no repository was created
        assert!(!temp.path().join(".atomic").exists());
    }

    // -------------------------------------------------------------------------
    // Path Resolution Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_resolve_path_absolute() {
        let init = Init::at_path("/absolute/path");
        // This will fail the exists check but should return the path
        let resolved = init.resolve_path();
        assert!(resolved.is_ok());
        assert_eq!(resolved.unwrap(), PathBuf::from("/absolute/path"));
    }

    #[test]
    #[serial]
    fn test_resolve_path_relative() {
        let temp = TempDir::new().unwrap();
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp.path()).unwrap();

        let init = Init::at_path("subdir");
        let resolved = init.resolve_path();

        std::env::set_current_dir(original_dir).unwrap();

        assert!(resolved.is_ok());
        assert!(resolved.unwrap().ends_with("subdir"));
    }

    #[test]
    #[serial]
    fn test_resolve_path_dot() {
        let temp = TempDir::new().unwrap();
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp.path()).unwrap();

        let init = Init::new(); // Uses "." by default
        let resolved = init.resolve_path();

        std::env::set_current_dir(original_dir).unwrap();

        assert!(resolved.is_ok());
    }

    // -------------------------------------------------------------------------
    // Edge Case Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_init_unicode_path() {
        let temp = TempDir::new().unwrap();
        let unicode_path = temp.path().join("项目");
        std::fs::create_dir(&unicode_path).unwrap();

        let init = Init::at_path(&unicode_path);
        let result = init.run();
        if let Err(ref e) = result {
            eprintln!("Init unicode path failed: {:?}", e);
        }
        assert!(result.is_ok(), "Init should succeed: {:?}", result.err());
    }

    #[test]
    fn test_init_path_with_spaces() {
        let temp = TempDir::new().unwrap();
        let path_with_spaces = temp.path().join("my project");
        std::fs::create_dir(&path_with_spaces).unwrap();

        let init = Init::at_path(&path_with_spaces);
        let result = init.run();
        if let Err(ref e) = result {
            eprintln!("Init path with spaces failed: {:?}", e);
        }
        assert!(result.is_ok(), "Init should succeed: {:?}", result.err());
    }

    #[test]
    fn test_default_view_name_constant() {
        assert_eq!(DEFAULT_VIEW_NAME, "dev");
    }

    #[test]
    fn test_init_creates_vault_dir_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let init = Init::at_path(dir.path());
        init.run().unwrap();

        // Vault is created by default
        let vault_dir = dir.path().join(".vault");
        assert!(vault_dir.exists());
        assert!(vault_dir.join("goals").exists());
        assert!(vault_dir.join("memory").exists());
        assert!(vault_dir.join("skills").exists());
        assert!(vault_dir.join("intents").exists());
        assert!(vault_dir.join("scratch").exists());
    }
}
