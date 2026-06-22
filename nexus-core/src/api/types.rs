//! API response types for the Nexus platform.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Identity (GET /api/mcp/identity)
// ---------------------------------------------------------------------------

/// Project membership entry from the identity endpoint.
/// Note: These come directly from Supabase and use snake_case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectMembership {
    pub project_id: String,
    pub role: String,
}

/// Agent assignment entry from the identity endpoint.
/// Note: These come directly from Supabase and use snake_case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAssignment {
    pub project_id: String,
    pub agent_id: String,
    pub agent_owner: Option<String>,
}

/// Identity response returned by `GET /api/mcp/identity`.
///
/// This is a flat JSON object (not wrapped in a `user` key).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityResponse {
    pub user_id: String,
    pub email: String,
    pub display_name: Option<String>,
    pub is_platform_admin: bool,
    pub is_platform_owner: bool,
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub memberships: Vec<ProjectMembership>,
    #[serde(default)]
    pub agent_assignments: Vec<AgentAssignment>,
}

// ---------------------------------------------------------------------------
// Legacy auth types (kept for backward compat, delegates to IdentityResponse)
// ---------------------------------------------------------------------------

/// Authentication status -- legacy wrapper around IdentityResponse fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthStatus {
    pub user_id: String,
    pub email: String,
    pub display_name: Option<String>,
    pub platform_role: String,
}

impl From<&IdentityResponse> for AuthStatus {
    fn from(id: &IdentityResponse) -> Self {
        let role = if id.is_platform_owner {
            "platform_owner"
        } else if id.is_platform_admin {
            "platform_admin"
        } else {
            "member"
        };
        Self {
            user_id: id.user_id.clone(),
            email: id.email.clone(),
            display_name: id.display_name.clone(),
            platform_role: role.to_string(),
        }
    }
}

/// Wrapper for auth status API response (legacy).
#[derive(Debug, Clone, Deserialize)]
pub struct AuthStatusResponse {
    pub user: AuthStatus,
}

// ---------------------------------------------------------------------------
// Skill list (POST /api/mcp/skills  action=sk_list)
// ---------------------------------------------------------------------------

/// A single skill summary returned by `sk_list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSummary {
    pub id: String,
    pub skill_id: String,
    pub name: String,
    pub description: Option<String>,
    pub status: String,
    pub auto_generate_command: Option<bool>,
    pub command_slug: Option<String>,
    pub version: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// Response from `sk_list` action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillListResponse {
    pub action: String,
    pub count: usize,
    pub skills: Vec<SkillSummary>,
}

// ---------------------------------------------------------------------------
// Skill export (POST /api/mcp/skills  action=sk_export)
// ---------------------------------------------------------------------------

/// Project summary included in the skill export response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillExportProject {
    pub id: String,
    pub slug: String,
    pub name: String,
}

/// A single skill resource file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillResource {
    pub filename: String,
    pub body: String,
}

/// A single exported skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedSkill {
    pub skill_id: String,
    pub name: String,
    pub description: Option<String>,
    pub version: i64,
    pub body: Option<String>,
    pub command_slug: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub resources: Vec<SkillResource>,
}

/// Response from `sk_export` action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillExportResponse {
    pub action: String,
    pub project: SkillExportProject,
    pub skills: Vec<ExportedSkill>,
    pub count: usize,
}

// ---------------------------------------------------------------------------
// Directive export (POST /api/mcp/directives  action=directive_export)
// ---------------------------------------------------------------------------

/// A single exported directive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedDirective {
    pub id: String,
    pub title: String,
    pub body: Option<String>,
    pub category: String,
    pub priority: String,
}

/// Response from `directive_export` action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectiveExportResponse {
    pub action: String,
    pub project: SkillExportProject,
    pub directives: Vec<ExportedDirective>,
    pub count: usize,
}

// ---------------------------------------------------------------------------
// Agent file export (POST /api/mcp/agent-files  action=af_export)
// ---------------------------------------------------------------------------

/// MCP server configuration for a plugin (e.g. task-master-ai).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env_keys: Vec<String>,
}

/// A single exported agent file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedAgentFile {
    pub file_key: String,
    pub target_path: String,
    pub name: String,
    pub description: Option<String>,
    pub category: String,
    pub version: i64,
    pub body: String,
    /// SHA-256 hash of the final exported body (after template substitution + directive injection).
    #[serde(default)]
    pub content_hash: Option<String>,
    /// The agent_file UUID in project_agent_files (for sync operations).
    #[serde(default)]
    pub agent_file_id: Option<String>,
}

/// LLM provider configuration (e.g. DGX Spark local models).
///
/// Stored as opaque JSON — the API delivers the exact opencode.json provider
/// format, so the CLI passes it through without interpretation.
pub type ProviderConfig = serde_json::Value;

/// A prerequisite tool required by this project's plugin configuration.
/// Returned by af_export so the CLI can warn or prompt the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prerequisite {
    /// Binary name (e.g. "rtk", "headroom").
    pub tool: String,
    /// Shell command to verify the tool is available (e.g. "rtk --version").
    pub check_command: String,
    /// Human-readable install hint shown when the tool is missing.
    pub install_hint: String,
    /// Which plugin or feature requires this tool.
    pub required_by: String,
}

/// Response from `af_export` action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentFileExportResponse {
    pub project_id: String,
    pub project_name: String,
    pub agent_files: Vec<ExportedAgentFile>,
    pub count: usize,
    /// The agentic root directory for this project (e.g. ".claude" or ".nexus").
    /// Defaults to ".claude" if not present in the server response.
    #[serde(default = "default_agentic_root")]
    pub agentic_root: String,
    /// Tool flavor: "opencode", "claude-cli", or "both".
    #[serde(default)]
    pub agent_owner: Option<String>,
    /// Active plugins for this project (e.g. ["taskmaster-ai"]).
    #[serde(default)]
    pub plugins: Vec<String>,
    /// Plugin MCP server configs keyed by server name (e.g. "task-master-ai").
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
    /// LLM provider configs keyed by provider name (e.g. "dgx-spark").
    /// The API uses the singular key "provider" matching the opencode.json schema.
    #[serde(default, alias = "providers")]
    pub provider: HashMap<String, ProviderConfig>,
    /// The authenticated API token echoed back from the request.
    /// Use this value directly in opencode.json rather than reading credentials.toml.
    /// Present only when the server supports auth_token echo (v0.7.4+).
    #[serde(default)]
    pub auth_token: Option<String>,
    /// Tools that must be installed for this project's plugins to work.
    /// The CLI should check each and warn/prompt when a binary is missing.
    #[serde(default)]
    pub prerequisites: Vec<Prerequisite>,
}

fn default_agentic_root() -> String {
    ".nexus".to_string()
}

// ---------------------------------------------------------------------------
// Projects
// ---------------------------------------------------------------------------

/// Project summary returned by listing endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub id: String,
    pub name: String,
    pub slug: Option<String>,
    pub description: Option<String>,
    pub status: String,
    pub created_at: String,
    /// Tool flavor: "opencode", "claude-cli", or "both".
    pub agent_owner: Option<String>,
    /// Agentic root directory (e.g. ".claude" or ".nexus").
    pub agentic_root: Option<String>,
    /// Per-project git identity config.
    pub git_config: Option<GitConfig>,
}

/// Per-project git identity settings (auto-applied by init/pull).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitConfig {
    pub user_name: Option<String>,
    pub user_email: Option<String>,
    pub signing_key: Option<String>,
    pub commit_gpgsign: Option<bool>,
}

/// Wrapper for project list API response.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectListResponse {
    pub projects: Vec<ProjectSummary>,
}

/// Single project detail response.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectDetailResponse {
    pub project: ProjectSummary,
}

// ---------------------------------------------------------------------------
// Import (POST /api/mcp/import  action=import)
// ---------------------------------------------------------------------------

/// A detected agentic file to import.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportAgenticFile {
    pub file_key: String,
    pub target_path: String,
    pub body: String,
    pub category: String,
}

/// A directive extracted from an agentic file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportDirective {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub category: String,
    pub priority: String,
    pub source_file: String,
}

/// A referenced document resolved from Markdown links.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportReferencedDoc {
    pub title: String,
    pub body: String,
    pub source_path: String,
}

/// Request payload for `POST /api/mcp/import`.
#[derive(Debug, Clone, Serialize)]
pub struct ImportPayload {
    pub action: String,
    pub project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub agentic_files: Vec<ImportAgenticFile>,
    pub directives: Vec<ImportDirective>,
    pub referenced_docs: Vec<ImportReferencedDoc>,
}

/// Summary counts returned by the import endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct ImportSummary {
    pub agentic_files_ingested: u32,
    pub directives_created: u32,
    pub docs_ingested: u32,
}

/// Response from the import endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct ImportResponse {
    pub action: String,
    pub project_id: String,
    pub summary: ImportSummary,
}

// ---------------------------------------------------------------------------
// Tasks (POST /api/mcp/tasks  action=task_list)
// ---------------------------------------------------------------------------

/// A single task summary returned by `task_list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub description: Option<String>,
    pub status: String,
    pub priority: String,
    pub assignee: Option<String>,
    pub created_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Response from `task_list` action.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskListResponse {
    pub action: String,
    pub project_id: String,
    pub count: usize,
    pub tasks: Vec<TaskSummary>,
}

// ---------------------------------------------------------------------------
// Generic error
// ---------------------------------------------------------------------------

/// A single workspace file in the export response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceScript {
    pub target_path: String,
    pub body: String,
    #[serde(default)]
    pub executable: bool,
}

/// The composed workspace template (devbox.json or similar).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceTemplate {
    pub target_path: String,
    pub body: String,
    pub provider: String,
}

/// Response from `wf_export` action (POST /api/mcp/workspace-files).
/// Legacy v1 format — kept for backward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceExportResponse {
    pub action: String,
    pub project_id: String,
    /// Whether workspace provisioning is enabled for this project.
    /// If false, no workspace files will be provisioned.
    #[serde(default)]
    pub workspace_provisioning_enabled: Option<bool>,
    #[serde(default)]
    pub shadow_mode: bool,
    #[serde(default)]
    pub scripts_path: String,
    pub workspace: Option<WorkspaceTemplate>,
    #[serde(default)]
    pub scripts: Vec<WorkspaceScript>,
    #[serde(default)]
    pub message: Option<String>,
}

// ---------------------------------------------------------------------------
// Workspace v2 – Blueprint + Fork architecture (ADR-0034)
// ---------------------------------------------------------------------------

/// A workspace fork summary from `GET /api/projects/:id/workspace-forks`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceForkSummary {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub source_version: i64,
    #[serde(default)]
    pub shadow_mode: bool,
    #[serde(default)]
    pub upstream_changed: bool,
    #[serde(default)]
    pub scripts_path: Option<String>,
}

/// Response from `GET /api/projects/:id/workspace-forks`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceForksResponse {
    pub forks: Vec<WorkspaceForkSummary>,
    #[serde(default)]
    pub count: usize,
}

/// Export metadata from `POST /api/projects/:id/workspace-forks/:forkId/export`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceForkExportMeta {
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub fork_id: Option<String>,
    #[serde(default)]
    pub workspace_name: Option<String>,
    #[serde(default)]
    pub version: i64,
    #[serde(default)]
    pub shadow_mode: bool,
    #[serde(default)]
    pub scripts_path: String,
    #[serde(default)]
    pub upstream_changed: bool,
}

/// A script entry in the v2 fork export.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceForkExportScript {
    pub path: String,
    pub body: String,
    #[serde(default)]
    pub executable: bool,
}

/// Response from `POST /api/projects/:id/workspace-forks/:forkId/export`
/// or from `POST /api/mcp/agent-files` with `action: "ws_export"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceForkExportResponse {
    pub devbox_json: String,
    #[serde(default)]
    pub scripts: Vec<WorkspaceForkExportScript>,
    pub meta: WorkspaceForkExportMeta,
    /// Present in ws_export (MCP) responses
    #[serde(default)]
    pub project_id: Option<String>,
    /// Present in ws_export (MCP) responses
    #[serde(default)]
    pub fork_id: Option<String>,
    /// Present in ws_export (MCP) responses
    #[serde(default)]
    pub workspace_name: Option<String>,
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

// ---------------------------------------------------------------------------
// Sync protocol (POST /api/mcp/agent-files  action=af_sync_check/af_sync/af_sync_status)
// ---------------------------------------------------------------------------

/// A single file hash entry sent by the client for sync check.
#[derive(Debug, Clone, Serialize)]
pub struct SyncFileHash {
    pub file_key: String,
    pub local_hash: String,
}

/// Per-file sync result from af_sync_check.
#[derive(Debug, Clone, Deserialize)]
pub struct SyncCheckResult {
    pub file_key: String,
    pub status: String,
    pub local_hash: Option<String>,
    pub remote_hash: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Response from `af_sync_check` action.
#[derive(Debug, Clone, Deserialize)]
pub struct SyncCheckResponse {
    pub action: String,
    pub project_id: String,
    pub results: Vec<SyncCheckResult>,
    #[serde(default)]
    pub deprecated_skills: Vec<String>,
}

/// Per-file sync status from af_sync_status.
#[derive(Debug, Clone, Deserialize)]
pub struct SyncStatusEntry {
    pub file_key: String,
    pub name: String,
    pub sync_status: String,
    pub content_hash: Option<String>,
    pub last_synced_at: Option<String>,
    #[serde(default)]
    pub body_override_source: Option<String>,
}

/// Response from `af_sync_status` action.
#[derive(Debug, Clone, Deserialize)]
pub struct SyncStatusResponse {
    pub action: String,
    pub project_id: String,
    pub files: Vec<SyncStatusEntry>,
    pub count: usize,
}

/// Response from `af_sync` action (push or pull direction).
#[derive(Debug, Clone, Deserialize)]
pub struct SyncResponse {
    pub action: String,
    pub project_id: String,
    pub file_key: String,
    pub direction: String,
    pub new_hash: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}
