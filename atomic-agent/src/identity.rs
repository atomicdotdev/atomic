//! Agent identity resolution for turn-level recording.
//!
//! This module derives agent identities from the user's default identity
//! using a `+tag` email format (like Gmail's plus-addressing). This ties
//! agent changes to the human who authorized them while making it clear
//! in the log, blame, and UI that the change was made by an agent.
//!
//! # Email Format
//!
//! Given a user identity `Lee Faus <lee@atomic.dev>` and a Claude Code
//! session with ID `60f5cbd2-aa23-40ee-9085-4375dd186ce7`:
//!
//! ```text
//! User:  Lee Faus <lee@atomic.dev>
//! Agent: claude+60f5 <lee@atomic.dev>
//!         │      │
//!         │      └── First 4 hex chars of session ID
//!         └── Agent name
//! ```
//!
//! The `+tag` is stripped by email servers, so replies still reach the user.
//! The short session suffix disambiguates multiple concurrent sessions.
//!
//! # Identity Resolution Order
//!
//! 1. A **delegated agent identity** (from [`AgentAuthorOptions::agent_identity`]
//!    or `ATOMIC_AGENT_IDENTITY`) — the agent has a keypair of its own, so the
//!    change is attributed to *its* public key and the human is recoverable
//!    through the delegation certificate.
//! 2. The user's default identity in `~/.atomic/identities/`, with a `+tag`
//!    author — attribution by naming convention, signed with the human's key.
//! 3. Neither: fall back to `{agent_display_name}` with no key at all.
//!
//! Only (1) is cryptographic. In (2) the author string says "an agent did
//! this" and the key says "the human did this"; anyone who can read the
//! repository can see the difference, but nothing *proves* which agent, or
//! that the human authorized it. That is the gap a delegated identity closes,
//! and why the author line looks the same either way: the visible format is
//! not what changed, the key behind it is.
//!
//! # Example
//!
//! ```rust
//! use atomic_agent::identity::{resolve_agent_author, AgentAuthorOptions};
//!
//! let options = AgentAuthorOptions {
//!     agent_name: "claude-code",
//!     agent_display_name: "Claude Code",
//!     session_id: "60f5cbd2-aa23-40ee-9085-4375dd186ce7",
//!     identity_dir: None, // use default ~/.atomic/identities/
//!     agent_identity: None, // or Some("alice+claude") to sign with the agent's own key
//! };
//!
//! let author = resolve_agent_author(&options);
//! // With user identity: author.name = "claude+60f5", author.email = Some("lee@atomic.dev")
//! // Without:            author.name = "Claude Code", author.email = None
//! ```

use std::path::{Path, PathBuf};

use atomic_core::change::Author;

// AgentAuthorOptions

/// Options for resolving an agent author identity.
#[derive(Debug, Clone)]
pub struct AgentAuthorOptions<'a> {
    /// Agent registry key (e.g., "claude-code", "gemini-cli").
    pub agent_name: &'a str,

    /// Human-readable agent name (e.g., "Claude Code").
    pub agent_display_name: &'a str,

    /// The session ID — first 4 hex chars are used as the `+tag` suffix.
    pub session_id: &'a str,

    /// Override for the identity store directory.
    ///
    /// If `None`, uses `~/.atomic/identities/`. Set this for testing.
    pub identity_dir: Option<PathBuf>,

    /// Name of a delegated agent identity to sign as.
    ///
    /// When set and resolvable, the change is attributed to the agent's own
    /// key instead of the human's. `None` falls back to `ATOMIC_AGENT_IDENTITY`
    /// and then to the plus-tag path, so an environment with no agent
    /// configured behaves exactly as it did before agent identities existed.
    pub agent_identity: Option<String>,
}

// Author Resolution

/// Resolve the agent author for a turn change.
///
/// Looks up the user's default identity and derives an agent author with
/// a `+tag` email. Falls back to a plain agent name if no user identity
/// is configured.
///
/// # Format
///
/// With user identity `Alice <alice@example.com>` and agent `claude-code`
/// with session `60f5cbd2-...`:
///
/// ```text
/// Author { name: "claude+60f5", email: Some("alice@example.com") }
/// ```
///
/// The `+tag` in the name (not the email) ensures:
/// - `atomic log` shows `claude+60f5` — clearly an agent, not a human
/// - `atomic blame` shows `claude+60f5` — per-line agent attribution
/// - The email links back to the human who authorized the agent
/// - The `60f5` suffix identifies which session produced the change
///
/// # Fallback
///
/// If no user identity is found:
///
/// ```text
/// Author { name: "Claude Code", email: None }
/// ```
pub fn resolve_agent_author(options: &AgentAuthorOptions<'_>) -> Author {
    // A delegated identity is the only path where the key in the change
    // header actually belongs to the agent, so it is tried first.
    if let Some(author) = delegated_agent_author(options) {
        return author;
    }

    // Otherwise: plus-tag the human's identity. Legible, not provable.
    match load_default_user_identity(options.identity_dir.as_deref()) {
        Some(user) => derive_agent_author(&user, options),
        None => fallback_agent_author(options),
    }
}

/// The delegation certificate currently authorizing this agent, as a URN.
///
/// Recorded on the change envelope so a reader months later can ask the server
/// whether that specific certificate was still good, rather than inferring
/// authority from an author string. Returns `None` when no agent identity is
/// configured or none of its certificates is currently in force — the plus-tag
/// path has no certificate to name.
pub fn active_delegation_urn(
    agent_identity: Option<&str>,
    identity_dir: Option<&Path>,
) -> Option<String> {
    let name = agent_identity
        .map(str::to_string)
        .or_else(|| std::env::var(AGENT_IDENTITY_ENV).ok())
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())?;

    let store = match identity_dir {
        Some(dir) => atomic_identity::IdentityStore::open(dir),
        None => atomic_identity::IdentityStore::open_default(),
    }
    .ok()?;

    let identity = store.load_by_name(&name).ok()?;
    atomic_canonical::delegation::active_for_delegate(&store, &identity)
        .map(|d| d.delegation.id.to_urn())
}

/// Environment variable naming the delegated agent identity to sign as.
///
/// Mirrors the CLI's `ATOMIC_AGENT_IDENTITY`, so a runner that sets it once
/// gets both authenticated pushes and correctly attributed changes.
pub const AGENT_IDENTITY_ENV: &str = "ATOMIC_AGENT_IDENTITY";

/// Build an author from a delegated agent identity, if one is configured and
/// resolvable.
///
/// Returns `None` — rather than failing — whenever the identity is missing or
/// unreadable. Recording a turn must not break because an agent identity was
/// mistyped; falling back to the plus-tag author keeps the work attributed to
/// *someone* and leaves a debug log explaining why it is not keyed.
fn delegated_agent_author(options: &AgentAuthorOptions<'_>) -> Option<Author> {
    let name = options
        .agent_identity
        .clone()
        .or_else(|| std::env::var(AGENT_IDENTITY_ENV).ok())
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())?;

    let store = match options.identity_dir.as_deref() {
        Some(dir) => atomic_identity::IdentityStore::open(dir),
        None => atomic_identity::IdentityStore::open_default(),
    }
    .ok()?;

    let identity = match store.load_by_name(&name) {
        Ok(identity) => identity,
        Err(e) => {
            log::debug!("Agent identity '{name}' not usable ({e}); falling back to plus-tag");
            return None;
        }
    };

    // A human identity here would silently sign agent work as the human with
    // no delegation behind it — worse than the plus-tag fallback, which at
    // least does not claim to be keyed to an agent.
    if !identity.identity_type.is_delegated() && !identity.identity_type.is_agent() {
        log::debug!("'{name}' is not an agent identity; falling back to plus-tag");
        return None;
    }

    let session_short = extract_session_short(options.session_id);
    let tag = format!(
        "{}+{}",
        normalize_agent_name(options.agent_name),
        session_short
    );

    Some(Author::with_identity(
        &tag,
        identity.email.clone(),
        identity.public_key_base32(),
    ))
}

/// Derive the agent author from the user's identity.
///
/// Constructs: `{agent_name}+{session_short} <{user_email}>`
///
/// If the user has no email, uses the agent name with the user's
/// public key reference.
fn derive_agent_author(user: &UserIdentityInfo, options: &AgentAuthorOptions<'_>) -> Author {
    let session_short = extract_session_short(options.session_id);
    let agent_tag = format!(
        "{}+{}",
        normalize_agent_name(options.agent_name),
        session_short
    );

    match &user.email {
        Some(email) => Author::with_identity(
            &agent_tag,
            Some(email.clone()),
            user.public_key_base32.clone(),
        ),
        None => Author::with_identity(&agent_tag, None::<String>, user.public_key_base32.clone()),
    }
}

/// Fallback author when no user identity is configured.
fn fallback_agent_author(options: &AgentAuthorOptions<'_>) -> Author {
    Author::new(options.agent_display_name, None::<String>)
}

// Session Short ID

/// Extract a short identifier from the session ID for the `+tag` suffix.
///
/// Takes the first 4 hex characters from the session ID. If the session ID
/// has a date prefix (e.g., `2026-01-15-abc123de-...`), skips the date part
/// and uses the first 4 chars of the UUID portion.
///
/// # Examples
///
/// ```rust
/// use atomic_agent::identity::extract_session_short;
///
/// assert_eq!(extract_session_short("60f5cbd2-aa23-40ee-9085-4375dd186ce7"), "60f5");
/// assert_eq!(extract_session_short("2026-01-15-abc123de-f456-7890"), "abc1");
/// assert_eq!(extract_session_short("short"), "shor");
/// assert_eq!(extract_session_short("ab"), "ab");
/// ```
pub fn extract_session_short(session_id: &str) -> String {
    // Check for date prefix pattern: YYYY-MM-DD-<rest>
    let id_part = if session_id.len() > 11
        && session_id.as_bytes()[4] == b'-'
        && session_id.as_bytes()[7] == b'-'
        && session_id.as_bytes()[10] == b'-'
    {
        // Skip the "YYYY-MM-DD-" prefix (11 chars)
        &session_id[11..]
    } else {
        session_id
    };

    // Take the first 4 alphanumeric characters
    let short: String = id_part
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(4)
        .collect();

    if short.is_empty() {
        "0000".to_string()
    } else {
        short
    }
}

/// Normalize the agent name for use in the `+tag`.
///
/// Removes hyphens and converts to lowercase for a clean tag format.
///
/// ```rust
/// use atomic_agent::identity::normalize_agent_name;
///
/// assert_eq!(normalize_agent_name("claude-code"), "claude");
/// assert_eq!(normalize_agent_name("gemini-cli"), "gemini");
/// assert_eq!(normalize_agent_name("codex"), "codex");
/// assert_eq!(normalize_agent_name("open-code"), "open");
/// ```
///
/// We take only the first segment (before the first hyphen) because:
/// - `claude+60f5` is shorter and cleaner than `claude-code+60f5`
/// - The agent is already identified by vendor in the Provenance
/// - Log output stays compact: `claude+60f5 <lee@atomic.dev>`
pub fn normalize_agent_name(agent_name: &str) -> String {
    agent_name
        .split('-')
        .next()
        .unwrap_or(agent_name)
        .to_lowercase()
}

// User Identity Loading

/// Minimal info extracted from the user's default identity.
///
/// We don't load the full `Identity` struct here to avoid coupling
/// tightly to the identity store's internal format. We just need
/// the name, email, and public key reference.
#[derive(Debug, Clone)]
struct UserIdentityInfo {
    #[allow(dead_code)]
    name: String,
    email: Option<String>,
    public_key_base32: String,
}

/// Load the user's default identity from the identity store.
///
/// Looks for `~/.atomic/identities/config.toml` to find the default
/// identity, then loads its `identity.toml` for name/email/public key.
///
/// Returns `None` if:
/// - The identity store directory doesn't exist
/// - No default identity is configured
/// - The default identity's files can't be read
fn load_default_user_identity(override_dir: Option<&Path>) -> Option<UserIdentityInfo> {
    let identities_dir = match override_dir {
        Some(dir) => dir.to_path_buf(),
        None => default_identities_dir()?,
    };

    if !identities_dir.is_dir() {
        return None;
    }

    // Read config.toml to find the default identity
    let config_path = identities_dir.join("config.toml");
    let config_content = std::fs::read_to_string(&config_path).ok()?;

    // Parse the config to find the default identity directory name
    let default_id = extract_default_identity_from_config(&config_content)?;

    // Try to find the identity directory
    // The store uses {name}-{usage}-{id_short} as directory names,
    // but we can also match by the ID directly
    load_identity_by_id(&identities_dir, &default_id)
        .or_else(|| load_identity_by_scanning(&identities_dir, &default_id))
}

/// Get the default identities directory: `~/.atomic/identities/`
fn default_identities_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".atomic").join("identities"))
}

/// Extract the default identity ID from config.toml content.
///
/// Looks for `default_identity = "BASE32_ID"` in the TOML.
fn extract_default_identity_from_config(content: &str) -> Option<String> {
    // Simple TOML parsing — look for default_identity key
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("default_identity") {
            // Extract the value between quotes
            if let Some(start) = trimmed.find('"') {
                if let Some(end) = trimmed.rfind('"') {
                    if end > start {
                        let value = &trimmed[start + 1..end];
                        if !value.is_empty() {
                            return Some(value.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

/// Load identity info from a directory matching the given ID.
fn load_identity_by_id(identities_dir: &Path, id_base32: &str) -> Option<UserIdentityInfo> {
    // The ID might be used directly as part of the directory name
    let entries = std::fs::read_dir(identities_dir).ok()?;

    for entry in entries.flatten() {
        let _dir_name = entry.file_name().to_string_lossy().to_string();

        // Skip non-directories and config files
        if !entry.path().is_dir() {
            continue;
        }

        // Check if this directory's identity.toml contains the matching ID
        let identity_path = entry.path().join("identity.toml");
        if let Ok(content) = std::fs::read_to_string(&identity_path) {
            if content.contains(id_base32) {
                return parse_identity_toml(&content);
            }
        }
    }
    None
}

/// Scan all identity directories and try to find one matching the ID.
fn load_identity_by_scanning(identities_dir: &Path, id_base32: &str) -> Option<UserIdentityInfo> {
    // Already tried by ID match; try loading the first User identity as fallback
    let entries = std::fs::read_dir(identities_dir).ok()?;

    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }

        let identity_path = entry.path().join("identity.toml");
        if let Ok(content) = std::fs::read_to_string(&identity_path) {
            // Check if it contains the right ID, or if it's a User type identity
            if content.contains(id_base32)
                || (content.contains("type = \"user\"")
                    || content.contains("identity_type = \"user\""))
            {
                return parse_identity_toml(&content);
            }
        }
    }
    None
}

/// Parse minimal identity info from an identity.toml file.
fn parse_identity_toml(content: &str) -> Option<UserIdentityInfo> {
    let mut name = None;
    let mut email = None;
    let mut public_key = None;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("name") && name.is_none() {
            name = extract_toml_string_value(trimmed);
        } else if trimmed.starts_with("email") && email.is_none() {
            email = extract_toml_string_value(trimmed);
        } else if trimmed.starts_with("public_key") && public_key.is_none() {
            public_key = extract_toml_string_value(trimmed);
        }
    }

    let name = name?;
    let public_key = public_key.unwrap_or_else(|| "unknown".to_string());

    Some(UserIdentityInfo {
        name,
        email,
        public_key_base32: public_key,
    })
}

/// Extract a string value from a TOML `key = "value"` line.
fn extract_toml_string_value(line: &str) -> Option<String> {
    let start = line.find('"')?;
    let rest = &line[start + 1..];
    let end = rest.find('"')?;
    let value = &rest[..end];
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

// Integration with record_turn

/// Build an `Author` for an agent turn, using the session's identity info.
///
/// This is the main entry point called by `record_turn()`. It resolves
/// the agent author from the user's default identity with `+tag` email.
///
/// # Arguments
///
/// * `agent_name` — Agent registry key (e.g., "claude-code")
/// * `agent_display_name` — Human-readable name (e.g., "Claude Code")
/// * `session_id` — Session identifier for the `+tag` suffix
///
/// # Returns
///
/// An `Author` suitable for `ChangeHeader`. Format depends on whether
/// a user identity is configured:
///
/// - With identity: `claude+60f5 <lee@atomic.dev>` (with public key ref)
/// - Without: `Claude Code` (no email)
pub fn build_agent_author(agent_name: &str, agent_display_name: &str, session_id: &str) -> Author {
    let options = AgentAuthorOptions {
        agent_name,
        agent_display_name,
        session_id,
        identity_dir: None,
        agent_identity: None,
    };
    resolve_agent_author(&options)
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // extract_session_short

    #[test]
    fn test_session_short_uuid() {
        assert_eq!(
            extract_session_short("60f5cbd2-aa23-40ee-9085-4375dd186ce7"),
            "60f5"
        );
    }

    #[test]
    fn test_session_short_with_date_prefix() {
        assert_eq!(
            extract_session_short("2026-01-15-abc123de-f456-7890"),
            "abc1"
        );
    }

    #[test]
    fn test_session_short_plain_string() {
        assert_eq!(extract_session_short("mysession"), "myse");
    }

    #[test]
    fn test_session_short_short_input() {
        assert_eq!(extract_session_short("ab"), "ab");
    }

    #[test]
    fn test_session_short_single_char() {
        assert_eq!(extract_session_short("x"), "x");
    }

    #[test]
    fn test_session_short_empty() {
        assert_eq!(extract_session_short(""), "0000");
    }

    #[test]
    fn test_session_short_with_hyphens() {
        // Hyphens are not alphanumeric, so they're filtered out
        assert_eq!(extract_session_short("a-b-c-d"), "abcd");
    }

    #[test]
    fn test_session_short_only_special_chars() {
        assert_eq!(extract_session_short("----"), "0000");
    }

    // normalize_agent_name

    #[test]
    fn test_normalize_claude_code() {
        assert_eq!(normalize_agent_name("claude-code"), "claude");
    }

    #[test]
    fn test_normalize_gemini_cli() {
        assert_eq!(normalize_agent_name("gemini-cli"), "gemini");
    }

    #[test]
    fn test_normalize_codex() {
        assert_eq!(normalize_agent_name("codex"), "codex");
    }

    #[test]
    fn test_normalize_open_code() {
        assert_eq!(normalize_agent_name("open-code"), "open");
    }

    #[test]
    fn test_normalize_uppercase() {
        assert_eq!(normalize_agent_name("Claude-Code"), "claude");
    }

    #[test]
    fn test_normalize_empty() {
        assert_eq!(normalize_agent_name(""), "");
    }

    // derive_agent_author

    #[test]
    fn test_derive_with_email() {
        let user = UserIdentityInfo {
            name: "Lee Faus".to_string(),
            email: Some("lee@atomic.dev".to_string()),
            public_key_base32: "ABCDEF1234567890".to_string(),
        };
        let options = AgentAuthorOptions {
            agent_name: "claude-code",
            agent_display_name: "Claude Code",
            session_id: "60f5cbd2-aa23-40ee-9085-4375dd186ce7",
            identity_dir: None,
            agent_identity: None,
        };

        let author = derive_agent_author(&user, &options);

        assert_eq!(author.name, "claude+60f5");
        assert_eq!(author.email, Some("lee@atomic.dev".to_string()));
        assert_eq!(author.identity, Some("ABCDEF1234567890".to_string()));
    }

    #[test]
    fn test_derive_without_email() {
        let user = UserIdentityInfo {
            name: "Anonymous".to_string(),
            email: None,
            public_key_base32: "KEYDATA".to_string(),
        };
        let options = AgentAuthorOptions {
            agent_name: "gemini-cli",
            agent_display_name: "Gemini CLI",
            session_id: "abcdef1234",
            identity_dir: None,
            agent_identity: None,
        };

        let author = derive_agent_author(&user, &options);

        assert_eq!(author.name, "gemini+abcd");
        assert!(author.email.is_none());
        assert_eq!(author.identity, Some("KEYDATA".to_string()));
    }

    #[test]
    fn test_derive_with_date_prefix_session() {
        let user = UserIdentityInfo {
            name: "Dev".to_string(),
            email: Some("dev@example.com".to_string()),
            public_key_base32: "KEY123".to_string(),
        };
        let options = AgentAuthorOptions {
            agent_name: "claude-code",
            agent_display_name: "Claude Code",
            session_id: "2026-01-15-abc123de-f456-7890",
            identity_dir: None,
            agent_identity: None,
        };

        let author = derive_agent_author(&user, &options);

        // Should skip the date prefix and use "abc1" from the UUID part
        assert_eq!(author.name, "claude+abc1");
        assert_eq!(author.email, Some("dev@example.com".to_string()));
    }

    // fallback_agent_author

    #[test]
    fn test_fallback_author() {
        let options = AgentAuthorOptions {
            agent_name: "claude-code",
            agent_display_name: "Claude Code",
            session_id: "sess-123",
            identity_dir: None,
            agent_identity: None,
        };

        let author = fallback_agent_author(&options);

        assert_eq!(author.name, "Claude Code");
        assert!(author.email.is_none());
        assert!(author.identity.is_none());
    }

    // Delegated agent identity (keyed attribution)

    /// The whole point of a delegated identity: the key in the change header
    /// is the agent's, not the human's. Same visible author, different key.
    #[test]
    fn a_delegated_identity_signs_with_its_own_key() {
        use atomic_identity::{Identity, IdentityStore, IdentityType, KeyPair};

        let dir = TempDir::new().unwrap();
        let store = IdentityStore::open(dir.path()).unwrap();

        let human_key = KeyPair::generate();
        let human = Identity::builder("alice")
            .email("alice@example.com")
            .public_key(human_key.public.clone())
            .build()
            .unwrap();
        store.save_with_keypair(&human, &human_key, None).unwrap();

        let agent_key = KeyPair::generate();
        let agent = Identity::builder("alice+claude")
            .identity_type(IdentityType::Agent)
            .email("alice+claude@example.com")
            .public_key(agent_key.public.clone())
            .delegated_by(human.id)
            .build()
            .unwrap();
        store.save_with_keypair(&agent, &agent_key, None).unwrap();

        let author = resolve_agent_author(&AgentAuthorOptions {
            agent_name: "claude-code",
            agent_display_name: "Claude Code",
            session_id: "60f5cbd2-aa23-40ee-9085-4375dd186ce7",
            identity_dir: Some(dir.path().to_path_buf()),
            agent_identity: Some("alice+claude".to_string()),
        });

        // The author line is unchanged — legibility was never the problem.
        assert_eq!(author.name, "claude+60f5");
        // The key is the agent's, which is what changed.
        assert_eq!(
            author.identity.as_deref(),
            Some(agent.public_key_base32().as_str())
        );
        assert_ne!(
            author.identity.as_deref(),
            Some(human.public_key_base32().as_str())
        );
    }

    /// A human identity passed as the agent identity must not be used: signing
    /// agent work with the human's key and *calling* it keyed attribution is
    /// worse than the honest plus-tag fallback.
    #[test]
    fn a_non_agent_identity_is_refused_and_falls_back() {
        use atomic_identity::{Identity, IdentityStore, KeyPair};

        let dir = TempDir::new().unwrap();
        let store = IdentityStore::open(dir.path()).unwrap();
        let key = KeyPair::generate();
        let human = Identity::builder("alice")
            .email("alice@example.com")
            .public_key(key.public.clone())
            .build()
            .unwrap();
        store.save_with_keypair(&human, &key, None).unwrap();

        let options = AgentAuthorOptions {
            agent_name: "claude-code",
            agent_display_name: "Claude Code",
            session_id: "sess1234",
            identity_dir: Some(dir.path().to_path_buf()),
            agent_identity: Some("alice".to_string()),
        };
        assert!(delegated_agent_author(&options).is_none());
    }

    /// A mistyped agent identity must not break recording.
    #[test]
    fn an_unknown_agent_identity_falls_back_rather_than_failing() {
        let dir = TempDir::new().unwrap();
        let options = AgentAuthorOptions {
            agent_name: "claude-code",
            agent_display_name: "Claude Code",
            session_id: "sess1234",
            identity_dir: Some(dir.path().to_path_buf()),
            agent_identity: Some("nobody+here".to_string()),
        };
        assert!(delegated_agent_author(&options).is_none());

        // And the public entry point still produces a usable author.
        let author = resolve_agent_author(&options);
        assert_eq!(author.name, "Claude Code");
    }

    /// No agent identity configured: unchanged behavior.
    #[test]
    fn no_agent_identity_means_no_keyed_path() {
        let dir = TempDir::new().unwrap();
        let options = AgentAuthorOptions {
            agent_name: "claude-code",
            agent_display_name: "Claude Code",
            session_id: "sess1234",
            identity_dir: Some(dir.path().to_path_buf()),
            agent_identity: None,
        };
        // Guard against a stray env var in the test environment.
        if std::env::var(AGENT_IDENTITY_ENV).is_err() {
            assert!(delegated_agent_author(&options).is_none());
        }
    }

    /// With no certificate installed there is nothing to name on the envelope.
    #[test]
    fn active_delegation_urn_is_none_without_a_certificate() {
        use atomic_identity::{Identity, IdentityStore, IdentityType, KeyPair};

        let dir = TempDir::new().unwrap();
        let store = IdentityStore::open(dir.path()).unwrap();
        let key = KeyPair::generate();
        let agent = Identity::builder("alice+claude")
            .identity_type(IdentityType::Agent)
            .public_key(key.public.clone())
            .build()
            .unwrap();
        store.save_with_keypair(&agent, &key, None).unwrap();

        assert_eq!(
            active_delegation_urn(Some("alice+claude"), Some(dir.path())),
            None
        );
    }

    // resolve_agent_author (integration)

    #[test]
    fn test_resolve_no_identity_dir() {
        let dir = TempDir::new().unwrap();
        let nonexistent = dir.path().join("does-not-exist");

        let options = AgentAuthorOptions {
            agent_name: "claude-code",
            agent_display_name: "Claude Code",
            session_id: "60f5cbd2",
            identity_dir: Some(nonexistent),
            agent_identity: None,
        };

        let author = resolve_agent_author(&options);

        // Should fall back to display name
        assert_eq!(author.name, "Claude Code");
        assert!(author.email.is_none());
    }

    #[test]
    fn test_resolve_empty_identity_dir() {
        let dir = TempDir::new().unwrap();

        let options = AgentAuthorOptions {
            agent_name: "claude-code",
            agent_display_name: "Claude Code",
            session_id: "60f5cbd2",
            identity_dir: Some(dir.path().to_path_buf()),
            agent_identity: None,
        };

        let author = resolve_agent_author(&options);

        // No config.toml → fallback
        assert_eq!(author.name, "Claude Code");
    }

    #[test]
    fn test_resolve_with_identity() {
        let dir = TempDir::new().unwrap();

        // Create a mock identity store
        let config = r#"
default_identity = "TESTID1234567890"
version = 1
"#;
        fs::write(dir.path().join("config.toml"), config).unwrap();

        // Create an identity directory
        let id_dir = dir.path().join("alice-personal");
        fs::create_dir(&id_dir).unwrap();
        let identity_toml = r#"
id = "TESTID1234567890"
name = "Alice"
email = "alice@example.com"
public_key = "PK_BASE32_ALICE"
identity_type = "user"
"#;
        fs::write(id_dir.join("identity.toml"), identity_toml).unwrap();

        let options = AgentAuthorOptions {
            agent_name: "claude-code",
            agent_display_name: "Claude Code",
            session_id: "60f5cbd2-aa23-40ee-9085-4375dd186ce7",
            identity_dir: Some(dir.path().to_path_buf()),
            agent_identity: None,
        };

        let author = resolve_agent_author(&options);

        assert_eq!(author.name, "claude+60f5");
        assert_eq!(author.email, Some("alice@example.com".to_string()));
        assert_eq!(author.identity, Some("PK_BASE32_ALICE".to_string()));
    }

    #[test]
    fn test_resolve_with_identity_no_email() {
        let dir = TempDir::new().unwrap();

        let config = r#"default_identity = "NOEMAIL_ID""#;
        fs::write(dir.path().join("config.toml"), config).unwrap();

        let id_dir = dir.path().join("bob-agent");
        fs::create_dir(&id_dir).unwrap();
        let identity_toml = r#"
id = "NOEMAIL_ID"
name = "Bob"
public_key = "PK_BOB"
identity_type = "user"
"#;
        fs::write(id_dir.join("identity.toml"), identity_toml).unwrap();

        let options = AgentAuthorOptions {
            agent_name: "gemini-cli",
            agent_display_name: "Gemini CLI",
            session_id: "abcdef12",
            identity_dir: Some(dir.path().to_path_buf()),
            agent_identity: None,
        };

        let author = resolve_agent_author(&options);

        assert_eq!(author.name, "gemini+abcd");
        assert!(author.email.is_none());
        assert_eq!(author.identity, Some("PK_BOB".to_string()));
    }

    // build_agent_author (convenience function)

    #[test]
    fn test_build_agent_author_fallback() {
        // Without a real identity store, this falls back to display name.
        // In a real install with ~/.atomic/identities/ configured, it would
        // produce the +tag format.
        let author = build_agent_author(
            "claude-code",
            "Claude Code",
            "60f5cbd2-aa23-40ee-9085-4375dd186ce7",
        );

        // Can't guarantee identity store exists in test env, so just verify
        // the author is valid (either tagged or fallback)
        assert!(!author.name.is_empty());
    }

    // extract_toml_string_value

    #[test]
    fn test_extract_toml_string_simple() {
        assert_eq!(
            extract_toml_string_value(r#"name = "Alice""#),
            Some("Alice".to_string())
        );
    }

    #[test]
    fn test_extract_toml_string_with_spaces() {
        assert_eq!(
            extract_toml_string_value(r#"  name  =  "Alice"  "#),
            Some("Alice".to_string())
        );
    }

    #[test]
    fn test_extract_toml_string_empty_value() {
        assert_eq!(extract_toml_string_value(r#"name = """#), None);
    }

    #[test]
    fn test_extract_toml_string_no_quotes() {
        assert_eq!(extract_toml_string_value("name = Alice"), None);
    }

    #[test]
    fn test_extract_toml_email() {
        assert_eq!(
            extract_toml_string_value(r#"email = "lee@atomic.dev""#),
            Some("lee@atomic.dev".to_string())
        );
    }

    // extract_default_identity_from_config

    #[test]
    fn test_extract_default_identity() {
        let config = r#"
default_identity = "ABCDEF1234567890"
version = 1

[default_by_usage]
"#;
        assert_eq!(
            extract_default_identity_from_config(config),
            Some("ABCDEF1234567890".to_string())
        );
    }

    #[test]
    fn test_extract_default_identity_empty_config() {
        assert_eq!(extract_default_identity_from_config(""), None);
    }

    #[test]
    fn test_extract_default_identity_no_default() {
        let config = r#"
version = 1

[default_by_usage]
"#;
        assert_eq!(extract_default_identity_from_config(config), None);
    }

    #[test]
    fn test_extract_default_identity_empty_value() {
        let config = r#"default_identity = """#;
        assert_eq!(extract_default_identity_from_config(config), None);
    }

    // Display format verification

    #[test]
    fn test_author_display_with_email() {
        let user = UserIdentityInfo {
            name: "Lee Faus".to_string(),
            email: Some("lee@atomic.dev".to_string()),
            public_key_base32: "KEYREF".to_string(),
        };
        let options = AgentAuthorOptions {
            agent_name: "claude-code",
            agent_display_name: "Claude Code",
            session_id: "60f5cbd2-aa23-40ee-9085-4375dd186ce7",
            identity_dir: None,
            agent_identity: None,
        };

        let author = derive_agent_author(&user, &options);
        let display = author.display_short();

        assert_eq!(display, "claude+60f5 <lee@atomic.dev>");
    }

    #[test]
    fn test_author_display_without_email() {
        let author = fallback_agent_author(&AgentAuthorOptions {
            agent_name: "claude-code",
            agent_display_name: "Claude Code",
            session_id: "sess",
            identity_dir: None,
            agent_identity: None,
        });

        assert_eq!(author.display_short(), "Claude Code");
    }

    #[test]
    fn test_author_display_different_agents() {
        let user = UserIdentityInfo {
            name: "Dev".to_string(),
            email: Some("dev@co.com".to_string()),
            public_key_base32: "KEY".to_string(),
        };

        let agents = vec![
            ("claude-code", "60f5cbd2", "claude+60f5 <dev@co.com>"),
            ("gemini-cli", "abcd1234", "gemini+abcd <dev@co.com>"),
            ("codex", "9876fedc", "codex+9876 <dev@co.com>"),
            ("open-code", "1111aaaa", "open+1111 <dev@co.com>"),
        ];

        for (agent, session, expected) in agents {
            let options = AgentAuthorOptions {
                agent_name: agent,
                agent_display_name: agent,
                session_id: session,
                identity_dir: None,
                agent_identity: None,
            };
            let author = derive_agent_author(&user, &options);
            assert_eq!(
                author.display_short(),
                expected,
                "Failed for agent: {}",
                agent
            );
        }
    }
}
