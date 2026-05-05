//! HTTP client for the atomic-storage management API.
//!
//! `StorageClient` provides typed access to CRUD operations on workspaces,
//! projects, and other management endpoints. It handles authentication
//! (Ed25519 public key as Bearer token), JSON serialization, and
//! response envelope unwrapping.
//!
//! The VCS protocol (push/pull/clone) uses `HttpRemote` instead.

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{de::DeserializeOwned, Serialize};

use crate::error::RemoteError;
use crate::storage_types::{
    ApiResponse, CreateProjectRequest, CreateWorkspaceRequest, IdentityInfo, ProjectInfo,
    UpdateProjectRequest, UpdateWorkspaceRequest, WorkspaceInfo,
};

/// HTTP client for atomic-storage management operations.
///
/// Handles authentication, URL construction, and API response unwrapping.
///
/// # Examples
///
/// ```ignore
/// use atomic_remote::StorageClient;
/// use atomic_remote::storage_types::CreateWorkspaceRequest;
/// use atomic_remote::Visibility;
///
/// let client = StorageClient::new(
///     "https://alice.atomic.storage",
///     "alice",
///     "EDPK_BASE32_TOKEN",
/// )?;
///
/// let ws = client.create_workspace(&CreateWorkspaceRequest {
///     name: "my-workspace".into(),
///     description: None,
///     visibility: Visibility::Private,
/// }).await?;
/// ```
pub struct StorageClient {
    http: reqwest::Client,
    base_url: String,
    org_slug: String,
}

impl StorageClient {
    /// Create a new client targeting a specific org.
    ///
    /// The `base_url` should be the full org-scoped URL, e.g.,
    /// `https://alice.atomic.storage`. The `bearer_token` is the
    /// base32-encoded Ed25519 public key.
    pub fn new(base_url: &str, org_slug: &str, bearer_token: &str) -> Result<Self, RemoteError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", bearer_token))
                .map_err(|e| RemoteError::other(format!("invalid bearer token: {}", e)))?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let user_agent = format!("atomic/{}", crate::VERSION);

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .user_agent(user_agent)
            .build()
            .map_err(|e| RemoteError::other(format!("failed to build HTTP client: {}", e)))?;

        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            org_slug: org_slug.to_string(),
        })
    }

    /// The org slug this client is scoped to.
    pub fn org_slug(&self) -> &str {
        &self.org_slug
    }

    /// The base URL this client targets.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    // -----------------------------------------------------------------------
    // Low-level HTTP helpers
    // -----------------------------------------------------------------------

    /// GET request, unwrap `ApiResponse` envelope.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, RemoteError> {
        let url = format!("{}{}", self.base_url, path);
        log::debug!("GET {}", url);

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| RemoteError::other(format!("request failed: {}", e)))?;

        self.handle_response(resp).await
    }

    /// POST request with JSON body, unwrap `ApiResponse` envelope.
    pub async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, RemoteError> {
        let url = format!("{}{}", self.base_url, path);
        log::debug!("POST {}", url);

        let resp = self
            .http
            .post(&url)
            .json(body)
            .send()
            .await
            .map_err(|e| RemoteError::other(format!("request failed: {}", e)))?;

        self.handle_response(resp).await
    }

    /// POST with no request body, unwrap `ApiResponse` envelope.
    pub async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> Result<T, RemoteError> {
        let url = format!("{}{}", self.base_url, path);
        log::debug!("POST {} (no body)", url);

        let resp = self
            .http
            .post(&url)
            .send()
            .await
            .map_err(|e| RemoteError::other(format!("request failed: {}", e)))?;

        self.handle_response(resp).await
    }

    /// PUT request with JSON body, unwrap `ApiResponse` envelope.
    pub async fn put<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, RemoteError> {
        let url = format!("{}{}", self.base_url, path);
        log::debug!("PUT {}", url);

        let resp = self
            .http
            .put(&url)
            .json(body)
            .send()
            .await
            .map_err(|e| RemoteError::other(format!("request failed: {}", e)))?;

        self.handle_response(resp).await
    }

    /// DELETE request, returns `()` on success.
    pub async fn delete(&self, path: &str) -> Result<(), RemoteError> {
        let url = format!("{}{}", self.base_url, path);
        log::debug!("DELETE {}", url);

        let resp = self
            .http
            .delete(&url)
            .send()
            .await
            .map_err(|e| RemoteError::other(format!("request failed: {}", e)))?;

        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(self.parse_error_body(status.as_u16(), &body))
        }
    }

    /// Handle a response: check status, parse `ApiResponse<T>`, unwrap data.
    async fn handle_response<T: DeserializeOwned>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T, RemoteError> {
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| RemoteError::other(format!("failed to read response: {}", e)))?;

        if !status.is_success() {
            return Err(self.parse_error_body(status.as_u16(), &body));
        }

        let api_resp: ApiResponse<T> = serde_json::from_str(&body).map_err(|e| {
            let preview_len = body.len().min(200);
            RemoteError::other(format!(
                "invalid response JSON: {} (body: {})",
                e,
                &body[..preview_len]
            ))
        })?;

        if !api_resp.success {
            if let Some(err) = api_resp.error {
                return Err(RemoteError::other(format!("{}: {}", err.code, err.message)));
            }
            return Err(RemoteError::other("request failed with no error details"));
        }

        api_resp
            .data
            .ok_or_else(|| RemoteError::other("response had success=true but no data"))
    }

    fn parse_error_body(&self, status: u16, body: &str) -> RemoteError {
        // Try to parse as ApiResponse or ApiError first.
        if let Ok(api_resp) = serde_json::from_str::<ApiResponse<serde_json::Value>>(body) {
            if let Some(err) = api_resp.error {
                return match status {
                    401 => RemoteError::unauthorized(err.message),
                    403 => RemoteError::forbidden(err.message),
                    404 => RemoteError::not_found(err.message),
                    409 => RemoteError::conflict(err.message),
                    _ => RemoteError::server_error(status, err.message),
                };
            }
        }
        RemoteError::server_error(status, format!("HTTP {}", status))
    }

    // -----------------------------------------------------------------------
    // Workspace operations
    // -----------------------------------------------------------------------

    /// List all workspaces visible to the authenticated identity.
    pub async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>, RemoteError> {
        self.get("/workspaces").await
    }

    /// Create a new workspace.
    pub async fn create_workspace(
        &self,
        req: &CreateWorkspaceRequest,
    ) -> Result<WorkspaceInfo, RemoteError> {
        self.post("/workspaces", req).await
    }

    /// Get a single workspace by slug.
    pub async fn get_workspace(&self, slug: &str) -> Result<WorkspaceInfo, RemoteError> {
        self.get(&format!("/workspaces/{}", slug)).await
    }

    /// Update an existing workspace.
    pub async fn update_workspace(
        &self,
        slug: &str,
        req: &UpdateWorkspaceRequest,
    ) -> Result<WorkspaceInfo, RemoteError> {
        self.put(&format!("/workspaces/{}", slug), req).await
    }

    /// Delete a workspace by slug.
    pub async fn delete_workspace(&self, slug: &str) -> Result<(), RemoteError> {
        self.delete(&format!("/workspaces/{}", slug)).await
    }

    // -----------------------------------------------------------------------
    // Project operations
    // -----------------------------------------------------------------------

    /// List all projects in a workspace.
    pub async fn list_projects(
        &self,
        workspace_slug: &str,
    ) -> Result<Vec<ProjectInfo>, RemoteError> {
        self.get(&format!("/workspaces/{}/projects", workspace_slug))
            .await
    }

    /// Create a new project in a workspace.
    pub async fn create_project(
        &self,
        workspace_slug: &str,
        req: &CreateProjectRequest,
    ) -> Result<ProjectInfo, RemoteError> {
        self.post(&format!("/workspaces/{}/projects", workspace_slug), req)
            .await
    }

    /// Get a single project by slug.
    pub async fn get_project(
        &self,
        workspace_slug: &str,
        project_slug: &str,
    ) -> Result<ProjectInfo, RemoteError> {
        self.get(&format!(
            "/workspaces/{}/projects/{}",
            workspace_slug, project_slug
        ))
        .await
    }

    /// Update an existing project.
    pub async fn update_project(
        &self,
        workspace_slug: &str,
        project_slug: &str,
        req: &UpdateProjectRequest,
    ) -> Result<ProjectInfo, RemoteError> {
        self.put(
            &format!("/workspaces/{}/projects/{}", workspace_slug, project_slug),
            req,
        )
        .await
    }

    /// Delete a project by slug.
    pub async fn delete_project(
        &self,
        workspace_slug: &str,
        project_slug: &str,
    ) -> Result<(), RemoteError> {
        self.delete(&format!(
            "/workspaces/{}/projects/{}",
            workspace_slug, project_slug
        ))
        .await
    }

    // -----------------------------------------------------------------------
    // Identity resolution
    // -----------------------------------------------------------------------

    /// Resolve an identity by email address.
    pub async fn resolve_identity_by_email(
        &self,
        email: &str,
    ) -> Result<IdentityInfo, RemoteError> {
        self.get(&format!(
            "/identities/resolve?email={}",
            urlencoding::encode(email)
        ))
        .await
    }

    /// Resolve an identity by display name.
    pub async fn resolve_identity_by_name(&self, name: &str) -> Result<IdentityInfo, RemoteError> {
        self.get(&format!(
            "/identities/resolve?name={}",
            urlencoding::encode(name)
        ))
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_trims_trailing_slash() {
        let client = StorageClient::new("https://example.com/", "acme", "tok").unwrap();
        assert_eq!(client.base_url(), "https://example.com");
    }

    #[test]
    fn org_slug_accessor() {
        let client = StorageClient::new("https://example.com", "acme", "tok").unwrap();
        assert_eq!(client.org_slug(), "acme");
    }

    #[test]
    fn parse_error_body_401() {
        let client = StorageClient::new("https://example.com", "acme", "tok").unwrap();
        let body = r#"{"success":false,"error":{"code":"UNAUTHORIZED","message":"bad token"}}"#;
        let err = client.parse_error_body(401, body);
        assert!(err.to_string().contains("bad token"));
        assert!(err.is_auth_error());
    }

    #[test]
    fn parse_error_body_404() {
        let client = StorageClient::new("https://example.com", "acme", "tok").unwrap();
        let body =
            r#"{"success":false,"error":{"code":"NOT_FOUND","message":"workspace not found"}}"#;
        let err = client.parse_error_body(404, body);
        assert!(err.is_not_found());
    }

    #[test]
    fn parse_error_body_non_json() {
        let client = StorageClient::new("https://example.com", "acme", "tok").unwrap();
        let err = client.parse_error_body(502, "Bad Gateway");
        assert!(err.to_string().contains("502"));
    }
}
