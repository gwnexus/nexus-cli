//! Nexus API HTTP client.
//!
//! Wraps `reqwest::Client` with Nexus-specific configuration:
//! - HTTPS enforcement
//! - Bearer token authentication
//! - Typed error mapping from HTTP status codes

use reqwest::{StatusCode, Url};
use serde_json::json;
use tracing::debug;

use crate::api::types::{
    ActorAvatarResponse, ActorExportResponse, ActorGetResponse, ActorImportPayload,
    ActorImportResponse, ActorListResponse, AgentFileExportResponse, ApiError, AuthStatus,
    AuthStatusResponse, DirectiveExportResponse, FileStatusResponse, IdentityResponse,
    InferenceTokenInfo, InferenceTokenIssueRequest, InferenceTokenResponse, ProjectDetailResponse,
    ProjectListResponse, SkillExportResponse, SkillListResponse, SyncCheckResponse, SyncFileHash,
    SyncResponse, SyncStatusResponse, TaskListResponse, WorkspaceExportResponse,
    WorkspaceForkExportResponse, WorkspaceForksResponse, WorkspacePushResponse,
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
        let is_local = match Url::parse(base_url) {
            Ok(u) => matches!(u.host_str(), Some("localhost") | Some("127.0.0.1")),
            Err(_) => false,
        };
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

    /// Export composed workspace files for a project via `POST /api/mcp/workspace-files`.
    ///
    /// Calls the `wf_export` action. Returns the merged devbox.json + scripts
    /// with template variables substituted and S3 content resolved.
    /// **Legacy v1** — kept for backward compatibility.
    pub async fn export_workspace(
        &self,
        project_id: &str,
    ) -> Result<WorkspaceExportResponse, Error> {
        let body = json!({
            "action": "wf_export",
            "project_id": project_id
        });
        self.post("/api/mcp/workspace-files", &body).await
    }

    // -- Workspace v2 (Blueprint + Fork) ------------------------------------

    /// Export workspace files via MCP-authenticated `ws_export` action.
    /// This is the preferred method — uses PAT auth via `/api/mcp/agent-files`.
    pub async fn export_workspace_mcp(
        &self,
        project_id: &str,
    ) -> Result<WorkspaceForkExportResponse, Error> {
        let body = json!({
            "action": "ws_export",
            "project_id": project_id
        });
        self.post("/api/mcp/agent-files", &body).await
    }

    /// List workspace forks for a project.
    /// NOTE: This endpoint uses session-auth and may fail with PAT tokens.
    /// Prefer `export_workspace_mcp` instead.
    pub async fn list_workspace_forks(
        &self,
        project_id: &str,
    ) -> Result<WorkspaceForksResponse, Error> {
        let path = format!("/api/projects/{}/workspace-forks", project_id);
        self.get(&path).await
    }

    /// Export a single workspace fork for CLI consumption.
    pub async fn export_workspace_fork(
        &self,
        project_id: &str,
        fork_id: &str,
    ) -> Result<WorkspaceForkExportResponse, Error> {
        let path = format!(
            "/api/projects/{}/workspace-forks/{}/export",
            project_id, fork_id
        );
        self.post(&path, &json!({})).await
    }

    // -- Actors ---------------------------------------------------------------

    /// List actors assigned to a project via `POST /api/mcp/actors`.
    ///
    /// Calls the `actor_list` action.
    pub async fn list_actors(&self, project_id: &str) -> Result<ActorListResponse, Error> {
        let body = json!({
            "action": "actor_list",
            "project_id": project_id
        });
        self.post("/api/mcp/actors", &body).await
    }

    /// Get a single actor profile via `POST /api/mcp/actors`.
    ///
    /// Calls the `actor_get` action. Accepts slug or UUID.
    pub async fn get_actor(
        &self,
        project_id: &str,
        actor_slug: &str,
    ) -> Result<ActorGetResponse, Error> {
        let body = json!({
            "action": "actor_get",
            "project_id": project_id,
            "actor_slug": actor_slug
        });
        self.post("/api/mcp/actors", &body).await
    }

    /// Trigger avatar regeneration for an actor via `POST /api/mcp/actors`.
    ///
    /// Calls the `actor_avatar_generate` action.
    pub async fn generate_actor_avatar(
        &self,
        project_id: &str,
        actor_slug: &str,
    ) -> Result<ActorAvatarResponse, Error> {
        let body = json!({
            "action": "actor_avatar_generate",
            "project_id": project_id,
            "actor_slug": actor_slug
        });
        self.post("/api/mcp/actors", &body).await
    }

    /// Reset actor avatar to DiceBear default via `POST /api/mcp/actors`.
    ///
    /// Calls the `actor_avatar_reset` action.
    pub async fn reset_actor_avatar(
        &self,
        project_id: &str,
        actor_slug: &str,
    ) -> Result<ActorAvatarResponse, Error> {
        let body = json!({
            "action": "actor_avatar_reset",
            "project_id": project_id,
            "actor_slug": actor_slug
        });
        self.post("/api/mcp/actors", &body).await
    }

    /// Download an actor avatar SVG from the provided URL.
    ///
    /// Returns the SVG content as bytes. Used by `nexus pull --with-actor-assets`.
    /// Only allows downloads from the same host as the configured API base URL.
    pub async fn download_actor_avatar(&self, url: &str) -> Result<Vec<u8>, Error> {
        // Validate URL domain matches the API host to prevent SSRF
        if let (Ok(avatar_url), Ok(api_url)) = (Url::parse(url), Url::parse(&self.base_url)) {
            let api_host = api_url.host_str().unwrap_or("");
            let avatar_host = avatar_url.host_str().unwrap_or("");
            // Allow same host or known CDN subdomains
            if avatar_host != api_host
                && !avatar_host.ends_with(&format!(".{}", api_host))
                && !avatar_host.ends_with(".supabase.co")
                && !avatar_host.ends_with(".supabase.in")
            {
                return Err(Error::Api(format!(
                    "Avatar URL host '{}' not allowed (expected '{}')",
                    avatar_host, api_host
                )));
            }
        } else {
            return Err(Error::Api(format!("Invalid avatar URL: {}", url)));
        }

        let resp = self.client.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Api(format!(
                "Avatar download failed: HTTP {}",
                resp.status()
            )));
        }
        Ok(resp.bytes().await?.to_vec())
    }

    /// Import actor profiles into the Actor Registry via `POST /api/mcp/actors`.
    ///
    /// Calls the `actor_import` action.
    pub async fn import_actors(
        &self,
        payload: &ActorImportPayload,
    ) -> Result<ActorImportResponse, Error> {
        self.post("/api/mcp/actors", payload).await
    }

    /// Export actor configuration for a target format via `POST /api/mcp/actors`.
    ///
    /// Calls the `actor_export` action. Target: "opencode" for opencode.json agent section.
    pub async fn export_actors(
        &self,
        project_id: &str,
        target: &str,
    ) -> Result<ActorExportResponse, Error> {
        let body = json!({
            "action": "actor_export",
            "project_id": project_id,
            "target": target
        });
        self.post("/api/mcp/actors", &body).await
    }

    // -- Sync protocol (ADR-0036) -------------------------------------------

    /// Check sync status by comparing local file hashes against the platform.
    ///
    /// Calls `af_sync_check` action on the MCP agent-files endpoint.
    pub async fn sync_check(
        &self,
        project_id: &str,
        files: &[SyncFileHash],
    ) -> Result<SyncCheckResponse, Error> {
        let body = json!({
            "action": "af_sync_check",
            "project_id": project_id,
            "files": files
        });
        self.post("/api/mcp/agent-files", &body).await
    }

    /// Push or pull a single file via the sync protocol.
    ///
    /// Calls `af_sync` action on the MCP agent-files endpoint.
    /// Direction: "push" (local → remote) or "pull" (remote → local).
    pub async fn sync_file(
        &self,
        project_id: &str,
        file_key: &str,
        direction: &str,
        body_content: Option<&str>,
        local_hash: Option<&str>,
    ) -> Result<SyncResponse, Error> {
        let mut payload = json!({
            "action": "af_sync",
            "project_id": project_id,
            "file_key": file_key,
            "direction": direction
        });
        if let Some(content) = body_content {
            payload["body"] = json!(content);
        }
        if let Some(hash) = local_hash {
            payload["local_hash"] = json!(hash);
        }
        self.post("/api/mcp/agent-files", &payload).await
    }

    /// Get bulk sync status for all project agent files.
    ///
    /// Calls `af_sync_status` action on the MCP agent-files endpoint.
    pub async fn sync_status(&self, project_id: &str) -> Result<SyncStatusResponse, Error> {
        let body = json!({
            "action": "af_sync_status",
            "project_id": project_id
        });
        self.post("/api/mcp/agent-files", &body).await
    }

    /// List open sessions for a project via `POST /api/mcp/sessions`.
    pub async fn list_sessions(&self, project_id: &str) -> Result<serde_json::Value, Error> {
        let body = json!({
            "action": "session_list",
            "project_id": project_id
        });
        self.post("/api/mcp/sessions", &body).await
    }

    /// Get a full session (with entries) via `POST /api/mcp/sessions`.
    pub async fn get_session(&self, entity_id: &str) -> Result<serde_json::Value, Error> {
        let body = json!({
            "action": "kb_get",
            "entity_type": "session",
            "entity_id": entity_id,
            "render_mode": "structured"
        });
        self.post("/api/mcp/kb", &body).await
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

    /// Send a DELETE request and deserialize the JSON response.
    ///
    /// Endpoints that return an empty body should be requested with
    /// `T = serde_json::Value`, which tolerates `null`/empty responses.
    async fn delete<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        let url = format!("{}{}", self.base_url, path);
        debug!("DELETE {}", url);

        let mut req = self.client.delete(&url);
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

    /// Push workspace changes (devbox.json + scripts) as a new fork.
    pub async fn workspace_push(
        &self,
        project_id: &str,
        devbox_json: Option<serde_json::Value>,
        script_files: Option<std::collections::HashMap<String, String>>,
        fork_name: Option<&str>,
    ) -> Result<WorkspacePushResponse, Error> {
        let mut payload = json!({
            "action": "ws_push",
            "project_id": project_id,
        });
        if let Some(dj) = devbox_json {
            payload["devbox_json"] = dj;
        }
        if let Some(sf) = script_files {
            payload["script_files"] = serde_json::to_value(sf).unwrap_or_default();
        }
        if let Some(name) = fork_name {
            payload["fork_name"] = serde_json::Value::String(name.to_string());
        }
        self.post("/api/mcp/agent-files", &payload).await
    }

    /// Compare local file hashes against server-side content hashes.
    pub async fn file_status(
        &self,
        project_id: &str,
        local_hashes: std::collections::HashMap<String, String>,
    ) -> Result<FileStatusResponse, Error> {
        let payload = json!({
            "action": "af_status",
            "project_id": project_id,
            "local_hashes": local_hashes,
        });
        self.post("/api/mcp/agent-files", &payload).await
    }

    // -- Project inference tokens (nxs_proj_*) — gateway ADR-0005 -----------

    /// Issue a new project inference token via
    /// `POST /api/projects/:projectId/inference-tokens`.
    ///
    /// PAT-authenticated. The server enforces `project.manage` membership.
    /// The raw token in the response is returned exactly once.
    pub async fn issue_inference_token(
        &self,
        project_id: &str,
        request: &InferenceTokenIssueRequest,
    ) -> Result<InferenceTokenResponse, Error> {
        let path = format!("/api/projects/{}/inference-tokens", project_id);
        self.post(&path, request).await
    }

    /// Rotate an existing project inference token via
    /// `POST /api/projects/:projectId/inference-tokens/:tokenId`.
    ///
    /// The new token inherits the runtime, capabilities, and ceilings of the
    /// old one. The old token stays valid during an overlapping window until
    /// it is revoked.
    pub async fn rotate_inference_token(
        &self,
        project_id: &str,
        token_id: &str,
    ) -> Result<InferenceTokenResponse, Error> {
        let path = format!("/api/projects/{}/inference-tokens/{}", project_id, token_id);
        self.post(&path, &json!({})).await
    }

    /// Revoke (soft-delete) a project inference token via
    /// `DELETE /api/projects/:projectId/inference-tokens/:tokenId`.
    pub async fn revoke_inference_token(
        &self,
        project_id: &str,
        token_id: &str,
    ) -> Result<(), Error> {
        let path = format!("/api/projects/{}/inference-tokens/{}", project_id, token_id);
        let _: serde_json::Value = self.delete(&path).await?;
        Ok(())
    }

    /// List project inference tokens via
    /// `GET /api/projects/:projectId/inference-tokens`.
    ///
    /// Tolerant of both a bare array response and an object with a `tokens`
    /// field. Never returns raw secrets.
    pub async fn list_inference_tokens(
        &self,
        project_id: &str,
    ) -> Result<Vec<InferenceTokenInfo>, Error> {
        let path = format!("/api/projects/{}/inference-tokens", project_id);
        let value: serde_json::Value = self.get(&path).await?;
        let tokens_value = match value {
            serde_json::Value::Array(_) => value,
            serde_json::Value::Object(ref map) => map
                .get("tokens")
                .cloned()
                .unwrap_or(serde_json::Value::Array(Vec::new())),
            _ => serde_json::Value::Array(Vec::new()),
        };
        let tokens: Vec<InferenceTokenInfo> = serde_json::from_value(tokens_value)?;
        Ok(tokens)
    }
}
