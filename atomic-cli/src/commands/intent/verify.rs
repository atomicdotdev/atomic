//! `atomic intent verify <ID>` — verify an intent's attestation sidecar.
//!
//! Reads the attested node written by `attest`, checks it is still fresh (the
//! intent hasn't changed since it was signed), and cryptographically verifies
//! its content hash + Ed25519 Data Integrity proof. This is the read-back that
//! makes the sidecar observable: `attest` writes it, `verify` proves it.
//!
//! The verifier key can come from two places:
//!
//! 1. The local identity store (`--identity <name>` or the default identity),
//!    exactly as before.
//! 2. A configured atomic-storage server: when `--identity <name>` is not
//!    present locally (e.g. after importing a review set authored by someone
//!    else), the CLI resolves the identity by name over
//!    `GET /identities/resolve?name=…` and uses the returned public key.
//!
//! In the remote case the attestation's `attributed_to` DID is required to
//! match the returned key (fingerprint check) before any signature is
//! verified — a swapped or malicious key fails before the crypto step.
//!
//! Keys resolved from the server are cached locally (24h TTL, namespaced by
//! (server URL, identity name) — see [`crate::commands::intent::key_cache`]),
//! so repeat and offline verifies don't hit the network. A cached key is
//! subject to the same fingerprint gate and falls through to a fresh fetch
//! when it no longer matches (rotated keys).

use clap::Parser;

use atomic_canonical::did::did_matches_public_key;
use atomic_canonical::verify;
use atomic_identity::keypair::PublicKey;
use atomic_identity::IdentityStore;
use atomic_repository::Repository;

use crate::commands::client::{apex_server_url, build_apex_client, remote_err};
use crate::commands::intent::{bridge, key_cache};
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Verify an intent's attestation (hash + signature) against a public key.
#[derive(Parser, Debug)]
#[command(name = "verify")]
pub struct IntentVerify {
    /// Intent ID (e.g. "PIMO-1" or "1").
    pub id: String,

    /// Identity whose public key to verify against. Defaults to the current
    /// default identity. Pass this if the intent was attested by someone else.
    ///
    /// If the named identity does not exist in the local identity store and a
    /// build_apex_client server is configured, it is resolved remotely via
    /// `identities/resolve` and its public key is fetched from there.
    #[arg(long)]
    pub identity: Option<String>,

    /// Named server profile from `~/.atomic/config.toml`.
    ///
    /// Only used when `--identity` resolves remotely. Defaults to the active
    /// `[server]` / `default_server` configuration.
    #[arg(long)]
    pub server: Option<String>,
}

impl Command for IntentVerify {
    fn run(&self) -> CliResult<()> {
        // Remote resolution (identities/resolve) needs async. Build a
        // one-shot runtime rather than depending on the caller being inside
        // tokio, exactly like `identity register`.
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl IntentVerify {
    async fn execute(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let inputs = bridge::read_intent(&repo, &self.id)?;
        let node = match bridge::load_attestation(&repo, &self.id, &inputs)? {
            bridge::Attestation::None => {
                return Err(CliError::InvalidArgument {
                    message: format!(
                        "no attestation found for {}; run `atomic intent attest {}` first",
                        self.id, self.id
                    ),
                })
            }
            bridge::Attestation::Stale(_) => {
                return Err(CliError::InvalidArgument {
                    message: format!(
                        "the attestation for {} is stale (the intent changed since it was \
                         signed); re-run `atomic intent attest {}`",
                        self.id, self.id
                    ),
                })
            }
            bridge::Attestation::Fresh(node) => *node,
        };

        // Resolve the public key to verify against. Local store first; when
        // `--identity <name>` is given but not present locally, fall back to
        // the configured storage server.
        let store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {e}"))
        })?;

        let (public_key, source) = if let Some(name) = &self.identity {
            match store.load_by_name(name) {
                Ok(identity) => {
                    let did = identity.id.to_base32();
                    (
                        identity.public_key,
                        source_line(name, &did, "local identity store"),
                    )
                }
                Err(_) => {
                    let resolved = self.resolve_remote(name, &node).await?;
                    (resolved.key, resolved.source)
                }
            }
        } else {
            let identity = store
                .get_default()
                .map_err(|e| {
                    CliError::Internal(anyhow::anyhow!("Failed to load default identity: {e}"))
                })?
                .ok_or_else(|| CliError::InvalidArgument {
                    message: "No default identity set. Create one first:\n  \
                              atomic identity new <name> --email <email> --set-default"
                        .to_string(),
                })?;
            (identity.public_key, "local identity store".to_string())
        };

        verify(&node, &public_key).map_err(|e| CliError::InvalidArgument {
            message: format!(
                "verification failed: {e} (if this intent was attested by a different \
                 identity, pass --identity <name>)"
            ),
        })?;

        println!("Verified intent: {}", self.id);
        println!(
            "  author: {}",
            node.attributed_to.as_deref().unwrap_or("(unknown)")
        );
        if let Some(h) = &node.content_hash {
            println!("  hash:   {h}");
        }
        println!("  key:    {} ({source})", public_key.to_base32());
        Ok(())
    }

    /// Resolve the identity's public key: local cache first, then the server.
    ///
    /// The attestation's `attributed_to` is a `did:atomic:` (or `did:key:`)
    /// fingerprint of the signing key. ANY key used for verification — cached
    /// or freshly fetched — must fingerprint-match the sidecar's signer; the
    /// cache is only a byte source, never a trust source.
    ///
    /// A cached key that fails the gate (or that has expired/rotated) falls
    /// through to a fresh server fetch; only a fresh server key that itself
    /// fails the gate is a hard error.
    async fn resolve_remote(
        &self,
        name: &str,
        node: &atomic_canonical::CanonicalNode,
    ) -> CliResult<ResolvedKey> {
        // The cache is namespaced by server, so we need the URL before the
        // lookup. `apex_server_url` is a pure local config read — the
        // offline-first property of the cache is preserved.
        let server_url = apex_server_url(self.server.as_deref())?;
        let mut cache = key_cache::PublicKeyCache::open();

        // 1) Local cache first — repeat/offline verifies skip the network.
        if let Some(cached) = cache.get(&server_url, name) {
            if let Ok(key) = PublicKey::from_base32(&cached.public_key) {
                match node.attributed_to.as_deref() {
                    // Cached key gate-matches the signer → use it.
                    Some(did) if did_matches_public_key(did, &key) => {
                        return Ok(ResolvedKey {
                            key,
                            source: cached.source_line(),
                        })
                    }
                    // No signer recorded → nothing to gate, trust the cache
                    // exactly like the server path trusts a fresh fetch.
                    None => {
                        return Ok(ResolvedKey {
                            key,
                            source: cached.source_line(),
                        })
                    }
                    // Gate miss: fall through and re-fetch — the signer may
                    // have rotated keys.
                    _ => {}
                }
            }
        }

        // 2) Fresh fetch from the configured storage server.
        let client = build_apex_client(self.server.as_deref()).await?;

        let info = client
            .resolve_identity_by_name(name)
            .await
            .map_err(remote_err)?;

        let encoded = info
            .public_key
            .as_deref()
            .ok_or_else(|| CliError::RemoteError {
                message: format!(
                    "server resolved identity '{name}' but did not return its public key; \
                 the server may not support public-key resolution yet — fetch the key \
                 manually with `atomic identity lookup-key <key>`"
                ),
                url: None,
            })?;

        let key = PublicKey::from_base32(encoded).map_err(|e| CliError::RemoteError {
            message: format!("server returned an invalid public key for '{name}': {e}"),
            url: None,
        })?;

        if let Some(did) = node.attributed_to.as_deref() {
            if !did_matches_public_key(did, &key) {
                return Err(CliError::InvalidArgument {
                    message: format!(
                        "the identity '{name}' resolved on the server does not match the \
                         attestation's signer ({did}): the returned public key \
                         ({}) is a different key. Refusing to verify.",
                        short_key(encoded)
                    ),
                });
            }
        }

        // 3) Cache the fresh, gate-passing key (best-effort).
        cache.put(&server_url, name, &key.to_base32(), &info.status);

        Ok(ResolvedKey {
            key,
            source: format!("fetched from {server_url} (identities/resolve?name={name})"),
        })
    }
}

/// A public key plus a human-readable note of where it was resolved from.
struct ResolvedKey {
    key: PublicKey,
    source: String,
}

/// A short display line describing where the key came from.
fn source_line(name: &str, did_short: &str, origin: &str) -> String {
    format!("{origin} (identity '{name}', id {did_short}...)")
}

/// Truncate a base32 key for display.
fn short_key(key: &str) -> String {
    if key.len() > 16 {
        format!("{}...", &key[..16])
    } else {
        key.to_string()
    }
}
