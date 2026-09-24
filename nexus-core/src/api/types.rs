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
    /// Paths to merge into `opencode.json`'s top-level `"instructions"` array.
    /// Typically `["<agentic_root>/AGENTS.md"]`, so OpenCode loads the
    /// project's agent policy deterministically at session start instead of
    /// relying on its own upward AGENTS.md auto-discovery, which has no
    /// awareness of the `agentic_root` convention (default `.nexus/`).
    #[serde(default)]
    pub opencode_instructions: Option<Vec<String>>,
    /// Warnings about model routing / provider configuration diverging from
    /// what the generated `opencode.json` can actually serve (e.g. an agent's
    /// model uses a provider Nexus cannot verify, or a route-alias migration
    /// hasn't been applied on this backend). Absent when there is nothing to
    /// report. The CLI renders these verbatim and gates the `opencode.json`
    /// write on operator confirmation rather than re-deriving the analysis
    /// client-side (only the backend knows execution_mode/agent_mode/gateway
    /// availability).
    #[serde(default)]
    pub export_warnings: Option<Vec<ExportWarning>>,
    /// Runtime-neutral project spec (ADR-C04/F1, nexus-app commit db5d053).
    /// Additive field: a `schema_version`-tagged JSON object projecting the
    /// same underlying data (`model_routes`, `actors`, `primary_agents`,
    /// `terminal_runtime`, etc.) into a shape shared by all terminal
    /// renderers. Kept as an untyped `serde_json::Value` since the schema is
    /// server-owned and may gain fields; consumers should read only the
    /// sub-keys they need rather than assuming an exhaustive shape.
    #[serde(default)]
    pub runtime_spec: Option<serde_json::Value>,
    /// Claude Code hook adapter scripts (Track B3, NEXUS-APP dispatch
    /// 2d5017f7). Additive field: mirrors how OpenCode plugin sources are
    /// already embedded server-side (`headroom-intercept-plugin.ts` etc.)
    /// and written out to `.opencode/plugins/*.ts` -- nexus-app/
    /// nexus-oc-plugins own the bundled script content and versioning;
    /// nexus-cli is a thin consumer that writes each `body` to its
    /// `target_path` and wires `hook_events` into `.claude/settings.json`.
    #[serde(default)]
    pub claude_hook_adapters: Option<Vec<ClaudeHookAdapter>>,
    /// Generic `.claude/settings.json` merge spec (NEXUS-APP ADR-0117
    /// "CCX"). Additive field; `None` for backends that don't send it yet.
    #[serde(default)]
    pub claude_settings: Option<ClaudeSettingsSpec>,
    /// Markdown to maintain inside the root `CLAUDE.md` between
    /// `<!-- BEGIN:nexus-managed -->`/`<!-- END:nexus-managed -->` markers
    /// (NEXUS-APP ADR-0117). Everything outside the markers is user-owned
    /// and never rewritten by the CLI.
    #[serde(default)]
    pub claude_md_managed_block: Option<String>,
    /// Claude Code Experience bundle metadata (NEXUS-APP ADR-0117, dispatch
    /// 99f335e8). `None` when CCX is disabled or this is not a Claude
    /// project; then nothing CCX-specific happens and any existing lock is
    /// left untouched. When present, `agent_files` entries with category
    /// `"claude_experience"` are reconciled against the CCX lock instead of
    /// being written unconditionally.
    #[serde(default)]
    pub ccx: Option<CcxBundleInfo>,
}

/// `af_export.ccx`: identifies the CCX bundle revision being delivered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CcxBundleInfo {
    pub bundle: String,
    pub version: String,
    pub revision: String,
    #[serde(default)]
    pub compatibility: CcxBundleCompatibility,
}

/// `af_export.ccx.compatibility`: supported Claude Code version range
/// (space-separated comparators, e.g. `">=2.1.257 <3.0.0"`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CcxBundleCompatibility {
    #[serde(default, rename = "claudeCode")]
    pub claude_code: Option<String>,
}

/// Generic, forward-compatible `.claude/settings.json` merge spec
/// (NEXUS-APP ADR-0117 "CCX"). `managed_keys` lists dot-paths into
/// `settings.json` that Nexus owns (e.g. `"permissions.deny"`); `values`
/// carries the JSON value to set at each path. Kept untyped per key
/// (`serde_json::Value`) since the Claude Code settings schema is
/// server-owned and evolves independently of the CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaudeSettingsSpec {
    pub managed_keys: Vec<String>,
    pub values: serde_json::Map<String, serde_json::Value>,
}

/// A single Claude Code hook adapter script to materialize on disk
/// (Track B3, NEXUS-APP dispatch 2d5017f7).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeHookAdapter {
    /// Canonical plugin name (e.g. "session-guard", "headroom-intercept",
    /// "compaction-plus", "routing-guard", "cost-control").
    pub plugin_name: String,
    /// Path, relative to the project root, where the adapter script should
    /// be written (e.g. ".claude/hooks/nexus-session-guard.mjs").
    pub target_path: String,
    /// The bundled adapter script source (single file, `core/*` deps
    /// inlined). Read via stdin at runtime by the script itself -- the
    /// only CLI argument the script needs is the hook subcommand (see
    /// `ClaudeHookEvent::event`).
    pub body: String,
    /// Hook event registrations this adapter should be wired to in
    /// `.claude/settings.json`'s `hooks` block. One adapter may register
    /// against multiple events (e.g. `headroom-intercept` on both
    /// `PostToolUse` and `Stop`).
    pub hook_events: Vec<ClaudeHookEvent>,
}

/// A single Claude Code hook event registration for a `ClaudeHookAdapter`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeHookEvent {
    /// Claude Code hook event name in the platform's own PascalCase
    /// (e.g. "PreToolUse", "PostToolUse", "Stop", "SessionStart",
    /// "UserPromptSubmit", "PreCompact"). Converted to kebab-case by the
    /// CLI when building the invocation command (`PostToolUse` ->
    /// `post-tool-use`), per the adapters' own `process.argv[2]` contract.
    pub event: String,
    /// Optional tool/command matcher (Claude Code hook matcher syntax,
    /// e.g. `"Edit|Write|Bash"` or `"nexus_.*|headroom_.*"`). Omitted for
    /// events that don't use one (`SessionStart`, `Stop`,
    /// `UserPromptSubmit` per the current adapters).
    #[serde(default)]
    pub matcher: Option<String>,
    /// Optional timeout in seconds for the hook command. None of the
    /// current adapters set one; reserved for future use.
    #[serde(default)]
    pub timeout: Option<u64>,
}

/// A single warning surfaced by `af_export` about model routing / provider
/// divergence. `code` is open-ended (the backend may add new codes); unknown
/// codes should render generically rather than error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportWarning {
    /// Warning code, e.g. `"unverifiable_provider"`, `"route_alias_missing"`,
    /// `"gateway_unavailable"`, `"execution_mode_divergence"`.
    pub code: String,
    /// Human-readable, single-line, already user-facing message.
    pub message: String,
    /// Agent/slot the warning applies to, if any.
    #[serde(default)]
    pub agent: Option<String>,
    /// The model id that triggered the warning, if any.
    #[serde(default)]
    pub model: Option<String>,
    /// Suggested operator action, phrased as something to do.
    #[serde(default)]
    pub hint: Option<String>,
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
    /// Effective per-project `gh` CLI profile, superseding
    /// `git_config.gh` (NEXUS-APP ADR-0116, dispatch 0350aee7). `None`
    /// (or the field missing entirely) means: do not touch `gh` or its
    /// environment at all.
    #[serde(default)]
    pub gh_effective: Option<GhEffective>,
}

/// Per-project git identity settings (auto-applied by init/pull).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitConfig {
    pub user_name: Option<String>,
    pub user_email: Option<String>,
    pub signing_key: Option<String>,
    pub commit_gpgsign: Option<bool>,
    /// Whether Claude Code should add a `Co-Authored-By: Claude ...`
    /// trailer to commits (uninverted, Claude Code's own semantics: `true`
    /// means the trailer is added). `None` for projects created before this
    /// field existed on the backend; rendered as suppressed (`false`) by
    /// the Claude Code renderer, not left to Claude Code's own default
    /// (NEXUS-APP dispatch 84e38bd7).
    #[serde(default)]
    pub include_co_authored_by: Option<bool>,
    /// Per-project GitHub CLI (`gh`) profile (NEXUS-APP dispatch 8776d208).
    /// `None` for projects that have not configured one.
    #[serde(default)]
    pub gh: Option<GhConfig>,
}

/// Per-project GitHub CLI (`gh`) profile.
///
/// The GitHub token itself never leaves the operator's machine and never
/// passes through the Nexus backend: this only records which local `gh`
/// profile (a dedicated `GH_CONFIG_DIR`) a project should use, so multiple
/// projects can cleanly use different GitHub identities on the same
/// machine (NEXUS-APP dispatch 8776d208).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GhConfig {
    /// Lower-cased hostname; `github.com` unless this is a GitHub
    /// Enterprise Server project. Validated and normalized server-side.
    #[serde(default = "default_gh_host")]
    pub host: String,
    /// Expected GitHub login for this profile (informational; used by
    /// `nexus git verify` to detect a mismatched login). Validated
    /// server-side.
    pub user: Option<String>,
    /// Local `gh` CLI profile name (`^[a-z0-9][a-z0-9._-]{0,63}$`,
    /// validated server-side). Always present when `gh` is present;
    /// defaults to the lower-cased login on the backend.
    pub profile: String,
}

fn default_gh_host() -> String {
    "github.com".to_string()
}

/// Effective per-project `gh` CLI profile (NEXUS-APP ADR-0116), the
/// canonical input for `nexus run`'s gh handling and `nexus git verify`,
/// superseding `git_config.gh` (ADR-0115, kept for backward
/// deserialization compatibility but no longer read).
///
/// `source` describes where the seeding token should come from if the
/// local profile has no login yet: `"auto"` (try the OS keyring first,
/// then `GH_TOKEN`, then `GITHUB_TOKEN`), `"keyring"` (keyring only), or
/// `"env:<VAR>"` (a specific named env var only). `origin` records whether
/// this effective value came from the project's own configuration or a
/// per-user default (`"project"` / `"user"`), for the login status line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GhEffective {
    /// Lower-cased hostname; `github.com` unless this is a GitHub
    /// Enterprise Server project.
    pub host: String,
    /// Expected GitHub login for this profile.
    pub user: Option<String>,
    /// Local `gh` CLI profile name.
    pub profile: String,
    /// Where to source a seeding token from if the profile is empty:
    /// `"auto"` | `"keyring"` | `"env:<VAR>"`.
    pub source: String,
    /// Whether this came from the project's own config or a per-user
    /// default: `"project"` | `"user"`.
    pub origin: String,
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

/// Response from `GET /api/mcp/projects/{id}/preflight`.
///
/// This is the same endpoint the `nexus-headroom-intercept` OpenCode plugin
/// calls at load time to gate transform-mode compression. Reusing it here lets
/// `nexus run`'s pre-launch "Headroom" check live-verify reachability +
/// project enablement instead of only checking whether HEADROOM_MODE=transform
/// is set locally (see Task a3bf595b, NEXUS-APP: a stale/invalid token caused
/// the plugin to silently downgrade to observe mode while this env-var-only
/// check kept reporting PASS).
#[derive(Debug, Clone, Deserialize)]
pub struct McpPreflightResponse {
    pub project_id: String,
    #[serde(default)]
    pub plugins: Vec<String>,
    #[serde(default)]
    pub headroom_enabled: bool,
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
