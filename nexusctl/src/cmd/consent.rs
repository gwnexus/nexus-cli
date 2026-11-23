// nexusctl/src/cmd/consent.rs
//
// Consent for executable content from the server. Hook scripts, settings
// commands, Claude Code plugins, MCP servers, layouts and scripts run on
// this machine, so a new or changed item needs the user's consent before
// `nexus pull` or `nexus init` writes it.
//
// The fingerprint covers what the CLI writes; the server's `sha256` is only
// compared. Accepted fingerprints live outside the repository
// (`<config dir>/consent/<workspace key>.json`), so a commit cannot
// pre-approve anything. A refused item keeps its local version: the export
// is changed in memory before the writers run, so they see "unchanged".

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use console::style;
use nexus_core::api::{
    AgentFileExportResponse, ClaudeSettingsSpec, ExportedExecutable, ExportedSkill,
    McpServerConfig, SkillExportResponse, WorkspaceExportResponse, WorkspaceForkExportResponse,
};
use nexus_core::hash::{sha256_hex, sha256_hex_normalized};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::claude_render;
use super::pull;

/// What kind of executable an item is. The names match the server's
/// `executables[].kind`; `claude_setting` covers settings keys such as
/// `apiKeyHelper` that Claude Code runs as commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    ClaudeHook,
    ClaudeStatusLine,
    ClaudeSetting,
    ClaudePlugin,
    ClaudeMarketplace,
    McpServer,
    ZellijLayout,
    OpencodePlugin,
    AgentScript,
    DevboxScript,
    DevboxConfig,
    SkillScript,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::ClaudeHook => "claude_hook",
            Kind::ClaudeStatusLine => "claude_status_line",
            Kind::ClaudeSetting => "claude_setting",
            Kind::ClaudePlugin => "claude_plugin",
            Kind::ClaudeMarketplace => "claude_marketplace",
            Kind::McpServer => "mcp_server",
            Kind::ZellijLayout => "zellij_layout",
            Kind::OpencodePlugin => "opencode_plugin",
            Kind::AgentScript => "agent_script",
            Kind::DevboxScript => "devbox_script",
            Kind::DevboxConfig => "devbox_config",
            Kind::SkillScript => "skill_script",
        }
    }
}

/// Top-level `.claude/settings.json` keys whose values Claude Code runs as
/// commands.
const COMMAND_SETTINGS: &[&str] = &[
    "statusLine",
    "hooks",
    "apiKeyHelper",
    "awsAuthRefresh",
    "awsCredentialExport",
    "otelHeadersHelper",
];

const SCRIPT_EXTENSIONS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "js", "mjs", "cjs", "ts", "mts", "cts", "py", "rb", "pl", "php",
    "ps1", "bat", "cmd",
];

/// Where an item lives in its export, to keep the local version when
/// consent is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Slot {
    AgentFile(usize),
    HookAdapter(usize),
    Setting(String),
    Plugin(String),
    Marketplace(String),
    McpServer(String),
    Layout,
    ZellijConfig,
    SkillBody(usize),
    SkillResource(usize, usize),
    Workspace,
}

/// One executable item of an export, as the CLI would write it.
#[derive(Debug, Clone)]
pub struct Item {
    pub kind: Kind,
    pub id: String,
    pub target_path: String,
    pub command: Option<String>,
    /// What the CLI writes, as shown to the user (env values masked).
    pub content: String,
    /// The local counterpart of `content`, if there is one.
    pub local: Option<String>,
    /// SHA-256 over what the CLI writes.
    pub fingerprint: String,
    /// The local state already equals what would be written.
    pub installed: bool,
    /// The CLI's equivalent of the server's `sha256`, where comparable.
    pub server_hash: Option<String>,
    slot: Slot,
}

impl Item {
    fn key(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.id)
    }

    fn label(&self) -> String {
        format!("{} {}", self.kind.as_str(), self.id)
    }
}

// ---------------------------------------------------------------------------
// Classification helpers
// ---------------------------------------------------------------------------

/// Claude Code inline shell in a skill or command body (!`cmd`).
fn has_inline_shell(body: &str) -> bool {
    body.match_indices("!`")
        .any(|(i, _)| body[i + 2..].find('`').is_some_and(|end| end > 0))
}

fn is_script(path: &str, body: &str) -> bool {
    body.starts_with("#!")
        || Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| SCRIPT_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// The kind of an agent file that lands in a runnable location, if any.
///
/// Compared case-insensitively: on case-insensitive file systems (macOS,
/// Windows) `.CLAUDE/settings.json` is the same file as
/// `.claude/settings.json` and must need the same consent. Non-canonical
/// spellings (`./x`, `a//b`) are refused by the path validator before any
/// write.
fn agent_file_kind(path: &str, body: &str) -> Option<Kind> {
    let lower = path.to_ascii_lowercase();
    let path = lower.as_str();
    if path.ends_with(".kdl") {
        return Some(Kind::ZellijLayout);
    }
    if path.starts_with(".opencode/plugin/") || path.starts_with(".opencode/plugins/") {
        return Some(Kind::OpencodePlugin);
    }
    let config = matches!(
        path,
        ".claude/settings.json"
            | ".claude/settings.local.json"
            | ".mcp.json"
            | "opencode.json"
            | "opencode.jsonc"
            | "devbox.json"
    );
    (config || path.starts_with(".claude/hooks/") || is_script(path, body))
        .then_some(Kind::AgentScript)
}

/// Compact JSON with keys sorted at every level (serde_json's map is
/// ordered), the server's canonical form.
fn canonical(value: &Value) -> String {
    value.to_string()
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default() + "\n"
}

fn get_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    if let Some(v) = value.get(path) {
        return Some(v);
    }
    path.split('.').try_fold(value, |v, part| v.get(part))
}

fn read_json(path: &Path) -> Value {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(Value::Null)
}

fn read_file(workspace: &Path, rel: &str) -> Option<String> {
    fs::read_to_string(workspace.join(rel)).ok()
}

/// An item for a whole file written as is.
fn file_item(workspace: &Path, kind: Kind, id: &str, target: &str, body: &str, slot: Slot) -> Item {
    let local = read_file(workspace, target);
    Item {
        kind,
        id: id.to_string(),
        target_path: target.to_string(),
        command: None,
        content: body.to_string(),
        installed: local.as_deref() == Some(body),
        local,
        fingerprint: sha256_hex(body),
        server_hash: Some(sha256_hex(body)),
        slot,
    }
}

// ---------------------------------------------------------------------------
// Collectors
// ---------------------------------------------------------------------------

/// Executable items of an `af_export`, for the selected runtime.
pub fn collect_agent_export(
    workspace: &Path,
    af: &AgentFileExportResponse,
    is_claude: bool,
    agentic_root: &str,
) -> Vec<Item> {
    let mut items = Vec::new();

    for (i, f) in af.agent_files.iter().enumerate() {
        if pull::is_other_runtime_path(&f.target_path, is_claude, agentic_root) {
            continue;
        }
        if pull::is_protected_path(&f.target_path) && workspace.join(&f.target_path).exists() {
            continue;
        }
        let Some(kind) = agent_file_kind(&f.target_path, &f.body) else {
            continue;
        };
        let local = read_file(workspace, &f.target_path);
        let fingerprint = sha256_hex_normalized(&f.body);
        items.push(Item {
            kind,
            id: f.target_path.clone(),
            target_path: f.target_path.clone(),
            command: None,
            content: f.body.clone(),
            installed: local
                .as_deref()
                .is_some_and(|l| sha256_hex_normalized(l) == fingerprint),
            local,
            fingerprint,
            server_hash: Some(sha256_hex(&f.body)),
            slot: Slot::AgentFile(i),
        });
    }

    let target = if is_claude {
        ".mcp.json"
    } else {
        "opencode.json"
    };
    let mut names: Vec<&String> = af.mcp_servers.keys().collect();
    names.sort();
    for name in names {
        let cfg = &af.mcp_servers[name];
        let value = mcp_value(cfg);
        let fingerprint = sha256_hex(&canonical(&value));
        let local = local_mcp(workspace, is_claude, name);
        let argv = mcp_argv(cfg);
        items.push(Item {
            kind: Kind::McpServer,
            id: name.clone(),
            target_path: target.to_string(),
            command: Some(argv.join(" ")),
            content: pretty(&mask_env(&value)),
            installed: local
                .as_ref()
                .is_some_and(|l| sha256_hex(&canonical(&mcp_value(l))) == fingerprint),
            local: local.as_ref().map(|l| pretty(&mask_env(&mcp_value(l)))),
            fingerprint,
            server_hash: Some(sha256_hex(&canonical(&json!({
                "argv": argv,
                // The server hashes the names of all env variables: the
                // templated keys and those with an inline value.
                "env_keys": cfg
                    .env_keys
                    .iter()
                    .chain(cfg.environment.keys())
                    .collect::<BTreeSet<_>>(),
                "id": name,
                "kind": Kind::McpServer.as_str(),
            })))),
            slot: Slot::McpServer(name.clone()),
        });
    }

    if !is_claude {
        return items;
    }

    let settings = read_json(&workspace.join(".claude").join("settings.json"));
    for (i, a) in af.claude_hook_adapters.iter().flatten().enumerate() {
        let content = hook_content(&a.body, adapter_events(a));
        let local = read_file(workspace, &a.target_path)
            .map(|body| hook_content(&body, registered_events(&settings, &a.target_path)));
        items.push(Item {
            kind: Kind::ClaudeHook,
            id: a.plugin_name.clone(),
            target_path: a.target_path.clone(),
            command: None,
            fingerprint: sha256_hex(&content),
            installed: local.as_deref() == Some(content.as_str()),
            content,
            local,
            server_hash: Some(sha256_hex(&a.body)),
            slot: Slot::HookAdapter(i),
        });
    }

    if let Some(spec) = af.claude_settings.as_ref() {
        items.extend(collect_settings(spec, &settings));
    }

    if let Some(layout) = af.claude_workspace.as_ref() {
        if !layout.body.trim().is_empty() {
            items.push(file_item(
                workspace,
                Kind::ZellijLayout,
                &layout.path,
                &layout.path,
                &layout.body,
                Slot::Layout,
            ));
        }
        if let Some(config) = layout
            .zellij_config
            .as_ref()
            .filter(|c| !c.body.trim().is_empty())
        {
            items.push(file_item(
                workspace,
                Kind::ZellijLayout,
                &config.path,
                &config.path,
                &config.body,
                Slot::ZellijConfig,
            ));
        }
    }
    items
}

/// Settings commands, plugins and marketplaces of the settings spec.
fn collect_settings(spec: &ClaudeSettingsSpec, settings: &Value) -> Vec<Item> {
    const TARGET: &str = ".claude/settings.json";
    let mut items = Vec::new();
    for key in &spec.managed_keys {
        let top = key.split('.').next().unwrap_or_default();
        if !COMMAND_SETTINGS.contains(&top) {
            continue;
        }
        let Some(value) = spec.value(key) else {
            continue;
        };
        let kind = if top == "statusLine" {
            Kind::ClaudeStatusLine
        } else {
            Kind::ClaudeSetting
        };
        let command = value
            .get("command")
            .and_then(Value::as_str)
            .or_else(|| value.as_str())
            .map(str::to_string);
        let local = get_path(settings, key);
        let server_hash = if kind == Kind::ClaudeStatusLine {
            let mut obj = json!({ "id": key, "kind": kind.as_str() });
            if let Some(ref c) = command {
                obj["command"] = json!(c);
            }
            sha256_hex(&canonical(&obj))
        } else {
            sha256_hex(&canonical(
                &json!({ "id": key, "kind": kind.as_str(), "value": value }),
            ))
        };
        items.push(Item {
            kind,
            id: key.clone(),
            target_path: TARGET.to_string(),
            command,
            content: pretty(value),
            local: local.map(pretty),
            fingerprint: sha256_hex(&canonical(value)),
            installed: local == Some(value),
            server_hash: Some(server_hash),
            slot: Slot::Setting(key.clone()),
        });
    }

    let managed = |key: &str| spec.managed_keys.iter().any(|k| k == key);
    if managed("enabledPlugins") {
        if let Some(plugins) = spec.value("enabledPlugins").and_then(Value::as_object) {
            for (id, value) in plugins.iter().filter(|(_, v)| **v != Value::Bool(false)) {
                let local = settings.get("enabledPlugins").and_then(|p| p.get(id));
                items.push(Item {
                    kind: Kind::ClaudePlugin,
                    id: id.clone(),
                    target_path: TARGET.to_string(),
                    command: None,
                    content: pretty(value),
                    local: local.map(pretty),
                    fingerprint: sha256_hex(&canonical(&json!({ "id": id, "value": value }))),
                    installed: local == Some(value),
                    server_hash: Some(sha256_hex(&canonical(
                        &json!({ "id": id, "kind": Kind::ClaudePlugin.as_str() }),
                    ))),
                    slot: Slot::Plugin(id.clone()),
                });
            }
        }
    }
    if managed("extraKnownMarketplaces") {
        if let Some(markets) = spec
            .value("extraKnownMarketplaces")
            .and_then(Value::as_object)
        {
            for (name, entry) in markets {
                let local = settings
                    .get("extraKnownMarketplaces")
                    .and_then(|m| m.get(name));
                let mut obj = json!({ "id": name, "kind": Kind::ClaudeMarketplace.as_str() });
                if let Some(source) = entry.get("source") {
                    obj["source"] = source.clone();
                }
                items.push(Item {
                    kind: Kind::ClaudeMarketplace,
                    id: name.clone(),
                    target_path: TARGET.to_string(),
                    command: super::claude_plugins::marketplace_source_arg(entry),
                    content: pretty(entry),
                    local: local.map(pretty),
                    fingerprint: sha256_hex(&canonical(&json!({ "id": name, "value": entry }))),
                    installed: local == Some(entry),
                    server_hash: Some(sha256_hex(&canonical(&obj))),
                    slot: Slot::Marketplace(name.clone()),
                });
            }
        }
    }
    items
}

/// Skill bodies with inline shell, and script resources.
pub fn collect_skills(
    workspace: &Path,
    skills: &[ExportedSkill],
    is_claude: bool,
    agentic_root: &str,
) -> Vec<Item> {
    let mut items = Vec::new();
    for (i, skill) in skills.iter().enumerate() {
        let rendered = if is_claude {
            claude_render::render_claude_skill_files(skill)
        } else {
            pull::render_skill_files(skill, agentic_root)
        };
        let Some((skill_md, _)) = rendered.first() else {
            continue;
        };
        let dir = skill_md.trim_end_matches("SKILL.md");
        let body = skill.body.as_deref().unwrap_or_default();
        if has_inline_shell(body) {
            let local = read_file(workspace, skill_md).map(|l| skill_md_body(&l));
            items.push(Item {
                kind: Kind::SkillScript,
                id: format!("{}/SKILL.md", skill.skill_id),
                target_path: skill_md.clone(),
                command: None,
                content: body.to_string(),
                installed: local.as_deref() == Some(claude_render::strip_frontmatter(body)),
                local,
                fingerprint: sha256_hex(body),
                server_hash: Some(sha256_hex(body)),
                slot: Slot::SkillBody(i),
            });
        }
        for (j, res) in skill.resources.iter().enumerate() {
            let filename = res.filename.replace(['/', '\\'], "_");
            if filename.is_empty() || filename == "SKILL.md" {
                continue;
            }
            if !(is_script(&filename, &res.body) || has_inline_shell(&res.body)) {
                continue;
            }
            items.push(file_item(
                workspace,
                Kind::SkillScript,
                &format!("{}/{}", skill.skill_id, res.filename),
                &format!("{dir}{filename}"),
                &res.body,
                Slot::SkillResource(i, j),
            ));
        }
    }
    items
}

/// The body of a rendered `SKILL.md`, as the renderer takes it.
fn skill_md_body(rendered: &str) -> String {
    let body = claude_render::strip_frontmatter(rendered);
    body.strip_suffix('\n').unwrap_or(body).to_string()
}

/// `devbox.json` and scripts of a v2 workspace export.
pub fn collect_workspace_v2(workspace: &Path, export: &WorkspaceForkExportResponse) -> Vec<Item> {
    let mut items = vec![file_item(
        workspace,
        Kind::DevboxConfig,
        "devbox.json",
        "devbox.json",
        &export.devbox_json,
        Slot::Workspace,
    )];
    for s in &export.scripts {
        if s.executable || is_script(&s.path, &s.body) {
            items.push(file_item(
                workspace,
                Kind::DevboxScript,
                &s.path,
                &s.path,
                &s.body,
                Slot::Workspace,
            ));
        }
    }
    // Machine-local scripts are sourced into the devbox shell: always
    // executable content.
    for (path, body) in export.local_scripts.iter().flatten() {
        items.push(file_item(
            workspace,
            Kind::DevboxScript,
            path,
            path,
            body,
            Slot::Workspace,
        ));
    }
    items
}

/// The workspace template and scripts of a v1 workspace export.
pub fn collect_workspace_v1(workspace: &Path, export: &WorkspaceExportResponse) -> Vec<Item> {
    let mut items = Vec::new();
    if let Some(ref tpl) = export.workspace {
        items.push(file_item(
            workspace,
            Kind::DevboxConfig,
            &tpl.target_path,
            &tpl.target_path,
            &tpl.body,
            Slot::Workspace,
        ));
    }
    for s in &export.scripts {
        if s.executable || is_script(&s.target_path, &s.body) {
            items.push(file_item(
                workspace,
                Kind::DevboxScript,
                &s.target_path,
                &s.target_path,
                &s.body,
                Slot::Workspace,
            ));
        }
    }
    items
}

// ---------------------------------------------------------------------------
// Hooks and MCP servers
// ---------------------------------------------------------------------------

fn event_line(event: &str, matcher: Option<&str>, timeout: Option<u64>) -> String {
    format!(
        "{event} matcher={} timeout={}",
        matcher.unwrap_or("-"),
        timeout.map_or_else(|| "-".to_string(), |t| t.to_string())
    )
}

fn adapter_events(adapter: &nexus_core::api::ClaudeHookAdapter) -> BTreeSet<String> {
    adapter
        .hook_events
        .iter()
        .filter(|e| pull::valid_path_segment(&e.event))
        .map(|e| event_line(&e.event, e.matcher.as_deref(), e.timeout))
        .collect()
}

/// The registrations in `settings.json` whose command runs `target_path`.
fn registered_events(settings: &Value, target_path: &str) -> BTreeSet<String> {
    let mut lines = BTreeSet::new();
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return lines;
    };
    for (event, entries) in hooks {
        for entry in entries.as_array().into_iter().flatten() {
            let matcher = entry.get("matcher").and_then(Value::as_str);
            for hook in entry
                .get("hooks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let runs = hook
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains(target_path));
                if runs {
                    let timeout = hook.get("timeout").and_then(Value::as_u64);
                    lines.insert(event_line(event, matcher, timeout));
                }
            }
        }
    }
    lines
}

/// Hook script plus its registrations: both decide what runs.
fn hook_content(body: &str, events: BTreeSet<String>) -> String {
    let mut out = body.trim_end_matches('\n').to_string();
    out.push_str("\n\n# registered in .claude/settings.json\n");
    for line in events {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn mcp_argv(cfg: &McpServerConfig) -> Vec<String> {
    cfg.command.iter().chain(&cfg.args).cloned().collect()
}

/// Env keys rendered as templates; an inline value wins over a template.
fn sorted_env_keys(cfg: &McpServerConfig) -> Vec<String> {
    let keys: BTreeSet<&String> = cfg
        .env_keys
        .iter()
        .filter(|k| !cfg.environment.contains_key(*k))
        .collect();
    keys.into_iter().cloned().collect()
}

/// What decides how an MCP server runs, including inline env values.
fn mcp_value(cfg: &McpServerConfig) -> Value {
    let environment: BTreeMap<&String, &String> = cfg.environment.iter().collect();
    json!({
        "argv": mcp_argv(cfg),
        "env_keys": sorted_env_keys(cfg),
        "environment": environment,
    })
}

/// Env values are shown as a short hash only.
fn mask_env(value: &Value) -> Value {
    let mut out = value.clone();
    if let Some(env) = out.get_mut("environment").and_then(Value::as_object_mut) {
        for v in env.values_mut() {
            let raw = v.as_str().unwrap_or_default();
            *v = json!(format!("sha256:{}", &sha256_hex(raw)[..12]));
        }
    }
    out
}

/// The MCP server `name` as the local `.mcp.json` / `opencode.json` has it,
/// read back into the export's shape.
fn local_mcp(workspace: &Path, is_claude: bool, name: &str) -> Option<McpServerConfig> {
    let (file, block) = if is_claude {
        (".mcp.json", "mcpServers")
    } else {
        ("opencode.json", "mcp")
    };
    let root = read_json(&workspace.join(file));
    let entry = root.get(block)?.get(name)?;
    let strings = |v: Option<&Value>| -> Vec<String> {
        v.and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect()
    };
    let (command, env) = if is_claude {
        let mut argv = vec![entry.get("command")?.as_str()?.to_string()];
        argv.extend(strings(entry.get("args")));
        (argv, entry.get("env"))
    } else {
        (strings(entry.get("command")), entry.get("environment"))
    };
    let mut env_keys = Vec::new();
    let mut environment = std::collections::HashMap::new();
    for (k, v) in env.and_then(Value::as_object).into_iter().flatten() {
        let v = v.as_str().unwrap_or_default();
        if v == format!("${{{k}}}") || v == format!("{{env:{k}}}") {
            env_keys.push(k.clone());
        } else {
            environment.insert(k.clone(), v.to_string());
        }
    }
    Some(McpServerConfig {
        command,
        args: Vec::new(),
        env_keys,
        environment,
    })
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    sha256: String,
    /// The server's hash at acceptance, when it differed from the CLI's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    server_sha256: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreData {
    #[serde(default)]
    workspace: String,
    #[serde(default)]
    items: BTreeMap<String, Record>,
}

/// Accepted fingerprints of one workspace.
#[derive(Debug, Default)]
pub struct Store {
    path: Option<PathBuf>,
    data: StoreData,
}

impl Store {
    /// `<config dir>/consent/<16 hex of the workspace path hash>.json`.
    pub fn path_for(config_dir: &Path, workspace: &Path) -> PathBuf {
        let canonical = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        let key = sha256_hex(&canonical.to_string_lossy());
        config_dir
            .join("consent")
            .join(format!("{}.json", &key[..16]))
    }

    pub fn load(path: PathBuf, workspace: &Path) -> Store {
        let mut data: StoreData = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        data.workspace = workspace.display().to_string();
        Store {
            path: Some(path),
            data,
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(ref path) = self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
            }
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&self.data)? + "\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&tmp, path)
    }

    fn record(&mut self, item: &Item, server_sha: Option<String>) {
        let server_sha256 = server_sha.filter(|s| item.server_hash.as_deref() != Some(s));
        self.data.items.insert(
            item.key(),
            Record {
                sha256: item.fingerprint.clone(),
                server_sha256,
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Decisions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Accepted earlier with the same fingerprint.
    Accepted,
    /// Already on disk as it would be written: recorded without asking.
    Adopted,
    New,
    Changed,
}

/// Compare an item with its record and the server's declaration.
fn classify(item: &Item, record: Option<&Record>, server_sha: Option<&str>) -> State {
    let mismatch =
        matches!((item.server_hash.as_deref(), server_sha), (Some(a), Some(b)) if a != b);
    match record {
        Some(r)
            if r.sha256 == item.fingerprint
                && (!mismatch || r.server_sha256.as_deref() == server_sha) =>
        {
            State::Accepted
        }
        _ if item.installed && !mismatch => State::Adopted,
        Some(_) => State::Changed,
        None if item.local.is_some() => State::Changed,
        None => State::New,
    }
}

fn server_entry<'a>(
    declared: &'a [ExportedExecutable],
    item: &Item,
) -> Option<&'a ExportedExecutable> {
    declared
        .iter()
        .find(|d| d.kind == item.kind.as_str() && d.id == item.id)
}

fn server_sha(entry: Option<&ExportedExecutable>) -> Option<String> {
    entry
        .map(|e| e.sha256.to_ascii_lowercase())
        .filter(|s| !s.is_empty())
}

/// Answers pending items (one bool per item, `true` = install).
type Ask = dyn FnMut(&[(&Item, State)]) -> Vec<bool>;

/// How pending items are answered.
pub enum Answer {
    /// `--trust`: accept all.
    Trust,
    /// No terminal: refuse all.
    Refuse,
    /// Ask in the terminal.
    Ask,
}

/// The consent step of one `nexus pull` / `nexus init`.
pub struct Gate {
    store: Store,
    answer: Answer,
    held: Vec<String>,
    accepted: usize,
}

impl Gate {
    pub fn open(workspace: &Path, trust: bool) -> Gate {
        let store = match nexus_core::config::Config::dir() {
            Ok(dir) => Store::load(Store::path_for(&dir, workspace), workspace),
            Err(_) => Store::default(),
        };
        let answer = if trust {
            Answer::Trust
        } else if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            Answer::Ask
        } else {
            Answer::Refuse
        };
        Gate::with(store, answer)
    }

    fn with(store: Store, answer: Answer) -> Gate {
        Gate {
            store,
            answer,
            held: Vec::new(),
            accepted: 0,
        }
    }

    /// Gate `af_export` and `sk_export` in place, before anything is
    /// written: a refused item keeps its local version or is left out.
    pub fn exports(
        &mut self,
        workspace: &Path,
        af: Option<&mut AgentFileExportResponse>,
        skills: Option<&mut SkillExportResponse>,
        is_claude: bool,
        agentic_root: &str,
    ) {
        let mut items = Vec::new();
        let mut declared = Vec::new();
        if let Some(af) = af.as_deref() {
            items.extend(collect_agent_export(workspace, af, is_claude, agentic_root));
            declared.extend(af.executables.iter().cloned());
        }
        if let Some(sk) = skills.as_deref() {
            items.extend(collect_skills(
                workspace,
                &sk.skills,
                is_claude,
                agentic_root,
            ));
            declared.extend(sk.executables.iter().cloned());
        }
        let refused = self.resolve(&items, &declared, &mut prompt);
        let refused: Vec<&Item> = refused.iter().map(|&i| &items[i]).collect();
        if let Some(af) = af {
            hold_agent_export(workspace, af, &refused, is_claude);
        }
        if let Some(sk) = skills {
            hold_skills(&mut sk.skills, &refused);
        }
    }

    /// Gate a v2 workspace export; returns the paths to leave untouched.
    pub fn workspace_v2(
        &mut self,
        workspace: &Path,
        export: &WorkspaceForkExportResponse,
    ) -> HashSet<String> {
        let items = collect_workspace_v2(workspace, export);
        let refused = self.resolve(&items, &export.executables, &mut prompt);
        refused
            .into_iter()
            .map(|i| items[i].target_path.clone())
            .collect()
    }

    /// Gate a v1 workspace export; returns the paths to leave untouched.
    pub fn workspace_v1(
        &mut self,
        workspace: &Path,
        export: &WorkspaceExportResponse,
    ) -> HashSet<String> {
        let items = collect_workspace_v1(workspace, export);
        let refused = self.resolve(&items, &[], &mut prompt);
        refused
            .into_iter()
            .map(|i| items[i].target_path.clone())
            .collect()
    }

    /// Decide every item; returns the indices of refused items.
    fn resolve(
        &mut self,
        items: &[Item],
        declared: &[ExportedExecutable],
        ask: &mut Ask,
    ) -> Vec<usize> {
        let mut pending: Vec<(usize, State, Option<String>)> = Vec::new();
        let mut undeclared = Vec::new();
        let mut adopted = false;
        for (i, item) in items.iter().enumerate() {
            let entry = server_entry(declared, item);
            if entry.is_none() && !declared.is_empty() {
                undeclared.push(item.label());
            }
            let sha = server_sha(entry);
            match classify(item, self.store.data.items.get(&item.key()), sha.as_deref()) {
                State::Accepted => {}
                State::Adopted => {
                    self.store.record(item, sha);
                    adopted = true;
                }
                state => {
                    if item.server_hash.is_some() && sha.is_some() && item.server_hash != sha {
                        println!(
                            "   {} {}: the server's fingerprint differs from what the CLI writes",
                            style("!").bold().yellow(),
                            item.label()
                        );
                    }
                    pending.push((i, state, sha));
                }
            }
        }
        if !undeclared.is_empty() {
            println!(
                "   {} Not declared as executable by the server: {}",
                style("·").dim(),
                undeclared.join(", ")
            );
        }

        let answers = if pending.is_empty() {
            Vec::new()
        } else {
            match self.answer {
                Answer::Trust => vec![true; pending.len()],
                Answer::Refuse => vec![false; pending.len()],
                Answer::Ask => {
                    let view: Vec<(&Item, State)> =
                        pending.iter().map(|(i, s, _)| (&items[*i], *s)).collect();
                    ask(&view)
                }
            }
        };

        let mut refused = Vec::new();
        for ((i, _, sha), ok) in pending.into_iter().zip(answers) {
            if ok {
                self.store.record(&items[i], sha);
                self.accepted += 1;
            } else {
                self.held.push(items[i].label());
                refused.push(i);
            }
        }
        if (adopted || self.accepted > 0) && self.store.save().is_err() {
            println!(
                "   {} Could not save accepted executables; nexus pull will ask again",
                style("!").bold().yellow()
            );
        }
        refused
    }

    /// Report accepted and refused items.
    pub fn finish(&self) {
        if self.accepted > 0 {
            println!(
                "   {} {} executable item(s) accepted",
                style("+").bold().green(),
                self.accepted
            );
        }
        if self.held.is_empty() {
            return;
        }
        println!();
        println!(
            "   {} {} executable item(s) not installed without your consent (local version kept):",
            style("!").bold().yellow(),
            self.held.len()
        );
        for label in &self.held {
            println!("      {}", style(label).yellow());
        }
        if matches!(self.answer, Answer::Refuse) {
            println!(
                "   Run {} in a terminal to review them, or {} to accept them.",
                style("nexus pull").bold(),
                style("nexus pull --trust").bold()
            );
        }
    }
}

/// Items of an export that a pull would ask about (for `--dry-run`).
pub fn preview(workspace: &Path, items: &[Item], declared: &[ExportedExecutable]) {
    let store = match nexus_core::config::Config::dir() {
        Ok(dir) => Store::load(Store::path_for(&dir, workspace), workspace),
        Err(_) => Store::default(),
    };
    let mut undeclared = Vec::new();
    for item in items {
        let entry = server_entry(declared, item);
        let sha = server_sha(entry);
        if entry.is_none() && !declared.is_empty() {
            undeclared.push(item.label());
        }
        if item.server_hash.is_some() && sha.is_some() && item.server_hash != sha {
            println!(
                "   {} {}: the server's fingerprint differs from what the CLI writes",
                style("!").bold().yellow(),
                item.label()
            );
        }
        let state = classify(item, store.data.items.get(&item.key()), sha.as_deref());
        if matches!(state, State::New | State::Changed) {
            println!(
                "   {} would ask: {} {} -> {}",
                style("?").bold().cyan(),
                state_label(state),
                item.label(),
                item.target_path
            );
        }
    }
    if !undeclared.is_empty() {
        println!(
            "   {} Not declared as executable by the server: {}",
            style("·").dim(),
            undeclared.join(", ")
        );
    }
    println!(
        "   {} {} executable item(s) checked, {} declared by the server",
        style("·").dim(),
        items.len(),
        declared.len()
    );
}

fn state_label(state: State) -> &'static str {
    match state {
        State::New => "new",
        State::Changed => "changed",
        State::Accepted => "accepted",
        State::Adopted => "installed",
    }
}

// ---------------------------------------------------------------------------
// Prompt
// ---------------------------------------------------------------------------

/// The host an MCP server's argv passes as `--host=<h>` or `--host <h>`,
/// when it is neither `sentry.io`/`*.sentry.io` nor trusted locally
/// (`NEXUS_TRUSTED_SENTRY_HOSTS`): such a server sends the Sentry token
/// there.
fn untrusted_token_host(item: &Item) -> Option<String> {
    if item.kind != Kind::McpServer {
        return None;
    }
    let argv: Vec<&str> = item.command.as_deref()?.split_whitespace().collect();
    let host = argv
        .iter()
        .enumerate()
        .find_map(|(i, a)| match a.strip_prefix("--host=") {
            Some(h) => Some(h.to_string()),
            None if *a == "--host" => argv.get(i + 1).map(|h| h.to_string()),
            None => None,
        })?;
    let trusted = std::env::var(super::plugin_env::TRUSTED_SENTRY_HOSTS_ENV).ok();
    (!super::plugin_env::sentry_host_trusted(&host, trusted.as_deref()))
        .then(|| super::dispatch_listen::flatten(&host))
}

/// The warning line for an MCP server with an untrusted token host.
fn host_warning(item: &Item) -> Option<String> {
    untrusted_token_host(item).map(|h| format!("sends your Sentry token to {h}"))
}

fn print_item(item: &Item, state: State) {
    println!();
    println!(
        "   {} {} {}  ({})",
        style(state_label(state)).bold(),
        style(item.kind.as_str()).cyan(),
        style(&item.id).bold(),
        item.target_path
    );
    if let Some(ref command) = item.command {
        println!("     runs: {}", style(command).yellow());
    }
    if let Some(w) = host_warning(item) {
        println!("     {} {}", style("!").bold().red(), style(w).bold().red());
    }
    let old = item.local.as_deref().unwrap_or_default();
    let diff = super::state_cmd::unified_diff(old, &item.content, "local", "nexus");
    const MAX_LINES: usize = 80;
    let lines: Vec<&str> = diff.lines().collect();
    for line in lines.iter().take(MAX_LINES) {
        let styled = if line.starts_with('+') && !line.starts_with("+++") {
            style(*line).green()
        } else if line.starts_with('-') && !line.starts_with("---") {
            style(*line).red()
        } else {
            style(*line).dim()
        };
        println!("     {styled}");
    }
    if lines.len() > MAX_LINES {
        println!("     ... {} more line(s)", lines.len() - MAX_LINES);
    }
}

/// Ask for the pending items: all at once or one by one.
fn prompt(pending: &[(&Item, State)]) -> Vec<bool> {
    println!();
    println!(
        "   {} {} executable item(s) from the server need your consent:",
        style("?").bold().cyan(),
        pending.len()
    );
    for (item, state) in pending {
        println!(
            "      {:8} {}  ->  {}",
            state_label(*state),
            item.label(),
            item.target_path
        );
        if let Some(w) = host_warning(item) {
            println!("               {}", style(format!("! {w}")).bold().red());
        }
    }
    let theme = dialoguer::theme::ColorfulTheme::default();
    let choice = dialoguer::Select::with_theme(&theme)
        .with_prompt("Install them?")
        .items(&["Review each", "Accept all", "Skip all"])
        .default(0)
        .interact_opt()
        .ok()
        .flatten();
    match choice {
        Some(1) => vec![true; pending.len()],
        Some(0) => pending
            .iter()
            .map(|(item, state)| {
                print_item(item, *state);
                dialoguer::Confirm::with_theme(&theme)
                    .with_prompt(format!("Install {}?", item.label()))
                    .default(false)
                    .interact()
                    .unwrap_or(false)
            })
            .collect(),
        _ => vec![false; pending.len()],
    }
}

// ---------------------------------------------------------------------------
// Holding refused items
// ---------------------------------------------------------------------------

fn set_value(map: &mut serde_json::Map<String, Value>, path: &str, value: Value) {
    if map.contains_key(path) || !path.contains('.') {
        map.insert(path.to_string(), value);
        return;
    }
    let (head, rest) = path.split_once('.').expect("path contains a dot");
    let child = map
        .entry(head.to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !child.is_object() {
        *child = Value::Object(serde_json::Map::new());
    }
    set_value(child.as_object_mut().expect("just ensured"), rest, value);
}

fn remove_value(map: &mut serde_json::Map<String, Value>, path: &str) {
    if map.remove(path).is_some() {
        return;
    }
    if let Some((head, rest)) = path.split_once('.') {
        if let Some(child) = map.get_mut(head).and_then(Value::as_object_mut) {
            remove_value(child, rest);
        }
    }
}

/// Keep the local value of a keyed-map entry, or leave the entry out.
fn hold_entry(spec: &mut ClaudeSettingsSpec, map_key: &str, id: &str, local: Option<&Value>) {
    if let Some(map) = spec.values.get_mut(map_key).and_then(Value::as_object_mut) {
        match local {
            Some(v) => {
                map.insert(id.to_string(), v.clone());
            }
            None => {
                map.remove(id);
            }
        }
    }
}

fn hold_agent_export(
    workspace: &Path,
    af: &mut AgentFileExportResponse,
    refused: &[&Item],
    is_claude: bool,
) {
    let settings = read_json(&workspace.join(".claude").join("settings.json"));
    let mut drop_files = Vec::new();
    let mut drop_hooks = Vec::new();
    for item in refused {
        match &item.slot {
            Slot::AgentFile(i) => match item.local {
                Some(ref body) => af.agent_files[*i].body = body.clone(),
                None => drop_files.push(*i),
            },
            Slot::HookAdapter(i) => {
                if let Some(adapters) = af.claude_hook_adapters.as_mut() {
                    // The script on disk stays; its registrations follow the
                    // export (only the first pull after an upgrade can get here
                    // with a hook that was never accepted).
                    match read_file(workspace, &adapters[*i].target_path) {
                        Some(body) => adapters[*i].body = body,
                        None => drop_hooks.push(*i),
                    }
                }
            }
            Slot::Setting(key) => {
                if let Some(spec) = af.claude_settings.as_mut() {
                    match get_path(&settings, key) {
                        Some(v) => set_value(&mut spec.values, key, v.clone()),
                        None => {
                            spec.managed_keys.retain(|k| k != key);
                            remove_value(&mut spec.values, key);
                        }
                    }
                }
            }
            Slot::Plugin(id) => {
                if let Some(spec) = af.claude_settings.as_mut() {
                    let local = settings.get("enabledPlugins").and_then(|p| p.get(id));
                    hold_entry(spec, "enabledPlugins", id, local);
                }
            }
            Slot::Marketplace(name) => {
                if let Some(spec) = af.claude_settings.as_mut() {
                    let local = settings
                        .get("extraKnownMarketplaces")
                        .and_then(|m| m.get(name));
                    hold_entry(spec, "extraKnownMarketplaces", name, local);
                }
            }
            Slot::McpServer(name) => match local_mcp(workspace, is_claude, name) {
                Some(local) => {
                    af.mcp_servers.insert(name.clone(), local);
                }
                None => {
                    af.mcp_servers.remove(name);
                }
            },
            Slot::Layout => {
                if let Some(layout) = af.claude_workspace.as_mut() {
                    layout.body = item.local.clone().unwrap_or_default();
                    layout.sha256 = None;
                }
            }
            Slot::ZellijConfig => {
                if let Some(layout) = af.claude_workspace.as_mut() {
                    match item.local {
                        Some(ref body) => {
                            if let Some(config) = layout.zellij_config.as_mut() {
                                config.body = body.clone();
                                config.sha256 = None;
                            }
                        }
                        None => layout.zellij_config = None,
                    }
                }
            }
            Slot::SkillBody(_) | Slot::SkillResource(..) | Slot::Workspace => {}
        }
    }
    drop_files.sort_unstable();
    for i in drop_files.into_iter().rev() {
        af.agent_files.remove(i);
    }
    if let Some(adapters) = af.claude_hook_adapters.as_mut() {
        drop_hooks.sort_unstable();
        for i in drop_hooks.into_iter().rev() {
            adapters.remove(i);
        }
    }
}

fn hold_skills(skills: &mut Vec<ExportedSkill>, refused: &[&Item]) {
    let mut drop_skills = BTreeSet::new();
    let mut drop_resources = BTreeSet::new();
    for item in refused {
        match (&item.slot, &item.local) {
            (Slot::SkillBody(i), Some(body)) => skills[*i].body = Some(body.clone()),
            (Slot::SkillBody(i), None) => {
                drop_skills.insert(*i);
            }
            (Slot::SkillResource(i, j), Some(body)) => skills[*i].resources[*j].body = body.clone(),
            (Slot::SkillResource(i, j), None) => {
                drop_resources.insert((*i, *j));
            }
            _ => {}
        }
    }
    for (i, j) in drop_resources.into_iter().rev() {
        skills[i].resources.remove(j);
    }
    for i in drop_skills.into_iter().rev() {
        skills.remove(i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn store_in(dir: &Path, workspace: &Path) -> Store {
        Store::load(Store::path_for(dir, workspace), workspace)
    }

    fn agent_export() -> AgentFileExportResponse {
        let af = |path: &str, body: &str| {
            json!({
                "file_key": path, "target_path": path, "name": path,
                "category": "agent", "version": 1, "body": body
            })
        };
        serde_json::from_value(json!({
            "project_id": "p",
            "project_name": "Demo",
            "count": 3,
            "agentic_root": ".nexus",
            "agent_files": [
                af(".nexus/AGENTS.md", "# agents\n"),
                af(".claude/hooks/usage.mjs", "console.log('new')\n"),
                af(".nexus/claude/team.kdl", "layout {}\n"),
            ],
            "mcp_servers": {
                "kept": { "command": ["npx", "-y", "kept"], "env_keys": ["TOKEN"] },
                "fresh": { "command": "fresh-server", "environment": { "MODE": "on" } }
            },
            "claude_hook_adapters": [{
                "plugin_name": "session-guard",
                "target_path": ".claude/hooks/nexus-session-guard.mjs",
                "body": "guard()\n",
                "hook_events": [{ "event": "Stop" }]
            }],
            "claude_settings": {
                "managed_keys": ["statusLine", "enabledPlugins", "extraKnownMarketplaces", "model"],
                "values": {
                    "statusLine": { "type": "command", "command": "nexus hud" },
                    "enabledPlugins": { "core@nexus": true, "off@nexus": false },
                    "extraKnownMarketplaces": {
                        "nexus": { "source": { "source": "github", "repo": "acme/plugins" } }
                    },
                    "model": "opus"
                }
            },
            "claude_workspace": { "path": ".nexus/claude/workspace.local.kdl", "body": "layout { pane }\n" }
        }))
        .unwrap()
    }

    #[test]
    fn test_mcp_host_outside_sentry_is_flagged() {
        let item = |command: &str| Item {
            kind: Kind::McpServer,
            id: "sentry".into(),
            target_path: ".mcp.json".into(),
            command: Some(command.into()),
            content: String::new(),
            installed: false,
            local: None,
            fingerprint: String::new(),
            server_hash: None,
            slot: Slot::McpServer("sentry".into()),
        };
        let base = "npx -y @sentry/mcp-server@0.42.0 --organization-slug=acme";
        assert_eq!(host_warning(&item(base)), None);
        assert_eq!(
            host_warning(&item(&format!("{base} --host=de.sentry.io"))),
            None
        );
        assert_eq!(
            host_warning(&item(&format!("{base} --host=sentry.evil.example"))).as_deref(),
            Some("sends your Sentry token to sentry.evil.example")
        );
        assert!(host_warning(&item(&format!("{base} --host evil.example"))).is_some());
        let mut other = item(&format!("{base} --host=evil.example"));
        other.kind = Kind::AgentScript;
        assert_eq!(host_warning(&other), None);
    }

    #[test]
    fn test_classification_helpers() {
        assert!(has_inline_shell("Run !`git status` first"));
        assert!(!has_inline_shell("No shell! `code` only"));
        assert!(!has_inline_shell("!``"));
        assert!(is_script("bin/x.sh", ""));
        assert!(is_script("tool", "#!/bin/sh\n"));
        assert!(!is_script("README.md", "# hi"));
        assert_eq!(agent_file_kind("a/b.kdl", ""), Some(Kind::ZellijLayout));
        assert_eq!(
            agent_file_kind(".CLAUDE/Settings.Local.json", "{}"),
            Some(Kind::AgentScript)
        );
        assert_eq!(
            agent_file_kind(".Claude/hooks/x", ""),
            Some(Kind::AgentScript)
        );
        assert_eq!(
            agent_file_kind("Opencode.JSON", "{}"),
            Some(Kind::AgentScript)
        );
        assert_eq!(
            agent_file_kind(".opencode/plugins/x.ts", ""),
            Some(Kind::OpencodePlugin)
        );
        assert_eq!(
            agent_file_kind(".claude/hooks/x", ""),
            Some(Kind::AgentScript)
        );
        assert_eq!(agent_file_kind(".mcp.json", "{}"), Some(Kind::AgentScript));
        assert_eq!(agent_file_kind(".nexus/AGENTS.md", "# a"), None);
    }

    #[test]
    fn test_canonical_json_sorts_keys_without_whitespace() {
        let v = json!({ "b": 1, "a": { "d": [2, 1], "c": null } });
        assert_eq!(canonical(&v), r#"{"a":{"c":null,"d":[2,1]},"b":1}"#);
    }

    fn item(fingerprint: &str, local: Option<&str>, installed: bool) -> Item {
        Item {
            kind: Kind::McpServer,
            id: "x".into(),
            target_path: ".mcp.json".into(),
            command: None,
            content: "new".into(),
            local: local.map(str::to_string),
            fingerprint: fingerprint.into(),
            installed,
            server_hash: Some("ours".into()),
            slot: Slot::Workspace,
        }
    }

    #[test]
    fn test_classify_states() {
        let rec = |sha: &str, server: Option<&str>| Record {
            sha256: sha.into(),
            server_sha256: server.map(str::to_string),
        };
        assert_eq!(classify(&item("f", None, false), None, None), State::New);
        assert_eq!(
            classify(&item("f", Some("old"), false), None, None),
            State::Changed
        );
        assert_eq!(
            classify(&item("f", Some("new"), true), None, Some("ours")),
            State::Adopted
        );
        assert_eq!(
            classify(&item("f", None, false), Some(&rec("f", None)), Some("ours")),
            State::Accepted
        );
        assert_eq!(
            classify(&item("g", Some("old"), false), Some(&rec("f", None)), None),
            State::Changed
        );
        // A server hash that differs: never adopted silently, accepted only
        // with the same server hash as at acceptance.
        assert_eq!(
            classify(&item("f", Some("new"), true), None, Some("theirs")),
            State::Changed
        );
        assert_eq!(
            classify(
                &item("f", None, false),
                Some(&rec("f", None)),
                Some("theirs")
            ),
            State::Changed
        );
        assert_eq!(
            classify(
                &item("f", None, false),
                Some(&rec("f", Some("theirs"))),
                Some("theirs")
            ),
            State::Accepted
        );
    }

    #[test]
    fn test_store_round_trip_is_per_workspace_and_private() {
        let cfg = tempfile::tempdir().unwrap();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        assert_ne!(
            Store::path_for(cfg.path(), a.path()),
            Store::path_for(cfg.path(), b.path())
        );
        let mut store = store_in(cfg.path(), a.path());
        store.record(&item("f", None, false), Some("theirs".into()));
        store.save().unwrap();
        let loaded = store_in(cfg.path(), a.path());
        assert_eq!(
            loaded.data.items.get("mcp_server:x"),
            Some(&Record {
                sha256: "f".into(),
                server_sha256: Some("theirs".into())
            })
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(Store::path_for(cfg.path(), a.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(store_in(cfg.path(), b.path()).data.items.is_empty());
    }

    #[test]
    fn test_collect_agent_export_finds_every_kind() {
        let ws = tempfile::tempdir().unwrap();
        let items = collect_agent_export(ws.path(), &agent_export(), true, ".nexus");
        let labels: Vec<String> = items.iter().map(Item::label).collect();
        for expected in [
            "agent_script .claude/hooks/usage.mjs",
            "zellij_layout .nexus/claude/team.kdl",
            "mcp_server fresh",
            "mcp_server kept",
            "claude_hook session-guard",
            "claude_status_line statusLine",
            "claude_plugin core@nexus",
            "claude_marketplace nexus",
            "zellij_layout .nexus/claude/workspace.local.kdl",
        ] {
            assert!(
                labels.contains(&expected.to_string()),
                "{expected}: {labels:?}"
            );
        }
        // Disabled plugins, plain settings and markdown are not executable.
        assert!(!labels
            .iter()
            .any(|l| l.contains("off@nexus") || l.contains("model")));
        assert!(!labels.iter().any(|l| l.contains("AGENTS.md")));
        let status = items
            .iter()
            .find(|i| i.kind == Kind::ClaudeStatusLine)
            .unwrap();
        assert_eq!(status.command.as_deref(), Some("nexus hud"));
        // OpenCode: no Claude Code settings, hooks or layouts.
        let items = collect_agent_export(ws.path(), &agent_export(), false, ".nexus");
        assert!(items.iter().all(|i| matches!(
            i.kind,
            Kind::McpServer | Kind::ZellijLayout | Kind::AgentScript
        )));
        assert!(!items.iter().any(|i| i.kind == Kind::ClaudeHook));
    }

    #[test]
    fn test_mcp_values_are_masked_and_hashed_with_values() {
        let ws = tempfile::tempdir().unwrap();
        let items = collect_agent_export(ws.path(), &agent_export(), true, ".nexus");
        let fresh = items.iter().find(|i| i.id == "fresh").unwrap();
        assert!(!fresh.content.contains("\"on\""), "{}", fresh.content);
        assert!(fresh.content.contains("sha256:"));
        assert_eq!(fresh.command.as_deref(), Some("fresh-server"));
        let mut export = agent_export();
        export
            .mcp_servers
            .get_mut("fresh")
            .unwrap()
            .environment
            .insert("MODE".into(), "off".into());
        let changed = collect_agent_export(ws.path(), &export, true, ".nexus");
        let changed = changed.iter().find(|i| i.id == "fresh").unwrap();
        assert_ne!(changed.fingerprint, fresh.fingerprint);
        // The server never sees env values, so its hash stays the same.
        assert_eq!(changed.server_hash, fresh.server_hash);
    }

    #[test]
    fn test_local_mcp_reads_back_both_formats() {
        let ws = tempfile::tempdir().unwrap();
        write(
            ws.path(),
            ".mcp.json",
            r#"{"mcpServers":{"kept":{"command":"npx","args":["-y","kept"],"env":{"TOKEN":"${TOKEN}","MODE":"on"}}}}"#,
        );
        write(
            ws.path(),
            "opencode.json",
            r#"{"mcp":{"kept":{"type":"local","command":["npx","-y","kept"],"environment":{"TOKEN":"{env:TOKEN}"}}}}"#,
        );
        let claude = local_mcp(ws.path(), true, "kept").unwrap();
        assert_eq!(claude.command, ["npx", "-y", "kept"]);
        assert_eq!(claude.env_keys, ["TOKEN"]);
        assert_eq!(
            claude.environment.get("MODE").map(String::as_str),
            Some("on")
        );
        let opencode = local_mcp(ws.path(), false, "kept").unwrap();
        let export = agent_export();
        assert_eq!(mcp_value(&opencode), mcp_value(&export.mcp_servers["kept"]));
        assert!(local_mcp(ws.path(), true, "missing").is_none());
    }

    #[test]
    fn test_installed_items_are_adopted_without_asking() {
        let cfg = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        write(ws.path(), ".claude/hooks/usage.mjs", "console.log('new')\n");
        write(
            ws.path(),
            ".mcp.json",
            r#"{"mcpServers":{"kept":{"command":"npx","args":["-y","kept"],"env":{"TOKEN":"${TOKEN}"}}}}"#,
        );
        write(
            ws.path(),
            ".claude/settings.json",
            r#"{"statusLine":{"type":"command","command":"nexus hud"},"enabledPlugins":{"core@nexus":true}}"#,
        );
        let items = collect_agent_export(ws.path(), &agent_export(), true, ".nexus");
        let mut gate = Gate::with(store_in(cfg.path(), ws.path()), Answer::Refuse);
        let refused = gate.resolve(&items, &[], &mut |_| unreachable!());
        let refused: Vec<String> = refused.iter().map(|&i| items[i].label()).collect();
        for adopted in [
            "agent_script .claude/hooks/usage.mjs",
            "mcp_server kept",
            "claude_status_line statusLine",
            "claude_plugin core@nexus",
        ] {
            assert!(!refused.contains(&adopted.to_string()), "{adopted}");
        }
        assert!(refused.contains(&"mcp_server fresh".to_string()));
        let saved = store_in(cfg.path(), ws.path());
        assert!(saved.data.items.contains_key("mcp_server:kept"));
        assert!(!saved.data.items.contains_key("mcp_server:fresh"));
    }

    #[test]
    fn test_trust_records_and_next_run_asks_nothing() {
        let cfg = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let items = collect_agent_export(ws.path(), &agent_export(), true, ".nexus");
        let mut gate = Gate::with(store_in(cfg.path(), ws.path()), Answer::Trust);
        assert!(gate
            .resolve(&items, &[], &mut |_| unreachable!())
            .is_empty());
        assert_eq!(gate.accepted, items.len());
        let mut gate = Gate::with(store_in(cfg.path(), ws.path()), Answer::Ask);
        assert!(gate
            .resolve(&items, &[], &mut |_| panic!("nothing to ask"))
            .is_empty());
    }

    #[test]
    fn test_ask_answers_per_item() {
        let cfg = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let items = collect_agent_export(ws.path(), &agent_export(), true, ".nexus");
        let mut gate = Gate::with(store_in(cfg.path(), ws.path()), Answer::Ask);
        let refused = gate.resolve(&items, &[], &mut |pending| {
            pending
                .iter()
                .map(|(i, _)| i.kind == Kind::McpServer)
                .collect()
        });
        assert!(refused.iter().all(|&i| items[i].kind != Kind::McpServer));
        assert_eq!(refused.len(), items.len() - 2);
    }

    #[test]
    fn test_server_mismatch_and_undeclared() {
        let cfg = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        write(ws.path(), ".claude/hooks/usage.mjs", "console.log('new')\n");
        let items = collect_agent_export(ws.path(), &agent_export(), true, ".nexus");
        let declared = vec![ExportedExecutable {
            kind: "agent_script".into(),
            id: ".claude/hooks/usage.mjs".into(),
            target_path: ".claude/hooks/usage.mjs".into(),
            command: None,
            sha256: "0".repeat(64),
        }];
        let mut gate = Gate::with(store_in(cfg.path(), ws.path()), Answer::Refuse);
        let refused = gate.resolve(&items, &declared, &mut |_| unreachable!());
        // Installed, but the server's hash differs: asked, so refused here.
        assert!(refused
            .iter()
            .any(|&i| items[i].id == ".claude/hooks/usage.mjs"));
    }

    #[test]
    fn test_matching_server_hashes() {
        let ws = tempfile::tempdir().unwrap();
        let items = collect_agent_export(ws.path(), &agent_export(), true, ".nexus");
        let by = |kind: Kind| items.iter().find(|i| i.kind == kind).unwrap();
        assert_eq!(
            by(Kind::ClaudeHook).server_hash.as_deref(),
            Some(sha256_hex("guard()\n").as_str())
        );
        let expected =
            sha256_hex(r#"{"command":"nexus hud","id":"statusLine","kind":"claude_status_line"}"#);
        assert_eq!(
            by(Kind::ClaudeStatusLine).server_hash.as_deref(),
            Some(expected.as_str())
        );
        let kept = items.iter().find(|i| i.id == "kept").unwrap();
        let expected = sha256_hex(
            r#"{"argv":["npx","-y","kept"],"env_keys":["TOKEN"],"id":"kept","kind":"mcp_server"}"#,
        );
        assert_eq!(kept.server_hash.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn test_mcp_server_hash_uses_every_env_key() {
        let ws = tempfile::tempdir().unwrap();
        let mut export = agent_export();
        let headroom: McpServerConfig = serde_json::from_value(json!({
            "command": ["headroom"],
            "args": ["mcp", "serve"],
            "env_keys": ["HEADROOM_MODE"],
            "environment": { "HEADROOM_MODE": "transform", "HEADROOM_DEBUG": "0" }
        }))
        .unwrap();
        export.mcp_servers.insert("nexus-headroom".into(), headroom);
        let items = collect_agent_export(ws.path(), &export, true, ".nexus");
        let item = items.iter().find(|i| i.id == "nexus-headroom").unwrap();
        let expected = sha256_hex(
            r#"{"argv":["headroom","mcp","serve"],"env_keys":["HEADROOM_DEBUG","HEADROOM_MODE"],"id":"nexus-headroom","kind":"mcp_server"}"#,
        );
        assert_eq!(item.server_hash.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn test_claude_setting_server_hash() {
        let spec: ClaudeSettingsSpec = serde_json::from_value(json!({
            "managed_keys": ["apiKeyHelper"],
            "values": { "apiKeyHelper": "/bin/key" }
        }))
        .unwrap();
        let items = collect_settings(&spec, &Value::Null);
        assert_eq!(items[0].kind, Kind::ClaudeSetting);
        assert_eq!(items[0].command.as_deref(), Some("/bin/key"));
        let expected =
            sha256_hex(r#"{"id":"apiKeyHelper","kind":"claude_setting","value":"/bin/key"}"#);
        assert_eq!(items[0].server_hash.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn test_hook_registration_is_part_of_the_fingerprint() {
        let ws = tempfile::tempdir().unwrap();
        write(
            ws.path(),
            ".claude/hooks/nexus-session-guard.mjs",
            "guard()\n",
        );
        write(
            ws.path(),
            ".claude/settings.json",
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"node \"${CLAUDE_PROJECT_DIR}/.claude/hooks/nexus-session-guard.mjs\" stop"}]}]}}"#,
        );
        let items = collect_agent_export(ws.path(), &agent_export(), true, ".nexus");
        assert!(
            items
                .iter()
                .find(|i| i.kind == Kind::ClaudeHook)
                .unwrap()
                .installed
        );
        let mut export = agent_export();
        export.claude_hook_adapters.as_mut().unwrap()[0].hook_events[0].matcher =
            Some("Bash".into());
        let items = collect_agent_export(ws.path(), &export, true, ".nexus");
        let hook = items.iter().find(|i| i.kind == Kind::ClaudeHook).unwrap();
        assert!(!hook.installed);
        assert!(hook.content.contains("Stop matcher=Bash"));
    }

    #[test]
    fn test_refused_items_keep_local_or_are_left_out() {
        let cfg = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        write(ws.path(), ".claude/hooks/usage.mjs", "console.log('old')\n");
        write(
            ws.path(),
            ".mcp.json",
            r#"{"mcpServers":{"kept":{"command":"old-cmd","args":[],"env":{}}}}"#,
        );
        write(
            ws.path(),
            ".claude/settings.json",
            r#"{"extraKnownMarketplaces":{"nexus":{"source":{"source":"github","repo":"acme/old"}}}}"#,
        );
        let mut export = agent_export();
        let mut gate = Gate::with(store_in(cfg.path(), ws.path()), Answer::Refuse);
        gate.exports(ws.path(), Some(&mut export), None, true, ".nexus");

        // Changed items keep the local version.
        let hook = export
            .agent_files
            .iter()
            .find(|f| f.target_path == ".claude/hooks/usage.mjs")
            .unwrap();
        assert_eq!(hook.body, "console.log('old')\n");
        assert_eq!(export.mcp_servers["kept"].command, ["old-cmd"]);
        let spec = export.claude_settings.as_ref().unwrap();
        assert_eq!(
            spec.values["extraKnownMarketplaces"]["nexus"]["source"]["repo"],
            "acme/old"
        );
        // New items are left out; harmless content stays.
        assert!(!export.mcp_servers.contains_key("fresh"));
        assert!(!export
            .agent_files
            .iter()
            .any(|f| f.target_path.ends_with(".kdl")));
        assert!(export
            .agent_files
            .iter()
            .any(|f| f.target_path == ".nexus/AGENTS.md"));
        assert!(export.claude_hook_adapters.as_ref().unwrap().is_empty());
        assert!(!spec.managed_keys.contains(&"statusLine".to_string()));
        assert!(spec.values.get("statusLine").is_none());
        assert!(spec.values["enabledPlugins"].get("core@nexus").is_none());
        assert_eq!(spec.values["model"], "opus");
        assert!(export.claude_workspace.as_ref().unwrap().body.is_empty());
        assert_eq!(gate.held.len(), 9);
    }

    fn skill(body: &str, resources: &[(&str, &str)]) -> ExportedSkill {
        serde_json::from_value(json!({
            "skill_id": "nx-run",
            "name": "Run",
            "description": "Runs things",
            "version": 2,
            "body": body,
            "command_slug": null,
            "resources": resources
                .iter()
                .map(|(f, b)| json!({ "filename": f, "body": b }))
                .collect::<Vec<_>>()
        }))
        .unwrap()
    }

    #[test]
    fn test_skill_scripts_hold_the_rendered_local_version() {
        let cfg = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let old = skill("Check !`git status` first.", &[("run.sh", "echo old\n")]);
        for (path, content) in claude_render::render_claude_skill_files(&old) {
            write(ws.path(), &path, &content);
        }
        let new = skill(
            "Check !`git status --short` first.",
            &[("run.sh", "echo new\n"), ("notes.md", "plain\n")],
        );
        let items = collect_skills(ws.path(), std::slice::from_ref(&new), true, ".nexus");
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["nx-run/SKILL.md", "nx-run/run.sh"]);

        let mut skills = vec![new, skill("!`rm -rf x`", &[])];
        skills[1].skill_id = "nx-new".into();
        let mut export: SkillExportResponse = serde_json::from_value(json!({
            "action": "sk_export",
            "project": { "id": "p", "slug": "p", "name": "P" },
            "skills": skills,
            "count": 2
        }))
        .unwrap();
        let mut gate = Gate::with(store_in(cfg.path(), ws.path()), Answer::Refuse);
        gate.exports(ws.path(), None, Some(&mut export), true, ".nexus");
        assert_eq!(export.skills.len(), 1, "the new skill is left out");
        // Rendering the held skill gives exactly what is on disk.
        for (path, content) in claude_render::render_claude_skill_files(&export.skills[0]) {
            if path.ends_with("notes.md") {
                continue;
            }
            assert_eq!(
                fs::read_to_string(ws.path().join(&path)).unwrap(),
                content,
                "{path}"
            );
        }
    }

    #[test]
    fn test_workspace_refusal_returns_paths_to_skip() {
        let cfg = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        write(ws.path(), "devbox.json", "{\"packages\":[]}\n");
        let export: WorkspaceForkExportResponse = serde_json::from_value(json!({
            "devbox_json": "{\"packages\":[]}\n",
            "scripts": [
                { "path": "scripts/setup.sh", "body": "echo hi\n" },
                { "path": "scripts/README.md", "body": "docs\n" },
                { "path": "scripts/tool", "body": "x\n", "executable": true }
            ],
            "meta": {}
        }))
        .unwrap();
        let mut gate = Gate::with(store_in(cfg.path(), ws.path()), Answer::Refuse);
        let held = gate.workspace_v2(ws.path(), &export);
        let expected: HashSet<String> = ["scripts/setup.sh", "scripts/tool"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            held, expected,
            "devbox.json is unchanged, README is no script"
        );
    }

    #[test]
    fn test_local_scripts_need_consent() {
        let cfg = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        write(ws.path(), "devbox.json", "{}\n");
        let export: WorkspaceForkExportResponse = serde_json::from_value(json!({
            "devbox_json": "{}\n",
            "scripts": [],
            "local_scripts": { "scripts/devbox/init/local.sh": "echo local\n" },
            "meta": {}
        }))
        .unwrap();
        let items = collect_workspace_v2(ws.path(), &export);
        assert!(items.iter().any(
            |i| i.kind == Kind::DevboxScript && i.target_path == "scripts/devbox/init/local.sh"
        ));
        let mut gate = Gate::with(store_in(cfg.path(), ws.path()), Answer::Refuse);
        let held = gate.workspace_v2(ws.path(), &export);
        assert!(held.contains("scripts/devbox/init/local.sh"));
    }
}
