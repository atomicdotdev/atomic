use std::path::Path;

use atomic_core::change::InodeKind;

use super::RepositoryError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WorkingInodeAttrs {
    pub mode: u16,
    pub kind: InodeKind,
}

pub(super) fn working_inode_attrs(path: &Path) -> Result<WorkingInodeAttrs, RepositoryError> {
    let metadata = std::fs::symlink_metadata(path).map_err(RepositoryError::Io)?;
    let file_type = metadata.file_type();
    let kind = if file_type.is_symlink() {
        InodeKind::Symlink
    } else if file_type.is_file() {
        InodeKind::Regular
    } else if file_type.is_dir() {
        InodeKind::Gitlink
    } else {
        return Err(RepositoryError::InvalidOperation {
            message: format!("unsupported filesystem kind at '{}'", path.display()),
        });
    };

    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        (metadata.permissions().mode() & 0o777) as u16
    };
    #[cfg(not(unix))]
    let mode = if metadata.permissions().readonly() {
        0o444
    } else {
        // Must match `operation.rs::metadata_mode`: the effect executor
        // observes writable windows files as 0o666, so the canonical mode
        // stored at record time has to agree or every write lease diverges.
        0o666
    };

    Ok(WorkingInodeAttrs { mode, kind })
}
