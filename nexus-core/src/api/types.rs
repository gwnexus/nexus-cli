//! API response types for the Nexus platform.

use serde::{Deserialize, Serialize};

/// Authentication status returned by the Nexus API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthStatus {
    pub user_id: String,
    pub email: String,
    pub display_name: Option<String>,
    pub platform_role: String,
}

/// Wrapper for auth status API response.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthStatusResponse {
    pub user: AuthStatus,
}

/// Project summary returned by listing endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub status: String,
    pub created_at: String,
}

/// Wrapper for project list API response.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectListResponse {
    pub projects: Vec<ProjectSummary>,
}

/// Generic API error shape returned by the Nexus server.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiError {
    pub error: Option<String>,
    pub message: Option<String>,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(msg) = &self.message {
            write!(f, "{}", msg)
        } else if let Some(err) = &self.error {
            write!(f, "{}", err)
        } else {
            write!(f, "unknown API error")
        }
    }
}
