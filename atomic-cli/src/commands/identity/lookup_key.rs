//! The `identity lookup-key` command.
//!
//! Resolves a base32-encoded Ed25519 public key against an atomic-storage
//! server and prints the registered identity — its username, canonical
//! public key, and `did:atomic:` identifier.
//!
//! This is the companion to the storage server's
//! `GET /identities/by-key/{public_key}` endpoint. Use it when you hold a
//! public key (e.g. the signer of an imported review intent or attestation)
//! but need to confirm whose key it is, or to re-verify an attestation that
//! currently reports `verifies: na` because the identity isn't available
//! locally.
//!
//! # Usage
//!
//! ```text
//! atomic identity lookup-key <PUBLIC_KEY> [OPTIONS]
//!
//! Arguments:
//!   <PUBLIC_KEY>  Base32-encoded Ed25519 public key (52 chars, A-Z2-7, no padding)
//!
//! Options:
//!       --server <NAME>  Named server profile from ~/.atomic/config.toml
//!   -f, --format <FMT>   Output format (default, json) [default: default]
//! ```
//!
//! # Examples
//!
//! ```text
//! # Resolve a key against the configured server
//! $ atomic identity lookup-key U4NNGFTE7KZZKRML6TQDBJJMOMKDCEHZO64IKHGACVRYGZOQSH5Q
//!
//! # JSON output for scripting
//! $ atomic identity lookup-key U4NNG... --format json
//! ```

use clap::Parser;
use data_encoding::BASE32_NOPAD;

use atomic_identity::PublicKey;
use atomic_remote::RemoteError;

use crate::commands::client::build_apex_client;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_section};

/// Prefix of `did:atomic:` identifiers.
///
/// Note: a `did:atomic:` value is a *fingerprint* of the key
/// (base32 of `blake3(pubkey)`), not the key itself, so it cannot be
/// reverse-fed into this command.
const DID_ATOMIC_PREFIX: &str = "did:atomic:";

/// Resolve a public key to the identity registered for it on a storage server.
///
/// Given a base32-encoded Ed25519 public key, asks the configured
/// atomic-storage server which identity it belongs to and prints the
/// canonical key, username, and `did:atomic:` identifier.
#[derive(Debug, Parser)]
pub struct LookupKey {
    /// Base32-encoded Ed25519 public key (52 chars, A-Z2-7, no padding).
    ///
    /// This is the same value used as the JWT `kid`, e.g. from
    /// `atomic identity show <name> --show-public-key`.
    #[arg(required = true)]
    pub public_key: String,

    /// Named server profile from `~/.atomic/config.toml`.
    ///
    /// Defaults to the active `[server]` / `default_server` configuration.
    #[arg(long)]
    pub server: Option<String>,

    /// Output format.
    ///
    /// - default: Human-readable output
    /// - json: JSON output for scripting
    #[arg(short, long, default_value = "default")]
    pub format: String,
}

impl Command for LookupKey {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl LookupKey {
    async fn execute(&self) -> CliResult<()> {
        let canonical = self.normalize_and_validate()?;

        let client = build_apex_client(self.server.as_deref()).await?;

        let info = client
            .resolve_identity_by_public_key(&canonical)
            .await
            .map_err(|e| self.map_remote_error(e, &canonical))?;

        let expected_did = did_atomic(&canonical);

        match self.format.to_lowercase().as_str() {
            "json" => self.output_json(&info, &canonical, &expected_did),
            _ => self.output_default(&info, &canonical, &expected_did),
        }

        Ok(())
    }

    /// Normalize user input (case, optional prefixes) and validate it as a
    /// base32-encoded Ed25519 public key.
    fn normalize_and_validate(&self) -> CliResult<String> {
        let input = self.public_key.trim();

        if input.starts_with(DID_ATOMIC_PREFIX) {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "'{input}' is a did:atomic: identifier. A did:atomic: value is a \
                     fingerprint of the public key (base32 of blake3(pubkey)), not the \
                     key itself, so it cannot be used for lookup. Provide the raw \
                     base32 public key instead (52 chars, A-Z2-7)."
                ),
            });
        }

        let normalized = input.to_ascii_uppercase();

        let key = PublicKey::from_base32(&normalized).map_err(|e| CliError::InvalidArgument {
            message: format!("Invalid public key: {e}"),
        })?;

        Ok(key.to_base32())
    }

    /// Map a remote error, giving a clear message for the common 404 case.
    fn map_remote_error(&self, e: RemoteError, canonical: &str) -> CliError {
        match &e {
            RemoteError::HttpError { status: 404, .. } => CliError::RemoteError {
                message: format!(
                    "No identity registered for public key {} on that server",
                    short_key(canonical)
                ),
                url: None,
            },
            _ => crate::commands::client::remote_err(e),
        }
    }

    /// Print human-readable output.
    fn output_default(
        &self,
        info: &atomic_remote::IdentityInfo,
        canonical: &str,
        expected_did: &str,
    ) {
        print_section(&format!("Identity: {}", info.name));
        println!();

        println!("  Server ID:     {}", info.id);
        println!("  Status:        {}", info.status);
        println!(
            "  Created:       {}",
            info.created_at.format("%Y-%m-%d %H:%M:%S UTC")
        );
        println!();
        println!("  Public Key:    {}", canonical);
        println!("  did:atomic:    {}", expected_did);
        println!();

        if let Some(server_key) = &info.public_key {
            if server_key != canonical {
                print_hint(&format!(
                    "Server returned a different canonical key ({}...) — verify you typed \
                     the key correctly.",
                    short_key(server_key)
                ));
            }
        } else {
            print_hint("Server did not return a public key; it may not support by-key lookup yet.");
        }
    }

    /// Print JSON output.
    fn output_json(&self, info: &atomic_remote::IdentityInfo, canonical: &str, expected_did: &str) {
        let obj = serde_json::json!({
            "name": info.name,
            "identity_id": info.id,
            "public_key": canonical,
            "status": info.status,
            "created_at": info.created_at.to_rfc3339(),
            "did_atomic": expected_did,
            "server_public_key": info.public_key,
        });
        println!("{}", serde_json::to_string_pretty(&obj).unwrap());
    }
}

/// Compute the `did:atomic:` identifier for a canonical base32 public key.
///
/// Matches `atomic-canonical::did_for_public_key`: the fingerprint is
/// base32 of `blake3(pubkey_bytes)`.
fn did_atomic(canonical: &str) -> String {
    let bytes: Vec<u8> = BASE32_NOPAD
        .decode(canonical.as_bytes())
        .expect("validated above");
    let hash = blake3::hash(&bytes);
    format!(
        "{DID_ATOMIC_PREFIX}{}",
        BASE32_NOPAD.encode(hash.as_bytes())
    )
}

/// Truncate a base32 key for display (matches the `identity list` style).
fn short_key(key: &str) -> String {
    if key.len() > 16 {
        format!("{}...", &key[..16])
    } else {
        key.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "U4NNGFTE7KZZKRML6TQDBJJMOMKDCEHZO64IKHGACVRYGZOQSH5Q";

    fn cmd(key: &str) -> LookupKey {
        LookupKey {
            public_key: key.to_string(),
            server: None,
            format: "default".to_string(),
        }
    }

    #[test]
    fn normalize_accepts_valid_key() {
        assert_eq!(cmd(KEY).normalize_and_validate().unwrap(), KEY);
    }

    #[test]
    fn normalize_accepts_lowercase() {
        assert_eq!(
            cmd(&KEY.to_lowercase()).normalize_and_validate().unwrap(),
            KEY
        );
    }

    #[test]
    fn normalize_rejects_did_prefix() {
        let err = cmd(&format!("did:atomic:{KEY}"))
            .normalize_and_validate()
            .unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument { .. }));
    }

    #[test]
    fn normalize_rejects_garbage() {
        let err = cmd("ZZZ").normalize_and_validate().unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument { .. }));
    }

    #[test]
    fn normalize_rejects_non_key_length() {
        let err = cmd(&KEY[..KEY.len() - 4])
            .normalize_and_validate()
            .unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument { .. }));
    }

    #[test]
    fn did_atomic_round_trips_known_key() {
        let did = did_atomic(KEY);
        assert!(did.starts_with("did:atomic:"));
        assert_eq!(did.len(), "did:atomic:".len() + 52);
    }
}
