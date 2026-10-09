use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use atomic_core::types::{Hash, WorkingCopyId};
use thiserror::Error;

use super::{ChangeSourceKind, ChangeSourceToken};

const MAGIC: &[u8; 8] = b"ACSTOKN\0";
pub const CHANGE_SOURCE_TOKEN_VERSION: u8 = 1;
const HEADER_SIZE: usize = 8 + 1 + 1 + 4;
const CHECKSUM_SIZE: usize = 32;
const MAX_TOKEN_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenInvalidationReason {
    UnknownVersion(u8),
    WrongSource,
    Malformed,
    ChecksumMismatch,
    Oversized,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TokenLoadResult {
    pub token: Option<ChangeSourceToken>,
    pub invalidation: Option<TokenInvalidationReason>,
}

#[derive(Debug, Error)]
pub enum TokenStoreError {
    #[error("cannot access change-source token '{}': {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Versioned, working-copy/source-scoped acceleration checkpoint storage.
///
/// Tokens are atomically replaced and fail closed. They are never included in
/// canonical roots or accepted as repository-state proof.
pub struct ChangeSourceTokenStore<'a> {
    atomic_dir: &'a Path,
    working_copy: WorkingCopyId,
}

impl<'a> ChangeSourceTokenStore<'a> {
    pub fn new(atomic_dir: &'a Path, working_copy: WorkingCopyId) -> Self {
        Self {
            atomic_dir,
            working_copy,
        }
    }

    pub fn load(&self, source: ChangeSourceKind) -> Result<TokenLoadResult, TokenStoreError> {
        if !matches!(
            source,
            ChangeSourceKind::Fsmonitor | ChangeSourceKind::Watchman
        ) {
            return Ok(TokenLoadResult::default());
        }
        let path = self.path(source);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(TokenLoadResult::default())
            }
            Err(source) => return Err(TokenStoreError::Io { path, source }),
        };
        match decode(source, &bytes) {
            Ok(token) => Ok(TokenLoadResult {
                token: Some(token),
                invalidation: None,
            }),
            Err(invalidation) => {
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(source) => return Err(TokenStoreError::Io { path, source }),
                }
                Ok(TokenLoadResult {
                    token: None,
                    invalidation: Some(invalidation),
                })
            }
        }
    }

    pub fn store(
        &self,
        source: ChangeSourceKind,
        token: &ChangeSourceToken,
    ) -> Result<(), TokenStoreError> {
        if !matches!(
            source,
            ChangeSourceKind::Fsmonitor | ChangeSourceKind::Watchman
        ) {
            return Ok(());
        }
        let path = self.path(source);
        let parent = path.parent().expect("token path has a parent");
        fs::create_dir_all(parent).map_err(|source| TokenStoreError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        let bytes = encode(source, token);
        let temporary = parent.join(format!(
            ".{}.{}.{}.tmp",
            source.as_str(),
            std::process::id(),
            ulid::Ulid::new()
        ));
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .map_err(|source| TokenStoreError::Io {
                path: temporary.clone(),
                source,
            })?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|source| TokenStoreError::Io {
                path: temporary.clone(),
                source,
            })?;
        fs::rename(&temporary, &path).map_err(|source| TokenStoreError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(())
    }

    pub fn invalidate(&self, source: ChangeSourceKind) -> Result<(), TokenStoreError> {
        let path = self.path(source);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(TokenStoreError::Io { path, source }),
        }
    }

    fn path(&self, source: ChangeSourceKind) -> PathBuf {
        self.atomic_dir
            .join("working-copies")
            .join(self.working_copy.to_string())
            .join("change-source-tokens")
            .join(format!("{}.token", source.as_str()))
    }
}

fn source_tag(source: ChangeSourceKind) -> u8 {
    match source {
        ChangeSourceKind::Scan => 0,
        ChangeSourceKind::Fsmonitor => 1,
        ChangeSourceKind::Watchman => 2,
        ChangeSourceKind::Custom => 3,
    }
}

fn encode(source: ChangeSourceKind, token: &ChangeSourceToken) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_SIZE + token.0.len() + CHECKSUM_SIZE);
    bytes.extend_from_slice(MAGIC);
    bytes.push(CHANGE_SOURCE_TOKEN_VERSION);
    bytes.push(source_tag(source));
    bytes.extend_from_slice(&(token.0.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&token.0);
    let checksum = Hash::of(&bytes);
    bytes.extend_from_slice(checksum.as_bytes());
    bytes
}

fn decode(
    expected_source: ChangeSourceKind,
    bytes: &[u8],
) -> Result<ChangeSourceToken, TokenInvalidationReason> {
    if bytes.len() < HEADER_SIZE + CHECKSUM_SIZE || &bytes[..8] != MAGIC {
        return Err(TokenInvalidationReason::Malformed);
    }
    let version = bytes[8];
    if version != CHANGE_SOURCE_TOKEN_VERSION {
        return Err(TokenInvalidationReason::UnknownVersion(version));
    }
    if bytes[9] != source_tag(expected_source) {
        return Err(TokenInvalidationReason::WrongSource);
    }
    let length = u32::from_le_bytes(bytes[10..14].try_into().expect("fixed slice")) as usize;
    if length > MAX_TOKEN_BYTES {
        return Err(TokenInvalidationReason::Oversized);
    }
    let expected_len = HEADER_SIZE + length + CHECKSUM_SIZE;
    if bytes.len() != expected_len {
        return Err(TokenInvalidationReason::Malformed);
    }
    let checksum_offset = HEADER_SIZE + length;
    if Hash::of(&bytes[..checksum_offset]).as_bytes() != &bytes[checksum_offset..] {
        return Err(TokenInvalidationReason::ChecksumMismatch);
    }
    Ok(ChangeSourceToken(
        bytes[HEADER_SIZE..checksum_offset].to_vec(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    #[allow(clippy::drop_non_drop)] // drop is deliberate scope documentation
    fn token_reopens_updates_and_is_source_scoped() {
        let dir = tempdir().unwrap();
        let working_copy = WorkingCopyId::new();
        let store = ChangeSourceTokenStore::new(dir.path(), working_copy);
        let first = ChangeSourceToken(b"watchman-clock-v1:c:1".to_vec());
        store.store(ChangeSourceKind::Watchman, &first).unwrap();
        drop(store);

        let reopened = ChangeSourceTokenStore::new(dir.path(), working_copy);
        assert_eq!(
            reopened.load(ChangeSourceKind::Watchman).unwrap().token,
            Some(first)
        );
        assert_eq!(
            reopened.load(ChangeSourceKind::Fsmonitor).unwrap(),
            TokenLoadResult::default()
        );
        let other_working_copy = ChangeSourceTokenStore::new(dir.path(), WorkingCopyId::new());
        assert_eq!(
            other_working_copy.load(ChangeSourceKind::Watchman).unwrap(),
            TokenLoadResult::default()
        );
        let second = ChangeSourceToken(b"watchman-clock-v1:c:2".to_vec());
        reopened.store(ChangeSourceKind::Watchman, &second).unwrap();
        assert_eq!(
            reopened.load(ChangeSourceKind::Watchman).unwrap().token,
            Some(second)
        );
    }

    #[test]
    fn unknown_version_and_corruption_fail_closed_and_invalidate() {
        let dir = tempdir().unwrap();
        let working_copy = WorkingCopyId::new();
        let store = ChangeSourceTokenStore::new(dir.path(), working_copy);
        let token = ChangeSourceToken(b"git-fsmonitor-v1:x".to_vec());
        store.store(ChangeSourceKind::Fsmonitor, &token).unwrap();
        let path = store.path(ChangeSourceKind::Fsmonitor);

        let mut bytes = fs::read(&path).unwrap();
        bytes[8] = CHANGE_SOURCE_TOKEN_VERSION + 1;
        fs::write(&path, bytes).unwrap();
        let unknown = store.load(ChangeSourceKind::Fsmonitor).unwrap();
        assert_eq!(
            unknown.invalidation,
            Some(TokenInvalidationReason::UnknownVersion(
                CHANGE_SOURCE_TOKEN_VERSION + 1
            ))
        );
        assert!(!path.exists());

        store.store(ChangeSourceKind::Fsmonitor, &token).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes[HEADER_SIZE] ^= 0xff;
        fs::write(&path, bytes).unwrap();
        let corrupt = store.load(ChangeSourceKind::Fsmonitor).unwrap();
        assert_eq!(
            corrupt.invalidation,
            Some(TokenInvalidationReason::ChecksumMismatch)
        );
        assert!(!path.exists());
    }
}
