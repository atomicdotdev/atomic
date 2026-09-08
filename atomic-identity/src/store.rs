//! Identity storage and management
//!
//! This module provides the `IdentityStore` for persisting and managing
//! multiple identities on disk. Identities are stored in a dedicated
//! directory with their metadata in TOML format and encrypted secret
//! keys in separate files.
//!
//! # Storage Layout
//!
//! ```text
//! ~/.atomic/identities/
//! ├── config.toml              # Store configuration (default identity, etc.)
//! ├── alice-personal/
//! │   ├── identity.toml        # Identity metadata
//! │   └── secret.key           # Encrypted secret key (optional)
//! ├── alice-work/
//! │   ├── identity.toml
//! │   └── secret.key
//! └── ci-bot/
//!     └── identity.toml        # Agent identity (may not have secret key)
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_identity::{Identity, IdentityStore, IdentityUsage};
//!
//! // Open or create the identity store
//! let store = IdentityStore::open_default()?;
//!
//! // Create and save a new identity
//! let identity = Identity::builder("alice")
//!     .email("alice@example.com")
//!     .usage(IdentityUsage::Personal)
//!     .build()?;
//!
//! store.save(&identity)?;
//!
//! // Set as default
//! store.set_default(&identity.id)?;
//!
//! // List all identities
//! for identity in store.list()? {
//!     println!("{}: {}", identity.name, identity.usage);
//! }
//! ```

use crate::identity::{Identity, IdentityId, IdentityMetadata};
use crate::keypair::{KeyPair, PublicKey, SecretKey};
use crate::usage::IdentityUsage;
use crate::IdentityError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Configuration for the identity store.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoreConfig {
    /// Default identity ID for general use.
    #[serde(default)]
    pub default_identity: Option<String>,

    /// Default identities by usage type.
    #[serde(default)]
    pub default_by_usage: HashMap<String, String>,

    /// Store format version.
    #[serde(default = "default_version")]
    pub version: u32,
}

fn default_version() -> u32 {
    1
}

impl StoreConfig {
    /// Create a new empty configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the default identity.
    pub fn set_default(&mut self, identity_id: &IdentityId) {
        self.default_identity = Some(identity_id.to_base32());
    }

    /// Set the default identity for a specific usage.
    pub fn set_default_for_usage(&mut self, usage: &IdentityUsage, identity_id: &IdentityId) {
        self.default_by_usage
            .insert(usage.to_string(), identity_id.to_base32());
    }

    /// Get the default identity ID.
    pub fn get_default(&self) -> Option<IdentityId> {
        self.default_identity
            .as_ref()
            .and_then(|s| IdentityId::from_base32(s).ok())
    }

    /// Get the default identity ID for a specific usage.
    pub fn get_default_for_usage(&self, usage: &IdentityUsage) -> Option<IdentityId> {
        self.default_by_usage
            .get(&usage.to_string())
            .and_then(|s| IdentityId::from_base32(s).ok())
    }
}

/// Stored identity data (without secret key).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredIdentity {
    /// Identity ID (base32 encoded).
    pub id: String,

    /// Display name.
    pub name: String,

    /// Email address.
    #[serde(default)]
    pub email: Option<String>,

    /// Public key (base32 encoded).
    pub public_key: String,

    /// Identity type.
    #[serde(default)]
    pub identity_type: String,

    /// Usage context.
    #[serde(default)]
    pub usage: String,

    /// Metadata.
    #[serde(default)]
    pub metadata: IdentityMetadata,

    /// Delegated by (base32 encoded identity ID).
    #[serde(default)]
    pub delegated_by: Option<String>,

    /// Whether a secret key file exists.
    #[serde(default)]
    pub has_secret_key: bool,
}

impl StoredIdentity {
    /// Convert from an Identity.
    fn from_identity(identity: &Identity, has_secret_key: bool) -> Self {
        Self {
            id: identity.id.to_base32(),
            name: identity.name.clone(),
            email: identity.email.clone(),
            public_key: identity.public_key.to_base32(),
            identity_type: format!("{:?}", identity.identity_type).to_lowercase(),
            usage: identity.usage.to_string(),
            metadata: identity.metadata.clone(),
            delegated_by: identity.delegated_by.map(|id| id.to_base32()),
            has_secret_key,
        }
    }

    /// Convert to an Identity.
    fn to_identity(&self) -> Result<Identity, IdentityError> {
        let public_key = PublicKey::from_base32(&self.public_key)?;
        let id = IdentityId::from_public_key(&public_key);

        let identity_type = match self.identity_type.as_str() {
            "user" => crate::identity::IdentityType::User,
            "agent" => crate::identity::IdentityType::Agent,
            "delegated" => crate::identity::IdentityType::Delegated,
            _ => crate::identity::IdentityType::User,
        };

        let usage = IdentityUsage::parse(&self.usage);

        let delegated_by = self
            .delegated_by
            .as_ref()
            .and_then(|s| IdentityId::from_base32(s).ok());

        Ok(Identity {
            id,
            name: self.name.clone(),
            email: self.email.clone(),
            public_key,
            identity_type,
            usage,
            metadata: self.metadata.clone(),
            delegated_by,
        })
    }
}

/// Encrypted secret key storage.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredSecretKey {
    /// Encrypted key data (base64 encoded).
    pub data: String,

    /// Encryption method.
    pub encryption: String,

    /// Salt for key derivation (if password protected).
    #[serde(default)]
    pub salt: Option<String>,

    /// Nonce for encryption.
    #[serde(default)]
    pub nonce: Option<String>,
}

/// Options for loading identities.
#[derive(Clone, Debug, Default)]
pub struct LoadOptions {
    /// Whether to load the secret key.
    pub load_secret: bool,

    /// Password for decrypting secret key.
    pub password: Option<String>,
}

impl LoadOptions {
    /// Create options to load only public data.
    pub fn public_only() -> Self {
        Self {
            load_secret: false,
            password: None,
        }
    }

    /// Create options to load with secret key (unencrypted).
    pub fn with_secret() -> Self {
        Self {
            load_secret: true,
            password: None,
        }
    }

    /// Create options to load with password-protected secret key.
    pub fn with_password(password: impl Into<String>) -> Self {
        Self {
            load_secret: true,
            password: Some(password.into()),
        }
    }
}

/// Filter for listing identities.
#[derive(Clone, Debug, Default)]
pub struct IdentityFilter {
    /// Filter by usage type.
    pub usage: Option<IdentityUsage>,

    /// Filter by identity type.
    pub identity_type: Option<crate::identity::IdentityType>,

    /// Filter by name pattern (substring match).
    pub name_pattern: Option<String>,

    /// Only include identities with secret keys.
    pub has_secret_key: Option<bool>,

    /// Only include valid (non-expired) identities.
    pub valid_only: bool,
}

impl IdentityFilter {
    /// Create an empty filter (matches all).
    pub fn all() -> Self {
        Self::default()
    }

    /// Filter by usage.
    pub fn usage(mut self, usage: IdentityUsage) -> Self {
        self.usage = Some(usage);
        self
    }

    /// Filter by identity type.
    pub fn identity_type(mut self, identity_type: crate::identity::IdentityType) -> Self {
        self.identity_type = Some(identity_type);
        self
    }

    /// Filter by name pattern.
    pub fn name_contains(mut self, pattern: impl Into<String>) -> Self {
        self.name_pattern = Some(pattern.into());
        self
    }

    /// Only include identities with secret keys.
    pub fn with_secret_key(mut self) -> Self {
        self.has_secret_key = Some(true);
        self
    }

    /// Only include valid identities.
    pub fn valid_only(mut self) -> Self {
        self.valid_only = true;
        self
    }

    /// Check if an identity matches this filter.
    fn matches(&self, identity: &Identity, has_secret: bool) -> bool {
        if let Some(ref usage) = self.usage {
            if &identity.usage != usage {
                return false;
            }
        }

        if let Some(ref identity_type) = self.identity_type {
            if &identity.identity_type != identity_type {
                return false;
            }
        }

        if let Some(ref pattern) = self.name_pattern {
            if !identity
                .name
                .to_lowercase()
                .contains(&pattern.to_lowercase())
            {
                return false;
            }
        }

        if let Some(require_secret) = self.has_secret_key {
            if has_secret != require_secret {
                return false;
            }
        }

        if self.valid_only && !identity.is_valid() {
            return false;
        }

        true
    }
}

/// Identity store for persisting and managing multiple identities.
pub struct IdentityStore {
    /// Root directory for the store.
    root: PathBuf,

    /// Store configuration.
    config: StoreConfig,
}

impl IdentityStore {
    /// Configuration file name.
    const CONFIG_FILE: &'static str = "config.toml";

    /// Identity file name within each identity directory.
    const IDENTITY_FILE: &'static str = "identity.toml";

    /// Secret key file name within each identity directory.
    const SECRET_KEY_FILE: &'static str = "secret.key";

    /// Directory holding signed delegation certificates, relative to the root.
    ///
    /// Certificates live inside the store rather than beside it so a store is
    /// one self-contained directory to back up, copy, or point a test at. The
    /// name cannot collide with an identity directory: those are BASE32_NOPAD
    /// (uppercase) renderings of a 32-byte id.
    const DELEGATIONS_DIR: &'static str = "delegations";

    /// Open or create the default identity store.
    ///
    /// The default location is `~/.atomic/identities/` in the user's home directory.
    pub fn open_default() -> Result<Self, IdentityError> {
        let root = Self::default_store_path()?;
        Self::open(&root)
    }

    /// Open or create an identity store at the specified path.
    pub fn open(root: &Path) -> Result<Self, IdentityError> {
        // Create the directory if it doesn't exist
        if !root.exists() {
            fs::create_dir_all(root)?;
        }

        // Load or create configuration
        let config_path = root.join(Self::CONFIG_FILE);
        let config = if config_path.exists() {
            let content = fs::read_to_string(&config_path)?;
            toml::from_str(&content)?
        } else {
            let config = StoreConfig::new();
            let content = toml::to_string_pretty(&config)?;
            fs::write(&config_path, content)?;
            config
        };

        Ok(Self {
            root: root.to_path_buf(),
            config,
        })
    }

    /// Get the default store path.
    ///
    /// The default location is `~/.atomic/identities/` in the user's home directory.
    fn default_store_path() -> Result<PathBuf, IdentityError> {
        let home_dir = dirs::home_dir().ok_or(IdentityError::Config(
            atomic_config::ConfigError::NoConfigDir,
        ))?;

        Ok(home_dir.join(".atomic").join("identities"))
    }

    /// Get the store's root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Save an identity to the store.
    pub fn save(&self, identity: &Identity) -> Result<(), IdentityError> {
        self.save_with_secret(identity, None)
    }

    /// Save an identity with its secret key.
    pub fn save_with_keypair(
        &self,
        identity: &Identity,
        keypair: &KeyPair,
        password: Option<&str>,
    ) -> Result<(), IdentityError> {
        self.save_with_secret(identity, Some((&keypair.secret, password)))
    }

    /// Save an identity with optional secret key.
    fn save_with_secret(
        &self,
        identity: &Identity,
        secret: Option<(&SecretKey, Option<&str>)>,
    ) -> Result<(), IdentityError> {
        // Create identity directory
        let identity_dir = self.identity_dir(identity);
        fs::create_dir_all(&identity_dir)?;

        // Save identity metadata
        let has_secret_key = secret.is_some();
        let stored = StoredIdentity::from_identity(identity, has_secret_key);
        let identity_path = identity_dir.join(Self::IDENTITY_FILE);
        let content = toml::to_string_pretty(&stored)?;
        fs::write(&identity_path, content)?;

        // Save secret key if provided
        if let Some((secret_key, password)) = secret {
            self.save_secret_key(&identity_dir, secret_key, password)?;
        }

        Ok(())
    }

    /// Save a secret key (with optional encryption).
    fn save_secret_key(
        &self,
        identity_dir: &Path,
        secret_key: &SecretKey,
        password: Option<&str>,
    ) -> Result<(), IdentityError> {
        let secret_path = identity_dir.join(Self::SECRET_KEY_FILE);

        if let Some(_password) = password {
            // TODO: Implement proper password-based encryption
            // For now, we'll store it with a marker that it should be encrypted
            let stored = StoredSecretKey {
                data: data_encoding::BASE64.encode(secret_key.as_bytes()),
                encryption: "none".to_string(), // TODO: Change to "argon2id+chacha20poly1305"
                salt: None,
                nonce: None,
            };
            let content = toml::to_string_pretty(&stored)?;
            fs::write(&secret_path, content)?;
        } else {
            // Store unencrypted (not recommended for production)
            let stored = StoredSecretKey {
                data: data_encoding::BASE64.encode(secret_key.as_bytes()),
                encryption: "none".to_string(),
                salt: None,
                nonce: None,
            };
            let content = toml::to_string_pretty(&stored)?;
            fs::write(&secret_path, content)?;
        }

        // Set restrictive permissions on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = fs::Permissions::from_mode(0o600);
            fs::set_permissions(&secret_path, permissions)?;
        }

        Ok(())
    }

    /// Load an identity by ID.
    pub fn load(&self, id: &IdentityId) -> Result<Identity, IdentityError> {
        self.load_with_options(id, &LoadOptions::public_only())
    }

    /// Load an identity with options.
    pub fn load_with_options(
        &self,
        id: &IdentityId,
        _options: &LoadOptions,
    ) -> Result<Identity, IdentityError> {
        let identity_dir = self.find_identity_dir(id)?;
        let identity_path = identity_dir.join(Self::IDENTITY_FILE);

        if !identity_path.exists() {
            return Err(IdentityError::NotFound {
                name: id.to_base32(),
            });
        }

        let content = fs::read_to_string(&identity_path)?;
        let stored: StoredIdentity = toml::from_str(&content)?;
        stored.to_identity()
    }

    /// Load an identity by name.
    pub fn load_by_name(&self, name: &str) -> Result<Identity, IdentityError> {
        // Try to find an identity directory with a matching name
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                let identity_path = path.join(Self::IDENTITY_FILE);
                if identity_path.exists() {
                    let content = fs::read_to_string(&identity_path)?;
                    let stored: StoredIdentity = toml::from_str(&content)?;

                    if stored.name == name {
                        return stored.to_identity();
                    }
                }
            }
        }

        Err(IdentityError::NotFound {
            name: name.to_string(),
        })
    }

    /// Load the secret key for an identity.
    pub fn load_secret_key(
        &self,
        id: &IdentityId,
        _password: Option<&str>,
    ) -> Result<SecretKey, IdentityError> {
        let identity_dir = self.find_identity_dir(id)?;
        let secret_path = identity_dir.join(Self::SECRET_KEY_FILE);

        if !secret_path.exists() {
            return Err(IdentityError::NotFound {
                name: format!("secret key for {}", id.short()),
            });
        }

        let content = fs::read_to_string(&secret_path)?;
        let stored: StoredSecretKey = toml::from_str(&content)?;

        if stored.encryption != "none" {
            // TODO: Implement decryption
            return Err(IdentityError::PasswordRequired);
        }

        let bytes = data_encoding::BASE64
            .decode(stored.data.as_bytes())
            .map_err(|_| IdentityError::InvalidKey("Invalid base64 in secret key".to_string()))?;

        if bytes.len() != 32 {
            return Err(IdentityError::InvalidKey(format!(
                "Invalid secret key length: expected 32, got {}",
                bytes.len()
            )));
        }

        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(SecretKey::from_bytes(&arr))
    }

    /// Load a keypair for an identity.
    pub fn load_keypair(
        &self,
        id: &IdentityId,
        password: Option<&str>,
    ) -> Result<KeyPair, IdentityError> {
        let identity = self.load(id)?;
        let secret = self.load_secret_key(id, password)?;

        // Verify the secret key matches the public key
        let derived_public = secret.public_key();
        if derived_public != identity.public_key {
            return Err(IdentityError::InvalidKey(
                "Secret key does not match public key".to_string(),
            ));
        }

        Ok(KeyPair::from_secret_key(secret))
    }

    /// Delete an identity from the store.
    pub fn delete(&mut self, id: &IdentityId) -> Result<(), IdentityError> {
        let identity_dir = self.find_identity_dir(id)?;

        // Remove from defaults if set
        if self.config.default_identity.as_ref() == Some(&id.to_base32()) {
            self.config.default_identity = None;
        }

        self.config
            .default_by_usage
            .retain(|_, v| v != &id.to_base32());

        // Save updated config
        self.save_config()?;

        // Remove the identity directory
        fs::remove_dir_all(identity_dir)?;

        Ok(())
    }

    /// List all identities in the store.
    pub fn list(&self) -> Result<Vec<Identity>, IdentityError> {
        self.list_filtered(&IdentityFilter::all())
    }

    /// List identities matching a filter.
    pub fn list_filtered(&self, filter: &IdentityFilter) -> Result<Vec<Identity>, IdentityError> {
        let mut identities = Vec::new();

        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                let identity_path = path.join(Self::IDENTITY_FILE);
                if identity_path.exists() {
                    let content = fs::read_to_string(&identity_path)?;
                    if let Ok(stored) = toml::from_str::<StoredIdentity>(&content) {
                        if let Ok(identity) = stored.to_identity() {
                            if filter.matches(&identity, stored.has_secret_key) {
                                identities.push(identity);
                            }
                        }
                    }
                }
            }
        }

        // Sort by name
        identities.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(identities)
    }

    /// Check if an identity exists in the store.
    pub fn exists(&self, id: &IdentityId) -> bool {
        self.find_identity_dir(id).is_ok()
    }

    /// Check if an identity with the given name exists.
    pub fn exists_by_name(&self, name: &str) -> bool {
        self.load_by_name(name).is_ok()
    }

    /// Get the default identity.
    pub fn get_default(&self) -> Result<Option<Identity>, IdentityError> {
        match self.config.get_default() {
            Some(id) => Ok(Some(self.load(&id)?)),
            None => Ok(None),
        }
    }

    /// Get the default identity for a specific usage.
    pub fn get_default_for_usage(
        &self,
        usage: &IdentityUsage,
    ) -> Result<Option<Identity>, IdentityError> {
        // First try usage-specific default
        if let Some(id) = self.config.get_default_for_usage(usage) {
            return Ok(Some(self.load(&id)?));
        }

        // Fall back to global default
        self.get_default()
    }

    /// Set the default identity.
    pub fn set_default(&mut self, id: &IdentityId) -> Result<(), IdentityError> {
        // Verify the identity exists
        let _ = self.load(id)?;

        self.config.set_default(id);
        self.save_config()
    }

    /// Set the default identity for a specific usage.
    pub fn set_default_for_usage(
        &mut self,
        usage: &IdentityUsage,
        id: &IdentityId,
    ) -> Result<(), IdentityError> {
        // Verify the identity exists
        let _ = self.load(id)?;

        self.config.set_default_for_usage(usage, id);
        self.save_config()
    }

    /// Clear the default identity.
    pub fn clear_default(&mut self) -> Result<(), IdentityError> {
        self.config.default_identity = None;
        self.save_config()
    }

    /// Get the number of identities in the store.
    pub fn count(&self) -> Result<usize, IdentityError> {
        Ok(self.list()?.len())
    }

    // -----------------------------------------------------------------------
    // Delegation certificates
    //
    // The store deals in *documents*, not typed delegations: what is persisted
    // is the exact signed JSON that `atomic-canonical` minted and that the
    // server holds. Re-serializing a parsed struct would risk producing
    // different bytes than the ones the proof covers, so the bytes are the
    // artifact and parsing is the caller's business.
    // -----------------------------------------------------------------------

    /// Directory holding delegation certificates.
    pub fn delegations_dir(&self) -> PathBuf {
        self.root.join(Self::DELEGATIONS_DIR)
    }

    /// Path of a certificate, by its base32 id.
    fn delegation_path(&self, id: &str) -> PathBuf {
        self.delegations_dir().join(format!("{id}.json"))
    }

    /// Path of a revocation, by the delegation's base32 id.
    fn revocation_path(&self, id: &str) -> PathBuf {
        self.delegations_dir().join(format!("{id}.revocation.json"))
    }

    /// Store a signed delegation certificate under its id.
    ///
    /// `document` is written verbatim — the bytes the proof covers.
    pub fn save_delegation(&self, id: &str, document: &str) -> Result<(), IdentityError> {
        let dir = self.delegations_dir();
        if !dir.exists() {
            fs::create_dir_all(&dir)?;
        }
        fs::write(self.delegation_path(id), document)?;
        Ok(())
    }

    /// Load a delegation certificate by base32 id.
    pub fn load_delegation(&self, id: &str) -> Result<String, IdentityError> {
        let path = self.delegation_path(id);
        if !path.exists() {
            return Err(IdentityError::DelegationNotFound { id: id.to_string() });
        }
        Ok(fs::read_to_string(path)?)
    }

    /// Is a certificate stored under this id?
    pub fn delegation_exists(&self, id: &str) -> bool {
        self.delegation_path(id).exists()
    }

    /// Every stored certificate, as `(base32 id, document)` pairs.
    ///
    /// Unreadable files are skipped rather than failing the whole listing: one
    /// corrupt certificate should not make `atomic identity agent list`
    /// unusable.
    pub fn list_delegations(&self) -> Result<Vec<(String, String)>, IdentityError> {
        let dir = self.delegations_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // Revocations sit beside certificates and share the id prefix.
            if !name.ends_with(".json") || name.ends_with(".revocation.json") {
                continue;
            }
            let id = name.trim_end_matches(".json").to_string();
            if let Ok(document) = fs::read_to_string(entry.path()) {
                out.push((id, document));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Delete a certificate and any revocation stored alongside it.
    pub fn delete_delegation(&self, id: &str) -> Result<(), IdentityError> {
        let path = self.delegation_path(id);
        if !path.exists() {
            return Err(IdentityError::DelegationNotFound { id: id.to_string() });
        }
        fs::remove_file(path)?;
        let revocation = self.revocation_path(id);
        if revocation.exists() {
            fs::remove_file(revocation)?;
        }
        Ok(())
    }

    /// Store a signed revocation for a delegation.
    pub fn save_revocation(&self, id: &str, document: &str) -> Result<(), IdentityError> {
        let dir = self.delegations_dir();
        if !dir.exists() {
            fs::create_dir_all(&dir)?;
        }
        fs::write(self.revocation_path(id), document)?;
        Ok(())
    }

    /// The stored revocation for a delegation, if one exists.
    pub fn load_revocation(&self, id: &str) -> Result<Option<String>, IdentityError> {
        let path = self.revocation_path(id);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(fs::read_to_string(path)?))
    }

    /// Is a revocation recorded locally for this delegation?
    ///
    /// A local revocation is authoritative for refusing to *use* a delegation,
    /// but never for believing one is still good: the server is the authority
    /// on revocations issued elsewhere.
    pub fn is_revoked_locally(&self, id: &str) -> bool {
        self.revocation_path(id).exists()
    }

    /// Get the directory for an identity.
    fn identity_dir(&self, identity: &Identity) -> PathBuf {
        // Use a sanitized name + short ID for the directory name
        let safe_name = identity
            .name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect::<String>();

        self.root
            .join(format!("{}-{}", safe_name, identity.id.short()))
    }

    /// Find the directory for an identity by ID.
    fn find_identity_dir(&self, id: &IdentityId) -> Result<PathBuf, IdentityError> {
        let id_suffix = id.short();

        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.ends_with(&id_suffix) {
                        return Ok(path);
                    }
                }
            }
        }

        Err(IdentityError::NotFound {
            name: id.to_base32(),
        })
    }

    /// Save the store configuration.
    fn save_config(&self) -> Result<(), IdentityError> {
        let config_path = self.root.join(Self::CONFIG_FILE);
        let content = toml::to_string_pretty(&self.config)?;
        fs::write(&config_path, content)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_store() -> (TempDir, IdentityStore) {
        let temp_dir = TempDir::new().unwrap();
        let store = IdentityStore::open(temp_dir.path()).unwrap();
        (temp_dir, store)
    }

    #[test]
    fn test_store_open_creates_directory() {
        let temp_dir = TempDir::new().unwrap();
        let store_path = temp_dir.path().join("identities");

        assert!(!store_path.exists());
        let _store = IdentityStore::open(&store_path).unwrap();
        assert!(store_path.exists());
    }

    #[test]
    fn test_store_save_and_load() {
        let (_temp, store) = create_test_store();

        let identity = Identity::generate("alice");
        store.save(&identity).unwrap();

        let loaded = store.load(&identity.id).unwrap();
        assert_eq!(loaded.name, "alice");
        assert_eq!(loaded.id, identity.id);
    }

    #[test]
    fn test_store_save_with_keypair() {
        let (_temp, store) = create_test_store();

        let keypair = KeyPair::generate();
        let identity = Identity::new("bob", &keypair);

        store.save_with_keypair(&identity, &keypair, None).unwrap();

        let loaded_keypair = store.load_keypair(&identity.id, None).unwrap();
        assert_eq!(loaded_keypair.public, keypair.public);
    }

    #[test]
    fn test_store_load_by_name() {
        let (_temp, store) = create_test_store();

        let identity = Identity::generate("charlie");
        store.save(&identity).unwrap();

        let loaded = store.load_by_name("charlie").unwrap();
        assert_eq!(loaded.id, identity.id);
    }

    #[test]
    fn test_store_list() {
        let (_temp, store) = create_test_store();

        let alice = Identity::generate("alice");
        let bob = Identity::generate("bob");

        store.save(&alice).unwrap();
        store.save(&bob).unwrap();

        let identities = store.list().unwrap();
        assert_eq!(identities.len(), 2);

        let names: Vec<_> = identities.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"alice"));
        assert!(names.contains(&"bob"));
    }

    #[test]
    fn test_store_list_filtered() {
        let (_temp, store) = create_test_store();

        let personal = Identity::builder("alice-personal")
            .usage(IdentityUsage::Personal)
            .build()
            .unwrap();
        let work = Identity::builder("alice-work")
            .usage(IdentityUsage::Work)
            .build()
            .unwrap();

        store.save(&personal).unwrap();
        store.save(&work).unwrap();

        let work_identities = store
            .list_filtered(&IdentityFilter::all().usage(IdentityUsage::Work))
            .unwrap();

        assert_eq!(work_identities.len(), 1);
        assert_eq!(work_identities[0].name, "alice-work");
    }

    #[test]
    fn test_store_delete() {
        let (_temp, mut store) = create_test_store();

        let identity = Identity::generate("to-delete");
        store.save(&identity).unwrap();
        assert!(store.exists(&identity.id));

        store.delete(&identity.id).unwrap();
        assert!(!store.exists(&identity.id));
    }

    #[test]
    fn test_store_default_identity() {
        let (_temp, mut store) = create_test_store();

        let identity = Identity::generate("default-user");
        store.save(&identity).unwrap();

        assert!(store.get_default().unwrap().is_none());

        store.set_default(&identity.id).unwrap();

        let default = store.get_default().unwrap().unwrap();
        assert_eq!(default.id, identity.id);
    }

    #[test]
    fn test_store_default_by_usage() {
        let (_temp, mut store) = create_test_store();

        let personal = Identity::builder("alice-personal")
            .usage(IdentityUsage::Personal)
            .build()
            .unwrap();
        let work = Identity::builder("alice-work")
            .usage(IdentityUsage::Work)
            .build()
            .unwrap();

        store.save(&personal).unwrap();
        store.save(&work).unwrap();

        store
            .set_default_for_usage(&IdentityUsage::Work, &work.id)
            .unwrap();

        let work_default = store
            .get_default_for_usage(&IdentityUsage::Work)
            .unwrap()
            .unwrap();
        assert_eq!(work_default.id, work.id);

        // Personal should fall back to None (no global default set)
        assert!(store
            .get_default_for_usage(&IdentityUsage::Personal)
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_store_exists() {
        let (_temp, store) = create_test_store();

        let identity = Identity::generate("exists-test");
        assert!(!store.exists(&identity.id));

        store.save(&identity).unwrap();
        assert!(store.exists(&identity.id));
    }
}

#[cfg(test)]
mod delegation_store_tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, IdentityStore) {
        let dir = TempDir::new().unwrap();
        let store = IdentityStore::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn save_and_load_round_trips_bytes_verbatim() {
        let (_dir, store) = store();
        // Deliberately odd whitespace: the proof covers these exact bytes, so
        // the store must not normalize them.
        let doc = "{\n  \"@type\" : \"AgentDelegation\"\n}";
        store.save_delegation("ABC123", doc).unwrap();

        assert!(store.delegation_exists("ABC123"));
        assert_eq!(store.load_delegation("ABC123").unwrap(), doc);
    }

    #[test]
    fn load_missing_delegation_is_not_found() {
        let (_dir, store) = store();
        match store.load_delegation("NOPE") {
            Err(IdentityError::DelegationNotFound { id }) => assert_eq!(id, "NOPE"),
            other => panic!("expected DelegationNotFound, got {other:?}"),
        }
    }

    #[test]
    fn list_skips_revocations_and_sorts() {
        let (_dir, store) = store();
        store.save_delegation("BBB", "{}").unwrap();
        store.save_delegation("AAA", "{}").unwrap();
        store.save_revocation("AAA", "{}").unwrap();

        let listed: Vec<String> = store
            .list_delegations()
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(listed, vec!["AAA".to_string(), "BBB".to_string()]);
    }

    #[test]
    fn list_on_a_store_with_no_delegations_is_empty_not_an_error() {
        let (_dir, store) = store();
        assert!(store.list_delegations().unwrap().is_empty());
    }

    #[test]
    fn revocation_is_recorded_and_readable() {
        let (_dir, store) = store();
        store.save_delegation("AAA", "{}").unwrap();
        assert!(!store.is_revoked_locally("AAA"));
        assert_eq!(store.load_revocation("AAA").unwrap(), None);

        store.save_revocation("AAA", r#"{"revoked":true}"#).unwrap();
        assert!(store.is_revoked_locally("AAA"));
        assert_eq!(
            store.load_revocation("AAA").unwrap().as_deref(),
            Some(r#"{"revoked":true}"#)
        );
    }

    #[test]
    fn delete_removes_the_revocation_too() {
        let (_dir, store) = store();
        store.save_delegation("AAA", "{}").unwrap();
        store.save_revocation("AAA", "{}").unwrap();

        store.delete_delegation("AAA").unwrap();
        assert!(!store.delegation_exists("AAA"));
        assert!(!store.is_revoked_locally("AAA"));
        assert!(store.delete_delegation("AAA").is_err());
    }
}
