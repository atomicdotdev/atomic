#![allow(dead_code)]
//! HTTP client for the Hive Agent Social Coding Platform.
//!
//! Handles agent registration, claim status checking, and profile fetching.
//! Uses `reqwest` for HTTP and `atomic-identity` for Ed25519 signing.
//!
//! # API Endpoints
//!
//! - `POST /agents/register` — Register a new agent
//! - `GET  /agents/{slug}`   — Get agent profile (public)
//! - `GET  /agents/me/status` — Check claim status (authenticated)
//! - `GET  /agents/me`       — Get own profile (authenticated)
//!
//! # Authentication
//!
//! Authenticated requests use Ed25519 signatures in HTTP headers:
//! - `Authorization: Agent {agent-id}`
//! - `X-Agent-Signature: {base64-signature}`
//! - `X-Agent-Timestamp: {unix-timestamp}`
//!
//! The signature message format is: `{method}:{path}:{timestamp}:{body-hash}`

use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::identity::{create_registration_message, generate_keypair, sign_message, HiveIdentity};

// =============================================================================
// Configuration
// =============================================================================

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("atomic-cli/", env!("CARGO_PKG_VERSION"));

// =============================================================================
// Response Types
// =============================================================================

/// Result of a successful agent registration.
pub struct RegistrationResult {
    /// The local identity (with keypair and claim info).
    pub identity: HiveIdentity,
}

/// A user identity pulled from the Hive API (includes secret key).
#[derive(Debug, Deserialize)]
pub struct PulledIdentity {
    pub name: String,
    pub slug: String,
    pub email: Option<String>,
    pub usage: String,
    #[serde(rename = "publicKey")]
    pub public_key: String,
    #[serde(rename = "secretKey")]
    pub secret_key: Option<String>,
    pub description: Option<String>,
    #[serde(rename = "isDefault", default)]
    pub is_default: bool,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// Response from the keypairs endpoint.
#[derive(Debug, Deserialize)]
struct KeypairsResponse {
    identities: Vec<PulledIdentity>,
}

/// Agent profile from the Hive API.
#[derive(Debug, Deserialize)]
pub struct AgentProfile {
    pub name: String,
    pub slug: String,
    #[serde(rename = "trustTier")]
    pub trust_tier: String,
    #[serde(rename = "isActive")]
    pub is_active: bool,
    pub reputation: Option<AgentReputation>,
}

/// Agent reputation metrics.
#[derive(Debug, Deserialize)]
pub struct AgentReputation {
    #[serde(rename = "overallScore")]
    pub overall_score: f64,
    #[serde(rename = "projectsAuthored")]
    pub projects_authored: u64,
    #[serde(rename = "projectsContributed")]
    pub projects_contributed: u64,
    #[serde(rename = "conceptsPublished")]
    pub concepts_published: u64,
    #[serde(rename = "totalStars")]
    pub total_stars: u64,
    #[serde(rename = "totalDownloads")]
    pub total_downloads: u64,
}

/// Registration API response.
#[derive(Debug, Deserialize)]
struct RegisterResponse {
    agent: RegisteredAgent,
    claim: ClaimInfo,
}

#[derive(Debug, Deserialize)]
struct RegisteredAgent {
    id: String,
    #[serde(rename = "publicKey")]
    public_key: String,
    name: String,
    slug: String,
}

#[derive(Debug, Deserialize)]
struct ClaimInfo {
    url: String,
    code: String,
}

/// Agent profile wrapper (API returns `{ data: ... }`).
#[derive(Debug, Deserialize)]
struct ProfileResponse {
    data: AgentProfile,
}

/// Claim status response from the API.
#[derive(Debug, Deserialize)]
struct ClaimStatusResponse {
    status: String,
}

/// Registration request body.
#[derive(Debug, Serialize)]
struct RegisterRequest {
    name: String,
    #[serde(rename = "publicKey")]
    public_key: String,
    vendor: String,
    model: String,
    #[serde(rename = "modelVersion", skip_serializing_if = "Option::is_none")]
    model_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    signature: String,
    timestamp: i64,
}

/// Generic API error response.
#[derive(Debug, Deserialize)]
struct ApiErrorResponse {
    error: Option<String>,
    message: Option<String>,
}

// =============================================================================
// Client
// =============================================================================

/// HTTP client for the Hive API.
///
/// Handles registration, authentication, and profile operations.
pub struct HiveClient {
    api_url: String,
    http: reqwest::Client,
}

impl HiveClient {
    /// Create a new Hive client pointing at the given API URL.
    ///
    /// # Arguments
    ///
    /// * `api_url` — Base URL for the Hive API (e.g. `https://hive.atomic.dev/api/v1`)
    pub fn new(api_url: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .expect("failed to build HTTP client");

        Self {
            api_url: api_url.trim_end_matches('/').to_string(),
            http,
        }
    }

    // =========================================================================
    // Registration
    // =========================================================================

    /// Register a new agent on Hive.
    ///
    /// Generates an Ed25519 keypair, signs the registration request,
    /// and sends it to the Hive API. Returns the local identity with
    /// the claim URL for human verification.
    ///
    /// # Arguments
    ///
    /// * `name` — Display name for the agent
    /// * `vendor` — AI vendor (anthropic, openai, google, etc.)
    /// * `model` — Model identifier (e.g. claude-sonnet-4)
    /// * `model_version` — Optional model version
    /// * `description` — Optional agent description
    ///
    /// # Errors
    ///
    /// Returns an error if keypair generation fails, the API is unreachable,
    /// or the registration is rejected.
    pub async fn register(
        &self,
        name: &str,
        vendor: &str,
        model: &str,
        model_version: Option<&str>,
        description: Option<&str>,
    ) -> Result<RegistrationResult, HiveClientError> {
        // Generate Ed25519 keypair
        let keypair = generate_keypair()
            .map_err(|e| HiveClientError::Internal(format!("Keypair generation failed: {}", e)))?;

        // Sign the registration
        let timestamp = chrono::Utc::now().timestamp();
        let message = create_registration_message(&keypair.public_key, timestamp);
        let signature = sign_message(&keypair.secret_key, message.as_bytes())
            .map_err(|e| HiveClientError::Internal(format!("Signing failed: {}", e)))?;

        // Build request
        let body = RegisterRequest {
            name: name.to_string(),
            public_key: keypair.public_key.clone(),
            vendor: vendor.to_string(),
            model: model.to_string(),
            model_version: model_version.map(String::from),
            description: description.map(String::from),
            signature,
            timestamp,
        };

        let url = format!("{}/agents/register", self.api_url);

        let response =
            self.http.post(&url).json(&body).send().await.map_err(|e| {
                HiveClientError::Network(format!("Failed to connect to Hive: {}", e))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body =
                response
                    .json::<ApiErrorResponse>()
                    .await
                    .unwrap_or(ApiErrorResponse {
                        error: Some(format!("HTTP {}", status)),
                        message: None,
                    });

            return Err(HiveClientError::Api {
                status: status.as_u16(),
                message: error_body
                    .error
                    .or(error_body.message)
                    .unwrap_or_else(|| format!("Registration failed with status {}", status)),
            });
        }

        let result: RegisterResponse = response
            .json()
            .await
            .map_err(|e| HiveClientError::Parse(format!("Invalid response: {}", e)))?;

        // Build the local identity
        let identity = HiveIdentity {
            id: result.agent.id,
            name: result.agent.name,
            slug: result.agent.slug,
            public_key: result.agent.public_key,
            secret_key: Some(keypair.secret_key),
            vendor: vendor.to_string(),
            model: model.to_string(),
            model_version: model_version.map(String::from),
            description: description.map(String::from),
            is_claimed: false,
            registered_at: timestamp,
            claimed_at: None,
            claim_url: Some(result.claim.url),
            claim_code: Some(result.claim.code),
        };

        Ok(RegistrationResult { identity })
    }

    // =========================================================================
    // Claim Status
    // =========================================================================

    /// Check if the agent has been claimed by a human.
    ///
    /// Queries the Hive API using the agent's slug to check the claim status.
    /// Returns `true` if claimed, `false` if still pending.
    ///
    /// # Arguments
    ///
    /// * `identity` — The local agent identity
    ///
    /// # Errors
    ///
    /// Returns an error if the API is unreachable or returns an unexpected response.
    pub async fn check_claim_status(
        &self,
        identity: &HiveIdentity,
    ) -> Result<bool, HiveClientError> {
        // Use the public agent endpoint to check if claimed
        let url = format!("{}/agents/{}", self.api_url, identity.slug);

        let response =
            self.http.get(&url).send().await.map_err(|e| {
                HiveClientError::Network(format!("Failed to connect to Hive: {}", e))
            })?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }

        if !response.status().is_success() {
            return Err(HiveClientError::Api {
                status: response.status().as_u16(),
                message: format!("Unexpected status: {}", response.status()),
            });
        }

        // The public agent endpoint returns the agent data directly
        // Check if the `isClaimed` field is true
        #[derive(Deserialize)]
        struct AgentData {
            data: AgentClaimed,
        }

        #[derive(Deserialize)]
        struct AgentClaimed {
            #[serde(rename = "isClaimed", default)]
            is_claimed: bool,
        }

        let data: AgentData = response
            .json()
            .await
            .map_err(|e| HiveClientError::Parse(format!("Invalid response: {}", e)))?;

        Ok(data.data.is_claimed)
    }

    // =========================================================================
    // Profile
    // =========================================================================

    /// Fetch the agent's profile from Hive.
    ///
    /// Uses the agent's slug to fetch the public profile including
    /// reputation metrics and trust tier.
    ///
    /// # Arguments
    ///
    /// * `identity` — The local agent identity
    ///
    /// # Errors
    ///
    /// Returns an error if the API is unreachable or the profile is not found.
    pub async fn get_profile(
        &self,
        identity: &HiveIdentity,
    ) -> Result<AgentProfile, HiveClientError> {
        let url = format!("{}/agents/{}", self.api_url, identity.slug);

        let response =
            self.http.get(&url).send().await.map_err(|e| {
                HiveClientError::Network(format!("Failed to connect to Hive: {}", e))
            })?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(HiveClientError::Api {
                status: 404,
                message: format!("Agent '{}' not found on Hive", identity.slug),
            });
        }

        if !response.status().is_success() {
            let status = response.status();
            return Err(HiveClientError::Api {
                status: status.as_u16(),
                message: format!("Failed to fetch profile: {}", status),
            });
        }

        let profile: ProfileResponse = response
            .json()
            .await
            .map_err(|e| HiveClientError::Parse(format!("Invalid profile response: {}", e)))?;

        Ok(profile.data)
    }

    // =========================================================================
    // Pull User Identities
    // =========================================================================

    /// Pull all user identities (with secret keys) from Hive.
    ///
    /// This is the endpoint that `atomic hive pull-identities` calls.
    /// Authenticates using a session token (from the browser cookie).
    ///
    /// # Arguments
    ///
    /// * `session_token` — Better Auth session token (from browser cookie)
    ///
    /// # Returns
    ///
    /// A list of identities including secret keys for local storage.
    pub async fn pull_identities(
        &self,
        session_token: &str,
    ) -> Result<Vec<PulledIdentity>, HiveClientError> {
        let url = format!("{}/identities/keypairs", self.api_url);

        let response = self
            .http
            .get(&url)
            .header(
                "Cookie",
                format!("better-auth.session_token={}", session_token),
            )
            .send()
            .await
            .map_err(|e| HiveClientError::Network(format!("Failed to connect to Hive: {}", e)))?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(HiveClientError::Api {
                status: 401,
                message: "Unauthorized — invalid or expired session token. Log in at the Hive web UI and copy your session cookie.".to_string(),
            });
        }

        if !response.status().is_success() {
            let status = response.status();
            return Err(HiveClientError::Api {
                status: status.as_u16(),
                message: format!("Failed to fetch identities: {}", status),
            });
        }

        let data: KeypairsResponse = response
            .json()
            .await
            .map_err(|e| HiveClientError::Parse(format!("Invalid response: {}", e)))?;

        Ok(data.identities)
    }

    // =========================================================================
    // Health Check
    // =========================================================================

    /// Check if the Hive API is reachable.
    ///
    /// Returns `true` if the health endpoint responds successfully.
    #[allow(dead_code)]
    pub async fn health_check(&self) -> Result<bool, HiveClientError> {
        // The health endpoint is at the root, not under /api/v1
        let base = self
            .api_url
            .trim_end_matches("/api/v1")
            .trim_end_matches("/api/v1/");

        let url = format!("{}/health", base);

        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| HiveClientError::Network(format!("Health check failed: {}", e)))?;

        Ok(response.status().is_success())
    }
}

// =============================================================================
// Error Type
// =============================================================================

/// Errors that can occur during Hive API operations.
#[derive(Debug, thiserror::Error)]
pub enum HiveClientError {
    /// Network connectivity error.
    #[error("Network error: {0}")]
    Network(String),

    /// API returned an error response.
    #[error("API error (HTTP {status}): {message}")]
    Api {
        /// HTTP status code.
        status: u16,
        /// Error message from the API.
        message: String,
    },

    /// Failed to parse the API response.
    #[error("Parse error: {0}")]
    Parse(String),

    /// Internal error (keypair generation, signing, etc.).
    #[error("Internal error: {0}")]
    Internal(String),
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_creation() {
        let client = HiveClient::new("https://hive.atomic.dev/api/v1");
        assert_eq!(client.api_url, "https://hive.atomic.dev/api/v1");
    }

    #[test]
    fn test_client_strips_trailing_slash() {
        let client = HiveClient::new("https://hive.atomic.dev/api/v1/");
        assert_eq!(client.api_url, "https://hive.atomic.dev/api/v1");
    }

    #[test]
    fn test_user_agent_format() {
        assert!(USER_AGENT.starts_with("atomic-cli/"));
    }

    #[test]
    fn test_pulled_identity_deserialization() {
        let json = r#"{
            "name": "leefaus",
            "slug": "leefaus",
            "email": "lee@example.com",
            "usage": "work",
            "publicKey": "ABCDEF123456",
            "secretKey": "SECRET123456",
            "description": "Professional identity",
            "isDefault": true,
            "createdAt": "2025-06-28T00:00:00Z"
        }"#;

        let id: PulledIdentity = serde_json::from_str(json).unwrap();
        assert_eq!(id.name, "leefaus");
        assert_eq!(id.slug, "leefaus");
        assert_eq!(id.email, Some("lee@example.com".to_string()));
        assert_eq!(id.usage, "work");
        assert_eq!(id.public_key, "ABCDEF123456");
        assert_eq!(id.secret_key, Some("SECRET123456".to_string()));
        assert!(id.is_default);
    }

    #[test]
    fn test_pulled_identity_minimal() {
        let json = r#"{
            "name": "hobby",
            "slug": "hobby",
            "email": null,
            "usage": "personal",
            "publicKey": "KEY123",
            "secretKey": null,
            "description": null,
            "isDefault": false,
            "createdAt": "2025-06-28T00:00:00Z"
        }"#;

        let id: PulledIdentity = serde_json::from_str(json).unwrap();
        assert_eq!(id.name, "hobby");
        assert!(id.email.is_none());
        assert!(id.secret_key.is_none());
        assert!(!id.is_default);
    }

    #[test]
    fn test_keypairs_response_deserialization() {
        let json = r#"{
            "identities": [
                {
                    "name": "id1",
                    "slug": "id1",
                    "email": null,
                    "usage": "work",
                    "publicKey": "PK1",
                    "secretKey": "SK1",
                    "description": null,
                    "isDefault": true,
                    "createdAt": "2025-01-01T00:00:00Z"
                },
                {
                    "name": "id2",
                    "slug": "id2",
                    "email": "a@b.com",
                    "usage": "personal",
                    "publicKey": "PK2",
                    "secretKey": "SK2",
                    "description": "test",
                    "isDefault": false,
                    "createdAt": "2025-01-02T00:00:00Z"
                }
            ],
            "warning": "This response contains secret keys."
        }"#;

        let resp: KeypairsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.identities.len(), 2);
        assert_eq!(resp.identities[0].name, "id1");
        assert_eq!(resp.identities[1].name, "id2");
    }

    #[test]
    fn test_error_display() {
        let err = HiveClientError::Network("connection refused".to_string());
        assert!(err.to_string().contains("connection refused"));

        let err = HiveClientError::Api {
            status: 404,
            message: "not found".to_string(),
        };
        assert!(err.to_string().contains("404"));
        assert!(err.to_string().contains("not found"));

        let err = HiveClientError::Parse("invalid json".to_string());
        assert!(err.to_string().contains("invalid json"));

        let err = HiveClientError::Internal("key error".to_string());
        assert!(err.to_string().contains("key error"));
    }

    #[test]
    fn test_register_request_serialization() {
        let req = RegisterRequest {
            name: "test-agent".to_string(),
            public_key: "ABCDEF".to_string(),
            vendor: "anthropic".to_string(),
            model: "claude-sonnet-4".to_string(),
            model_version: None,
            description: Some("Test agent".to_string()),
            signature: "sig123".to_string(),
            timestamp: 1719500000,
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"publicKey\":\"ABCDEF\""));
        assert!(json.contains("\"name\":\"test-agent\""));
        assert!(json.contains("\"vendor\":\"anthropic\""));
        // model_version should be absent (None + skip_serializing_if)
        assert!(!json.contains("modelVersion"));
    }

    #[test]
    fn test_register_request_with_version() {
        let req = RegisterRequest {
            name: "test".to_string(),
            public_key: "KEY".to_string(),
            vendor: "openai".to_string(),
            model: "gpt-4".to_string(),
            model_version: Some("0125".to_string()),
            description: None,
            signature: "sig".to_string(),
            timestamp: 0,
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"modelVersion\":\"0125\""));
        // description should be absent
        assert!(!json.contains("description"));
    }

    #[test]
    fn test_api_error_response_deserialization() {
        let json = r#"{"error": "Agent already exists", "code": "AGENT_EXISTS"}"#;
        let resp: ApiErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.error.unwrap(), "Agent already exists");
    }

    #[test]
    fn test_register_response_deserialization() {
        let json = r#"{
            "agent": {
                "id": "uuid-1234",
                "publicKey": "ABCDEF",
                "name": "test-agent",
                "slug": "test-agent"
            },
            "claim": {
                "url": "https://hive.atomic.dev/claim/abc123",
                "code": "HIVE-AB12"
            }
        }"#;

        let resp: RegisterResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.agent.id, "uuid-1234");
        assert_eq!(resp.agent.slug, "test-agent");
        assert_eq!(resp.claim.url, "https://hive.atomic.dev/claim/abc123");
        assert_eq!(resp.claim.code, "HIVE-AB12");
    }

    #[test]
    fn test_agent_profile_deserialization() {
        let json = r#"{
            "name": "test-agent",
            "slug": "test-agent",
            "trustTier": "verified",
            "isActive": true,
            "reputation": {
                "overallScore": 85.5,
                "projectsAuthored": 10,
                "projectsContributed": 25,
                "conceptsPublished": 3,
                "totalStars": 150,
                "totalDownloads": 5000
            }
        }"#;

        let profile: AgentProfile = serde_json::from_str(json).unwrap();
        assert_eq!(profile.name, "test-agent");
        assert_eq!(profile.trust_tier, "verified");
        assert!(profile.is_active);
        assert!(profile.reputation.is_some());

        let rep = profile.reputation.unwrap();
        assert_eq!(rep.overall_score, 85.5);
        assert_eq!(rep.projects_authored, 10);
        assert_eq!(rep.total_stars, 150);
    }

    #[test]
    fn test_agent_profile_without_reputation() {
        let json = r#"{
            "name": "new-agent",
            "slug": "new-agent",
            "trustTier": "new",
            "isActive": false,
            "reputation": null
        }"#;

        let profile: AgentProfile = serde_json::from_str(json).unwrap();
        assert_eq!(profile.trust_tier, "new");
        assert!(!profile.is_active);
        assert!(profile.reputation.is_none());
    }

    #[test]
    fn test_claim_status_deserialization() {
        let json = r#"{"status": "claimed"}"#;
        let resp: ClaimStatusResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.status, "claimed");

        let json = r#"{"status": "pending_claim"}"#;
        let resp: ClaimStatusResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.status, "pending_claim");
    }
}
