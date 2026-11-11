//! Client transport for project-identity adoption and binding.
//!
//! The server half (atomic-storage's `?project-identity=1` handlers) binds one
//! canonical [`ProjectIdentity`](https://atomic.dev/ns/workflow#ProjectIdentity)
//! JSON-LD object per project, first-write-wins. This module is the client
//! half:
//!
//! - [`HttpRemote::fetch_project_identity`] reads the binding so a clone or
//!   pull can adopt it (every clone of one project shares one identity).
//! - [`HttpRemote::bind_project_identity`] offers the local identity on the
//!   first push to an unbound project.
//!
//! Both endpoints are JSON on the same `/code` URL as the binary sync
//! protocol, gated by the same credentials (the client's extra headers).
//! Servers that predate the endpoints answer `GET` with the project-info ping
//! (no `bound` field) and `POST` with `404`/`405` — both surface as
//! [`IdentityFetch::Unsupported`]/[`BindOutcome::Unsupported`] so callers skip
//! transport gracefully against older storage versions.

use reqwest::header::CONTENT_TYPE;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::HttpRemote;
use crate::error::{RemoteError, RemoteResult};

/// The result of reading a remote's project-identity binding.
#[derive(Debug, Clone)]
pub enum IdentityFetch {
    /// The remote has no binding yet (or the endpoint is unsupported).
    Unbound,
    /// The remote's bound identity document.
    Bound(Value),
    /// The remote predates the identity transport; nothing to do.
    Unsupported,
}

/// The server's structured error body on a `409` bind rejection. Both fields
/// are optional and the body itself may be empty or non-JSON, in which case
/// the caller simply gets no reason to quote.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindServerError {
    /// The server's machine-readable reason (e.g. `IDENTITY_CONFLICT`).
    #[serde(default)]
    pub code: Option<String>,
    /// The server's human-readable reason, quoted verbatim to the operator.
    #[serde(default)]
    pub message: Option<String>,
}

impl BindServerError {
    /// Parse a `409` response body; `None` when it is not a usable JSON
    /// object. The flat `{"code", "message"}` spelling is expected, and the
    /// nested `{"error": {…}}` spelling is accepted so either server shape
    /// surfaces its reason.
    fn from_body(body: &str) -> Option<Self> {
        let parsed: Value = serde_json::from_str(body).ok()?;
        let (code, message) = match parsed.get("error") {
            Some(nested) => (
                nested.get("code").and_then(|v| v.as_str()),
                nested.get("message").and_then(|v| v.as_str()),
            ),
            None => (
                parsed.get("code").and_then(|v| v.as_str()),
                parsed.get("message").and_then(|v| v.as_str()),
            ),
        };
        Some(Self {
            code: code.map(str::to_owned),
            message: message.map(str::to_owned),
        })
    }
}

/// What the follow-up read of the remote binding resolved to after a `409`,
/// so callers can tell a genuine divergence from a server that disagrees with
/// its own rejection.
#[derive(Debug, Clone, PartialEq)]
pub enum ConflictRemote {
    /// The follow-up GET resolved to a bound identity document.
    Bound(Value),
    /// The follow-up GET succeeded but the server reports the project unbound
    /// (server disagrees with its own 409).
    Unbound,
    /// The follow-up GET failed or the document was unusable.
    Unknown,
}

impl ConflictRemote {
    fn from_fetch(fetched: RemoteResult<IdentityFetch>) -> Self {
        match fetched {
            Ok(IdentityFetch::Bound(identity)) => Self::Bound(identity),
            Ok(IdentityFetch::Unbound) => Self::Unbound,
            // A pre-transport ping or any transport trouble leaves the
            // binding unreadable.
            Ok(IdentityFetch::Unsupported) | Err(_) => Self::Unknown,
        }
    }
}

/// The result of offering the local identity to a remote.
#[derive(Debug, Clone)]
pub enum BindOutcome {
    /// Bound now, or already identically bound.
    Bound,
    /// A *different* identity is already bound; carries what the follow-up
    /// read of the remote binding resolved to and the server's error body
    /// when it sent one.
    Conflict {
        remote: ConflictRemote,
        server: Option<BindServerError>,
    },
    /// The remote predates the identity transport; nothing to do.
    Unsupported,
}

/// The server's identity JSON response (`{ "bound": bool, "hash"?, "identity"? }`).
/// `bound` is optional so a pre-transport server's project-info ping (which
/// carries none of these fields) decodes and is recognized as unsupported.
#[derive(Debug, Deserialize)]
struct IdentityResponse {
    #[serde(default)]
    bound: Option<bool>,
    #[serde(default)]
    identity: Option<Value>,
}

impl HttpRemote {
    /// Read the remote's project-identity binding.
    pub async fn fetch_project_identity(&self) -> RemoteResult<IdentityFetch> {
        let url = format!("{}?project-identity=1", self.base_url.as_str());
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;
        match response.status() {
            StatusCode::OK => {
                let parsed: IdentityResponse = response.json().await.map_err(|e| {
                    RemoteError::protocol(format!("invalid project-identity response: {e}"))
                })?;
                match (parsed.bound, parsed.identity) {
                    (Some(true), Some(identity)) => Ok(IdentityFetch::Bound(identity)),
                    (Some(false), _) => Ok(IdentityFetch::Unbound),
                    // A 200 without `bound` is a pre-transport server's
                    // project-info ping (it ignores unknown query params).
                    _ => Ok(IdentityFetch::Unsupported),
                }
            }
            // Old servers: `GET …/code` answers the project-info ping (no
            // `bound` field) and unknown paths 404. Both mean "unsupported".
            StatusCode::NOT_FOUND
            | StatusCode::METHOD_NOT_ALLOWED
            | StatusCode::NOT_IMPLEMENTED => Ok(IdentityFetch::Unsupported),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(RemoteError::auth_failed(
                &url,
                response.text().await.unwrap_or_default(),
            )),
            s => Err(RemoteError::http(
                s.as_u16(),
                response.text().await.unwrap_or_default(),
            )),
        }
    }

    /// Offer the local identity document for binding (first-write-wins).
    pub async fn bind_project_identity(&self, identity: &Value) -> RemoteResult<BindOutcome> {
        let url = format!("{}?project-identity=1", self.base_url.as_str());
        let body = serde_json::to_vec(identity)
            .map_err(|e| RemoteError::protocol(format!("identity is not valid JSON: {e}")))?;
        let response = self
            .client
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;
        match response.status() {
            StatusCode::OK | StatusCode::CREATED => Ok(BindOutcome::Bound),
            StatusCode::CONFLICT => {
                // Read the rejection body (the server's reason) and the
                // binding the server already holds so the caller can name the
                // divergence; neither read is fatal here.
                let body = response.text().await.unwrap_or_default();
                let server = BindServerError::from_body(&body);
                let remote = ConflictRemote::from_fetch(self.fetch_project_identity().await);
                Ok(BindOutcome::Conflict { remote, server })
            }
            StatusCode::NOT_FOUND
            | StatusCode::METHOD_NOT_ALLOWED
            | StatusCode::NOT_IMPLEMENTED => Ok(BindOutcome::Unsupported),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(RemoteError::auth_failed(
                &url,
                response.text().await.unwrap_or_default(),
            )),
            s => Err(RemoteError::http(
                s.as_u16(),
                response.text().await.unwrap_or_default(),
            )),
        }
    }
}
