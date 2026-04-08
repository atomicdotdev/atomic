//! Path normalization and permission helpers for the filesystem module.
//!
//! These are internal utilities used by [`super::FileSystem`] and the
//! [`super::walk`] module for path sanitization and cross-platform
//! permission handling.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

// CONSTANTS

/// Default file permissions on Unix (rw-r--r--)
#[cfg(unix)]
pub const DEFAULT_FILE_MODE: u32 = 0o644;

/// Default directory permissions on Unix (rwxr-xr-x)
#[cfg(unix)]
pub const DEFAULT_DIR_MODE: u32 = 0o755;

/// Default executable permissions on Unix (rwxr-xr-x)
#[cfg(unix)]
pub const DEFAULT_EXEC_MODE: u32 = 0o755;

// Non-Unix stubs so the constants exist on all platforms
#[cfg(not(unix))]
pub const DEFAULT_FILE_MODE: u32 = 0o644;

#[cfg(not(unix))]
pub const DEFAULT_DIR_MODE: u32 = 0o755;

#[cfg(not(unix))]
pub const DEFAULT_EXEC_MODE: u32 = 0o755;

// PATH NORMALIZATION

/// Normalize a path by resolving `.` and `..` components.
///
/// Unlike `canonicalize()`, this doesn't require the path to exist.
pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(p) => normalized.push(p.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {} // Skip `.`
            Component::ParentDir => {
                // Go up one level, but don't go above root
                normalized.pop();
            }
            Component::Normal(name) => normalized.push(name),
        }
    }

    normalized
}

// PERMISSION HELPERS

/// Get permissions from file metadata.
#[cfg(unix)]
pub(crate) fn get_permissions(metadata: &fs::Metadata) -> u16 {
    use std::os::unix::fs::PermissionsExt;
    (metadata.permissions().mode() & 0o777) as u16
}

/// Get permissions from file metadata (non-Unix fallback).
#[cfg(not(unix))]
pub(crate) fn get_permissions(metadata: &fs::Metadata) -> u16 {
    // On non-Unix, approximate with readable/writable
    if metadata.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

/// Set permissions on a path.
#[cfg(unix)]
pub(crate) fn set_permissions(path: &Path, permissions: u16) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = fs::Permissions::from_mode(permissions as u32);
    fs::set_permissions(path, perms)
}

/// Set permissions on a path (non-Unix fallback).
#[cfg(not(unix))]
pub(crate) fn set_permissions(path: &Path, permissions: u16) -> io::Result<()> {
    // On non-Unix, we can only set read-only
    let readonly = (permissions & 0o222) == 0;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_readonly(readonly);
    fs::set_permissions(path, perms)
}
