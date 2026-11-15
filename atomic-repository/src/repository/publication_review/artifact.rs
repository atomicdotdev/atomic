//! Materialize without following pre-existing symlinks or overwriting user files.
use super::*;
use std::{
    io::Write,
    path::{Path, PathBuf},
};

pub(super) fn destination(repo: &Repository, hash: &str) -> Result<PathBuf, RepositoryError> {
    let mut path = repo.vault_dir();
    for component in [None, Some("publication-reviews"), Some(hash)] {
        if let Some(component) = component {
            path.push(component);
        }
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                return Err(refused(
                    "review materialization parent is not a regular directory",
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&path)?
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path.join("review.md"))
}

pub(super) fn write(
    repo: &Repository,
    vault_path: &str,
    disk_path: &Path,
) -> Result<(), RepositoryError> {
    let entry = repo
        .vault_retrieve(vault_path)?
        .ok_or_else(|| refused("stored review disappeared"))?;
    let expected = super::super::vault::render_entry_to_markdown(&entry);
    match std::fs::symlink_metadata(disk_path) {
        Ok(metadata)
            if metadata.file_type().is_file()
                && std::fs::read(disk_path)? == expected.as_bytes() =>
        {
            return Ok(())
        }
        Ok(_) => {
            return Err(refused(
                "refusing to overwrite a modified or non-regular review file",
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let parent = disk_path
        .parent()
        .ok_or_else(|| refused("review has no destination parent"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(expected.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist_noclobber(disk_path)
        .map_err(|error| refused(error.error))?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
