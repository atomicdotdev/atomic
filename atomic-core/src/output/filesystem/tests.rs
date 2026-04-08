//! Tests for the filesystem working copy implementation.

use super::paths::normalize_path;
use super::*;
use crate::output::traits::{WorkingCopy, WorkingCopyRead};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tempfile::TempDir;

/// Create a temporary directory for testing.
fn temp_dir() -> TempDir {
    tempfile::tempdir().expect("Failed to create temp dir")
}

/// Create a FileSystem rooted at a temporary directory.
fn temp_fs() -> (TempDir, FileSystem) {
    let dir = temp_dir();
    let fs = FileSystem::from_root(dir.path());
    (dir, fs)
}

// ------------------------------------------------------------------------
// Construction and Basic Properties
// ------------------------------------------------------------------------

#[test]
fn test_from_root_path() {
    let fs = FileSystem::from_root("/some/path");
    assert_eq!(fs.root(), Path::new("/some/path"));
}

#[test]
fn test_from_root_pathbuf() {
    let path = PathBuf::from("/another/path");
    let fs = FileSystem::from_root(&path);
    assert_eq!(fs.root(), Path::new("/another/path"));
}

#[test]
fn test_clone() {
    let fs1 = FileSystem::from_root("/test");
    let fs2 = fs1.clone();
    assert_eq!(fs1.root(), fs2.root());
}

#[test]
fn test_debug() {
    let fs = FileSystem::from_root("/test");
    let debug = format!("{:?}", fs);
    assert!(debug.contains("FileSystem"));
    assert!(debug.contains("/test"));
}

// ------------------------------------------------------------------------
// Path Resolution
// ------------------------------------------------------------------------

#[test]
fn test_resolve_path_simple() {
    let (dir, fs) = temp_fs();
    let resolved = fs.resolve_path("file.txt").unwrap();
    assert_eq!(resolved, dir.path().join("file.txt"));
}

#[test]
fn test_resolve_path_nested() {
    let (dir, fs) = temp_fs();
    let resolved = fs.resolve_path("a/b/c.txt").unwrap();
    assert_eq!(resolved, dir.path().join("a/b/c.txt"));
}

#[test]
fn test_resolve_path_with_dot() {
    let (dir, fs) = temp_fs();
    let resolved = fs.resolve_path("./file.txt").unwrap();
    assert_eq!(resolved, dir.path().join("file.txt"));
}

#[test]
fn test_resolve_path_traversal_blocked() {
    let (_dir, fs) = temp_fs();
    let result = fs.resolve_path("../escape");
    assert!(result.is_err());
}

#[test]
fn test_resolve_path_deep_traversal_blocked() {
    let (_dir, fs) = temp_fs();
    let result = fs.resolve_path("a/b/../../..");
    assert!(result.is_err());
}

// ------------------------------------------------------------------------
// File Existence and Type Checks
// ------------------------------------------------------------------------

#[test]
fn test_exists_file() {
    let (dir, fs) = temp_fs();
    let path = dir.path().join("test.txt");
    std::fs::write(&path, "content").unwrap();

    assert!(fs.exists("test.txt"));
    assert!(!fs.exists("nonexistent.txt"));
}

#[test]
fn test_exists_directory() {
    let (dir, fs) = temp_fs();
    std::fs::create_dir(dir.path().join("subdir")).unwrap();

    assert!(fs.exists("subdir"));
}

#[test]
fn test_is_directory() {
    let (dir, fs) = temp_fs();
    std::fs::create_dir(dir.path().join("subdir")).unwrap();
    std::fs::write(dir.path().join("file.txt"), "").unwrap();

    assert!(fs.is_directory("subdir"));
    assert!(!fs.is_directory("file.txt"));
    assert!(!fs.is_directory("nonexistent"));
}

#[test]
fn test_is_atomic_path() {
    let (_dir, fs) = temp_fs();

    assert!(fs.is_atomic_path(".atomic"));
    assert!(fs.is_atomic_path(".atomic/pristine"));
    assert!(fs.is_atomic_path(".atomic/changes/AB/CD"));
    assert!(!fs.is_atomic_path("src/main.rs"));
    assert!(!fs.is_atomic_path(".atomicfoo")); // Not a prefix match
}

// ------------------------------------------------------------------------
// Reading Files
// ------------------------------------------------------------------------

#[test]
fn test_read_file_simple() {
    let (dir, fs) = temp_fs();
    std::fs::write(dir.path().join("test.txt"), "hello world").unwrap();

    let mut buffer = Vec::new();
    fs.read_file("test.txt", &mut buffer).unwrap();
    assert_eq!(buffer, b"hello world");
}

#[test]
fn test_read_file_binary() {
    let (dir, fs) = temp_fs();
    let binary_data: Vec<u8> = (0..=255).collect();
    std::fs::write(dir.path().join("binary.bin"), &binary_data).unwrap();

    let mut buffer = Vec::new();
    fs.read_file("binary.bin", &mut buffer).unwrap();
    assert_eq!(buffer, binary_data);
}

#[test]
fn test_read_file_appends_to_buffer() {
    let (dir, fs) = temp_fs();
    std::fs::write(dir.path().join("test.txt"), "world").unwrap();

    let mut buffer = b"hello ".to_vec();
    fs.read_file("test.txt", &mut buffer).unwrap();
    assert_eq!(buffer, b"hello world");
}

#[test]
fn test_read_file_nested() {
    let (dir, fs) = temp_fs();
    std::fs::create_dir_all(dir.path().join("a/b/c")).unwrap();
    std::fs::write(dir.path().join("a/b/c/deep.txt"), "deep content").unwrap();

    let mut buffer = Vec::new();
    fs.read_file("a/b/c/deep.txt", &mut buffer).unwrap();
    assert_eq!(buffer, b"deep content");
}

#[test]
fn test_read_file_not_found() {
    let (_dir, fs) = temp_fs();
    let mut buffer = Vec::new();
    let result = fs.read_file("nonexistent.txt", &mut buffer);
    assert!(result.is_err());
}

#[test]
fn test_read_file_is_directory() {
    let (dir, fs) = temp_fs();
    std::fs::create_dir(dir.path().join("subdir")).unwrap();

    let mut buffer = Vec::new();
    let result = fs.read_file("subdir", &mut buffer);
    assert!(result.is_err());
}

#[test]
fn test_read_file_empty() {
    let (dir, fs) = temp_fs();
    std::fs::write(dir.path().join("empty.txt"), "").unwrap();

    let mut buffer = Vec::new();
    fs.read_file("empty.txt", &mut buffer).unwrap();
    assert!(buffer.is_empty());
}

// ------------------------------------------------------------------------
// File Metadata
// ------------------------------------------------------------------------

#[test]
fn test_file_metadata_regular_file() {
    let (dir, fs) = temp_fs();
    std::fs::write(dir.path().join("file.txt"), "content").unwrap();

    let meta = fs.file_metadata("file.txt").unwrap();
    assert!(!meta.is_dir);
    assert!(!meta.is_symlink);
}

#[test]
fn test_file_metadata_directory() {
    let (dir, fs) = temp_fs();
    std::fs::create_dir(dir.path().join("subdir")).unwrap();

    let meta = fs.file_metadata("subdir").unwrap();
    assert!(meta.is_dir);
    assert!(!meta.is_symlink);
}

#[test]
fn test_file_metadata_not_found() {
    let (_dir, fs) = temp_fs();
    let result = fs.file_metadata("nonexistent");
    assert!(result.is_err());
}

#[cfg(unix)]
#[test]
fn test_file_metadata_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let (dir, fs) = temp_fs();
    let path = dir.path().join("executable.sh");
    std::fs::write(&path, "#!/bin/bash").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

    let meta = fs.file_metadata("executable.sh").unwrap();
    assert_eq!(meta.permissions & 0o111, 0o111); // Has execute bits
}

#[cfg(unix)]
#[test]
fn test_file_metadata_symlink() {
    let (dir, fs) = temp_fs();
    std::fs::write(dir.path().join("target.txt"), "content").unwrap();
    std::os::unix::fs::symlink(dir.path().join("target.txt"), dir.path().join("link.txt")).unwrap();

    let meta = fs.file_metadata("link.txt").unwrap();
    assert!(meta.is_symlink);
}

// ------------------------------------------------------------------------
// Modified Time
// ------------------------------------------------------------------------

#[test]
fn test_modified_time_exists() {
    let (dir, fs) = temp_fs();
    std::fs::write(dir.path().join("file.txt"), "content").unwrap();

    let mtime = fs.modified_time("file.txt").unwrap();
    let now = SystemTime::now();

    // Modified time should be very recent (within last 10 seconds)
    let elapsed = now.duration_since(mtime).unwrap();
    assert!(elapsed.as_secs() < 10);
}

#[test]
fn test_modified_time_not_found() {
    let (_dir, fs) = temp_fs();
    let result = fs.modified_time("nonexistent.txt");
    assert!(result.is_err());
}

// ------------------------------------------------------------------------
// Writing Files
// ------------------------------------------------------------------------

#[test]
fn test_write_file_simple() {
    let (dir, fs) = temp_fs();

    {
        let mut writer = fs.write_file("output.txt", Inode::new(1)).unwrap();
        writer.write_all(b"hello world").unwrap();
    }

    let contents = std::fs::read_to_string(dir.path().join("output.txt")).unwrap();
    assert_eq!(contents, "hello world");
}

#[test]
fn test_write_file_creates_parents() {
    let (dir, fs) = temp_fs();

    {
        let mut writer = fs.write_file("a/b/c/deep.txt", Inode::new(1)).unwrap();
        writer.write_all(b"deep content").unwrap();
    }

    let contents = std::fs::read_to_string(dir.path().join("a/b/c/deep.txt")).unwrap();
    assert_eq!(contents, "deep content");
}

#[test]
fn test_write_file_overwrites() {
    let (dir, fs) = temp_fs();
    std::fs::write(dir.path().join("existing.txt"), "old content").unwrap();

    {
        let mut writer = fs.write_file("existing.txt", Inode::new(1)).unwrap();
        writer.write_all(b"new content").unwrap();
    }

    let contents = std::fs::read_to_string(dir.path().join("existing.txt")).unwrap();
    assert_eq!(contents, "new content");
}

#[test]
fn test_write_file_binary() {
    let (dir, fs) = temp_fs();
    let binary_data: Vec<u8> = (0..=255).collect();

    {
        let mut writer = fs.write_file("binary.bin", Inode::new(1)).unwrap();
        writer.write_all(&binary_data).unwrap();
    }

    let contents = std::fs::read(dir.path().join("binary.bin")).unwrap();
    assert_eq!(contents, binary_data);
}

#[test]
fn test_file_writer_inode() {
    let (_dir, fs) = temp_fs();
    let writer = fs.write_file("test.txt", Inode::new(42)).unwrap();
    assert_eq!(writer.inode(), Inode::new(42));
}

#[test]
fn test_file_writer_path() {
    let (dir, fs) = temp_fs();
    let writer = fs.write_file("test.txt", Inode::new(1)).unwrap();
    assert_eq!(writer.path(), dir.path().join("test.txt"));
}

#[test]
fn test_file_writer_finish() {
    let (dir, fs) = temp_fs();

    let mut writer = fs.write_file("test.txt", Inode::new(1)).unwrap();
    writer.write_all(b"content").unwrap();
    writer.finish().unwrap();

    let contents = std::fs::read_to_string(dir.path().join("test.txt")).unwrap();
    assert_eq!(contents, "content");
}

// ------------------------------------------------------------------------
// Directory Operations
// ------------------------------------------------------------------------

#[test]
fn test_create_dir_all_simple() {
    let (dir, fs) = temp_fs();

    fs.create_dir_all("new_dir").unwrap();
    assert!(dir.path().join("new_dir").is_dir());
}

#[test]
fn test_create_dir_all_nested() {
    let (dir, fs) = temp_fs();

    fs.create_dir_all("a/b/c/d").unwrap();
    assert!(dir.path().join("a/b/c/d").is_dir());
}

#[test]
fn test_create_dir_all_existing() {
    let (dir, fs) = temp_fs();
    std::fs::create_dir(dir.path().join("existing")).unwrap();

    // Should not error for existing directory
    fs.create_dir_all("existing").unwrap();
    assert!(dir.path().join("existing").is_dir());
}

#[test]
fn test_list_dir() {
    let (dir, fs) = temp_fs();
    std::fs::write(dir.path().join("file1.txt"), "").unwrap();
    std::fs::write(dir.path().join("file2.txt"), "").unwrap();
    std::fs::create_dir(dir.path().join("subdir")).unwrap();

    let entries = fs.list_dir("").unwrap();

    assert_eq!(entries.len(), 3);
    assert!(entries.contains(&("file1.txt".to_string(), false)));
    assert!(entries.contains(&("file2.txt".to_string(), false)));
    assert!(entries.contains(&("subdir".to_string(), true)));
}

#[test]
fn test_list_dir_sorted() {
    let (dir, fs) = temp_fs();
    std::fs::write(dir.path().join("c.txt"), "").unwrap();
    std::fs::write(dir.path().join("a.txt"), "").unwrap();
    std::fs::write(dir.path().join("b.txt"), "").unwrap();

    let entries = fs.list_dir("").unwrap();

    // Should be alphabetically sorted
    assert_eq!(entries[0].0, "a.txt");
    assert_eq!(entries[1].0, "b.txt");
    assert_eq!(entries[2].0, "c.txt");
}

#[test]
fn test_list_dir_not_found() {
    let (_dir, fs) = temp_fs();
    let result = fs.list_dir("nonexistent");
    assert!(result.is_err());
}

// ------------------------------------------------------------------------
// Remove Operations
// ------------------------------------------------------------------------

#[test]
fn test_remove_file() {
    let (dir, fs) = temp_fs();
    std::fs::write(dir.path().join("file.txt"), "content").unwrap();

    fs.remove_path("file.txt", false).unwrap();
    assert!(!dir.path().join("file.txt").exists());
}

#[test]
fn test_remove_empty_directory() {
    let (dir, fs) = temp_fs();
    std::fs::create_dir(dir.path().join("empty_dir")).unwrap();

    fs.remove_path("empty_dir", false).unwrap();
    assert!(!dir.path().join("empty_dir").exists());
}

#[test]
fn test_remove_nonempty_directory_fails() {
    let (dir, fs) = temp_fs();
    std::fs::create_dir(dir.path().join("nonempty")).unwrap();
    std::fs::write(dir.path().join("nonempty/file.txt"), "").unwrap();

    let result = fs.remove_path("nonempty", false);
    assert!(result.is_err());
}

#[test]
fn test_remove_recursive() {
    let (dir, fs) = temp_fs();
    std::fs::create_dir_all(dir.path().join("a/b/c")).unwrap();
    std::fs::write(dir.path().join("a/b/c/file.txt"), "").unwrap();
    std::fs::write(dir.path().join("a/b/other.txt"), "").unwrap();

    fs.remove_path("a", true).unwrap();
    assert!(!dir.path().join("a").exists());
}

#[test]
fn test_remove_not_found() {
    let (_dir, fs) = temp_fs();
    let result = fs.remove_path("nonexistent", false);
    assert!(result.is_err());
}

// ------------------------------------------------------------------------
// Rename Operations
// ------------------------------------------------------------------------

#[test]
fn test_rename_file() {
    let (dir, fs) = temp_fs();
    std::fs::write(dir.path().join("old.txt"), "content").unwrap();

    fs.rename("old.txt", "new.txt").unwrap();

    assert!(!dir.path().join("old.txt").exists());
    assert!(dir.path().join("new.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("new.txt")).unwrap(),
        "content"
    );
}

#[test]
fn test_rename_directory() {
    let (dir, fs) = temp_fs();
    std::fs::create_dir(dir.path().join("old_dir")).unwrap();
    std::fs::write(dir.path().join("old_dir/file.txt"), "content").unwrap();

    fs.rename("old_dir", "new_dir").unwrap();

    assert!(!dir.path().join("old_dir").exists());
    assert!(dir.path().join("new_dir/file.txt").exists());
}

#[test]
fn test_rename_creates_parent() {
    let (dir, fs) = temp_fs();
    std::fs::write(dir.path().join("file.txt"), "content").unwrap();

    fs.rename("file.txt", "new/path/file.txt").unwrap();

    assert!(dir.path().join("new/path/file.txt").exists());
}

#[test]
fn test_rename_not_found() {
    let (_dir, fs) = temp_fs();
    let result = fs.rename("nonexistent", "new_name");
    assert!(result.is_err());
}

// ------------------------------------------------------------------------
// Permissions
// ------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn test_set_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let (dir, fs) = temp_fs();
    let path = dir.path().join("file.txt");
    std::fs::write(&path, "content").unwrap();

    fs.set_permissions("file.txt", 0o755).unwrap();

    let perms = std::fs::metadata(&path).unwrap().permissions();
    assert_eq!(perms.mode() & 0o777, 0o755);
}

#[test]
fn test_is_writable_file() {
    let (dir, fs) = temp_fs();
    std::fs::write(dir.path().join("file.txt"), "content").unwrap();

    assert!(fs.is_writable("file.txt").unwrap());
}

#[test]
fn test_is_writable_nonexistent() {
    let (_dir, fs) = temp_fs();
    // Non-existent files in writable directory should be writable
    assert!(fs.is_writable("nonexistent.txt").unwrap());
}

// ------------------------------------------------------------------------
// Walk Files
// ------------------------------------------------------------------------

#[test]
fn test_walk_files_empty() {
    let (_dir, fs) = temp_fs();
    let files = fs.walk_files("").unwrap();
    assert!(files.is_empty());
}

#[test]
fn test_walk_files_simple() {
    let (dir, fs) = temp_fs();
    std::fs::write(dir.path().join("file1.txt"), "").unwrap();
    std::fs::write(dir.path().join("file2.txt"), "").unwrap();

    let files = fs.walk_files("").unwrap();

    assert_eq!(files.len(), 2);
    assert!(files.contains(&"file1.txt".to_string()));
    assert!(files.contains(&"file2.txt".to_string()));
}

#[test]
fn test_walk_files_nested() {
    let (dir, fs) = temp_fs();
    std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
    std::fs::write(dir.path().join("root.txt"), "").unwrap();
    std::fs::write(dir.path().join("a/middle.txt"), "").unwrap();
    std::fs::write(dir.path().join("a/b/deep.txt"), "").unwrap();

    let files = fs.walk_files("").unwrap();

    assert_eq!(files.len(), 3);
    assert!(files.contains(&"root.txt".to_string()));
    assert!(
        files.contains(&"a/middle.txt".to_string()) || files.contains(&"a\\middle.txt".to_string())
    ); // Windows compat
}

#[test]
fn test_walk_files_excludes_atomic_dir() {
    let (dir, fs) = temp_fs();
    std::fs::write(dir.path().join("file.txt"), "").unwrap();
    std::fs::create_dir(dir.path().join(".atomic")).unwrap();
    std::fs::write(dir.path().join(".atomic/pristine"), "").unwrap();

    let files = fs.walk_files("").unwrap();

    assert_eq!(files.len(), 1);
    assert!(files.contains(&"file.txt".to_string()));
}

#[test]
fn test_walk_files_sorted() {
    let (dir, fs) = temp_fs();
    std::fs::write(dir.path().join("c.txt"), "").unwrap();
    std::fs::write(dir.path().join("a.txt"), "").unwrap();
    std::fs::write(dir.path().join("b.txt"), "").unwrap();

    let files = fs.walk_files("").unwrap();

    assert_eq!(files[0], "a.txt");
    assert_eq!(files[1], "b.txt");
    assert_eq!(files[2], "c.txt");
}

#[test]
fn test_walk_files_from_subdir() {
    let (dir, fs) = temp_fs();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("root.txt"), "").unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();

    let files = fs.walk_files("src").unwrap();

    assert_eq!(files.len(), 2);
    // Files should still have full relative paths from root
}

// ------------------------------------------------------------------------
// Helper Functions
// ------------------------------------------------------------------------

#[test]
fn test_normalize_path_simple() {
    let path = PathBuf::from("/a/b/c");
    let normalized = normalize_path(&path);
    assert_eq!(normalized, PathBuf::from("/a/b/c"));
}

#[test]
fn test_normalize_path_with_dot() {
    let path = PathBuf::from("/a/./b/./c");
    let normalized = normalize_path(&path);
    assert_eq!(normalized, PathBuf::from("/a/b/c"));
}

#[test]
fn test_normalize_path_with_dotdot() {
    let path = PathBuf::from("/a/b/../c");
    let normalized = normalize_path(&path);
    assert_eq!(normalized, PathBuf::from("/a/c"));
}

#[test]
fn test_normalize_path_complex() {
    let path = PathBuf::from("/a/b/c/../../d/./e");
    let normalized = normalize_path(&path);
    assert_eq!(normalized, PathBuf::from("/a/d/e"));
}

// ------------------------------------------------------------------------
// Integration Tests
// ------------------------------------------------------------------------

#[test]
fn test_roundtrip_write_read() {
    let (_dir, fs) = temp_fs();
    let content = b"This is test content with unicode: \xc3\xa9\xc3\xa8\xc3\xa0";

    // Write
    {
        let mut writer = fs.write_file("test.txt", Inode::new(1)).unwrap();
        writer.write_all(content).unwrap();
    }

    // Read
    let mut buffer = Vec::new();
    fs.read_file("test.txt", &mut buffer).unwrap();
    assert_eq!(buffer, content);
}

#[test]
fn test_full_workflow() {
    let (_dir, fs) = temp_fs();

    // Create directory structure
    fs.create_dir_all("src/utils").unwrap();

    // Write files
    {
        let mut w = fs.write_file("src/main.rs", Inode::new(1)).unwrap();
        w.write_all(b"fn main() {}").unwrap();
    }
    {
        let mut w = fs
            .write_file("src/utils/helpers.rs", Inode::new(2))
            .unwrap();
        w.write_all(b"pub fn help() {}").unwrap();
    }

    // Verify structure
    assert!(fs.is_directory("src"));
    assert!(fs.is_directory("src/utils"));
    assert!(fs.exists("src/main.rs"));
    assert!(fs.exists("src/utils/helpers.rs"));

    // Read back
    let mut main_content = Vec::new();
    fs.read_file("src/main.rs", &mut main_content).unwrap();
    assert_eq!(main_content, b"fn main() {}");

    // Rename
    fs.rename("src/utils/helpers.rs", "src/utils/lib.rs")
        .unwrap();
    assert!(!fs.exists("src/utils/helpers.rs"));
    assert!(fs.exists("src/utils/lib.rs"));

    // Remove
    fs.remove_path("src/utils/lib.rs", false).unwrap();
    assert!(!fs.exists("src/utils/lib.rs"));

    // Clean up
    fs.remove_path("src", true).unwrap();
    assert!(!fs.exists("src"));
}
