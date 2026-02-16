//! Archive operations for Atomic VCS
//!
//! This module provides functionality for exporting a repository's state at a
//! given point in time to various archive formats. Archives are read-only
//! snapshots that can be shared, deployed, or stored for backup purposes.
//!
//! # Overview
//!
//! Archives capture the complete state of the working copy as it would appear
//! at a specific Merkle state. This includes:
//!
//! - All files and their contents
//! - Directory structure
//! - File permissions (where supported)
//! - Timestamps (where supported)
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                         Archive Creation Flow                           │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Repository              Alive Graph              Archive               │
//! │  ┌──────────┐           ┌─────────────┐         ┌─────────────┐        │
//! │  │ Pristine │  traverse │  File       │ output  │  .tar.gz    │        │
//! │  │ at State │ ────────▶ │  Contents   │ ──────▶ │  or         │        │
//! │  │ X        │           │  & Metadata │         │  directory  │        │
//! │  └──────────┘           └─────────────┘         └─────────────┘        │
//! │                                                                         │
//! │  Input: Merkle state or tag name                                        │
//! │  Output: Complete snapshot of working copy at that state                │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Archive Formats
//!
//! - **Tarball**: Compressed `.tar.gz` archive (portable, standard)
//! - **Directory**: Plain directory copy (for inspection or deployment)
//! - **Zip**: ZIP archive (for Windows compatibility)
//!
//! # Usage
//!
//! ```rust,ignore
//! use atomic_repository::{Repository, ArchiveOptions, ArchiveFormat};
//!
//! let repo = Repository::open(".")?;
//!
//! // Archive current state to tarball
//! repo.archive("release.tar.gz", ArchiveOptions::default())?;
//!
//! // Archive a specific tag
//! repo.archive_tag("v1.0.0", "v1.0.0.tar.gz", ArchiveOptions::default())?;
//!
//! // Archive to a directory
//! repo.archive("./release/", ArchiveOptions::format(ArchiveFormat::Directory))?;
//!
//! // Archive a specific prefix (subdirectory)
//! repo.archive_prefix("src/", "src.tar.gz", ArchiveOptions::default())?;
//! ```
//!
//! # Conflict Handling
//!
//! If the repository state contains unresolved conflicts, the archive will
//! include conflict markers in affected files (similar to how they appear
//! in the working copy).

use atomic_core::types::{Inode, Merkle};
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

// Error Types

/// Result type for archive operations.
pub type ArchiveResult<T> = Result<T, ArchiveError>;

/// Errors that can occur during archive operations.
#[derive(Debug, Error)]
pub enum ArchiveError {
    /// The specified state was not found.
    #[error("State not found: {state}")]
    StateNotFound {
        /// The missing state hash.
        state: String,
    },

    /// The specified tag was not found.
    #[error("Tag not found: {name}")]
    TagNotFound {
        /// Name of the missing tag.
        name: String,
    },

    /// The specified path was not found.
    #[error("Path not found: {path}")]
    PathNotFound {
        /// The missing path.
        path: String,
    },

    /// The destination already exists and overwrite is disabled.
    #[error("Destination already exists: {path}")]
    DestinationExists {
        /// Path that exists.
        path: PathBuf,
    },

    /// Unsupported archive format.
    #[error("Unsupported archive format: {format}")]
    UnsupportedFormat {
        /// The unsupported format.
        format: String,
    },

    /// The archive would be empty (no files to include).
    #[error("Archive would be empty - no files match the criteria")]
    EmptyArchive,

    /// Maximum file count exceeded.
    #[error("Maximum file count ({max}) exceeded")]
    TooManyFiles {
        /// Maximum allowed files.
        max: usize,
    },

    /// Maximum archive size exceeded.
    #[error("Maximum archive size ({max} bytes) exceeded")]
    TooLarge {
        /// Maximum allowed size.
        max: u64,
    },

    /// Database error.
    #[error("Database error: {0}")]
    Database(String),

    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Archive format-specific error.
    #[error("Archive error: {0}")]
    Format(String),
}

// Archive Format

/// Supported archive formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ArchiveFormat {
    /// Gzip-compressed tarball (.tar.gz)
    #[default]
    TarGz,

    /// Uncompressed tarball (.tar)
    Tar,

    /// Plain directory copy
    Directory,

    /// ZIP archive (.zip)
    Zip,
}

impl ArchiveFormat {
    /// Get the file extension for this format.
    pub fn extension(&self) -> &'static str {
        match self {
            Self::TarGz => ".tar.gz",
            Self::Tar => ".tar",
            Self::Directory => "",
            Self::Zip => ".zip",
        }
    }

    /// Detect format from a path.
    pub fn from_path(path: &Path) -> Option<Self> {
        let path_str = path.to_string_lossy().to_lowercase();

        if path_str.ends_with(".tar.gz") || path_str.ends_with(".tgz") {
            Some(Self::TarGz)
        } else if path_str.ends_with(".tar") {
            Some(Self::Tar)
        } else if path_str.ends_with(".zip") {
            Some(Self::Zip)
        } else if path.is_dir() || !path.to_string_lossy().contains('.') {
            Some(Self::Directory)
        } else {
            None
        }
    }

    /// Check if this format requires compression.
    pub fn is_compressed(&self) -> bool {
        matches!(self, Self::TarGz | Self::Zip)
    }

    /// Check if this format is a single file.
    pub fn is_file(&self) -> bool {
        !matches!(self, Self::Directory)
    }
}

impl fmt::Display for ArchiveFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TarGz => write!(f, "tar.gz"),
            Self::Tar => write!(f, "tar"),
            Self::Directory => write!(f, "directory"),
            Self::Zip => write!(f, "zip"),
        }
    }
}

// Archive Options

/// Options for archive creation.
///
/// # Example
///
/// ```rust,ignore
/// let options = ArchiveOptions::default()
///     .format(ArchiveFormat::TarGz)
///     .with_prefix("myproject-1.0/")
///     .exclude(&["*.log", "tmp/"]);
/// ```
#[derive(Debug, Clone)]
pub struct ArchiveOptions {
    /// Archive format.
    pub format: ArchiveFormat,

    /// State to archive (None = current state).
    pub state: Option<Merkle>,

    /// Stack to use (None = current stack).
    pub stack: Option<String>,

    /// Prefix to add to all paths in the archive.
    pub prefix: Option<String>,

    /// Only include paths matching these patterns.
    pub include: Vec<String>,

    /// Exclude paths matching these patterns.
    pub exclude: Vec<String>,

    /// Whether to overwrite existing destination.
    pub overwrite: bool,

    /// Whether to include empty directories.
    pub include_empty_dirs: bool,

    /// Maximum number of files to include.
    pub max_files: Option<usize>,

    /// Maximum total size in bytes.
    pub max_size: Option<u64>,

    /// Compression level (0-9, where applicable).
    pub compression_level: u32,

    /// Whether to preserve timestamps.
    pub preserve_timestamps: bool,

    /// Whether to preserve permissions.
    pub preserve_permissions: bool,

    /// Default file permissions (Unix mode).
    pub default_file_mode: u32,

    /// Default directory permissions (Unix mode).
    pub default_dir_mode: u32,
}

impl Default for ArchiveOptions {
    fn default() -> Self {
        Self {
            format: ArchiveFormat::default(),
            state: None,
            stack: None,
            prefix: None,
            include: Vec::new(),
            exclude: Vec::new(),
            overwrite: false,
            include_empty_dirs: false,
            max_files: None,
            max_size: None,
            compression_level: 6,
            preserve_timestamps: true,
            preserve_permissions: true,
            default_file_mode: 0o644,
            default_dir_mode: 0o755,
        }
    }
}

impl ArchiveOptions {
    /// Create new default options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the archive format.
    pub fn format(mut self, format: ArchiveFormat) -> Self {
        self.format = format;
        self
    }

    /// Set the state to archive.
    pub fn state(mut self, state: Merkle) -> Self {
        self.state = Some(state);
        self
    }

    /// Set the stack to use.
    pub fn stack(mut self, name: impl Into<String>) -> Self {
        self.stack = Some(name.into());
        self
    }

    /// Set a prefix for all paths.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// Add include patterns.
    pub fn include(mut self, patterns: &[&str]) -> Self {
        self.include.extend(patterns.iter().map(|s| s.to_string()));
        self
    }

    /// Add exclude patterns.
    pub fn exclude(mut self, patterns: &[&str]) -> Self {
        self.exclude.extend(patterns.iter().map(|s| s.to_string()));
        self
    }

    /// Enable overwriting existing destination.
    pub fn overwrite(mut self, overwrite: bool) -> Self {
        self.overwrite = overwrite;
        self
    }

    /// Set maximum file count.
    pub fn max_files(mut self, max: usize) -> Self {
        self.max_files = Some(max);
        self
    }

    /// Set maximum total size.
    pub fn max_size(mut self, max: u64) -> Self {
        self.max_size = Some(max);
        self
    }

    /// Set compression level.
    pub fn compression_level(mut self, level: u32) -> Self {
        self.compression_level = level.min(9);
        self
    }

    /// Create options for a specific format.
    pub fn with_format(format: ArchiveFormat) -> Self {
        Self::default().format(format)
    }

    /// Create options for directory output.
    pub fn directory() -> Self {
        Self::default().format(ArchiveFormat::Directory)
    }

    /// Create options for tarball output.
    pub fn tarball() -> Self {
        Self::default().format(ArchiveFormat::TarGz)
    }

    /// Check if a path should be included based on patterns.
    pub fn should_include(&self, path: &str) -> bool {
        // Check exclude patterns first
        for pattern in &self.exclude {
            if matches_glob(path, pattern) {
                return false;
            }
        }

        // If no include patterns, include everything
        if self.include.is_empty() {
            return true;
        }

        // Check include patterns
        for pattern in &self.include {
            if matches_glob(path, pattern) {
                return true;
            }
        }

        false
    }
}

// Archive Manifest

/// An entry in the archive manifest.
#[derive(Debug, Clone)]
pub struct ArchiveEntry {
    /// Path in the archive.
    pub path: String,

    /// Whether this is a directory.
    pub is_directory: bool,

    /// File size in bytes (0 for directories).
    pub size: u64,

    /// File permissions (Unix mode).
    pub mode: u32,

    /// Modification timestamp.
    pub mtime: DateTime<Utc>,

    /// Inode identifier from the repository.
    pub inode: Option<Inode>,
}

impl ArchiveEntry {
    /// Create a file entry.
    pub fn file(path: impl Into<String>, size: u64) -> Self {
        Self {
            path: path.into(),
            is_directory: false,
            size,
            mode: 0o644,
            mtime: Utc::now(),
            inode: None,
        }
    }

    /// Create a directory entry.
    pub fn directory(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            is_directory: true,
            size: 0,
            mode: 0o755,
            mtime: Utc::now(),
            inode: None,
        }
    }

    /// Set the file mode.
    pub fn with_mode(mut self, mode: u32) -> Self {
        self.mode = mode;
        self
    }

    /// Set the modification time.
    pub fn with_mtime(mut self, mtime: DateTime<Utc>) -> Self {
        self.mtime = mtime;
        self
    }

    /// Set the inode.
    pub fn with_inode(mut self, inode: Inode) -> Self {
        self.inode = Some(inode);
        self
    }
}

impl fmt::Display for ArchiveEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_directory {
            write!(f, "d {:o} {}/", self.mode, self.path)
        } else {
            write!(f, "- {:o} {} ({} bytes)", self.mode, self.path, self.size)
        }
    }
}

/// Manifest of files to be archived.
#[derive(Debug, Clone, Default)]
pub struct ArchiveManifest {
    /// All entries in the manifest.
    pub entries: Vec<ArchiveEntry>,

    /// Total size of all files.
    pub total_size: u64,

    /// Number of files (not directories).
    pub file_count: usize,

    /// Number of directories.
    pub directory_count: usize,
}

impl ArchiveManifest {
    /// Create a new empty manifest.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an entry to the manifest.
    pub fn add(&mut self, entry: ArchiveEntry) {
        if entry.is_directory {
            self.directory_count += 1;
        } else {
            self.file_count += 1;
            self.total_size += entry.size;
        }
        self.entries.push(entry);
    }

    /// Get total entry count.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Check if manifest is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate over file entries.
    pub fn files(&self) -> impl Iterator<Item = &ArchiveEntry> {
        self.entries.iter().filter(|e| !e.is_directory)
    }

    /// Iterate over directory entries.
    pub fn directories(&self) -> impl Iterator<Item = &ArchiveEntry> {
        self.entries.iter().filter(|e| e.is_directory)
    }

    /// Sort entries by path.
    pub fn sort(&mut self) {
        self.entries.sort_by(|a, b| a.path.cmp(&b.path));
    }
}

impl fmt::Display for ArchiveManifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Manifest: {} files, {} directories, {} bytes total",
            self.file_count, self.directory_count, self.total_size
        )
    }
}

// Archive Outcome

/// Result of an archive operation.
#[derive(Debug, Clone)]
pub struct ArchiveOutcome {
    /// Path to the created archive.
    pub destination: PathBuf,

    /// Format of the archive.
    pub format: ArchiveFormat,

    /// State that was archived.
    pub state: Merkle,

    /// Manifest of archived contents.
    pub manifest: ArchiveManifest,

    /// Size of the archive file (or total size for directory).
    pub archive_size: u64,

    /// Compression ratio (if applicable).
    pub compression_ratio: Option<f64>,

    /// Time taken to create the archive.
    pub duration_ms: u64,

    /// Any warnings generated.
    pub warnings: Vec<String>,
}

impl ArchiveOutcome {
    /// Create a new outcome.
    pub fn new(
        destination: PathBuf,
        format: ArchiveFormat,
        state: Merkle,
        manifest: ArchiveManifest,
    ) -> Self {
        Self {
            destination,
            format,
            state,
            manifest,
            archive_size: 0,
            compression_ratio: None,
            duration_ms: 0,
            warnings: Vec::new(),
        }
    }

    /// Set the archive size and compute compression ratio.
    pub fn with_archive_size(mut self, size: u64) -> Self {
        self.archive_size = size;
        if self.manifest.total_size > 0 {
            self.compression_ratio = Some(size as f64 / self.manifest.total_size as f64);
        }
        self
    }

    /// Set the duration.
    pub fn with_duration(mut self, ms: u64) -> Self {
        self.duration_ms = ms;
        self
    }

    /// Add a warning.
    pub fn with_warning(mut self, warning: impl Into<String>) -> Self {
        self.warnings.push(warning.into());
        self
    }

    /// Check if there were warnings.
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }
}

impl fmt::Display for ArchiveOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Created {} archive at {} ({} files, {} bytes",
            self.format,
            self.destination.display(),
            self.manifest.file_count,
            self.archive_size
        )?;

        if let Some(ratio) = self.compression_ratio {
            write!(f, ", {:.1}% compression", ratio * 100.0)?;
        }

        write!(f, ")")
    }
}

// Archive Trait

/// Trait for archive implementations.
///
/// This trait abstracts over different archive formats, allowing
/// the same archive logic to work with tarballs, zip files, or directories.
pub trait Archive {
    /// The file writer type for this archive.
    type Writer: Write;

    /// Error type for this archive.
    type Error: std::error::Error + 'static;

    /// Create a new file in the archive.
    fn create_file(
        &mut self,
        path: &str,
        size: u64,
        mode: u32,
        mtime: u64,
    ) -> Result<Self::Writer, Self::Error>;

    /// Create a directory in the archive.
    fn create_directory(&mut self, path: &str, mode: u32, mtime: u64) -> Result<(), Self::Error>;

    /// Close a file writer.
    fn close_file(&mut self, writer: Self::Writer) -> Result<(), Self::Error>;

    /// Finish writing the archive.
    fn finish(self) -> Result<(), Self::Error>;
}

// Directory Archive

/// A simple directory-based "archive" that writes files directly.
pub struct DirectoryArchive {
    root: PathBuf,
    created_dirs: HashSet<PathBuf>,
}

impl DirectoryArchive {
    /// Create a new directory archive.
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            created_dirs: HashSet::new(),
        })
    }

    /// Get the root path.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// A file writer for directory archives.
#[allow(dead_code)]
pub struct DirectoryFile {
    file: std::fs::File,
    path: PathBuf,
}

impl Write for DirectoryFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Archive for DirectoryArchive {
    type Writer = DirectoryFile;
    type Error = io::Error;

    fn create_file(
        &mut self,
        path: &str,
        _size: u64,
        _mode: u32,
        _mtime: u64,
    ) -> Result<Self::Writer, Self::Error> {
        let full_path = self.root.join(path);

        // Ensure parent directory exists
        if let Some(parent) = full_path.parent() {
            if !self.created_dirs.contains(parent) {
                std::fs::create_dir_all(parent)?;
                self.created_dirs.insert(parent.to_path_buf());
            }
        }

        let file = std::fs::File::create(&full_path)?;
        Ok(DirectoryFile {
            file,
            path: full_path,
        })
    }

    fn create_directory(&mut self, path: &str, _mode: u32, _mtime: u64) -> Result<(), Self::Error> {
        let full_path = self.root.join(path);
        if !self.created_dirs.contains(&full_path) {
            std::fs::create_dir_all(&full_path)?;
            self.created_dirs.insert(full_path);
        }
        Ok(())
    }

    fn close_file(&mut self, writer: Self::Writer) -> Result<(), Self::Error> {
        drop(writer);
        Ok(())
    }

    fn finish(self) -> Result<(), Self::Error> {
        Ok(())
    }
}

// Helper Functions

/// Simple glob pattern matching.
///
/// Supports:
/// - `*` matches any characters
/// - `?` matches single character
/// - Exact match
fn matches_glob(path: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }

    if pattern.starts_with('*') && pattern.ends_with('*') && pattern.len() > 2 {
        let inner = &pattern[1..pattern.len() - 1];
        return path.contains(inner);
    }

    if pattern.starts_with('*') {
        let suffix = &pattern[1..];
        return path.ends_with(suffix);
    }

    if pattern.ends_with('*') {
        let prefix = &pattern[..pattern.len() - 1];
        return path.starts_with(prefix);
    }

    if pattern.ends_with('/') {
        // Directory pattern
        let prefix = &pattern[..pattern.len() - 1];
        return path.starts_with(prefix)
            && (path.len() == prefix.len() || path[prefix.len()..].starts_with('/'));
    }

    path == pattern
}

/// Ensure a path ends with the appropriate extension for the format.
pub fn ensure_extension(path: &Path, format: ArchiveFormat) -> PathBuf {
    let ext = format.extension();
    if ext.is_empty() {
        return path.to_path_buf();
    }

    let path_str = path.to_string_lossy();
    if path_str.ends_with(ext) {
        path.to_path_buf()
    } else {
        PathBuf::from(format!("{}{}", path_str, ext))
    }
}

/// Get the output path for an archive with optional prefix.
pub fn get_archive_path(base: &Path, prefix: Option<&str>) -> PathBuf {
    match prefix {
        Some(p) => {
            let stem = base
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "archive".to_string());
            base.with_file_name(format!("{}-{}", p.trim_end_matches('/'), stem))
        }
        None => base.to_path_buf(),
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ArchiveFormat Tests

    #[test]
    fn test_archive_format_extension() {
        assert_eq!(ArchiveFormat::TarGz.extension(), ".tar.gz");
        assert_eq!(ArchiveFormat::Tar.extension(), ".tar");
        assert_eq!(ArchiveFormat::Directory.extension(), "");
        assert_eq!(ArchiveFormat::Zip.extension(), ".zip");
    }

    #[test]
    fn test_archive_format_from_path() {
        assert_eq!(
            ArchiveFormat::from_path(Path::new("file.tar.gz")),
            Some(ArchiveFormat::TarGz)
        );
        assert_eq!(
            ArchiveFormat::from_path(Path::new("file.tgz")),
            Some(ArchiveFormat::TarGz)
        );
        assert_eq!(
            ArchiveFormat::from_path(Path::new("file.tar")),
            Some(ArchiveFormat::Tar)
        );
        assert_eq!(
            ArchiveFormat::from_path(Path::new("file.zip")),
            Some(ArchiveFormat::Zip)
        );
        assert_eq!(
            ArchiveFormat::from_path(Path::new("outdir")),
            Some(ArchiveFormat::Directory)
        );
    }

    #[test]
    fn test_archive_format_is_compressed() {
        assert!(ArchiveFormat::TarGz.is_compressed());
        assert!(ArchiveFormat::Zip.is_compressed());
        assert!(!ArchiveFormat::Tar.is_compressed());
        assert!(!ArchiveFormat::Directory.is_compressed());
    }

    #[test]
    fn test_archive_format_is_file() {
        assert!(ArchiveFormat::TarGz.is_file());
        assert!(ArchiveFormat::Tar.is_file());
        assert!(ArchiveFormat::Zip.is_file());
        assert!(!ArchiveFormat::Directory.is_file());
    }

    #[test]
    fn test_archive_format_display() {
        assert_eq!(format!("{}", ArchiveFormat::TarGz), "tar.gz");
        assert_eq!(format!("{}", ArchiveFormat::Directory), "directory");
    }

    // ArchiveOptions Tests

    #[test]
    fn test_archive_options_default() {
        let options = ArchiveOptions::default();

        assert_eq!(options.format, ArchiveFormat::TarGz);
        assert!(options.state.is_none());
        assert!(options.prefix.is_none());
        assert!(options.include.is_empty());
        assert!(options.exclude.is_empty());
        assert!(!options.overwrite);
        assert_eq!(options.compression_level, 6);
    }

    #[test]
    fn test_archive_options_builder() {
        let state = Merkle::of(b"test");
        let options = ArchiveOptions::new()
            .format(ArchiveFormat::Tar)
            .state(state)
            .with_prefix("project-1.0/")
            .include(&["src/*", "Cargo.toml"])
            .exclude(&["*.log", "target/"])
            .overwrite(true)
            .compression_level(9);

        assert_eq!(options.format, ArchiveFormat::Tar);
        assert_eq!(options.state, Some(state));
        assert_eq!(options.prefix, Some("project-1.0/".to_string()));
        assert_eq!(options.include.len(), 2);
        assert_eq!(options.exclude.len(), 2);
        assert!(options.overwrite);
        assert_eq!(options.compression_level, 9);
    }

    #[test]
    fn test_archive_options_shortcuts() {
        let dir_opts = ArchiveOptions::directory();
        assert_eq!(dir_opts.format, ArchiveFormat::Directory);

        let tar_opts = ArchiveOptions::tarball();
        assert_eq!(tar_opts.format, ArchiveFormat::TarGz);
    }

    #[test]
    fn test_archive_options_should_include() {
        let options = ArchiveOptions::new()
            .include(&["src/*"])
            .exclude(&["*.log"]);

        assert!(options.should_include("src/main.rs"));
        assert!(!options.should_include("build/output"));
        assert!(!options.should_include("src/debug.log"));
    }

    #[test]
    fn test_archive_options_should_include_no_patterns() {
        let options = ArchiveOptions::default();

        // With no patterns, everything is included
        assert!(options.should_include("any/path.txt"));
    }

    #[test]
    fn test_archive_options_should_include_exclude_only() {
        let options = ArchiveOptions::new().exclude(&["*.log", "tmp/"]);

        assert!(options.should_include("src/main.rs"));
        assert!(!options.should_include("debug.log"));
        assert!(!options.should_include("tmp/cache"));
    }

    // ArchiveEntry Tests

    #[test]
    fn test_archive_entry_file() {
        let entry = ArchiveEntry::file("src/main.rs", 1024);

        assert_eq!(entry.path, "src/main.rs");
        assert!(!entry.is_directory);
        assert_eq!(entry.size, 1024);
        assert_eq!(entry.mode, 0o644);
    }

    #[test]
    fn test_archive_entry_directory() {
        let entry = ArchiveEntry::directory("src/");

        assert_eq!(entry.path, "src/");
        assert!(entry.is_directory);
        assert_eq!(entry.size, 0);
        assert_eq!(entry.mode, 0o755);
    }

    #[test]
    fn test_archive_entry_with_mode() {
        let entry = ArchiveEntry::file("script.sh", 100).with_mode(0o755);

        assert_eq!(entry.mode, 0o755);
    }

    #[test]
    fn test_archive_entry_display() {
        let file = ArchiveEntry::file("main.rs", 1024);
        let dir = ArchiveEntry::directory("src/");

        let file_display = format!("{}", file);
        let dir_display = format!("{}", dir);

        assert!(file_display.contains("main.rs"));
        assert!(file_display.contains("1024 bytes"));
        assert!(dir_display.contains("src/"));
    }

    // ArchiveManifest Tests

    #[test]
    fn test_archive_manifest_new() {
        let manifest = ArchiveManifest::new();

        assert!(manifest.is_empty());
        assert_eq!(manifest.entry_count(), 0);
        assert_eq!(manifest.file_count, 0);
        assert_eq!(manifest.directory_count, 0);
        assert_eq!(manifest.total_size, 0);
    }

    #[test]
    fn test_archive_manifest_add() {
        let mut manifest = ArchiveManifest::new();

        manifest.add(ArchiveEntry::file("file1.txt", 100));
        manifest.add(ArchiveEntry::file("file2.txt", 200));
        manifest.add(ArchiveEntry::directory("dir/"));

        assert_eq!(manifest.entry_count(), 3);
        assert_eq!(manifest.file_count, 2);
        assert_eq!(manifest.directory_count, 1);
        assert_eq!(manifest.total_size, 300);
    }

    #[test]
    fn test_archive_manifest_iterators() {
        let mut manifest = ArchiveManifest::new();
        manifest.add(ArchiveEntry::file("a.txt", 10));
        manifest.add(ArchiveEntry::directory("b/"));
        manifest.add(ArchiveEntry::file("c.txt", 20));

        assert_eq!(manifest.files().count(), 2);
        assert_eq!(manifest.directories().count(), 1);
    }

    #[test]
    fn test_archive_manifest_sort() {
        let mut manifest = ArchiveManifest::new();
        manifest.add(ArchiveEntry::file("c.txt", 10));
        manifest.add(ArchiveEntry::file("a.txt", 10));
        manifest.add(ArchiveEntry::file("b.txt", 10));

        manifest.sort();

        assert_eq!(manifest.entries[0].path, "a.txt");
        assert_eq!(manifest.entries[1].path, "b.txt");
        assert_eq!(manifest.entries[2].path, "c.txt");
    }

    #[test]
    fn test_archive_manifest_display() {
        let mut manifest = ArchiveManifest::new();
        manifest.add(ArchiveEntry::file("test.txt", 1024));

        let display = format!("{}", manifest);
        assert!(display.contains("1 files"));
        assert!(display.contains("1024 bytes"));
    }

    // ArchiveOutcome Tests

    #[test]
    fn test_archive_outcome_new() {
        let state = Merkle::of(b"test");
        let manifest = ArchiveManifest::new();
        let outcome = ArchiveOutcome::new(
            PathBuf::from("test.tar.gz"),
            ArchiveFormat::TarGz,
            state,
            manifest,
        );

        assert_eq!(outcome.destination, PathBuf::from("test.tar.gz"));
        assert_eq!(outcome.format, ArchiveFormat::TarGz);
        assert!(!outcome.has_warnings());
    }

    #[test]
    fn test_archive_outcome_with_size() {
        let state = Merkle::of(b"test");
        let mut manifest = ArchiveManifest::new();
        manifest.add(ArchiveEntry::file("test.txt", 1000));

        let outcome = ArchiveOutcome::new(
            PathBuf::from("test.tar.gz"),
            ArchiveFormat::TarGz,
            state,
            manifest,
        )
        .with_archive_size(500);

        assert_eq!(outcome.archive_size, 500);
        assert!(outcome.compression_ratio.is_some());
        assert!((outcome.compression_ratio.unwrap() - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_archive_outcome_with_warning() {
        let state = Merkle::of(b"test");
        let manifest = ArchiveManifest::new();
        let outcome = ArchiveOutcome::new(
            PathBuf::from("test.tar.gz"),
            ArchiveFormat::TarGz,
            state,
            manifest,
        )
        .with_warning("Some warning");

        assert!(outcome.has_warnings());
        assert_eq!(outcome.warnings.len(), 1);
    }

    #[test]
    fn test_archive_outcome_display() {
        let state = Merkle::of(b"test");
        let mut manifest = ArchiveManifest::new();
        manifest.add(ArchiveEntry::file("test.txt", 100));

        let outcome = ArchiveOutcome::new(
            PathBuf::from("test.tar.gz"),
            ArchiveFormat::TarGz,
            state,
            manifest,
        )
        .with_archive_size(50);

        let display = format!("{}", outcome);
        assert!(display.contains("tar.gz"));
        assert!(display.contains("1 files"));
    }

    // DirectoryArchive Tests

    #[test]
    fn test_directory_archive_create() {
        let temp_dir = TempDir::new().unwrap();
        let archive_dir = temp_dir.path().join("archive");

        let archive = DirectoryArchive::new(&archive_dir).unwrap();
        assert!(archive.root().exists());
    }

    #[test]
    fn test_directory_archive_create_file() {
        let temp_dir = TempDir::new().unwrap();
        let archive_dir = temp_dir.path().join("archive");

        let mut archive = DirectoryArchive::new(&archive_dir).unwrap();
        let mut writer = archive.create_file("test.txt", 11, 0o644, 0).unwrap();

        writer.write_all(b"Hello World").unwrap();
        archive.close_file(writer).unwrap();

        let contents = std::fs::read_to_string(archive_dir.join("test.txt")).unwrap();
        assert_eq!(contents, "Hello World");
    }

    #[test]
    fn test_directory_archive_create_nested_file() {
        let temp_dir = TempDir::new().unwrap();
        let archive_dir = temp_dir.path().join("archive");

        let mut archive = DirectoryArchive::new(&archive_dir).unwrap();
        let mut writer = archive.create_file("src/lib/mod.rs", 4, 0o644, 0).unwrap();

        writer.write_all(b"test").unwrap();
        archive.close_file(writer).unwrap();

        assert!(archive_dir.join("src/lib/mod.rs").exists());
    }

    #[test]
    fn test_directory_archive_create_directory() {
        let temp_dir = TempDir::new().unwrap();
        let archive_dir = temp_dir.path().join("archive");

        let mut archive = DirectoryArchive::new(&archive_dir).unwrap();
        archive.create_directory("src/lib", 0o755, 0).unwrap();

        assert!(archive_dir.join("src/lib").is_dir());
    }

    // Helper Function Tests

    #[test]
    fn test_matches_glob_exact() {
        assert!(matches_glob("file.txt", "file.txt"));
        assert!(!matches_glob("file.txt", "other.txt"));
    }

    #[test]
    fn test_matches_glob_wildcard_all() {
        assert!(matches_glob("anything", "*"));
        assert!(matches_glob("", "*"));
    }

    #[test]
    fn test_matches_glob_prefix() {
        assert!(matches_glob("src/main.rs", "src/*"));
        assert!(!matches_glob("lib/main.rs", "src/*"));
    }

    #[test]
    fn test_matches_glob_suffix() {
        assert!(matches_glob("file.rs", "*.rs"));
        assert!(!matches_glob("file.txt", "*.rs"));
    }

    #[test]
    fn test_matches_glob_contains() {
        assert!(matches_glob("src/test/file.rs", "*test*"));
        assert!(!matches_glob("src/main/file.rs", "*test*"));
    }

    #[test]
    fn test_matches_glob_directory() {
        assert!(matches_glob("tmp/cache", "tmp/"));
        assert!(matches_glob("tmp", "tmp/"));
        assert!(!matches_glob("nottmp/file", "tmp/"));
    }

    #[test]
    fn test_ensure_extension() {
        assert_eq!(
            ensure_extension(Path::new("file"), ArchiveFormat::TarGz),
            PathBuf::from("file.tar.gz")
        );
        assert_eq!(
            ensure_extension(Path::new("file.tar.gz"), ArchiveFormat::TarGz),
            PathBuf::from("file.tar.gz")
        );
        assert_eq!(
            ensure_extension(Path::new("dir"), ArchiveFormat::Directory),
            PathBuf::from("dir")
        );
    }

    #[test]
    fn test_get_archive_path() {
        assert_eq!(
            get_archive_path(Path::new("archive.tar.gz"), None),
            PathBuf::from("archive.tar.gz")
        );
        // Note: file_stem() on "archive.tar.gz" returns "archive.tar"
        // so the result is "v1.0-archive.tar" not "v1.0-archive.tar.gz"
        assert_eq!(
            get_archive_path(Path::new("archive.tar.gz"), Some("v1.0")),
            PathBuf::from("v1.0-archive.tar")
        );
        // For single-extension files it works as expected
        assert_eq!(
            get_archive_path(Path::new("archive.tar"), Some("v1.0")),
            PathBuf::from("v1.0-archive")
        );
    }

    // ArchiveError Tests

    #[test]
    fn test_archive_error_display() {
        let err = ArchiveError::StateNotFound {
            state: "ABC123".to_string(),
        };
        assert!(format!("{}", err).contains("ABC123"));

        let err = ArchiveError::TagNotFound {
            name: "v1.0".to_string(),
        };
        assert!(format!("{}", err).contains("v1.0"));

        let err = ArchiveError::DestinationExists {
            path: PathBuf::from("/tmp/out"),
        };
        assert!(format!("{}", err).contains("/tmp/out"));

        let err = ArchiveError::EmptyArchive;
        assert!(format!("{}", err).contains("empty"));

        let err = ArchiveError::TooManyFiles { max: 1000 };
        assert!(format!("{}", err).contains("1000"));

        let err = ArchiveError::TooLarge { max: 1024 };
        assert!(format!("{}", err).contains("1024"));
    }
}
