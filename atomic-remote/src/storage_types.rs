//! Types for the atomic-storage management API.
//!
//! These types model the JSON request/response bodies for CRUD operations
//! on workspaces, projects, and other management endpoints.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Standard API response envelope from atomic-storage.
///
/// All endpoints return this wrapper. On success, `data` is populated.
/// On failure, `error` is populated.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiResponse<T> {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ResponseMetadata>,
}

/// Error payload returned inside an [`ApiResponse`] on failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub details: Option<serde_json::Value>,
}

/// Pagination metadata returned alongside list responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseMetadata {
    pub page: u32,
    pub per_page: u32,
    pub total: u64,
    pub total_pages: u32,
}

/// Visibility for workspaces and projects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Public,
    Private,
}

impl fmt::Display for Visibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Visibility::Public => write!(f, "public"),
            Visibility::Private => write!(f, "private"),
        }
    }
}

impl std::str::FromStr for Visibility {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "public" => Ok(Visibility::Public),
            "private" => Ok(Visibility::Private),
            _ => Err(format!(
                "invalid visibility: '{}' (expected 'public' or 'private')",
                s
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

/// Workspace returned by the management API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInfo {
    pub id: uuid::Uuid,
    pub tenant_id: uuid::Uuid,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    pub owner_id: uuid::Uuid,
    pub visibility: Visibility,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Project returned by the management API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInfo {
    pub id: uuid::Uuid,
    pub workspace_id: uuid::Uuid,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    pub default_view: String,
    pub visibility: Visibility,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Identity info returned by the resolve endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityInfo {
    pub id: uuid::Uuid,
    pub name: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

/// Request body for creating a new workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorkspaceRequest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub visibility: Visibility,
}

/// Request body for updating an existing workspace.
///
/// All fields are optional — only the fields that are present (non-`None`)
/// will be sent to the server.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateWorkspaceRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
}

/// Request body for creating a new project within a workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProjectRequest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub default_view: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub visibility: Visibility,
}

/// Request body for updating an existing project.
///
/// All fields are optional — only the fields that are present (non-`None`)
/// will be sent to the server.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProjectRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_view: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Visibility Display / FromStr --

    #[test]
    fn visibility_display() {
        assert_eq!(Visibility::Public.to_string(), "public");
        assert_eq!(Visibility::Private.to_string(), "private");
    }

    #[test]
    fn visibility_from_str_valid() {
        assert_eq!("public".parse::<Visibility>().unwrap(), Visibility::Public);
        assert_eq!(
            "private".parse::<Visibility>().unwrap(),
            Visibility::Private
        );
        // case-insensitive
        assert_eq!("PUBLIC".parse::<Visibility>().unwrap(), Visibility::Public);
        assert_eq!(
            "Private".parse::<Visibility>().unwrap(),
            Visibility::Private
        );
    }

    #[test]
    fn visibility_from_str_invalid() {
        let err = "nope".parse::<Visibility>().unwrap_err();
        assert!(err.contains("invalid visibility"));
        assert!(err.contains("nope"));
    }

    // -- Visibility serde --

    #[test]
    fn visibility_serde_roundtrip() {
        let v = Visibility::Public;
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, "\"public\"");
        let back: Visibility = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);

        let v = Visibility::Private;
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, "\"private\"");
        let back: Visibility = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);
    }

    // -- ApiResponse envelope --

    #[test]
    fn api_response_success_roundtrip() {
        let resp = ApiResponse {
            success: true,
            data: Some("hello".to_string()),
            error: None,
            metadata: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: ApiResponse<String> = serde_json::from_str(&json).unwrap();
        assert!(back.success);
        assert_eq!(back.data.unwrap(), "hello");
        assert!(back.error.is_none());
    }

    #[test]
    fn api_response_error_roundtrip() {
        let resp: ApiResponse<()> = ApiResponse {
            success: false,
            data: None,
            error: Some(ApiError {
                code: "NOT_FOUND".to_string(),
                message: "workspace not found".to_string(),
                details: None,
            }),
            metadata: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: ApiResponse<()> = serde_json::from_str(&json).unwrap();
        assert!(!back.success);
        let err = back.error.unwrap();
        assert_eq!(err.code, "NOT_FOUND");
        assert_eq!(err.message, "workspace not found");
    }

    #[test]
    fn api_response_camel_case_keys() {
        let resp = ApiResponse {
            success: true,
            data: Some(42u32),
            error: None,
            metadata: Some(ResponseMetadata {
                page: 1,
                per_page: 20,
                total: 100,
                total_pages: 5,
            }),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"perPage\""));
        assert!(json.contains("\"totalPages\""));
        assert!(!json.contains("per_page"));
        assert!(!json.contains("total_pages"));
    }

    #[test]
    fn api_response_missing_optional_fields() {
        // Minimal JSON with only the required `success` field — optional
        // fields should deserialize to None.
        let json = r#"{"success":true}"#;
        let resp: ApiResponse<String> = serde_json::from_str(json).unwrap();
        assert!(resp.success);
        assert!(resp.data.is_none());
        assert!(resp.error.is_none());
        assert!(resp.metadata.is_none());
    }

    // -- CreateWorkspaceRequest --

    #[test]
    fn create_workspace_request_roundtrip() {
        let req = CreateWorkspaceRequest {
            name: "my-workspace".to_string(),
            description: Some("A workspace".to_string()),
            visibility: Visibility::Public,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"name\""));
        let back: CreateWorkspaceRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "my-workspace");
        assert_eq!(back.visibility, Visibility::Public);
    }

    #[test]
    fn create_workspace_request_no_description() {
        let req = CreateWorkspaceRequest {
            name: "ws".to_string(),
            description: None,
            visibility: Visibility::Private,
        };
        let json = serde_json::to_string(&req).unwrap();
        // description should be omitted entirely
        assert!(!json.contains("description"));
    }

    // -- UpdateWorkspaceRequest --

    #[test]
    fn update_workspace_request_skip_none() {
        let req = UpdateWorkspaceRequest {
            name: Some("new-name".to_string()),
            description: None,
            visibility: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"name\""));
        assert!(!json.contains("description"));
        assert!(!json.contains("visibility"));
    }

    #[test]
    fn update_workspace_request_empty() {
        let req = UpdateWorkspaceRequest::default();
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, "{}");
    }

    // -- CreateProjectRequest --

    #[test]
    fn create_project_request_roundtrip() {
        let req = CreateProjectRequest {
            name: "my-project".to_string(),
            description: Some("A project".to_string()),
            default_view: "main".to_string(),
            kind: Some("rust".to_string()),
            visibility: Visibility::Private,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"defaultView\""));
        let back: CreateProjectRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.default_view, "main");
        assert_eq!(back.kind.as_deref(), Some("rust"));
    }

    #[test]
    fn create_project_request_omits_none_optionals() {
        let req = CreateProjectRequest {
            name: "proj".to_string(),
            description: None,
            default_view: "main".to_string(),
            kind: None,
            visibility: Visibility::Public,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("description"));
        assert!(!json.contains("kind"));
    }

    // -- UpdateProjectRequest --

    #[test]
    fn update_project_request_skip_none() {
        let req = UpdateProjectRequest {
            name: None,
            description: None,
            default_view: Some("dev".to_string()),
            visibility: Some(Visibility::Public),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("\"name\""));
        assert!(!json.contains("description"));
        assert!(json.contains("\"defaultView\""));
        assert!(json.contains("\"visibility\""));
    }

    #[test]
    fn update_project_request_empty() {
        let req = UpdateProjectRequest::default();
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, "{}");
    }

    // -- WorkspaceInfo / ProjectInfo camelCase --

    #[test]
    fn workspace_info_camel_case() {
        let json = r#"{
            "id": "00000000-0000-0000-0000-000000000001",
            "tenantId": "00000000-0000-0000-0000-000000000002",
            "name": "workspace",
            "slug": "workspace",
            "description": null,
            "ownerId": "00000000-0000-0000-0000-000000000003",
            "visibility": "public",
            "createdAt": "2025-01-01T00:00:00Z",
            "updatedAt": "2025-01-01T00:00:00Z"
        }"#;
        let ws: WorkspaceInfo = serde_json::from_str(json).unwrap();
        assert_eq!(ws.name, "workspace");
        assert_eq!(ws.visibility, Visibility::Public);

        // Re-serialize and verify camelCase keys
        let out = serde_json::to_string(&ws).unwrap();
        assert!(out.contains("tenantId"));
        assert!(out.contains("ownerId"));
        assert!(out.contains("createdAt"));
        assert!(out.contains("updatedAt"));
        assert!(!out.contains("tenant_id"));
    }

    #[test]
    fn project_info_camel_case() {
        let json = r#"{
            "id": "00000000-0000-0000-0000-000000000010",
            "workspaceId": "00000000-0000-0000-0000-000000000020",
            "name": "project",
            "slug": "project",
            "description": "desc",
            "defaultView": "main",
            "visibility": "private",
            "createdAt": "2025-06-01T12:00:00Z",
            "updatedAt": "2025-06-01T12:00:00Z"
        }"#;
        let proj: ProjectInfo = serde_json::from_str(json).unwrap();
        assert_eq!(proj.default_view, "main");
        assert_eq!(proj.visibility, Visibility::Private);

        let out = serde_json::to_string(&proj).unwrap();
        assert!(out.contains("workspaceId"));
        assert!(out.contains("defaultView"));
        assert!(!out.contains("workspace_id"));
        assert!(!out.contains("default_view"));
    }
}
