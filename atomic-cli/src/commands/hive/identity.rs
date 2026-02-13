//! Local Hive identity storage.
//!
//! Manages the agent's Hive identity as a JSON file stored at
//! `~/.config/atomic/hive-identity.json`. The identity contains the
//! agent's UUID, name, slug, Ed25519 keypair, vendor/model info,
//! and claim status.
//!
//! # Security
//!
//! The secret key is stored in the identity file. In production,
//! consider encrypting the file or using OS keychain integration.
//! The file is created with restricted permissions (0600 on Unix).

use atomic_core::types::Merkle;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::error::{CliError, CliResult};

// =============================================================================
// Identity Type
// =============================================================================

/// A Hive agent identity stored locally.
///
/// Contains everything needed to authenticate with the Hive API
/// and prove ownership of the agent's Ed25519 keypair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HiveIdentity {
    /// Agent UUID from Hive API.
    pub id: String,

    /// Agent display name.
    pub name: String,

    /// URL-safe slug (unique on Hive).
    pub slug: String,

    /// Ed25519 public key (base32 encoded).
    pub public_key: String,

    /// Ed25519 secret key (base32 encoded).
    /// This is sensitive — keep secure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_key: Option<String>,

    /// AI vendor (anthropic, openai, google, etc.).
    pub vendor: String,

    /// AI model identifier (e.g. claude-sonnet-4).
    pub model: String,

    /// Model version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_version: Option<String>,

    /// Agent description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Whether the agent has been claimed by a human.
    #[serde(default)]
    pub is_claimed: bool,

    /// Unix timestamp (seconds) when the agent was registered.
    pub registered_at: i64,

    /// Unix timestamp (seconds) when the agent was claimed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<i64>,

    /// Claim URL for human verification.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim_url: Option<String>,

    /// Human-readable claim code (e.g. "HIVE-AB12").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim_code: Option<String>,
}

// =============================================================================
// Identity Store
// =============================================================================

/// Manages reading/writing the Hive identity file.
///
/// The default location is `~/.config/atomic/hive-identity.json`.
pub struct HiveIdentityStore {
    path: PathBuf,
}

impl HiveIdentityStore {
    /// Open the identity store at the default location.
    ///
    /// Creates the parent directory (`~/.config/atomic/`) if it doesn't exist.
    pub fn open() -> CliResult<Self> {
        let path = default_identity_path()?;

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).map_err(|e| {
                    CliError::Internal(anyhow::anyhow!(
                        "Failed to create config directory {}: {}",
                        parent.display(),
                        e
                    ))
                })?;
            }
        }

        Ok(Self { path })
    }

    /// Open the identity store at a custom path (for testing).
    #[allow(dead_code)]
    pub fn open_at(path: PathBuf) -> CliResult<Self> {
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).map_err(|e| {
                    CliError::Internal(anyhow::anyhow!(
                        "Failed to create directory {}: {}",
                        parent.display(),
                        e
                    ))
                })?;
            }
        }

        Ok(Self { path })
    }

    /// Load the identity from disk, if it exists.
    ///
    /// Returns `Ok(None)` if the file doesn't exist.
    /// Returns `Err` if the file exists but can't be parsed.
    pub fn load(&self) -> CliResult<Option<HiveIdentity>> {
        if !self.path.exists() {
            return Ok(None);
        }

        let contents = fs::read_to_string(&self.path).map_err(|e| {
            CliError::Internal(anyhow::anyhow!(
                "Failed to read identity file {}: {}",
                self.path.display(),
                e
            ))
        })?;

        let identity: HiveIdentity = serde_json::from_str(&contents).map_err(|e| {
            CliError::Internal(anyhow::anyhow!(
                "Failed to parse identity file {}: {}",
                self.path.display(),
                e
            ))
        })?;

        Ok(Some(identity))
    }

    /// Save the identity to disk.
    ///
    /// Writes the file with pretty-printed JSON and restricted permissions.
    pub fn save(&self, identity: &HiveIdentity) -> CliResult<()> {
        let contents = serde_json::to_string_pretty(identity).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to serialize identity: {}", e))
        })?;

        fs::write(&self.path, &contents).map_err(|e| {
            CliError::Internal(anyhow::anyhow!(
                "Failed to write identity file {}: {}",
                self.path.display(),
                e
            ))
        })?;

        // Set restrictive permissions on Unix (0600 = owner read/write only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(0o600);
            let _ = fs::set_permissions(&self.path, perms);
        }

        Ok(())
    }

    /// Clear the identity file (delete it).
    pub fn clear(&self) -> CliResult<()> {
        if self.path.exists() {
            fs::remove_file(&self.path).map_err(|e| {
                CliError::Internal(anyhow::anyhow!(
                    "Failed to delete identity file {}: {}",
                    self.path.display(),
                    e
                ))
            })?;
        }
        Ok(())
    }

    /// Check if an identity file exists.
    #[allow(dead_code)]
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Get the path to the identity file.
    #[allow(dead_code)]
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

// =============================================================================
// Keypair Generation
// =============================================================================

/// An Ed25519 keypair encoded as base32 strings.
pub struct HiveKeyPair {
    /// Public key (base32 encoded, ~52 characters).
    pub public_key: String,
    /// Secret key (base32 encoded, ~52 characters).
    pub secret_key: String,
}

/// Generate a new Ed25519 keypair for Hive agent registration.
///
/// Uses `atomic-identity`'s Ed25519 implementation (ed25519-dalek)
/// with OS-provided randomness. The secret key is encoded as base32
/// manually since `SecretKey` intentionally doesn't expose `to_base32`
/// (to discourage accidental leakage).
pub fn generate_keypair() -> CliResult<HiveKeyPair> {
    use atomic_identity::KeyPair;

    let keypair = KeyPair::generate();
    let public_key = keypair.public.to_base32();
    let secret_key = data_encoding::BASE32_NOPAD.encode(keypair.secret.as_bytes());

    Ok(HiveKeyPair {
        public_key,
        secret_key,
    })
}

/// Sign a message with the agent's secret key.
///
/// Decodes the base32 secret key, reconstructs the `KeyPair`,
/// signs the message, and returns the signature as base64.
pub fn sign_message(secret_key_base32: &str, message: &[u8]) -> CliResult<String> {
    use atomic_identity::{KeyPair, SecretKey};

    let secret_bytes = data_encoding::BASE32_NOPAD
        .decode(secret_key_base32.as_bytes())
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Invalid base32 secret key: {}", e)))?;

    if secret_bytes.len() != 32 {
        return Err(CliError::Internal(anyhow::anyhow!(
            "Secret key must be 32 bytes, got {}",
            secret_bytes.len()
        )));
    }

    let mut key_bytes = [0u8; 32];
    key_bytes.copy_from_slice(&secret_bytes);

    let secret = SecretKey::from_bytes(&key_bytes);
    let keypair = KeyPair::from_secret_key(secret);
    let signature = keypair.sign(message);

    Ok(data_encoding::BASE64.encode(&signature))
}

/// Create the registration signature message.
///
/// The message format matches the Hive API expectation:
/// `"REGISTER:{public_key}:{timestamp}"`
pub fn create_registration_message(public_key: &str, timestamp: i64) -> String {
    format!("REGISTER:{}:{}", public_key, timestamp)
}

/// Create the authentication signature message for API requests.
///
/// The message format matches the Hive API expectation:
/// `"{method}:{path}:{timestamp}:{body_hash}"`
#[allow(dead_code)]
pub fn create_auth_message(method: &str, path: &str, timestamp: i64, body: Option<&str>) -> String {
    let body_hash = match body {
        Some(b) => hash_hex(b.as_bytes()),
        None => hash_hex(b""),
    };
    format!("{}:{}:{}:{}", method, path, timestamp, body_hash)
}

/// Compute a cryptographic hex digest of data.
///
/// Uses Blake3 via `atomic_core::types::Merkle` (the same hash used
/// throughout Atomic for content-addressing).
fn hash_hex(data: &[u8]) -> String {
    use std::fmt::Write;

    let merkle = Merkle::of(data);
    let bytes = merkle.as_bytes();
    let mut hex = String::with_capacity(64);
    for byte in bytes {
        write!(hex, "{:02x}", byte).unwrap();
    }
    hex
}

// =============================================================================
// Path Helpers
// =============================================================================

/// Get the default identity file path.
///
/// Returns `~/.config/atomic/hive-identity.json` on all platforms,
/// using the OS-appropriate config directory.
fn default_identity_path() -> CliResult<PathBuf> {
    // Use ~/.config/atomic/ (Unix convention) rather than dirs::config_dir()
    // which gives ~/Library/Application Support on macOS
    let home = dirs::home_dir().ok_or_else(|| {
        CliError::Internal(anyhow::anyhow!(
            "Could not determine home directory. Set $HOME."
        ))
    })?;

    Ok(home
        .join(".config")
        .join("atomic")
        .join("hive-identity.json"))
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_identity_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hive-identity.json");
        let store = HiveIdentityStore::open_at(path).unwrap();

        let identity = HiveIdentity {
            id: "test-uuid-1234".to_string(),
            name: "Test Agent".to_string(),
            slug: "test-agent".to_string(),
            public_key: "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567ABCDEFGHIJKLMNOPQRST".to_string(),
            secret_key: Some("SECRETKEYABCDEFGHIJKLMNOPQRSTUVWXYZ234567ABCDEFGHIJKL".to_string()),
            vendor: "anthropic".to_string(),
            model: "claude-sonnet-4".to_string(),
            model_version: Some("20250514".to_string()),
            description: Some("A test agent".to_string()),
            is_claimed: false,
            registered_at: 1719500000,
            claimed_at: None,
            claim_url: Some("https://hive.atomic.dev/claim/abc123".to_string()),
            claim_code: Some("HIVE-AB12".to_string()),
        };

        // Save
        store.save(&identity).unwrap();

        // Load
        let loaded = store.load().unwrap().expect("should exist");
        assert_eq!(loaded.id, identity.id);
        assert_eq!(loaded.name, identity.name);
        assert_eq!(loaded.slug, identity.slug);
        assert_eq!(loaded.public_key, identity.public_key);
        assert_eq!(loaded.secret_key, identity.secret_key);
        assert_eq!(loaded.vendor, identity.vendor);
        assert_eq!(loaded.model, identity.model);
        assert_eq!(loaded.is_claimed, false);
        assert_eq!(loaded.claim_url, identity.claim_url);
        assert_eq!(loaded.claim_code, identity.claim_code);
    }

    #[test]
    fn test_load_nonexistent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let store = HiveIdentityStore::open_at(path).unwrap();

        let result = store.load().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_clear_identity() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hive-identity.json");
        let store = HiveIdentityStore::open_at(path).unwrap();

        let identity = HiveIdentity {
            id: "test".to_string(),
            name: "Test".to_string(),
            slug: "test".to_string(),
            public_key: "KEY".to_string(),
            secret_key: None,
            vendor: "anthropic".to_string(),
            model: "test".to_string(),
            model_version: None,
            description: None,
            is_claimed: false,
            registered_at: 0,
            claimed_at: None,
            claim_url: None,
            claim_code: None,
        };

        store.save(&identity).unwrap();
        assert!(store.exists());

        store.clear().unwrap();
        assert!(!store.exists());

        let loaded = store.load().unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn test_clear_nonexistent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nope.json");
        let store = HiveIdentityStore::open_at(path).unwrap();

        // Should not error
        store.clear().unwrap();
    }

    #[test]
    fn test_identity_serialization_skips_none() {
        let identity = HiveIdentity {
            id: "id".to_string(),
            name: "name".to_string(),
            slug: "slug".to_string(),
            public_key: "key".to_string(),
            secret_key: None,
            vendor: "anthropic".to_string(),
            model: "model".to_string(),
            model_version: None,
            description: None,
            is_claimed: false,
            registered_at: 0,
            claimed_at: None,
            claim_url: None,
            claim_code: None,
        };

        let json = serde_json::to_string(&identity).unwrap();
        // None fields with skip_serializing_if should be absent
        assert!(!json.contains("secret_key"));
        assert!(!json.contains("model_version"));
        assert!(!json.contains("description"));
        assert!(!json.contains("claimed_at"));
        assert!(!json.contains("claim_url"));
        assert!(!json.contains("claim_code"));
    }

    #[test]
    fn test_create_registration_message() {
        let msg = create_registration_message("MYPUBLICKEY", 1719500000);
        assert_eq!(msg, "REGISTER:MYPUBLICKEY:1719500000");
    }

    #[test]
    fn test_create_auth_message() {
        let msg = create_auth_message("GET", "/agents/me", 1719500000, None);
        assert!(msg.starts_with("GET:/agents/me:1719500000:"));
        assert_eq!(msg.split(':').count(), 4);
    }

    #[test]
    fn test_create_auth_message_with_body() {
        let msg1 = create_auth_message("POST", "/agents", 100, Some("{}"));
        let msg2 = create_auth_message("POST", "/agents", 100, Some("{}"));
        // Same input produces same hash
        assert_eq!(msg1, msg2);

        // Different body produces different hash
        let msg3 = create_auth_message("POST", "/agents", 100, Some("{\"x\":1}"));
        assert_ne!(msg1, msg3);
    }

    #[test]
    fn test_hash_hex_deterministic() {
        let h1 = hash_hex(b"hello");
        let h2 = hash_hex(b"hello");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // 32 bytes = 64 hex chars
    }

    #[test]
    fn test_hash_hex_different_inputs() {
        let h1 = hash_hex(b"hello");
        let h2 = hash_hex(b"world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_generate_keypair() {
        let kp = generate_keypair().unwrap();
        assert!(!kp.public_key.is_empty());
        assert!(!kp.secret_key.is_empty());
        // Base32 encoded 32-byte key should be ~52 chars
        assert!(kp.public_key.len() >= 40);
        assert!(kp.secret_key.len() >= 40);

        // Two keypairs should be different
        let kp2 = generate_keypair().unwrap();
        assert_ne!(kp.public_key, kp2.public_key);
        assert_ne!(kp.secret_key, kp2.secret_key);
    }

    #[test]
    fn test_default_identity_path() {
        // Should not panic, and should end with the expected filename
        let path = default_identity_path().unwrap();
        assert!(path.ends_with("atomic/hive-identity.json"));
    }
}
