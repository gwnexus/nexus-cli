//! Nexus API HTTP client.
//!
//! Wraps `reqwest::Client` with Nexus-specific configuration:
//! - HTTPS enforcement
//! - Bearer token authentication
//! - Typed error mapping from HTTP status codes

use reqwest::StatusCode;
use serde_json::json;
use tracing::debug;

use crate::api::types::{
    AgentFileExportResponse, ApiError, AuthStatus, AuthStatusResponse, DirectiveExportResponse,
    IdentityResponse, ProjectDetailResponse, ProjectListResponse, SkillExportResponse,
    SkillListResponse, TaskListResponse,
};
use crate::Error;

/// HTTP client for the Nexus API.
#[derive(Clone)]
pub struct NexusClient {
    client: reqwest::Client,
    base_url: String,
    token: Option<String>,
    machine_id: Option<String>,
}

impl std::fmt::Debug for NexusClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NexusClient")
            .field("base_url", &self.base_url)
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("machine_id", &self.machine_id)
            .finish()
    }
}

impl NexusClient {
    /// Create a new Nexus API client.
    ///
    /// Enforces HTTPS for the base URL unless it targets localhost.
    pub fn new(base_url: &str, token: Option<String>) -> Result<Self, Error> {
        // Allow http for localhost/127.0.0.1 during development
        let is_local = base_url.contains("localhost") || base_url.contains("127.0.0.1");
        if !is_local && !base_url.starts_with("https://") {
            return Err(Error::Config(format!(
                "API URL must use HTTPS: {}",
                base_url
            )));
        }

        let client = reqwest::Client::builder()
            .user_agent(format!(
                "{}/{}",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION")
            ))
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()?;

        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            machine_id: crate::machine::resolve_machine_id().ok(),
        })
    }

    /// Set or replace the authentication token.
    pub fn set_token(&mut self, token: String) {
        self.token = Some(token);
    }

    /// Check authentication status via the MCP identity endpoint.
    ///
    /// Returns a legacy `AuthStatusResponse` wrapper for backward compatibility
    /// with the login/status commands.
    pub async fn auth_status(&self) -> Result<AuthStatusResponse, Error> {
        let identity: IdentityResponse = self.get("/api/mcp/identity").await?;
        let user = AuthStatus::from(&identity);
        Ok(AuthStatusResponse { user })
    }

    /// Get the full identity response from `GET /api/mcp/identity`.
    ///
    /// Returns user info, platform role, project memberships, and agent assignments.
    pub async fn get_identity(&self) -> Result<IdentityResponse, Error> {
        self.get("/api/mcp/identity").await
    }

    /// Export all enabled skills for a project via `POST /api/mcp/skills`.
    ///
    /// Calls the `sk_export` action on the MCP skills endpoint.
    pub async fn export_skills(&self, project_id: &str) -> Result<SkillExportResponse, Error> {
        let body = json!({
            "action": "sk_export",
            "project_id": project_id
        });
        self.post("/api/mcp/skills", &body).await
    }

    /// List all skills for the current tenant via `POST /api/mcp/skills`.
    ///
    /// Calls the `sk_list` action on the MCP skills endpoint.
    pub async fn list_skills(
        &self,
        status_filter: Option<&[String]>,
        limit: Option<u32>,
    ) -> Result<SkillListResponse, Error> {
        let mut body = json!({ "action": "sk_list" });
        if let Some(statuses) = status_filter {
            body["status_filter"] = json!(statuses);
        }
        if let Some(lim) = limit {
            body["limit"] = json!(lim);
        }
        self.post("/api/mcp/skills", &body).await
    }

    /// Export all enabled directives for a project via `POST /api/mcp/directives`.
    ///
    /// Calls the `directive_export` action on the MCP directives endpoint.
    pub async fn export_directives(
        &self,
        project_id: &str,
    ) -> Result<DirectiveExportResponse, Error> {
        let body = json!({
            "action": "directive_export",
            "project_id": project_id
        });
        self.post("/api/mcp/directives", &body).await
    }

    /// Export all active agent files for a project via `POST /api/mcp/agent-files`.
    ///
    /// Calls the `af_export` action on the MCP agent-files endpoint.
    /// Returns agent files with template variables substituted for the given project.
    pub async fn export_agent_files(
        &self,
        project_id: &str,
    ) -> Result<AgentFileExportResponse, Error> {
        let body = json!({
            "action": "af_export",
            "project_id": project_id
        });
        self.post("/api/mcp/agent-files", &body).await
    }

    /// Import agentic files, directives, and referenced docs into a project.
    ///
    /// Calls `POST /api/mcp/import` with the full import payload.
    pub async fn import(
        &self,
        payload: &crate::api::types::ImportPayload,
    ) -> Result<crate::api::types::ImportResponse, Error> {
        self.post("/api/mcp/import", payload).await
    }

    /// List all projects accessible to the authenticated user.
    pub async fn list_projects(&self) -> Result<ProjectListResponse, Error> {
        self.get("/api/mcp/projects").await
    }

    /// List tasks for a project via `POST /api/mcp/tasks` with `task_list` action.
    ///
    /// By default fetches open/in-progress/blocked tasks (non-terminal).
    /// Pass `status_filter` to override.
    pub async fn list_tasks(
        &self,
        project_id: &str,
        status_filter: Option<&[&str]>,
    ) -> Result<TaskListResponse, Error> {
        let mut body = json!({
            "action": "task_list",
            "project_id": project_id,
            "limit": 100
        });
        if let Some(statuses) = status_filter {
            body["status_filter"] = json!(statuses);
        }
        self.post("/api/mcp/tasks", &body).await
    }

    /// Get a single project by ID to validate access.
    pub async fn get_project(&self, project_id: &str) -> Result<ProjectDetailResponse, Error> {
        let path = format!("/api/mcp/projects/{}", project_id);
        self.get(&path).await
    }

    /// Send a GET request and deserialize the JSON response.
    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        let url = format!("{}{}", self.base_url, path);
        debug!("GET {}", url);

        let mut req = self.client.get(&url);
        if let Some(ref token) = self.token {
            req = req.bearer_auth(token);
        }
        if let Some(ref mid) = self.machine_id {
            req = req.header("X-Nexus-Machine-Id", mid);
        }

        let resp = req.send().await?;
        self.handle_response(resp).await
    }

    /// Send a POST request with a JSON body and deserialize the response.
    async fn post<T, B>(&self, path: &str, body: &B) -> Result<T, Error>
    where
        T: serde::de::DeserializeOwned,
        B: serde::Serialize,
    {
        let url = format!("{}{}", self.base_url, path);
        debug!("POST {}", url);

        let mut req = self.client.post(&url).json(body);
        if let Some(ref token) = self.token {
            req = req.bearer_auth(token);
        }
        if let Some(ref mid) = self.machine_id {
            req = req.header("X-Nexus-Machine-Id", mid);
        }

        let resp = req.send().await?;
        self.handle_response(resp).await
    }

    /// Map HTTP response status to typed errors or deserialize the body.
    async fn handle_response<T: serde::de::DeserializeOwned>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T, Error> {
        let status = resp.status();

        if status.is_success() {
            return Ok(resp.json().await?);
        }

        // Try to extract a structured error message from the body
        let error_msg = match resp.json::<ApiError>().await {
            Ok(api_err) => api_err.to_string(),
            Err(_) => format!("HTTP {}", status),
        };

        match status {
            StatusCode::UNAUTHORIZED => Err(Error::Unauthorized(error_msg)),
            StatusCode::FORBIDDEN => Err(Error::Forbidden(error_msg)),
            StatusCode::NOT_FOUND => Err(Error::NotFound(error_msg)),
            _ => Err(Error::Api(error_msg)),
        }
    }
}
