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
}

fn default_agentic_root() -> String {
    ".claude".to_string()
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
// Generic error
// ---------------------------------------------------------------------------

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
