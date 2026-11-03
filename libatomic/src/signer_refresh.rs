//! Asynchronous signer discovery shared by CLI and daemon orchestration.
//! Projection and cache writes remain offline repository operations.

use atomic_identity::PublicKey;
use atomic_remote::{HttpRemoteConfig, StorageClient};
use atomic_repository::{Repository, RepositoryError};

/// Reuse the authenticated remote's origin and credentials, never an unrelated
/// globally selected server. Anonymous remotes do not trigger discovery.
pub fn client_for_remote(remote: &str, config: &HttpRemoteConfig) -> Option<StorageClient> {
    let header = |name: &str| {
        config
            .extra_headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    let token = header("authorization")?.strip_prefix("Bearer ")?;
    let origin = url::Url::parse(remote).ok()?.origin().ascii_serialization();
    StorageClient::with_delegation(&origin, "", token, header("atomic-delegation")).ok()
}

/// Discover only unresolved DIDs, with no pristine transaction held over I/O.
/// Older storage versions cannot resolve fingerprint DIDs: that is a cache
/// miss, never permission to interpret the fingerprint bytes as a public key.
pub async fn refresh(repo: &Repository, client: &StorageClient) -> Result<usize, RepositoryError> {
    let signers = repo.unresolved_attestation_signers()?;
    let mut cached = 0;
    for did in signers {
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.resolve_identity_by_public_key(&did),
        )
        .await;
        let Some(key) = response
            .ok()
            .and_then(Result::ok)
            .and_then(|identity| identity.public_key)
            .and_then(|key| PublicKey::from_base32(&key).ok())
        else {
            continue;
        };
        if !atomic_canonical::did::did_matches_public_key(&did, &key) {
            continue;
        }
        repo.cache_signer_key(&did, &key)?;
        cached += 1;
    }
    Ok(cached)
}
