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

/// MCP server configuration for a plugin (e.g. task-master-ai, nexus-headroom).
///
/// The `command` field is normalised to a `Vec<String>` on deserialisation.
/// The API may send it as either a plain string (`"headroom"`) or an array
/// (`["headroom", "mcp", "serve"]`).  Both forms are accepted via the custom
/// `StringOrVec` deserializer so that the CLI never fails on the array form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Executable + optional sub-command arguments from the platform.
    /// Deserialized from either `"cmd"` (string) or `["cmd", "arg1", ...]` (array).
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    pub command: Vec<String>,
    /// Extra arguments appended after `command` when building the full argv.
    #[serde(default)]
    pub args: Vec<String>,
    /// Secret env-var names whose values are resolved at runtime via `{env:KEY}`.
    #[serde(default)]
    pub env_keys: Vec<String>,
    /// Inline environment variables delivered directly (e.g. HEADROOM_* from
    /// the platform).  Written into opencode.json `environment` block verbatim.
    #[serde(default)]
    pub environment: std::collections::HashMap<String, String>,
}

/// Serde helper: deserialise a JSON value that is either a plain string or an
/// array of strings into `Vec<String>`.
///
/// * `"headroom"` → `["headroom"]`
/// * `["headroom", "mcp", "serve"]` → `["headroom", "mcp", "serve"]`
fn deserialize_string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{SeqAccess, Visitor};
    use std::fmt;

    struct StringOrVec;

    impl<'de> Visitor<'de> for StringOrVec {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "a string or an array of strings")
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Vec<String>, E> {
            Ok(vec![v.to_string()])
        }

        fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Vec<String>, E> {
            Ok(vec![v])
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<String>, A::Error> {
            let mut out = Vec::new();
            while let Some(s) = seq.next_element::<String>()? {
                out.push(s);
            }
            Ok(out)
        }
    }

    deserializer.deserialize_any(StringOrVec)
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
    /// Actors assigned to this project (delivered as profile markdown files).
    /// Written to `<agentic_root>/actors/<slug>.md` during pull.
    #[serde(default)]
    pub actors: Vec<ExportedActorFile>,
    /// Flat map of non-sensitive, platform-managed env vars for `.nexus/env`.
    /// Written by `nexus pull` / `nexus init`; read by `nexus run` and dbx_init.sh.
    /// Absent when no plugin provides env vars.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub plugin_env: HashMap<String, String>,
    /// OpenCode agent configs to merge into `opencode.json` `"agent"` section.
    /// Delivered by the backend when actors have opencode-compatible agent definitions.
    #[serde(default)]
    pub opencode_agents: Option<serde_json::Value>,
    /// Global default model for opencode.json (ADR-0057: local-first, DGX Spark).
    /// Written as top-level `"model"` key in opencode.json when present.
    #[serde(default)]
    pub opencode_default_model: Option<String>,
    /// Default agent for opencode.json (ADR-0058: "nexus-plan" for actor-based projects).
    #[serde(default)]
    pub opencode_default_agent: Option<String>,
    /// Model routes map for .nexus/generated/model-routes.json (ADR-0057).
    /// Key is route alias, value is route metadata.
    #[serde(default)]
    pub model_routes: Option<serde_json::Value>,
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

// ---------------------------------------------------------------------------
// Workspace Push (POST /api/mcp/agent-files  action=ws_push)
// ---------------------------------------------------------------------------

/// Response from `ws_push` action — push local workspace changes as a new fork.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspacePushResponse {
    pub action: String,
    pub project_id: String,
    pub fork_id: String,
    pub fork_name: String,
    pub version: i64,
    pub previous_fork_id: String,
    pub previous_fork_name: String,
    pub files_pushed: Vec<String>,
}

// ---------------------------------------------------------------------------
// Agent File Status (POST /api/mcp/agent-files  action=af_status)
// ---------------------------------------------------------------------------

/// A file that differs between local and remote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusModifiedFile {
    pub path: String,
    pub local_hash: String,
    pub remote_hash: String,
    pub category: String,
}

/// A file that exists locally but not on the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusNewFile {
    pub path: String,
}

/// A file that exists on the server but not locally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusDeletedFile {
    pub path: String,
    pub remote_hash: String,
    pub category: String,
}

/// A file that is unchanged between local and remote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusUnchangedFile {
    pub path: String,
    pub category: String,
}

/// Response from `af_status` action — compare local vs server file hashes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileStatusResponse {
    pub action: String,
    pub project_id: String,
    pub modified: Vec<StatusModifiedFile>,
    pub new_local: Vec<StatusNewFile>,
    pub deleted_local: Vec<StatusDeletedFile>,
    pub unchanged: Vec<StatusUnchangedFile>,
    #[serde(default)]
    pub server_file_count: usize,
}

// ---------------------------------------------------------------------------
// Actors (POST /api/mcp/actors  action=actor_list / actor_get)
// ---------------------------------------------------------------------------

/// Avatar metadata for an actor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorAvatar {
    /// Avatar style (e.g. "dicebear", "custom", "ai-generated").
    #[serde(default)]
    pub style: Option<String>,
    /// Seed used for DiceBear generation.
    #[serde(default)]
    pub seed: Option<String>,
    /// S3/CDN URL for the cached avatar SVG.
    #[serde(default)]
    pub url: Option<String>,
    /// Content hash of the avatar SVG (for cache invalidation).
    #[serde(default)]
    pub content_hash: Option<String>,
}

/// A single actor summary returned by `actor_list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorSummary {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub role: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub model_routing: Option<String>,
    #[serde(default)]
    pub avatar: Option<ActorAvatar>,
    #[serde(default)]
    pub status: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Response from `actor_list` action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorListResponse {
    pub action: String,
    pub project_id: String,
    pub count: usize,
    pub actors: Vec<ActorSummary>,
}

/// Full actor profile returned by `actor_get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorProfile {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub role: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Full Markdown profile body for the actor.
    #[serde(default)]
    pub profile_body: Option<String>,
    #[serde(default)]
    pub model_routing: Option<String>,
    #[serde(default)]
    pub permissions: Option<serde_json::Value>,
    #[serde(default)]
    pub avatar: Option<ActorAvatar>,
    #[serde(default)]
    pub status: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Response from `actor_get` action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorGetResponse {
    pub action: String,
    pub project_id: String,
    pub actor: ActorProfile,
}

/// Response from actor avatar operations (generate/reset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorAvatarResponse {
    pub action: String,
    pub actor_id: String,
    #[serde(default)]
    pub avatar: Option<ActorAvatar>,
    #[serde(default)]
    pub message: Option<String>,
}

/// An exported actor file entry from af_export (actor profiles delivered during pull).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedActorFile {
    pub slug: String,
    pub name: String,
    pub role: String,
    /// Markdown content for `.nexus/actors/<slug>.md`
    pub body: String,
    #[serde(default)]
    pub avatar: Option<ActorAvatar>,
    /// Model route alias used by this actor (ADR-0055).
    #[serde(default)]
    pub route_alias: Option<String>,
}

// ---------------------------------------------------------------------------
// Model Routes (ADR-0055)
// ---------------------------------------------------------------------------

/// A model route entry from the route catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRoute {
    pub alias: String,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub deprecated: bool,
    #[serde(default)]
    pub deprecated_message: Option<String>,
}

// ---------------------------------------------------------------------------
// Actor import (POST /api/mcp/actors  action=actor_import)
// ---------------------------------------------------------------------------

/// A single actor profile to import into the Actor Registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorImportEntry {
    pub slug: String,
    pub name: String,
    pub role: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub model_routing: Option<String>,
    #[serde(default)]
    pub route_alias: Option<String>,
    /// Full Markdown profile body.
    #[serde(default)]
    pub profile_body: Option<String>,
}

/// Request payload for actor import.
#[derive(Debug, Clone, Serialize)]
pub struct ActorImportPayload {
    pub action: String,
    pub project_id: String,
    pub actors: Vec<ActorImportEntry>,
}

/// Response from actor import.
#[derive(Debug, Clone, Deserialize)]
pub struct ActorImportResponse {
    pub action: String,
    pub project_id: String,
    pub imported: usize,
    #[serde(default)]
    pub message: Option<String>,
}

/// Response from actor export action.
#[derive(Debug, Clone, Deserialize)]
pub struct ActorExportResponse {
    pub action: String,
    pub project_id: String,
    /// OpenCode-compatible agent configuration.
    #[serde(default)]
    pub opencode_agents: Option<serde_json::Value>,
    #[serde(default)]
    pub count: usize,
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

// ---------------------------------------------------------------------------
// Project inference tokens (nxs_proj_*) — gateway ADR-0005
// ---------------------------------------------------------------------------

/// Optional profile ceiling for an issued project inference token.
///
/// Tighten-only: `inherit` keeps the project policy, `restrict` narrows it to
/// the listed profile slugs. Serialized only when explicitly requested.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileCeiling {
    /// Ceiling mode: `inherit` or `restrict`.
    pub mode: String,

    /// Profile slugs the token is restricted to (only meaningful for `restrict`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<String>,
}

/// Request body for `POST /api/projects/:projectId/inference-tokens`.
///
/// `runtime_id` is a logical runtime name (e.g. `developer-workstation`),
/// not a device attestation. All other fields are optional and default to
/// inheriting the project policy server-side.
#[derive(Debug, Clone, Serialize)]
pub struct InferenceTokenIssueRequest {
    pub runtime_id: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_ceiling: Option<ProfileCeiling>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_ceiling_ref: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// Response from issuing or rotating a project inference token.
///
/// The raw `token` value is returned exactly once and can never be retrieved
/// again. Callers must persist it immediately.
#[derive(Debug, Clone, Deserialize)]
pub struct InferenceTokenResponse {
    /// The raw `nxs_proj_*` token. Shown once, never retrievable again.
    pub token: String,

    /// Stable identifier for the token record (used for rotate/revoke).
    pub token_id: String,

    /// Non-secret display prefix (safe to print / store in listings).
    #[serde(default)]
    pub token_prefix: Option<String>,

    /// Logical runtime name the token was issued for.
    #[serde(default)]
    pub runtime_id: Option<String>,

    /// Optional ISO 8601 expiry timestamp.
    #[serde(default)]
    pub expires_at: Option<String>,

    /// Optional server-side advisory (e.g. missing-expiry warning).
    #[serde(default)]
    pub warning: Option<String>,
}

/// A single project inference token record as returned by the list endpoint.
///
/// Never contains the raw secret — only metadata.
#[derive(Debug, Clone, Deserialize)]
pub struct InferenceTokenInfo {
    pub token_id: String,

    #[serde(default)]
    pub token_prefix: Option<String>,

    #[serde(default)]
    pub runtime_id: Option<String>,

    /// Lifecycle status: `active`, `expired`, or `revoked`.
    #[serde(default)]
    pub status: Option<String>,

    #[serde(default)]
    pub created_at: Option<String>,

    #[serde(default)]
    pub last_used_at: Option<String>,

    #[serde(default)]
    pub expires_at: Option<String>,
}

/// Response from `GET /api/projects/:projectId/inference-tokens`.
///
/// The server may return either a bare array or an object with a `tokens`
/// field; `#[serde(default)]` keeps deserialization tolerant.
#[derive(Debug, Clone, Deserialize)]
pub struct InferenceTokenListResponse {
    #[serde(default)]
    pub tokens: Vec<InferenceTokenInfo>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // T1: McpServerConfig with string command deserializes to Vec<String>
    #[test]
    fn test_mcp_server_config_string_command() {
        let json = r#"{"command": "headroom"}"#;
        let cfg: McpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.command, vec!["headroom"]);
        assert!(cfg.args.is_empty());
        assert!(cfg.env_keys.is_empty());
        assert!(cfg.environment.is_empty());
    }

    // T2: McpServerConfig with array command deserializes to Vec<String>
    #[test]
    fn test_mcp_server_config_array_command() {
        let json = r#"{"command": ["headroom", "mcp", "serve"]}"#;
        let cfg: McpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.command, vec!["headroom", "mcp", "serve"]);
    }

    // T3: McpServerConfig with environment map deserializes correctly
    #[test]
    fn test_mcp_server_config_with_environment() {
        let json = r#"{
            "command": ["headroom", "mcp", "serve"],
            "environment": {
                "HEADROOM_MODE": "transform",
                "HEADROOM_DEBUG": "false"
            }
        }"#;
        let cfg: McpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.command, vec!["headroom", "mcp", "serve"]);
        assert_eq!(
            cfg.environment.get("HEADROOM_MODE").map(|s| s.as_str()),
            Some("transform")
        );
        assert_eq!(
            cfg.environment.get("HEADROOM_DEBUG").map(|s| s.as_str()),
            Some("false")
        );
    }

    // T4: Full af_export with nexus-headroom (array command + environment) round-trips
    #[test]
    fn test_af_export_with_headroom_mcp_server() {
        let json = r#"{
            "project_id": "test-proj-id",
            "project_name": "Test Project",
            "agent_files": [],
            "count": 0,
            "mcp_servers": {
                "nexus-headroom": {
                    "command": ["headroom", "mcp", "serve"],
                    "args": [],
                    "env_keys": [],
                    "environment": {
                        "HEADROOM_MODE": "transform",
                        "HEADROOM_REQUIRE_PREFLIGHT": "true"
                    }
                }
            },
            "plugin_env": {
                "HEADROOM_MODE": "transform"
            }
        }"#;
        let resp: AgentFileExportResponse = serde_json::from_str(json).unwrap();
        let headroom = resp.mcp_servers.get("nexus-headroom").unwrap();
        assert_eq!(headroom.command, vec!["headroom", "mcp", "serve"]);
        assert_eq!(
            headroom
                .environment
                .get("HEADROOM_MODE")
                .map(|s| s.as_str()),
            Some("transform")
        );
        assert_eq!(
            headroom
                .environment
                .get("HEADROOM_REQUIRE_PREFLIGHT")
                .map(|s| s.as_str()),
            Some("true")
        );
        assert_eq!(
            resp.plugin_env.get("HEADROOM_MODE").map(|s| s.as_str()),
            Some("transform")
        );
    }
}
