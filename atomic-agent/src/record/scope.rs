//! Explicit, fingerprinted file ownership for hooks sharing a working tree.
use std::collections::BTreeMap;
use std::path::{Component, Path};

use atomic_repository::status::{RepositoryStatus, StatusOptions};
use atomic_repository::Repository;
use sha2::{Digest, Sha256};

use super::TurnRecordOptions;
use crate::error::{AgentError, AgentResult};

pub type FileManifest = BTreeMap<String, Option<String>>;

fn checked_path(root: &Path, path: &str) -> Result<std::path::PathBuf, String> {
    if path.is_empty()
        || !Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(format!("invalid scoped file path: {path:?}"));
    }
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let full = root.join(path);
    let mut parent = full.parent();
    while let Some(p) = parent {
        if p.exists() {
            if !p
                .canonicalize()
                .map_err(|e| e.to_string())?
                .starts_with(&root)
            {
                return Err(format!("scoped path escapes repository: {path}"));
            }
            break;
        }
        parent = p.parent();
    }
    if Path::new(path)
        .components()
        .any(|c| matches!(c, Component::Normal(n) if n == ".atomic" || n == ".git"))
    {
        return Err(format!("repository metadata cannot be scoped: {path}"));
    }
    Ok(full)
}

/// Fingerprint a file without following its final symlink. `None` means deleted.
/// Reject directories and paths outside the working tree.
pub fn fingerprint(root: &Path, path: &str) -> Result<Option<String>, String> {
    let full = checked_path(root, path)?;
    let meta = match std::fs::symlink_metadata(&full) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(full).map_err(|e| e.to_string())?;
        return Ok(Some(format!(
            "link:{:x}",
            Sha256::digest(target.as_os_str().as_encoded_bytes())
        )));
    }
    if !meta.is_file() {
        return Err(format!("not a regular scoped file: {path}"));
    }
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    let mut file = std::fs::File::open(full).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash).map_err(|e| e.to_string())?;
    Ok(Some(format!(
        "file:{hash:x}:{executable}",
        hash = hash.finalize()
    )))
}

/// Return fingerprints for dirty files and explicitly requested prior candidates.
/// The protocol version also lets plugins refuse an older, unscoped CLI.
pub fn snapshot(root: &Path, extra: &[String]) -> Result<serde_json::Value, String> {
    let repo = Repository::open_readonly_wait(root, std::time::Duration::from_secs(10))
        .map_err(|e| e.to_string())?;
    let status = repo
        .status(StatusOptions::default().with_untracked(true))
        .map_err(|e| e.to_string())?;
    let dirty: Vec<String> = status
        .entries()
        .iter()
        .map(|e| e.path().to_string_lossy().to_string())
        .collect();
    let mut files = FileManifest::new();
    for path in status
        .entries()
        .iter()
        .map(|e| e.path().to_string_lossy().to_string())
        .chain(extra.iter().cloned())
    {
        if root.join(&path).is_dir() && !root.join(&path).is_symlink() {
            continue;
        }
        files.insert(path.clone(), fingerprint(root, &path)?);
    }
    Ok(
        serde_json::json!({"scope_version":1, "view":repo.current_view(), "files":files, "dirty":dirty}),
    )
}

pub(super) fn manifest(options: &TurnRecordOptions<'_>) -> AgentResult<Option<FileManifest>> {
    let raw = options
        .event
        .raw_json
        .as_ref()
        .and_then(|v| v.get("record_files"));
    let fail = |reason| AgentError::RecordFailed {
        session_id: options.session.session_id.clone(),
        turn_number: options.turn_number,
        reason,
    };
    match raw {
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|e| fail(format!("invalid record_files manifest: {e}"))),
        None if options.session.explicit_record_files => Err(fail(
            "explicit record_files manifest required; refusing to record shared working tree"
                .into(),
        )),
        None => Ok(None),
    }
}

pub(super) fn validate(
    root: &Path,
    files: &FileManifest,
    options: &TurnRecordOptions<'_>,
) -> AgentResult<()> {
    for (path, expected) in files {
        let actual = fingerprint(root, path).map_err(|reason| AgentError::RecordFailed {
            session_id: options.session.session_id.clone(),
            turn_number: options.turn_number,
            reason,
        })?;
        if &actual != expected {
            return Err(AgentError::RecordFailed { session_id: options.session.session_id.clone(), turn_number: options.turn_number, reason: format!("scoped file changed after this session's tool finished: {path}; refusing ambiguous ownership") });
        }
    }
    Ok(())
}

pub(super) fn filter(status: RepositoryStatus, files: Option<&FileManifest>) -> RepositoryStatus {
    let Some(files) = files else {
        return status;
    };
    let mut result = RepositoryStatus::new(status.view().to_string(), status.state().copied());
    for entry in status.entries() {
        if entry.details() != Some("directory")
            && files.contains_key(entry.path().to_string_lossy().as_ref())
        {
            result.add_entry(entry.clone());
        }
    }
    result
}
