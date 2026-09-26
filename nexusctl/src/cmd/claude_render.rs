//! Claude Code renderer (Track B1, ADR-C04 / ADR-C06).
//!
//! Generates native Claude Code project artifacts from the same canonical
//! skill and actor data the OpenCode renderer already consumes:
//!
//! - `CLAUDE.md` (project root, thin wrapper)
//! - `.claude/settings.json` (Claude runtime behavior only, no secrets)
//! - `.claude/skills/<canonical-id>/SKILL.md` (one dir per Nexus skill)
//! - `.claude/agents/<slug>.md` (one file per assigned actor)
//!
//! `.mcp.json` (project root) is written separately by the shared MCP
//! writer (`write_mcp_configs` in `init.rs` / `pull.rs`) per the path
//! decision in ADR-C04 (2026-09-20 amendment, Dispatch b8e001e3).
//!
//! This module is purely additive: it never touches the OpenCode
//! projection (`opencode.json`, `.opencode/`) or the `agentic_root`-relative
//! canonical paths (`<agentic_root>/skills/`, `<agentic_root>/actors/`).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use console::style;
use nexus_core::api::{
    ClaudeHookAdapter, ClaudeSettingsSpec, ExportedActorFile, ExportedAgentFile, ExportedSkill,
};

use super::ccx;

/// Map a canonical Nexus skill ID to the public Claude Code command
/// namespace. Legacy `nx-*` skill IDs are migrated to `nexus-*`
/// (ADR-C06 "Canonical command identity"); IDs already in the `nexus-*`
/// namespace pass through unchanged.
pub fn canonical_claude_skill_id(skill_id: &str) -> String {
    match skill_id.strip_prefix("nx-") {
        Some(rest) => format!("nexus-{}", rest),
        None => skill_id.to_string(),
    }
}

/// Strip a leading YAML frontmatter block (`---\n...\n---`) from `body`, if
/// present.
///
/// `ExportedSkill.body` (and agent-file bodies) already come back from the
/// backend with their own frontmatter block baked in (nexus-app's own
/// `stripFrontmatter()`, `src/lib/skill-frontmatter.ts`). Every local writer
/// in this CLI (`write_claude_skill` here, and `write_skill` in
/// `init.rs`/`pull.rs`) rebuilds its own frontmatter around `body` — using
/// this side's canonicalized `skill_id` for Claude Code, plus fields the
/// backend's copy may not carry — so the backend's block must be removed
/// first, or it is duplicated verbatim underneath the local one
/// (NEXUS-APP dispatch 5ddd6355, confirmed live: a `.nexus/skills/nx-init/
/// SKILL.md` in this very repo carried two stacked frontmatter blocks
/// before this fix).
///
/// Only strips a block that starts at the very beginning of `body` (after
/// leading whitespace) with a `---` line and closes with another `---`
/// line; returns `body` unchanged otherwise (no frontmatter, or a body that
/// merely contains a `---` horizontal rule further down).
pub(crate) fn strip_frontmatter(body: &str) -> &str {
    let trimmed = body.trim_start();
    let Some(after_open) = trimmed.strip_prefix("---\n") else {
        return body;
    };
    let Some(close_pos) = after_open.find("\n---") else {
        return body;
    };
    let after_close = &after_open[close_pos + "\n---".len()..];
    // The closing delimiter must end the line (EOF or a newline) rather
    // than just happen to prefix a longer line (e.g. "----" or "--- foo").
    if !(after_close.is_empty() || after_close.starts_with('\n')) {
        return body;
    }
    after_close.trim_start_matches('\n')
}

/// Write a single skill as a native Claude Code project skill:
/// `.claude/skills/<canonical-id>/SKILL.md` (+ any resource files).
/// The directory name becomes the `/<canonical-id>` slash-command in
/// Claude Code (project skill directory names are command names).
/// Returns `true` if any file was written (unchanged files are left alone).
pub fn write_claude_skill(target: &Path, skill: &ExportedSkill) -> anyhow::Result<bool> {
    let mut written = false;
    for (rel, content) in render_claude_skill_files(skill) {
        let path = target.join(&rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        written |= write_if_changed(&path, &content)?;
    }
    Ok(written)
}

/// The files [`write_claude_skill`] writes for `skill`, as
/// `(workspace-relative path, content)` pairs: `SKILL.md` first, then the
/// resource files.
pub fn render_claude_skill_files(skill: &ExportedSkill) -> Vec<(String, String)> {
    let canonical_id = canonical_claude_skill_id(&skill.skill_id);
    let skill_dir = format!(".claude/skills/{canonical_id}");

    let raw_body = skill
        .body
        .as_deref()
        .unwrap_or("<!-- No skill body defined -->");
    let body = strip_frontmatter(raw_body);

    let content = format!(
        r#"---
skill_id: {skill_id}
name: {name}
description: {description}
version: {version}
command_slug: {command_slug}
source: nexus-platform
---

{body}
"#,
        skill_id = canonical_id,
        name = skill.name,
        description = yaml_escape(skill.description.as_deref().unwrap_or("")),
        version = skill.version,
        command_slug = skill.command_slug.as_deref().unwrap_or("none"),
        body = body,
    );

    let mut files = vec![(format!("{skill_dir}/SKILL.md"), content)];
    for res in &skill.resources {
        // Sanitize filename: prevent directory traversal.
        let filename = res.filename.replace(['/', '\\'], "_");
        if filename.is_empty() || filename == "SKILL.md" {
            continue;
        }
        files.push((format!("{skill_dir}/{filename}"), res.body.clone()));
    }
    files
}

/// Write `content` to `path` unless it already has exactly that content.
/// Returns `true` if the file was written.
fn write_if_changed(path: &Path, content: &str) -> anyhow::Result<bool> {
    if fs::read(path).is_ok_and(|c| c == content.as_bytes()) {
        return Ok(false);
    }
    fs::write(path, content)?;
    Ok(true)
}

/// Quote and escape a string for safe use as a YAML flow scalar in the
/// frontmatter templates in this file. Skill descriptions are free text and
/// may contain `:` or other characters that break an unquoted YAML scalar;
/// `name`/`version`/`command_slug` are left unquoted to match the existing,
/// already-shipped template style.
pub(crate) fn yaml_escape(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Derive an actor slug from an `ExportedAgentFile`'s `target_path`, if that
/// file lives directly under an `.../actors/` directory (e.g.
/// `.nexus/actors/technical-project-manager.md` -> `technical-project-manager`).
///
/// Some backend versions deliver actor-based project actors exclusively
/// through the generic `agent_files` list (with `target_path` already
/// pointing at `<agentic_root>/actors/<slug>.md`) rather than through the
/// dedicated `actors` field on the af_export response — the dedicated field
/// can be empty even when actors are assigned (NEXUS-APP dispatch c7701485
/// follow-up: actor_based + claude-cli projects reported empty
/// `.claude/agents/` despite `.nexus/actors/*.md` rendering correctly via
/// this exact `agent_files` path). Both sources must be considered.
pub fn actor_slug_from_agent_file(target_path: &str) -> Option<String> {
    let path = Path::new(target_path);
    if path.extension().and_then(|e| e.to_str()) != Some("md") {
        return None;
    }
    let parent_is_actors = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n == "actors")
        .unwrap_or(false);
    if !parent_is_actors {
        return None;
    }
    path.file_stem().map(|s| s.to_string_lossy().to_string())
}

/// Write all assigned actors as native Claude Code sub-agent definitions:
/// `.claude/agents/<slug>.md`. Merges two possible data sources so no
/// backend response shape leaves this empty:
///
/// 1. The dedicated `actors` field on af_export (`ExportedActorFile`).
/// 2. Generic `agent_files` entries whose `target_path` lives under an
///    `.../actors/` directory (see `actor_slug_from_agent_file`).
///
/// Reuses the same profile markdown body already delivered for
/// `<agentic_root>/actors/<slug>.md` — one canonical actor definition, two
/// projections. Returns the number of files written.
pub fn write_claude_agents(
    target: &Path,
    actors: &[ExportedActorFile],
    agent_files: &[ExportedAgentFile],
) -> anyhow::Result<usize> {
    let files = claude_agent_files(actors, agent_files);
    if files.is_empty() {
        return Ok(0);
    }

    let agents_dir = target.join(".claude").join("agents");
    fs::create_dir_all(&agents_dir)?;

    for (rel, body) in &files {
        fs::write(target.join(rel), body)?;
    }
    Ok(files.len())
}

/// The files [`write_claude_agents`] writes, as `(workspace-relative path,
/// content)` pairs (`.claude/agents/<slug>.md`), one per actor slug.
pub fn claude_agent_files(
    actors: &[ExportedActorFile],
    agent_files: &[ExportedAgentFile],
) -> Vec<(String, String)> {
    let mut by_slug: BTreeMap<String, String> = BTreeMap::new();

    for actor in actors {
        by_slug.insert(actor.slug.clone(), actor.body.clone());
    }
    for af in agent_files {
        if let Some(slug) = actor_slug_from_agent_file(&af.target_path) {
            by_slug.entry(slug).or_insert_with(|| af.body.clone());
        }
    }

    by_slug
        .into_iter()
        .map(|(slug, body)| (format!(".claude/agents/{slug}.md"), body))
        .collect()
}

/// Write the provider/model routing catalog consumed by the Claude Code
/// `routing-guard` plugin adapter (env var `NEXUS_ROUTING_GUARD_CATALOG_PATH`,
/// NEXUS-APP dispatch 7a2d2adb, ADR-C05 Track B2). Sourced from
/// `runtime_spec.model_routes` (ADR-C04/F1, af_export commit db5d053) —
/// OpenCode's equivalent adapter reads the same data live from the SDK
/// (`client.config.providers()`), so Claude Code needs it materialized to
/// disk instead. Returns the workspace-relative path if written, `None` if
/// no `runtime_spec` (or no `model_routes` within it) was available.
pub fn write_routing_catalog(
    target: &Path,
    agentic_root: &str,
    runtime_spec: Option<&serde_json::Value>,
) -> anyhow::Result<Option<String>> {
    let Some(routes) = runtime_spec.and_then(|rs| rs.get("model_routes")) else {
        return Ok(None);
    };
    let generated_dir = target.join(agentic_root).join("generated");
    fs::create_dir_all(&generated_dir)?;
    let content = serde_json::to_string_pretty(&serde_json::json!({ "model_routes": routes }))?;
    fs::write(generated_dir.join("routing-catalog.json"), content + "\n")?;
    Ok(Some(format!(
        "{}/generated/routing-catalog.json",
        agentic_root
    )))
}

/// Write the agent-routing table consumed by the Claude Code `routing-guard`
/// plugin adapter (env var `NEXUS_ROUTING_GUARD_AGENTS_PATH`, NEXUS-APP
/// dispatch 7a2d2adb). Sourced from `runtime_spec.actors` and
/// `runtime_spec.primary_agents` — OpenCode's equivalent reads the live
/// agent-routing table from `client.app.agents()`. Returns the
/// workspace-relative path if written, `None` if `runtime_spec` had neither
/// field.
pub fn write_agent_routing(
    target: &Path,
    agentic_root: &str,
    runtime_spec: Option<&serde_json::Value>,
) -> anyhow::Result<Option<String>> {
    let Some(rs) = runtime_spec else {
        return Ok(None);
    };
    let actors = rs.get("actors");
    let primary_agents = rs.get("primary_agents");
    if actors.is_none() && primary_agents.is_none() {
        return Ok(None);
    }

    let generated_dir = target.join(agentic_root).join("generated");
    fs::create_dir_all(&generated_dir)?;
    let content = serde_json::to_string_pretty(&serde_json::json!({
        "actors": actors.cloned().unwrap_or(serde_json::Value::Array(vec![])),
        "primary_agents": primary_agents.cloned().unwrap_or(serde_json::Value::Array(vec![])),
    }))?;
    fs::write(generated_dir.join("agent-routing.json"), content + "\n")?;
    Ok(Some(format!(
        "{}/generated/agent-routing.json",
        agentic_root
    )))
}

/// Convert a Claude Code hook event name from PascalCase to kebab-case,
/// matching the adapters' own `process.argv[2]` subcommand contract
/// (confirmed against adapter source, NEXUS-APP dispatch 2d5017f7:
/// `PostToolUse` -> `post-tool-use`, `PreCompact` -> `pre-compact`).
fn kebab_case_event(event: &str) -> String {
    let mut out = String::with_capacity(event.len() + 4);
    for (i, c) in event.chars().enumerate() {
        if c.is_uppercase() {
            if i != 0 {
                out.push('-');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Write each Claude Code hook adapter's bundled script to its
/// `target_path` (NEXUS-APP dispatch 2d5017f7, Track B3). Platform-managed
/// generated code: always (re)written, matching how `.opencode/plugins/*.ts`
/// is kept in sync on every `nexus pull`. Returns the number of scripts
/// written.
pub fn write_claude_hook_adapters(
    target: &Path,
    adapters: &[ClaudeHookAdapter],
) -> anyhow::Result<usize> {
    let mut written = 0;
    for adapter in adapters {
        let path = target.join(&adapter.target_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, &adapter.body)?;
        written += 1;
    }
    Ok(written)
}

/// Merge hook registrations for each adapter into `.claude/settings.json`'s
/// `hooks` block (NEXUS-APP dispatch 2d5017f7, Track B3).
///
/// Idempotent per plugin, and safe to run against an already-existing,
/// operator-customized `settings.json` (unlike the `env` block, which is
/// create-once-only): for each `(event, adapter)` pair, only appends a new
/// `{matcher, hooks: [...]}` entry to that event's array if no existing
/// entry's command already references the adapter's `target_path`. Never
/// replaces or removes an existing entry. Multiple plugins may register
/// against the same event (e.g. `headroom-intercept` and `cost-control`
/// both on `Stop`) as independent array entries — confirmed live against a
/// real Claude Code session, per the dispatch's verification notes.
///
/// Returns `(appended, removed_plugin_names)`: the number of new hook
/// entries appended, and the plugin names of any adapters that were
/// previously Nexus-managed (per `previous_hooks`, from the CCX lock) but
/// are no longer in `adapters` -- e.g. because the `nexus-core` Claude
/// plugin now provides the same hooks (NEXUS-APP dispatch 99f335e8
/// follow-up). For each such adapter, only the exact settings.json hook
/// entries Nexus itself wrote (matched on event/matcher/command) are
/// removed -- anything an operator added is left alone -- and the hook
/// script file under `.claude/hooks/` is deleted only if its content on
/// disk still matches what Nexus last wrote there (the "orphaned" rule:
/// never delete a file the operator modified).
///
/// The file is only rewritten when `appended > 0` or something was
/// actually removed.
pub fn merge_claude_hooks(
    target: &Path,
    adapters: &[ClaudeHookAdapter],
    previous_hooks: Option<&std::collections::BTreeMap<String, ccx::CcxLockHookEntry>>,
) -> anyhow::Result<(usize, Vec<String>)> {
    let removable: Vec<&ccx::CcxLockHookEntry> = previous_hooks
        .map(|prev| {
            let current_paths: std::collections::HashSet<&str> =
                adapters.iter().map(|a| a.target_path.as_str()).collect();
            prev.values()
                .filter(|entry| !current_paths.contains(entry.target_path.as_str()))
                .collect()
        })
        .unwrap_or_default();

    if adapters.is_empty() && removable.is_empty() {
        return Ok((0, Vec::new()));
    }

    let settings_path = target.join(".claude").join("settings.json");
    let mut settings: serde_json::Value = if settings_path.exists() {
        let raw = fs::read_to_string(&settings_path)?;
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if !settings.is_object() {
        settings = serde_json::json!({});
    }
    let settings_obj = settings.as_object_mut().expect("just ensured object");
    let hooks = settings_obj
        .entry("hooks")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !hooks.is_object() {
        *hooks = serde_json::Value::Object(serde_json::Map::new());
    }
    let hooks_obj = hooks.as_object_mut().expect("just ensured object");

    let mut removed_plugin_names = Vec::new();
    for entry in &removable {
        remove_hook_registrations(hooks_obj, entry);
        let script_path = target.join(&entry.target_path);
        if let Ok(content) = fs::read_to_string(&script_path) {
            if nexus_core::hash::sha256_hex(&content) == entry.file_sha256 {
                let _ = fs::remove_file(&script_path);
            }
        }
        removed_plugin_names.push(entry.plugin_name.clone());
    }

    let mut appended = 0;
    for adapter in adapters {
        for hook_event in &adapter.hook_events {
            let event_array = hooks_obj
                .entry(hook_event.event.clone())
                .or_insert_with(|| serde_json::Value::Array(Vec::new()));
            if !event_array.is_array() {
                *event_array = serde_json::Value::Array(Vec::new());
            }
            let event_array = event_array.as_array_mut().expect("just ensured array");

            let already_registered = event_array.iter().any(|entry| {
                entry
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|hooks| {
                        hooks.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .is_some_and(|c| c.contains(&adapter.target_path))
                        })
                    })
                    .unwrap_or(false)
            });
            if already_registered {
                continue;
            }

            let command = format!(
                "node \"${{CLAUDE_PROJECT_DIR}}/{}\" {}",
                adapter.target_path,
                kebab_case_event(&hook_event.event)
            );
            let mut hook_command = serde_json::json!({
                "type": "command",
                "command": command,
            });
            if let Some(timeout) = hook_event.timeout {
                hook_command["timeout"] = serde_json::json!(timeout);
            }

            let mut new_entry = serde_json::Map::new();
            if let Some(ref matcher) = hook_event.matcher {
                new_entry.insert(
                    "matcher".to_string(),
                    serde_json::Value::String(matcher.clone()),
                );
            }
            new_entry.insert(
                "hooks".to_string(),
                serde_json::Value::Array(vec![hook_command]),
            );

            event_array.push(serde_json::Value::Object(new_entry));
            appended += 1;
        }
    }

    if appended > 0 || !removed_plugin_names.is_empty() {
        if let Some(parent) = settings_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(&settings)? + "\n";
        fs::write(&settings_path, content)?;
    }

    Ok((appended, removed_plugin_names))
}

/// Remove exactly the `.claude/settings.json` hook entries Nexus wrote for
/// one adapter (matched on event, matcher and command), leaving every other
/// entry alone. Returns the number of entries removed.
pub(crate) fn remove_hook_registrations(
    hooks_obj: &mut serde_json::Map<String, serde_json::Value>,
    entry: &ccx::CcxLockHookEntry,
) -> usize {
    let mut removed = 0;
    for reg in &entry.registrations {
        if let Some(event_array) = hooks_obj.get_mut(&reg.event).and_then(|v| v.as_array_mut()) {
            let before = event_array.len();
            event_array.retain(|item| {
                let matcher_matches =
                    item.get("matcher").and_then(|m| m.as_str()) == reg.matcher.as_deref();
                let command_matches =
                    item.get("hooks")
                        .and_then(|h| h.as_array())
                        .is_some_and(|hooks| {
                            hooks.iter().any(|h| {
                                h.get("command").and_then(|c| c.as_str())
                                    == Some(reg.command.as_str())
                            })
                        });
                !(matcher_matches && command_matches)
            });
            removed += before - event_array.len();
        }
    }
    removed
}

/// Merge `includeCoAuthoredBy` into `.claude/settings.json` from the
/// project's `git_config.include_co_authored_by` (NEXUS-APP dispatch
/// 84e38bd7). Stored uninverted in Claude Code's own semantics: `true`
/// means the `Co-Authored-By: Claude ...` trailer is added.
///
/// A hard operator requirement ("must never appear"), not a cosmetic
/// preference, so this runs on every init/pull and merges into an
/// already-existing, operator-customized `settings.json` exactly like
/// [`merge_claude_hooks`] — unlike the `env` block in
/// [`write_claude_settings`], which is create-once-only. Every other key,
/// including the `hooks` block, is preserved verbatim; only
/// `includeCoAuthoredBy` is touched.
///
/// `include` is `None` for any project created before this field existed
/// on the backend. Absent means suppress: `false` is written, not Claude
/// Code's own default, since the entire point of this field is that
/// operators must not have to remember to configure it per project (the
/// original directive-only version of this requirement was exactly the
/// "have to remember every time" failure mode this change replaces).
///
/// Returns `true` if the file was created or its `includeCoAuthoredBy`
/// value changed; `false` if it already matched (idempotent, avoids
/// needless rewrites on every pull).
pub fn merge_claude_co_authored_by(target: &Path, include: Option<bool>) -> anyhow::Result<bool> {
    let resolved = include.unwrap_or(false);
    let settings_path = target.join(".claude").join("settings.json");
    let mut settings: serde_json::Value = if settings_path.exists() {
        let raw = fs::read_to_string(&settings_path)?;
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    if !settings.is_object() {
        settings = serde_json::json!({});
    }
    let obj = settings.as_object_mut().expect("just ensured object");

    if obj.get("includeCoAuthoredBy").and_then(|v| v.as_bool()) == Some(resolved) {
        return Ok(false);
    }

    obj.insert(
        "includeCoAuthoredBy".to_string(),
        serde_json::Value::Bool(resolved),
    );

    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(&settings)? + "\n";
    fs::write(&settings_path, content)?;
    Ok(true)
}

/// Merge a generic, forward-compatible set of Nexus-managed keys into
/// `.claude/settings.json` (NEXUS-APP ADR-0117 "CCX", dispatch bb782869).
///
/// Runs on every init/pull, not create-once (same pattern as
/// [`merge_claude_hooks`]/[`merge_claude_co_authored_by`]): for each dot
/// path in `spec.managed_keys`, the corresponding value from
/// `spec.values` is set/replaced at that path. If both the existing value
/// and the new value at a path are JSON arrays (e.g.
/// `"permissions.deny"`), they are unioned rather than replaced outright,
/// so an operator's own entries are preserved alongside Nexus's.
///
/// Keys (or array entries) Nexus used to manage per `previous` (the CCX
/// lock) but no longer sends are removed, see
/// [`super::ccx::reconcile_settings_removed_keys`].
///
/// Returns the number of managed keys whose value actually changed (0 if
/// nothing changed or `spec` is `None`; the file is only rewritten when
/// this is non-zero).
pub fn merge_claude_generic_settings(
    target: &Path,
    spec: Option<&ClaudeSettingsSpec>,
    previous: Option<&ClaudeSettingsSpec>,
) -> anyhow::Result<usize> {
    let spec_has_keys = spec.is_some_and(|s| !s.managed_keys.is_empty());
    let previous_has_keys = previous.is_some_and(|p| !p.managed_keys.is_empty());
    if !spec_has_keys && !previous_has_keys {
        return Ok(0);
    }

    let settings_path = target.join(".claude").join("settings.json");
    let mut settings: serde_json::Value = if settings_path.exists() {
        let raw = fs::read_to_string(&settings_path)?;
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    if !settings.is_object() {
        settings = serde_json::json!({});
    }

    let changed = apply_generic_settings(&mut settings, spec, previous);
    if changed > 0 {
        if let Some(parent) = settings_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(&settings)? + "\n";
        fs::write(&settings_path, content)?;
    }

    Ok(changed)
}

/// The pure part of [`merge_claude_generic_settings`]: apply `spec` (and
/// the removal rules against `previous`) to an in-memory `settings.json`
/// value. Returns the number of keys/paths changed. Also used read-only on
/// a copy by `nexus claude status`/`diff`.
pub fn apply_generic_settings(
    settings: &mut serde_json::Value,
    spec: Option<&ClaudeSettingsSpec>,
    previous: Option<&ClaudeSettingsSpec>,
) -> usize {
    if !settings.is_object() {
        *settings = serde_json::json!({});
    }
    let mut changed = 0usize;
    if let Some(spec) = spec {
        for key_path in &spec.managed_keys {
            let Some(new_value) = spec.value(key_path) else {
                continue;
            };
            let merged_value = match (json_get_path(settings, key_path), new_value) {
                (
                    Some(serde_json::Value::Array(existing_arr)),
                    serde_json::Value::Array(new_arr),
                ) => {
                    let mut merged = existing_arr.clone();
                    for item in new_arr {
                        if !merged.contains(item) {
                            merged.push(item.clone());
                        }
                    }
                    serde_json::Value::Array(merged)
                }
                _ => new_value.clone(),
            };
            if json_get_path(settings, key_path) != Some(&merged_value) {
                json_set_path(settings, key_path, merged_value);
                changed += 1;
            }
        }
    }

    // Remove a key/array-entries Nexus used to manage but no longer sends
    // (NEXUS-APP ADR-0117 follow-up, dispatch 99f335e8): never clobbers a
    // value the operator changed since it was last recorded.
    changed += super::ccx::reconcile_settings_removed_keys(settings, spec, previous);

    changed
}

/// Read a value at a dot-separated path (e.g. `"permissions.deny"`) inside
/// a JSON object tree, without creating anything.
fn json_get_path<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut current = value;
    for part in path.split('.') {
        current = current.as_object()?.get(part)?;
    }
    Some(current)
}

/// Set a value at a dot-separated path inside a JSON object tree,
/// creating intermediate objects as needed (replacing anything at an
/// intermediate step that is not already an object).
fn json_set_path(value: &mut serde_json::Value, path: &str, new_value: serde_json::Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = value;
    for (i, part) in parts.iter().enumerate() {
        if !current.is_object() {
            *current = serde_json::Value::Object(serde_json::Map::new());
        }
        let obj = current.as_object_mut().expect("just ensured object");
        if i == parts.len() - 1 {
            obj.insert((*part).to_string(), new_value);
            return;
        }
        current = obj
            .entry((*part).to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    }
}

pub(crate) const CLAUDE_MD_MANAGED_BEGIN: &str = "<!-- BEGIN:nexus-managed -->";
pub(crate) const CLAUDE_MD_MANAGED_END: &str = "<!-- END:nexus-managed -->";

/// Result of [`merge_claude_md_managed_block`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeMdOutcome {
    /// No block was sent (or the markers are malformed); nothing touched.
    Skipped,
    /// The block already matched. Carries the sha256 of the block text.
    Unchanged(String),
    /// The block was created or replaced. Carries the sha256 of the block
    /// text now on disk.
    Written(String),
    /// The block was edited locally since the CLI last wrote it and a
    /// different block arrived: kept as is (without `--force`).
    Conflict,
}

/// The exact text between the managed-block markers, if both are present
/// and in order.
pub fn claude_md_block_text(content: &str) -> Option<&str> {
    let start = content.find(CLAUDE_MD_MANAGED_BEGIN)? + CLAUDE_MD_MANAGED_BEGIN.len();
    let end = content.find(CLAUDE_MD_MANAGED_END)?;
    (end >= start).then(|| &content[start..end])
}

/// The block text as the CLI writes it between the markers.
pub fn claude_md_desired_block_text(block: &str) -> String {
    format!("\n{}\n", block.trim())
}

/// Maintain a Nexus-managed block inside the root `CLAUDE.md` between
/// `<!-- BEGIN:nexus-managed -->`/`<!-- END:nexus-managed -->` markers
/// (NEXUS-APP ADR-0117 "CCX", dispatch bb782869). Everything outside the
/// markers is user-owned and never rewritten.
///
/// Runs on every init/pull (unlike [`write_claude_root_md`], which is
/// create-once for the file as a whole). If the file already has markers,
/// only the content between them is replaced. If the file exists but has
/// no markers yet, the block is inserted at the top once, and the file's
/// existing content is preserved below it untouched -- this coexists with
/// other tools' own managed blocks (e.g. a Next.js `nextjs-agent-rules`
/// block) further down the file. If the file doesn't exist at all, it is
/// created containing only the managed block (the create-once bootstrap
/// template itself is [`write_claude_root_md`]'s job, called separately).
///
/// `locked_sha256` is the block hash recorded in the CCX lock (dispatch
/// 99f335e8): if the text between the markers no longer matches it (edited
/// locally) and a different block arrives, the block is kept and
/// [`ClaudeMdOutcome::Conflict`] returned, unless `force` is set.
pub fn merge_claude_md_managed_block(
    target: &Path,
    managed_block: Option<&str>,
    locked_sha256: Option<&str>,
    force: bool,
) -> anyhow::Result<ClaudeMdOutcome> {
    let Some(block) = managed_block else {
        return Ok(ClaudeMdOutcome::Skipped);
    };
    let path = target.join("CLAUDE.md");
    let existing = if path.exists() {
        fs::read_to_string(&path)?
    } else {
        String::new()
    };
    let desired = claude_md_desired_block_text(block);
    let desired_sha = nexus_core::hash::sha256_hex(&desired);

    let new_content = match (
        existing.find(CLAUDE_MD_MANAGED_BEGIN),
        existing.find(CLAUDE_MD_MANAGED_END),
    ) {
        (Some(start), Some(end_marker_start)) if end_marker_start > start => {
            let current = &existing[start + CLAUDE_MD_MANAGED_BEGIN.len()..end_marker_start];
            if current == desired {
                return Ok(ClaudeMdOutcome::Unchanged(desired_sha));
            }
            let edited_locally =
                locked_sha256.is_some_and(|k| nexus_core::hash::sha256_hex(current) != k);
            if edited_locally && !force {
                return Ok(ClaudeMdOutcome::Conflict);
            }
            let end = end_marker_start + CLAUDE_MD_MANAGED_END.len();
            format!(
                "{}{}{}{}{}",
                &existing[..start],
                CLAUDE_MD_MANAGED_BEGIN,
                desired,
                CLAUDE_MD_MANAGED_END,
                &existing[end..]
            )
        }
        (Some(_), Some(_)) => {
            // Malformed markers (END before BEGIN): don't attempt to
            // merge into a file we can't safely parse -- leave it alone.
            return Ok(ClaudeMdOutcome::Skipped);
        }
        _ => {
            // No markers yet: insert the block at the top, once,
            // preserving any pre-existing content (including other
            // tools' own managed blocks) below it untouched.
            format!(
                "{}{}{}\n\n{}",
                CLAUDE_MD_MANAGED_BEGIN, desired, CLAUDE_MD_MANAGED_END, existing
            )
        }
    };

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, new_content)?;
    Ok(ClaudeMdOutcome::Written(desired_sha))
}

/// Read-only, side-effect-free Nexus MCP tools pre-approved by default so a
/// fresh Claude Code session doesn't hit an approval prompt for the calls
/// every session-bootstrap skill makes in its first few turns (`/nexus-init`,
/// `/nexus-dispatch-sweep`). Deliberately excludes anything that creates,
/// mutates, or deletes platform state (`session_append`/`session_create`
/// included on the same "expected every session, low risk" basis as the
/// read-only calls, per the allowlist Claude Code itself had already
/// accumulated live in a real project's `settings.local.json` before this
/// change existed) — decision-bearing actions (`adr_create`, `adr_decide`,
/// `task_create`, `dispatch_create`, `dispatch_resolve`, `sk_update`,
/// `doc_ingest`, `doc_delete`, etc.) stay behind an explicit per-session
/// approval on purpose.
pub(crate) const BASELINE_MCP_PERMISSIONS: &[&str] = &[
    "mcp__nexus__session_list",
    "mcp__nexus__session_create",
    "mcp__nexus__session_append",
    "mcp__nexus__kb_memory",
    "mcp__nexus__kb_search",
    "mcp__nexus__kb_get",
    "mcp__nexus__dispatch_sweep",
    "mcp__nexus__dispatch_inbox",
    "mcp__nexus__dispatch_get",
    "mcp__nexus__task_list",
    "mcp__nexus__sk_list",
    "mcp__nexus__sk_get",
    "mcp__nexus__pd_list",
    "mcp__nexus__project_list",
];

/// Merge the baseline read-only MCP permission allowlist into
/// `.claude/settings.json`'s `permissions.allow` array (NEXUS-APP dispatch
/// TBD, follow-up to run-1 claude-cli diagnostic pass).
///
/// Safe to run against an already-existing, operator-customized
/// `settings.json`, exactly like [`merge_claude_hooks`] and
/// [`merge_claude_co_authored_by`]: only appends entries from
/// [`BASELINE_MCP_PERMISSIONS`] that aren't already present (by exact
/// string match) in `permissions.allow`. Never removes or reorders any
/// existing entry, so an operator who already broadened or narrowed their
/// own allowlist keeps full control — this only lowers the floor, it never
/// raises it above whatever the operator has already granted.
///
/// Returns the number of new entries appended (0 if nothing changed; the
/// file is only rewritten when this is non-zero).
pub fn merge_claude_baseline_permissions(target: &Path) -> anyhow::Result<usize> {
    let settings_path = target.join(".claude").join("settings.json");
    let mut settings: serde_json::Value = if settings_path.exists() {
        let raw = fs::read_to_string(&settings_path)?;
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    if !settings.is_object() {
        settings = serde_json::json!({});
    }
    let settings_obj = settings.as_object_mut().expect("just ensured object");

    let permissions = settings_obj
        .entry("permissions")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !permissions.is_object() {
        *permissions = serde_json::Value::Object(serde_json::Map::new());
    }
    let permissions_obj = permissions.as_object_mut().expect("just ensured object");

    let allow = permissions_obj
        .entry("allow")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    if !allow.is_array() {
        *allow = serde_json::Value::Array(Vec::new());
    }
    let allow_arr = allow.as_array_mut().expect("just ensured array");

    let mut appended = 0;
    for entry in BASELINE_MCP_PERMISSIONS {
        let already_present = allow_arr.iter().any(|v| v.as_str() == Some(*entry));
        if already_present {
            continue;
        }
        allow_arr.push(serde_json::Value::String((*entry).to_string()));
        appended += 1;
    }

    if appended > 0 {
        if let Some(parent) = settings_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(&settings)? + "\n";
        fs::write(&settings_path, content)?;
    }

    Ok(appended)
}

/// Write `.claude/settings.json` if it does not already exist (user-managed
/// once created, never overwritten). Contains Claude runtime behavior only —
/// no secrets, no OpenCode-specific statements (ADR-C04 "CLAUDE.md design").
/// When `routing_catalog_path` / `agent_routing_path` are provided, an `env`
/// block is included pointing the `routing-guard` plugin adapter at the
/// generated catalog/routing files (NEXUS-APP dispatch 7a2d2adb). Returns
/// `true` if the file was created.
pub fn write_claude_settings(
    target: &Path,
    routing_catalog_path: Option<&str>,
    agent_routing_path: Option<&str>,
) -> anyhow::Result<bool> {
    let path = target.join(".claude").join("settings.json");
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut settings = serde_json::Map::new();
    settings.insert(
        "$schema".to_string(),
        serde_json::Value::String(
            "https://json.schemastore.org/claude-code-settings.json".to_string(),
        ),
    );
    settings.insert(
        "permissions".to_string(),
        serde_json::Value::Object(serde_json::Map::new()),
    );

    let mut env = serde_json::Map::new();
    if let Some(p) = routing_catalog_path {
        env.insert(
            "NEXUS_ROUTING_GUARD_CATALOG_PATH".to_string(),
            serde_json::Value::String(p.to_string()),
        );
    }
    if let Some(p) = agent_routing_path {
        env.insert(
            "NEXUS_ROUTING_GUARD_AGENTS_PATH".to_string(),
            serde_json::Value::String(p.to_string()),
        );
    }
    if !env.is_empty() {
        settings.insert("env".to_string(), serde_json::Value::Object(env));
    }

    let content = serde_json::to_string_pretty(&serde_json::Value::Object(settings))? + "\n";
    fs::write(&path, content)?;
    Ok(true)
}

/// `env` keys pointing the routing-guard adapter at its generated inputs
/// (see [`write_claude_settings`]).
pub(crate) const ROUTING_ENV_KEYS: [&str; 2] = [
    "NEXUS_ROUTING_GUARD_CATALOG_PATH",
    "NEXUS_ROUTING_GUARD_AGENTS_PATH",
];

/// Add the routing-guard `env` keys to an existing `.claude/settings.json`
/// when they are missing (e.g. after switching a project back to Claude
/// Code removed them). Existing values are never changed. Returns `true` if
/// the file was rewritten.
pub fn merge_claude_routing_env(
    target: &Path,
    routing_catalog_path: Option<&str>,
    agent_routing_path: Option<&str>,
) -> anyhow::Result<bool> {
    let path = target.join(".claude").join("settings.json");
    let Ok(raw) = fs::read_to_string(&path) else {
        return Ok(false);
    };
    let Ok(mut settings) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Ok(false);
    };
    let Some(obj) = settings.as_object_mut() else {
        return Ok(false);
    };
    let mut changed = false;
    for (key, value) in ROUTING_ENV_KEYS
        .iter()
        .zip([routing_catalog_path, agent_routing_path])
    {
        let Some(value) = value else { continue };
        let env = obj
            .entry("env")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        let Some(env) = env.as_object_mut() else {
            continue;
        };
        if !env.contains_key(*key) {
            env.insert(
                (*key).to_string(),
                serde_json::Value::String(value.to_string()),
            );
            changed = true;
        }
    }
    if changed {
        fs::write(&path, serde_json::to_string_pretty(&settings)? + "\n")?;
    }
    Ok(changed)
}

/// Write the root `CLAUDE.md` as a thin wrapper importing Nexus-owned
/// instructions, if it does not already exist (user-managed, never
/// overwritten). Per ADR-C04, runtime-neutral policy stays in
/// `<agentic_root>/AGENTS.md` and `<agentic_root>/directives.md`; this file
/// only bootstraps Claude Code into reading them. Returns `true` if the
/// file was created.
pub fn write_claude_root_md(
    target: &Path,
    project_name: &str,
    agentic_root: &str,
) -> anyhow::Result<bool> {
    let path = target.join("CLAUDE.md");
    if path.exists() {
        return Ok(false);
    }
    fs::write(&path, render_claude_root_md(project_name, agentic_root))?;
    Ok(true)
}

/// The bootstrap template [`write_claude_root_md`] creates.
pub fn render_claude_root_md(project_name: &str, agentic_root: &str) -> String {
    format!(
        r#"---
type: bootstrap
scope: repo
project: {name}
status: active
source: nexus-platform
---

# BOOTSTRAP SEQUENCE

1. Load agent identity and directives from `{agentic_root}/AGENTS.md`
2. Load project directives from `{agentic_root}/directives.md`
3. Connect to the Nexus MCP server (`.mcp.json`)
4. Load the project index from the Nexus platform
5. Continue with the active workstream

---

# PROJECT

This workspace is configured for the **{name}** project via the Nexus
platform. Treat all project memory and coordination artifacts as
architecture-critical.

Instructions in `{agentic_root}/AGENTS.md` and `{agentic_root}/directives.md`
are managed by the Nexus platform and take precedence for agent identity
and project policy. Skills are available under `.claude/skills/`; MCP
servers are configured in `.mcp.json`.
"#,
        name = project_name,
        agentic_root = agentic_root,
    )
}

/// Render the full Claude Code projection for a project: root `CLAUDE.md`,
/// `.claude/settings.json`, `.claude/skills/`, and `.claude/agents/`.
///
/// Additive only — never touches the OpenCode projection or the
/// `agentic_root`-relative canonical paths. `.mcp.json` is written
/// separately by the shared MCP writer (root path, per ADR-C04).
///
/// `.claude/hooks/` is intentionally omitted in this pass (Track B1):
/// hook *behavior* is Track B2 (Nexus plugin Claude adapter, ADR-C05) and
/// Claude Code does not require the directory to exist for a hookless
/// project to function.
///
/// `force` lets a locally edited `CLAUDE.md` managed block be replaced
/// (CCX conflict rule, dispatch 99f335e8).
#[allow(clippy::too_many_arguments)]
pub fn render_claude_projection(
    target: &Path,
    project_name: &str,
    agentic_root: &str,
    skills: &[ExportedSkill],
    actors: &[ExportedActorFile],
    agent_files: &[ExportedAgentFile],
    runtime_spec: Option<&serde_json::Value>,
    hook_adapters: &[ClaudeHookAdapter],
    include_co_authored_by: Option<bool>,
    claude_settings: Option<&ClaudeSettingsSpec>,
    claude_md_managed_block: Option<&str>,
    force: bool,
) -> anyhow::Result<ClaudeProjectionReport> {
    let mut skills_written = 0;
    for skill in skills {
        if write_claude_skill(target, skill)? {
            skills_written += 1;
        }
    }
    if skills_written > 0 {
        println!(
            "   {} .claude/skills/ ({} skill(s))",
            style("+").bold().green(),
            skills_written
        );
    }

    let agents_written = write_claude_agents(target, actors, agent_files)?;
    if agents_written > 0 {
        println!(
            "   {} .claude/agents/ ({} agent(s))",
            style("+").bold().green(),
            agents_written
        );
    }

    // Record what was written in the pull manifest, so a later switch to
    // OpenCode can tell these files (unmodified) apart from local edits
    // and the operator's own skills/agents (v0.29.0 projection cleanup).
    let mut rendered: Vec<(String, String)> =
        skills.iter().flat_map(render_claude_skill_files).collect();
    rendered.extend(claude_agent_files(actors, agent_files));
    super::pull::record_generated_many(target, agentic_root, &rendered)?;

    // Routing-guard adapter inputs (NEXUS-APP dispatch 7a2d2adb, ADR-C05
    // Track B2): only written when the backend supplies runtime_spec.
    let routing_catalog_path = write_routing_catalog(target, agentic_root, runtime_spec)?;
    if let Some(ref p) = routing_catalog_path {
        println!("   {} {}", style("+").bold().green(), p);
    }
    let agent_routing_path = write_agent_routing(target, agentic_root, runtime_spec)?;
    if let Some(ref p) = agent_routing_path {
        println!("   {} {}", style("+").bold().green(), p);
    }

    if write_claude_settings(
        target,
        routing_catalog_path.as_deref(),
        agent_routing_path.as_deref(),
    )? {
        println!("   {} .claude/settings.json", style("+").bold().green());
    } else if merge_claude_routing_env(
        target,
        routing_catalog_path.as_deref(),
        agent_routing_path.as_deref(),
    )? {
        println!(
            "   {} .claude/settings.json (routing-guard env)",
            style("+").bold().green()
        );
    }

    if write_claude_root_md(target, project_name, agentic_root)? {
        println!("   {} CLAUDE.md", style("+").bold().green());
    }

    // Claude Code hook adapter scripts (NEXUS-APP dispatch 2d5017f7, Track
    // B3): only present when the backend supplies claude_hook_adapters.
    let scripts_written = write_claude_hook_adapters(target, hook_adapters)?;
    if scripts_written > 0 {
        println!(
            "   {} .claude/hooks/ ({} adapter script(s))",
            style("+").bold().green(),
            scripts_written
        );
    }
    let previous_hooks = ccx::load_lock(target, agentic_root)
        .map(|l| l.hooks)
        .unwrap_or_default();
    let (hooks_appended, removed_hook_plugins) =
        merge_claude_hooks(target, hook_adapters, Some(&previous_hooks))?;
    if hooks_appended > 0 {
        println!(
            "   {} .claude/settings.json (+{} hook registration(s))",
            style("+").bold().green(),
            hooks_appended
        );
    }
    // Hook adapter removal (NEXUS-APP dispatch 99f335e8 follow-up): an
    // adapter previously managed by Nexus that is no longer sent, e.g.
    // because the nexus-core Claude plugin now provides the same hooks.
    for plugin_name in &removed_hook_plugins {
        println!(
            "   {} .claude/settings.json / .claude/hooks/ ({} removed -- no longer sent)",
            style("-").bold().yellow(),
            plugin_name
        );
    }
    let new_hooks: std::collections::BTreeMap<String, ccx::CcxLockHookEntry> = hook_adapters
        .iter()
        .map(|a| {
            (
                a.target_path.clone(),
                ccx::CcxLockHookEntry {
                    plugin_name: a.plugin_name.clone(),
                    target_path: a.target_path.clone(),
                    file_sha256: nexus_core::hash::sha256_hex(&a.body),
                    registrations: a
                        .hook_events
                        .iter()
                        .map(|e| ccx::CcxLockHookRegistration {
                            event: e.event.clone(),
                            matcher: e.matcher.clone(),
                            command: format!(
                                "node \"${{CLAUDE_PROJECT_DIR}}/{}\" {}",
                                a.target_path,
                                kebab_case_event(&e.event)
                            ),
                        })
                        .collect(),
                },
            )
        })
        .collect();
    ccx::record_hooks_in_lock(target, agentic_root, new_hooks)?;

    // Co-authored-by trailer suppression (NEXUS-APP dispatch 84e38bd7):
    // hard operator requirement, absent-means-suppress, merges alongside
    // the hooks block above rather than replacing the file.
    if merge_claude_co_authored_by(target, include_co_authored_by)? {
        println!(
            "   {} .claude/settings.json (includeCoAuthoredBy: {})",
            style("+").bold().green(),
            include_co_authored_by.unwrap_or(false)
        );
    }

    // Baseline read-only MCP permission allowlist (follow-up to the run-1
    // claude-cli diagnostic pass): reduces first-session approval-prompt
    // friction for the calls every session-bootstrap skill makes. Merges
    // alongside the hooks/includeCoAuthoredBy blocks, never replaces the
    // file, never touches an operator's own additions.
    let permissions_appended = merge_claude_baseline_permissions(target)?;
    if permissions_appended > 0 {
        println!(
            "   {} .claude/settings.json (+{} baseline permission(s))",
            style("+").bold().green(),
            permissions_appended
        );
    }

    // Generic Nexus-managed settings.json keys (NEXUS-APP ADR-0117 "CCX"):
    // statusline, attribution, permissions.deny, plugin enablement, known
    // marketplaces. Merges alongside everything above; only present keys
    // change, nothing else in the file is touched. The CCX lock's
    // previously-recorded settings state (if any) drives removal of a key
    // Nexus no longer manages (dispatch 99f335e8 follow-up); the lock is
    // updated afterward to reflect what is now managed.
    let previous_settings = super::ccx::load_lock(target, agentic_root).and_then(|l| l.settings);
    let settings_before = read_claude_settings(target);
    let generic_settings_changed =
        merge_claude_generic_settings(target, claude_settings, previous_settings.as_ref())?;
    let settings_changes = super::ccx::describe_settings_changes(
        &settings_before,
        &read_claude_settings(target),
        claude_settings,
        previous_settings.as_ref(),
    );
    if generic_settings_changed > 0 {
        println!(
            "   {} .claude/settings.json (+{} managed key(s))",
            style("+").bold().green(),
            generic_settings_changed
        );
    }
    super::ccx::record_settings_in_lock(target, agentic_root, claude_settings)?;

    // Root CLAUDE.md managed block (NEXUS-APP ADR-0117 "CCX"): everything
    // outside the markers is user-owned and never rewritten. A block edited
    // locally since the last write is kept unless `force` (dispatch
    // 99f335e8); its lock hash then stays as is, so it keeps reporting.
    let locked_block_sha =
        super::ccx::load_lock(target, agentic_root).and_then(|l| l.claude_md_block_sha256);
    let claude_md = merge_claude_md_managed_block(
        target,
        claude_md_managed_block,
        locked_block_sha.as_deref(),
        force,
    )?;
    match &claude_md {
        ClaudeMdOutcome::Written(sha) => {
            println!(
                "   {} CLAUDE.md (nexus-managed block)",
                style("+").bold().green()
            );
            super::ccx::record_claude_md_block_in_lock(target, agentic_root, sha)?;
        }
        ClaudeMdOutcome::Unchanged(sha) => {
            super::ccx::record_claude_md_block_in_lock(target, agentic_root, sha)?;
        }
        ClaudeMdOutcome::Conflict => {
            println!(
                "   {} CLAUDE.md nexus-managed block modified locally, kept (run nexus claude diff; nexus pull --force replaces it)",
                style("!").bold().yellow()
            );
        }
        ClaudeMdOutcome::Skipped => {}
    }

    Ok(ClaudeProjectionReport {
        removed_hook_plugins,
        settings_changes,
        claude_md,
    })
}

/// What [`render_claude_projection`] changed that the CCX pull summary
/// reports on.
#[derive(Debug)]
pub struct ClaudeProjectionReport {
    /// Hook adapters removed because they are no longer sent.
    pub removed_hook_plugins: Vec<String>,
    /// One entry per (current or previously) managed settings key, see
    /// [`super::ccx::describe_settings_changes`].
    pub settings_changes: Vec<String>,
    pub claude_md: ClaudeMdOutcome,
}

/// Current `.claude/settings.json` as JSON (`{}` if absent or invalid).
pub fn read_claude_settings(target: &Path) -> serde_json::Value {
    fs::read_to_string(target.join(".claude").join("settings.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::api::{ActorAvatar, SkillResource};

    fn temp_dir(suffix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nexus-claude-render-test-{}-{}",
            std::process::id(),
            suffix
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_skill(skill_id: &str) -> ExportedSkill {
        ExportedSkill {
            skill_id: skill_id.to_string(),
            name: "Test Skill".to_string(),
            description: Some("desc".to_string()),
            version: 1,
            body: Some("# Instructions\n\nDo the thing.".to_string()),
            command_slug: Some(skill_id.to_string()),
            pinned: false,
            resources: vec![],
        }
    }

    // ── strip_frontmatter / yaml_escape (NEXUS-APP dispatch 5ddd6355) ──────

    #[test]
    fn test_strip_frontmatter_removes_leading_block() {
        let body = "---\nskill_id: nx-init\nname: Init\n---\n\n# Instructions\n\nDo it.";
        assert_eq!(strip_frontmatter(body), "# Instructions\n\nDo it.");
    }

    #[test]
    fn test_strip_frontmatter_no_block_returns_unchanged() {
        let body = "# Instructions\n\nDo it.\n\n---\n\nA horizontal rule, not frontmatter.";
        assert_eq!(strip_frontmatter(body), body);
    }

    #[test]
    fn test_strip_frontmatter_empty_body() {
        assert_eq!(strip_frontmatter(""), "");
    }

    #[test]
    fn test_strip_frontmatter_only_frontmatter_no_content() {
        let body = "---\nskill_id: x\n---\n";
        assert_eq!(strip_frontmatter(body), "");
    }

    #[test]
    fn test_strip_frontmatter_does_not_match_longer_dash_runs() {
        // "----" is not a valid frontmatter delimiter; must not be treated
        // as one and must not corrupt the body.
        let body = "----\nnot frontmatter\n----\n";
        assert_eq!(strip_frontmatter(body), body);
    }

    #[test]
    fn test_yaml_escape_quotes_and_escapes() {
        assert_eq!(yaml_escape("simple"), "\"simple\"");
        assert_eq!(yaml_escape("has: a colon"), "\"has: a colon\"");
        assert_eq!(yaml_escape("has \"quotes\""), "\"has \\\"quotes\\\"\"");
        assert_eq!(yaml_escape(""), "\"\"");
    }

    #[test]
    fn test_canonical_claude_skill_id_migrates_nx_prefix() {
        assert_eq!(canonical_claude_skill_id("nx-init"), "nexus-init");
        assert_eq!(canonical_claude_skill_id("nx-sec-scan"), "nexus-sec-scan");
    }

    #[test]
    fn test_canonical_claude_skill_id_passes_through_nexus_prefix() {
        assert_eq!(canonical_claude_skill_id("nexus-init"), "nexus-init");
    }

    #[test]
    fn test_canonical_claude_skill_id_passes_through_other_ids() {
        assert_eq!(canonical_claude_skill_id("custom-skill"), "custom-skill");
    }

    #[test]
    fn test_write_claude_skill_migrates_nx_prefix_to_nexus_dir() {
        let dir = temp_dir("skill-nx-prefix");
        let skill = sample_skill("nx-sec-scan");

        write_claude_skill(&dir, &skill).unwrap();

        // Command-relevant directory name must be the migrated nexus-* id.
        assert!(dir.join(".claude/skills/nexus-sec-scan/SKILL.md").exists());
        assert!(!dir.join(".claude/skills/nx-sec-scan").exists());

        let content =
            fs::read_to_string(dir.join(".claude/skills/nexus-sec-scan/SKILL.md")).unwrap();
        assert!(content.contains("skill_id: nexus-sec-scan"));
        assert!(content.contains("Do the thing."));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_claude_skill_includes_description_and_command_slug() {
        // NEXUS-APP dispatch 5ddd6355: these were silently dropped in
        // favor of a hardcoded local template.
        let dir = temp_dir("skill-desc-slug");
        let skill = sample_skill("nexus-init");

        write_claude_skill(&dir, &skill).unwrap();

        let content = fs::read_to_string(dir.join(".claude/skills/nexus-init/SKILL.md")).unwrap();
        assert!(content.contains(r#"description: "desc""#), "got: {content}");
        assert!(
            content.contains("command_slug: nexus-init"),
            "got: {content}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_claude_skill_absent_description_and_command_slug() {
        let dir = temp_dir("skill-no-desc-slug");
        let mut skill = sample_skill("nexus-bare");
        skill.description = None;
        skill.command_slug = None;

        write_claude_skill(&dir, &skill).unwrap();

        let content = fs::read_to_string(dir.join(".claude/skills/nexus-bare/SKILL.md")).unwrap();
        assert!(content.contains(r#"description: """#), "got: {content}");
        assert!(content.contains("command_slug: none"), "got: {content}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_claude_skill_strips_duplicate_backend_frontmatter() {
        // Reproduces the exact defect confirmed live in this repo
        // (.nexus/skills/nx-init/SKILL.md carried two stacked frontmatter
        // blocks): the backend's ExportedSkill.body already embeds its own
        // frontmatter, which must not be duplicated underneath the local
        // template's own block.
        let dir = temp_dir("skill-dup-frontmatter");
        let mut skill = sample_skill("nexus-dup");
        skill.body = Some(
            "---\nskill_id: nexus-dup\nname: Test Skill\nversion: 1\n\
             command_slug: nexus-dup\nsource: nexus-platform\n---\n\n\
             # Instructions\n\nDo the thing."
                .to_string(),
        );

        write_claude_skill(&dir, &skill).unwrap();

        let content = fs::read_to_string(dir.join(".claude/skills/nexus-dup/SKILL.md")).unwrap();
        assert_eq!(
            content.matches("---").count(),
            2,
            "expected exactly one frontmatter block (opening + closing '---'), got: {content}"
        );
        assert!(content.contains("Do the thing."));
        assert!(!content.contains("source: nexus-platform\nsource: nexus-platform"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_claude_skill_writes_resources() {
        let dir = temp_dir("skill-resources");
        let mut skill = sample_skill("nexus-with-resources");
        skill.resources.push(SkillResource {
            filename: "helper.py".to_string(),
            body: "print('hi')".to_string(),
        });

        write_claude_skill(&dir, &skill).unwrap();

        let resource_path = dir.join(".claude/skills/nexus-with-resources/helper.py");
        assert!(resource_path.exists());
        assert_eq!(fs::read_to_string(resource_path).unwrap(), "print('hi')");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_claude_agents_writes_one_file_per_actor() {
        let dir = temp_dir("agents");
        let actors = vec![
            ExportedActorFile {
                slug: "planner".to_string(),
                name: "Planner".to_string(),
                role: "primary".to_string(),
                body: "# Planner\n\nPlan things.".to_string(),
                avatar: None::<ActorAvatar>,
                route_alias: None,
            },
            ExportedActorFile {
                slug: "reviewer".to_string(),
                name: "Reviewer".to_string(),
                role: "secondary".to_string(),
                body: "# Reviewer\n\nReview things.".to_string(),
                avatar: None,
                route_alias: None,
            },
        ];

        let written = write_claude_agents(&dir, &actors, &[]).unwrap();
        assert_eq!(written, 2);
        assert!(dir.join(".claude/agents/planner.md").exists());
        assert!(dir.join(".claude/agents/reviewer.md").exists());
        assert_eq!(
            fs::read_to_string(dir.join(".claude/agents/planner.md")).unwrap(),
            "# Planner\n\nPlan things."
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_claude_agents_noop_when_empty() {
        let dir = temp_dir("agents-empty");
        let written = write_claude_agents(&dir, &[], &[]).unwrap();
        assert_eq!(written, 0);
        assert!(!dir.join(".claude/agents").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_actor_slug_from_agent_file_matches_actors_dir() {
        assert_eq!(
            actor_slug_from_agent_file(".nexus/actors/technical-project-manager.md"),
            Some("technical-project-manager".to_string())
        );
        assert_eq!(
            actor_slug_from_agent_file(".claude/actors/planner.md"),
            Some("planner".to_string())
        );
    }

    #[test]
    fn test_actor_slug_from_agent_file_ignores_non_actor_paths() {
        assert_eq!(actor_slug_from_agent_file(".nexus/AGENTS.md"), None);
        assert_eq!(actor_slug_from_agent_file("CLAUDE.md"), None);
        assert_eq!(
            actor_slug_from_agent_file(".nexus/primary-agents/lead.md"),
            None
        );
        assert_eq!(actor_slug_from_agent_file(".nexus/actors/notes.txt"), None);
    }

    /// Regression test for NEXUS-APP dispatch c7701485 follow-up:
    /// actor-based + claude-cli projects delivered actor profiles
    /// exclusively through `agent_files` (generic export list), with an
    /// empty dedicated `actors` field. `.claude/agents/` must still be
    /// populated from that source.
    #[test]
    fn test_write_claude_agents_falls_back_to_agent_files_when_actors_field_empty() {
        let dir = temp_dir("agents-from-agent-files");
        let agent_files = vec![
            ExportedAgentFile {
                file_key: "af1".to_string(),
                target_path: ".nexus/actors/technical-project-manager.md".to_string(),
                name: "Technical Project Manager".to_string(),
                description: None,
                category: "agent".to_string(),
                version: 1,
                body: "# Technical Project Manager".to_string(),
                content_hash: None,
                agent_file_id: None,
            },
            ExportedAgentFile {
                file_key: "af2".to_string(),
                target_path: ".nexus/AGENTS.md".to_string(),
                name: "AGENTS".to_string(),
                description: None,
                category: "agent".to_string(),
                version: 1,
                body: "# Not an actor".to_string(),
                content_hash: None,
                agent_file_id: None,
            },
        ];

        // Dedicated `actors` field is empty, as observed in the field report.
        let written = write_claude_agents(&dir, &[], &agent_files).unwrap();
        assert_eq!(written, 1);
        assert!(dir
            .join(".claude/agents/technical-project-manager.md")
            .exists());
        // AGENTS.md itself must not be mistaken for an actor file.
        assert!(!dir.join(".claude/agents/AGENTS.md").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_claude_agents_merges_actors_field_and_agent_files_without_duplicates() {
        let dir = temp_dir("agents-merge");
        let actors = vec![ExportedActorFile {
            slug: "planner".to_string(),
            name: "Planner".to_string(),
            role: "primary".to_string(),
            body: "# Planner (from actors field)".to_string(),
            avatar: None,
            route_alias: None,
        }];
        let agent_files = vec![ExportedAgentFile {
            file_key: "af1".to_string(),
            target_path: ".nexus/actors/reviewer.md".to_string(),
            name: "Reviewer".to_string(),
            description: None,
            category: "agent".to_string(),
            version: 1,
            body: "# Reviewer (from agent_files)".to_string(),
            content_hash: None,
            agent_file_id: None,
        }];

        let written = write_claude_agents(&dir, &actors, &agent_files).unwrap();
        assert_eq!(written, 2);
        assert!(dir.join(".claude/agents/planner.md").exists());
        assert!(dir.join(".claude/agents/reviewer.md").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_claude_settings_creates_once() {
        let dir = temp_dir("settings");

        let created = write_claude_settings(&dir, None, None).unwrap();
        assert!(created);
        assert!(dir.join(".claude/settings.json").exists());

        // Simulate user edit, then re-run: must not overwrite.
        fs::write(dir.join(".claude/settings.json"), "user-edited").unwrap();
        let created_again = write_claude_settings(&dir, None, None).unwrap();
        assert!(!created_again);
        assert_eq!(
            fs::read_to_string(dir.join(".claude/settings.json")).unwrap(),
            "user-edited"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_claude_settings_contains_no_secrets() {
        let dir = temp_dir("settings-no-secrets");
        write_claude_settings(&dir, None, None).unwrap();
        let content = fs::read_to_string(dir.join(".claude/settings.json")).unwrap();
        assert!(!content.contains("NEXUS_PRIVATE_TOKEN"));
        assert!(!content.contains("nxs_pat_"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_claude_settings_includes_routing_guard_env_when_provided() {
        let dir = temp_dir("settings-routing-env");
        write_claude_settings(
            &dir,
            Some(".nexus/generated/routing-catalog.json"),
            Some(".nexus/generated/agent-routing.json"),
        )
        .unwrap();
        let content = fs::read_to_string(dir.join(".claude/settings.json")).unwrap();
        assert!(content.contains("NEXUS_ROUTING_GUARD_CATALOG_PATH"));
        assert!(content.contains(".nexus/generated/routing-catalog.json"));
        assert!(content.contains("NEXUS_ROUTING_GUARD_AGENTS_PATH"));
        assert!(content.contains(".nexus/generated/agent-routing.json"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_claude_settings_omits_env_block_when_no_routing_paths() {
        let dir = temp_dir("settings-no-env");
        write_claude_settings(&dir, None, None).unwrap();
        let content = fs::read_to_string(dir.join(".claude/settings.json")).unwrap();
        assert!(!content.contains("\"env\""));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_routing_catalog_writes_model_routes() {
        let dir = temp_dir("routing-catalog");
        let runtime_spec = serde_json::json!({
            "model_routes": [
                {"route_alias": "fast", "provider": "openrouter", "model": "gpt-4o-mini", "lifecycle_status": "active"}
            ]
        });

        let rel_path = write_routing_catalog(&dir, ".nexus", Some(&runtime_spec)).unwrap();
        assert_eq!(
            rel_path,
            Some(".nexus/generated/routing-catalog.json".to_string())
        );
        let content =
            fs::read_to_string(dir.join(".nexus/generated/routing-catalog.json")).unwrap();
        assert!(content.contains("fast"));
        assert!(content.contains("openrouter"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_routing_catalog_noop_without_runtime_spec() {
        let dir = temp_dir("routing-catalog-none");
        let rel_path = write_routing_catalog(&dir, ".nexus", None).unwrap();
        assert_eq!(rel_path, None);
        assert!(!dir.join(".nexus/generated/routing-catalog.json").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_routing_catalog_noop_when_model_routes_absent() {
        let dir = temp_dir("routing-catalog-absent");
        let runtime_spec = serde_json::json!({ "schema_version": 1 });
        let rel_path = write_routing_catalog(&dir, ".nexus", Some(&runtime_spec)).unwrap();
        assert_eq!(rel_path, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_agent_routing_writes_actors_and_primary_agents() {
        let dir = temp_dir("agent-routing");
        let runtime_spec = serde_json::json!({
            "actors": [{"slug": "planner", "title": "Planner"}],
            "primary_agents": [{"file_key": "af1", "name": "Lead"}]
        });

        let rel_path = write_agent_routing(&dir, ".nexus", Some(&runtime_spec)).unwrap();
        assert_eq!(
            rel_path,
            Some(".nexus/generated/agent-routing.json".to_string())
        );
        let content = fs::read_to_string(dir.join(".nexus/generated/agent-routing.json")).unwrap();
        assert!(content.contains("planner"));
        assert!(content.contains("Lead"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_agent_routing_noop_when_neither_field_present() {
        let dir = temp_dir("agent-routing-absent");
        let runtime_spec = serde_json::json!({ "schema_version": 1 });
        let rel_path = write_agent_routing(&dir, ".nexus", Some(&runtime_spec)).unwrap();
        assert_eq!(rel_path, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_claude_root_md_creates_once() {
        let dir = temp_dir("root-md");

        let created = write_claude_root_md(&dir, "Test Project", ".nexus").unwrap();
        assert!(created);
        let content = fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        assert!(content.contains("Test Project"));
        assert!(content.contains(".nexus/AGENTS.md"));
        assert!(content.contains(".mcp.json"));

        // Must not overwrite an existing (user-managed) CLAUDE.md.
        fs::write(dir.join("CLAUDE.md"), "user-edited").unwrap();
        let created_again = write_claude_root_md(&dir, "Test Project", ".nexus").unwrap();
        assert!(!created_again);
        assert_eq!(
            fs::read_to_string(dir.join("CLAUDE.md")).unwrap(),
            "user-edited"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_render_claude_projection_writes_all_artifacts() {
        let dir = temp_dir("full-render");
        let skills = vec![
            sample_skill("nx-sec-scan"),
            sample_skill("nexus-code-review"),
        ];
        let actors = vec![ExportedActorFile {
            slug: "planner".to_string(),
            name: "Planner".to_string(),
            role: "primary".to_string(),
            body: "# Planner".to_string(),
            avatar: None,
            route_alias: None,
        }];

        render_claude_projection(
            &dir,
            "Test Project",
            ".nexus",
            &skills,
            &actors,
            &[],
            None,
            &[],
            None,
            None,
            None,
            false,
        )
        .unwrap();

        assert!(dir.join("CLAUDE.md").exists());
        assert!(dir.join(".claude/settings.json").exists());
        assert!(dir.join(".claude/skills/nexus-sec-scan/SKILL.md").exists());
        assert!(dir
            .join(".claude/skills/nexus-code-review/SKILL.md")
            .exists());
        assert!(dir.join(".claude/agents/planner.md").exists());
        // Hooks scaffolding is intentionally omitted in this pass.
        assert!(!dir.join(".claude/hooks").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_render_claude_projection_writes_routing_guard_inputs_when_runtime_spec_present() {
        let dir = temp_dir("full-render-runtime-spec");
        let skills = vec![sample_skill("nexus-init")];
        let runtime_spec = serde_json::json!({
            "model_routes": [{"route_alias": "fast", "provider": "openrouter", "model": "gpt-4o-mini"}],
            "actors": [{"slug": "planner", "title": "Planner"}]
        });

        render_claude_projection(
            &dir,
            "Test Project",
            ".nexus",
            &skills,
            &[],
            &[],
            Some(&runtime_spec),
            &[],
            None,
            None,
            None,
            false,
        )
        .unwrap();

        assert!(dir.join(".nexus/generated/routing-catalog.json").exists());
        assert!(dir.join(".nexus/generated/agent-routing.json").exists());
        let settings = fs::read_to_string(dir.join(".claude/settings.json")).unwrap();
        assert!(settings.contains("NEXUS_ROUTING_GUARD_CATALOG_PATH"));
        assert!(settings.contains("NEXUS_ROUTING_GUARD_AGENTS_PATH"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// Parity check (ADR-C04 acceptance criteria: "A compatibility test
    /// validates the same skill set ... in both projections"). The set of
    /// canonical Claude skill directory names must be derivable 1:1 from
    /// the same skill list the OpenCode command writer consumes, with no
    /// skill silently dropped by the nx-* -> nexus-* migration.
    #[test]
    fn test_claude_skill_set_has_same_cardinality_as_input() {
        let skills = [
            sample_skill("nx-init"),
            sample_skill("nx-sec-scan"),
            sample_skill("nexus-code-review"),
        ];
        let canonical_ids: std::collections::HashSet<String> = skills
            .iter()
            .map(|s| canonical_claude_skill_id(&s.skill_id))
            .collect();
        // No collisions introduced by the nx-* -> nexus-* migration.
        assert_eq!(canonical_ids.len(), skills.len());
        assert!(canonical_ids.contains("nexus-init"));
        assert!(canonical_ids.contains("nexus-sec-scan"));
        assert!(canonical_ids.contains("nexus-code-review"));
    }

    // -------------------------------------------------------------------
    // Track B3 (NEXUS-APP dispatch 2d5017f7): hook adapter distribution
    // -------------------------------------------------------------------

    fn sample_adapter(plugin_name: &str, target_path: &str) -> ClaudeHookAdapter {
        ClaudeHookAdapter {
            plugin_name: plugin_name.to_string(),
            target_path: target_path.to_string(),
            body: format!("// {} adapter body", plugin_name),
            hook_events: vec![],
        }
    }

    #[test]
    fn test_kebab_case_event_matches_confirmed_adapter_contract() {
        assert_eq!(kebab_case_event("PostToolUse"), "post-tool-use");
        assert_eq!(kebab_case_event("PreCompact"), "pre-compact");
        assert_eq!(kebab_case_event("PostCompact"), "post-compact");
        assert_eq!(kebab_case_event("SessionStart"), "session-start");
        assert_eq!(kebab_case_event("UserPromptSubmit"), "user-prompt-submit");
        assert_eq!(kebab_case_event("Stop"), "stop");
    }

    #[test]
    fn test_write_claude_hook_adapters_writes_scripts() {
        let dir = temp_dir("hook-adapters-write");
        let mut adapter = sample_adapter("session-guard", ".claude/hooks/nexus-session-guard.mjs");
        adapter.body = "export default function() {}".to_string();

        let written = write_claude_hook_adapters(&dir, &[adapter]).unwrap();
        assert_eq!(written, 1);
        let content =
            fs::read_to_string(dir.join(".claude/hooks/nexus-session-guard.mjs")).unwrap();
        assert_eq!(content, "export default function() {}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_claude_hook_adapters_resyncs_on_rerun() {
        // Platform-managed generated code: always rewritten, unlike
        // CLAUDE.md/settings.json which are create-once-only.
        let dir = temp_dir("hook-adapters-resync");
        let mut adapter = sample_adapter("session-guard", ".claude/hooks/nexus-session-guard.mjs");
        adapter.body = "// v1".to_string();
        write_claude_hook_adapters(&dir, &[adapter.clone()]).unwrap();

        adapter.body = "// v2".to_string();
        write_claude_hook_adapters(&dir, &[adapter]).unwrap();

        let content =
            fs::read_to_string(dir.join(".claude/hooks/nexus-session-guard.mjs")).unwrap();
        assert_eq!(content, "// v2");

        let _ = fs::remove_dir_all(&dir);
    }

    // ── merge_claude_co_authored_by: hard suppression of the
    // Co-Authored-By trailer (NEXUS-APP dispatch 84e38bd7) ─────────────────

    #[test]
    fn test_merge_co_authored_by_absent_means_suppress() {
        // No settings.json yet, no git_config value at all: must still be
        // written as false, not left for Claude Code's own default.
        let dir = temp_dir("coauthor-absent");
        let changed = merge_claude_co_authored_by(&dir, None).unwrap();
        assert!(changed);

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings["includeCoAuthoredBy"], false);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_co_authored_by_explicit_false() {
        let dir = temp_dir("coauthor-false");
        merge_claude_co_authored_by(&dir, Some(false)).unwrap();
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings["includeCoAuthoredBy"], false);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_co_authored_by_explicit_true() {
        let dir = temp_dir("coauthor-true");
        merge_claude_co_authored_by(&dir, Some(true)).unwrap();
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings["includeCoAuthoredBy"], true);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_co_authored_by_updates_existing_value() {
        // Must overwrite a stale/incorrect value on re-pull, not just skip
        // because the key already exists — this is a correctness flag, not
        // a create-once default.
        let dir = temp_dir("coauthor-update");
        merge_claude_co_authored_by(&dir, Some(true)).unwrap();
        let changed = merge_claude_co_authored_by(&dir, Some(false)).unwrap();
        assert!(changed);
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings["includeCoAuthoredBy"], false);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_co_authored_by_is_idempotent() {
        let dir = temp_dir("coauthor-idempotent");
        assert!(merge_claude_co_authored_by(&dir, Some(false)).unwrap());
        // Second call with the same resolved value must be a no-op.
        assert!(!merge_claude_co_authored_by(&dir, Some(false)).unwrap());
        assert!(!merge_claude_co_authored_by(&dir, None).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_co_authored_by_preserves_other_keys_including_hooks() {
        // Must merge alongside the hooks block and any operator
        // customization, never replace the file.
        let dir = temp_dir("coauthor-preserve");
        fs::create_dir_all(dir.join(".claude")).unwrap();
        fs::write(
            dir.join(".claude/settings.json"),
            r#"{
  "permissions": {},
  "hooks": {
    "PostToolUse": [
      { "matcher": "MyCustomTool", "hooks": [{ "type": "command", "command": "echo custom" }] }
    ]
  }
}
"#,
        )
        .unwrap();

        merge_claude_co_authored_by(&dir, Some(false)).unwrap();

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings["includeCoAuthoredBy"], false);
        assert_eq!(
            settings["hooks"]["PostToolUse"][0]["matcher"],
            "MyCustomTool"
        );
        assert!(settings["permissions"].is_object());

        let _ = fs::remove_dir_all(&dir);
    }

    // ── merge_claude_generic_settings (NEXUS-APP ADR-0117 "CCX",
    // dispatch bb782869) ────────────────────────────────────────────────

    fn sample_ccx_spec(keys: &[&str], values: &[(&str, serde_json::Value)]) -> ClaudeSettingsSpec {
        let mut map = serde_json::Map::new();
        for (k, v) in values {
            map.insert((*k).to_string(), v.clone());
        }
        ClaudeSettingsSpec {
            managed_keys: keys.iter().map(|s| s.to_string()).collect(),
            values: map,
        }
    }

    #[test]
    fn test_merge_generic_settings_none_spec_is_noop() {
        let dir = temp_dir("ccx-none");
        let appended = merge_claude_generic_settings(&dir, None, None).unwrap();
        assert_eq!(appended, 0);
        assert!(!dir.join(".claude/settings.json").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_generic_settings_sets_top_level_key() {
        let dir = temp_dir("ccx-top-level");
        let spec = sample_ccx_spec(
            &["statusLine"],
            &[(
                "statusLine",
                serde_json::json!({"type": "command", "command": "node hud.mjs"}),
            )],
        );
        let changed = merge_claude_generic_settings(&dir, Some(&spec), None).unwrap();
        assert_eq!(changed, 1);

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings["statusLine"]["command"], "node hud.mjs");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_generic_settings_sets_nested_dot_path() {
        let dir = temp_dir("ccx-nested");
        let spec = sample_ccx_spec(
            &["permissions.deny"],
            &[(
                "permissions.deny",
                serde_json::json!(["Read(./.env)", "Read(./.env.*)"]),
            )],
        );
        merge_claude_generic_settings(&dir, Some(&spec), None).unwrap();

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        let deny = settings["permissions"]["deny"].as_array().unwrap();
        assert!(deny.iter().any(|v| v == "Read(./.env)"));
        assert!(deny.iter().any(|v| v == "Read(./.env.*)"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_generic_settings_unions_array_with_operator_entries() {
        // An operator's own permissions.deny entries must survive
        // alongside Nexus's, not be replaced by them.
        let dir = temp_dir("ccx-array-union");
        fs::create_dir_all(dir.join(".claude")).unwrap();
        fs::write(
            dir.join(".claude/settings.json"),
            r#"{ "permissions": { "deny": ["Read(./secrets.json)"] } }"#,
        )
        .unwrap();

        let spec = sample_ccx_spec(
            &["permissions.deny"],
            &[("permissions.deny", serde_json::json!(["Read(./.env)"]))],
        );
        merge_claude_generic_settings(&dir, Some(&spec), None).unwrap();

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        let deny = settings["permissions"]["deny"].as_array().unwrap();
        assert_eq!(deny.len(), 2);
        assert!(deny.iter().any(|v| v == "Read(./secrets.json)"));
        assert!(deny.iter().any(|v| v == "Read(./.env)"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_generic_settings_applies_nested_values() {
        // The backend sends `values` nested, not under the dotted key
        // (NEXUS-APP dispatch 95d81511): the deny list must still land.
        let dir = temp_dir("ccx-nested-values");
        let spec = sample_ccx_spec(
            &["permissions.deny", "statusLine"],
            &[
                (
                    "permissions",
                    serde_json::json!({"deny": ["Read(./.env)", "Read(./.env.*)"]}),
                ),
                ("statusLine", serde_json::json!({"type": "command"})),
            ],
        );
        assert_eq!(
            merge_claude_generic_settings(&dir, Some(&spec), None).unwrap(),
            2
        );
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            settings["permissions"]["deny"],
            serde_json::json!(["Read(./.env)", "Read(./.env.*)"])
        );
        assert_eq!(settings["statusLine"]["type"], "command");
        assert_eq!(
            merge_claude_generic_settings(&dir, Some(&spec), Some(&spec)).unwrap(),
            0
        );

        // Dropping the key later removes exactly the entries Nexus added.
        let mut raw = settings.clone();
        raw["permissions"]["deny"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!("Read(./mine)"));
        fs::write(dir.join(".claude/settings.json"), raw.to_string()).unwrap();
        let current = sample_ccx_spec(
            &["statusLine"],
            &[("statusLine", serde_json::json!({"type": "command"}))],
        );
        merge_claude_generic_settings(&dir, Some(&current), Some(&spec)).unwrap();
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            settings["permissions"]["deny"],
            serde_json::json!(["Read(./mine)"])
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_generic_settings_array_union_is_idempotent() {
        let dir = temp_dir("ccx-array-idempotent");
        let spec = sample_ccx_spec(
            &["permissions.deny"],
            &[("permissions.deny", serde_json::json!(["Read(./.env)"]))],
        );
        let first = merge_claude_generic_settings(&dir, Some(&spec), None).unwrap();
        assert_eq!(first, 1);
        let second = merge_claude_generic_settings(&dir, Some(&spec), None).unwrap();
        assert_eq!(second, 0, "re-sending the same array must not duplicate it");

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings["permissions"]["deny"].as_array().unwrap().len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_generic_settings_preserves_untouched_keys() {
        let dir = temp_dir("ccx-preserve-untouched");
        fs::create_dir_all(dir.join(".claude")).unwrap();
        fs::write(
            dir.join(".claude/settings.json"),
            r#"{ "includeCoAuthoredBy": false, "hooks": { "Stop": [] } }"#,
        )
        .unwrap();

        let spec = sample_ccx_spec(
            &["enabledPlugins"],
            &[(
                "enabledPlugins",
                serde_json::json!({"nexus-core@gatewarden-nexus": true}),
            )],
        );
        merge_claude_generic_settings(&dir, Some(&spec), None).unwrap();

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings["includeCoAuthoredBy"], false);
        assert!(settings["hooks"]["Stop"].is_array());
        assert_eq!(
            settings["enabledPlugins"]["nexus-core@gatewarden-nexus"],
            true
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_generic_settings_missing_value_for_key_is_skipped() {
        // managed_keys lists a path with no corresponding entry in
        // `values`: must not panic, must not write a null.
        let dir = temp_dir("ccx-missing-value");
        let spec = ClaudeSettingsSpec {
            managed_keys: vec!["statusLine".to_string()],
            values: serde_json::Map::new(),
        };
        let changed = merge_claude_generic_settings(&dir, Some(&spec), None).unwrap();
        assert_eq!(changed, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_generic_settings_removes_key_dropped_from_previous() {
        // End-to-end through merge_claude_generic_settings itself (not
        // just ccx::reconcile_settings_removed_keys directly): a key that
        // was previously managed and is no longer sent gets removed.
        let dir = temp_dir("ccx-removes-via-previous");
        let previous = sample_ccx_spec(
            &["statusLine"],
            &[(
                "statusLine",
                serde_json::json!({"type": "command", "command": "node hud.mjs"}),
            )],
        );
        // First call establishes the file with the managed key present.
        merge_claude_generic_settings(&dir, Some(&previous), None).unwrap();
        assert!(fs::read_to_string(dir.join(".claude/settings.json"))
            .unwrap()
            .contains("hud.mjs"));

        // Second call: statusLine is no longer in the new spec at all.
        let current = sample_ccx_spec(&[], &[]);
        let changed = merge_claude_generic_settings(&dir, Some(&current), Some(&previous)).unwrap();
        assert_eq!(changed, 1);

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert!(settings.get("statusLine").is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_render_claude_projection_removes_settings_key_across_two_pulls() {
        // Full round trip through render_claude_projection + the CCX
        // lock: first "pull" writes a managed key and records it in the
        // lock; second "pull" (simulating the backend no longer sending
        // that key) removes it via the lock's recorded previous state.
        let dir = temp_dir("ccx-two-pulls");
        let first_settings = sample_ccx_spec(
            &["statusLine"],
            &[(
                "statusLine",
                serde_json::json!({"type": "command", "command": "node hud.mjs"}),
            )],
        );

        render_claude_projection(
            &dir,
            "Test Project",
            ".nexus",
            &[],
            &[],
            &[],
            None,
            &[],
            None,
            Some(&first_settings),
            None,
            false,
        )
        .unwrap();

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert!(settings.get("statusLine").is_some());

        // Second pull: backend stops sending claude_settings entirely.
        render_claude_projection(
            &dir,
            "Test Project",
            ".nexus",
            &[],
            &[],
            &[],
            None,
            &[],
            None,
            None,
            None,
            false,
        )
        .unwrap();

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert!(
            settings.get("statusLine").is_none(),
            "statusLine should have been removed once no longer sent"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // ── merge_claude_md_managed_block (NEXUS-APP ADR-0117 "CCX") ───────────

    #[test]
    fn test_merge_claude_md_none_block_is_noop() {
        let dir = temp_dir("claudemd-none");
        let changed = merge_claude_md_managed_block(&dir, None, None, false).unwrap();
        assert_eq!(changed, ClaudeMdOutcome::Skipped);
        assert!(!dir.join("CLAUDE.md").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_claude_md_creates_file_with_markers() {
        let dir = temp_dir("claudemd-create");
        let changed =
            merge_claude_md_managed_block(&dir, Some("Nexus rules here."), None, false).unwrap();
        assert!(matches!(changed, ClaudeMdOutcome::Written(_)));
        let content = fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        assert!(content.contains(CLAUDE_MD_MANAGED_BEGIN));
        assert!(content.contains(CLAUDE_MD_MANAGED_END));
        assert!(content.contains("Nexus rules here."));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_claude_md_inserts_at_top_preserving_existing_content() {
        let dir = temp_dir("claudemd-insert-top");
        fs::write(
            dir.join("CLAUDE.md"),
            "# My own project notes\n\nDo not touch.",
        )
        .unwrap();

        merge_claude_md_managed_block(&dir, Some("Nexus rules here."), None, false).unwrap();

        let content = fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        assert!(content.contains("Nexus rules here."));
        assert!(content.contains("# My own project notes"));
        assert!(content.contains("Do not touch."));
        // Managed block must come before the user's own content.
        assert!(
            content.find(CLAUDE_MD_MANAGED_BEGIN).unwrap()
                < content.find("My own project notes").unwrap()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_claude_md_replaces_only_between_existing_markers() {
        let dir = temp_dir("claudemd-replace");
        fs::write(
            dir.join("CLAUDE.md"),
            format!(
                "Preamble.\n\n{}\nold content\n{}\n\n# User section\nUser text.",
                CLAUDE_MD_MANAGED_BEGIN, CLAUDE_MD_MANAGED_END
            ),
        )
        .unwrap();

        let changed =
            merge_claude_md_managed_block(&dir, Some("new content"), None, false).unwrap();
        assert!(matches!(changed, ClaudeMdOutcome::Written(_)));

        let content = fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        assert!(content.contains("new content"));
        assert!(!content.contains("old content"));
        assert!(content.contains("Preamble."));
        assert!(content.contains("# User section"));
        assert!(content.contains("User text."));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_claude_md_is_idempotent() {
        let dir = temp_dir("claudemd-idempotent");
        let first =
            merge_claude_md_managed_block(&dir, Some("stable content"), None, false).unwrap();
        let ClaudeMdOutcome::Written(sha) = first else {
            panic!("expected Written, got {first:?}");
        };
        let second =
            merge_claude_md_managed_block(&dir, Some("stable content"), Some(&sha), false).unwrap();
        assert_eq!(
            second,
            ClaudeMdOutcome::Unchanged(sha),
            "re-sending the same block must be a no-op"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_claude_md_coexists_with_other_managed_blocks() {
        // Must not disturb a different tool's own managed block further
        // down the file (e.g. a Next.js "nextjs-agent-rules" block).
        let dir = temp_dir("claudemd-coexist");
        fs::write(
            dir.join("CLAUDE.md"),
            "<!-- BEGIN:nextjs-agent-rules -->\nnextjs stuff\n<!-- END:nextjs-agent-rules -->\n",
        )
        .unwrap();

        merge_claude_md_managed_block(&dir, Some("nexus stuff"), None, false).unwrap();

        let content = fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        assert!(content.contains("nexus stuff"));
        assert!(content.contains("nextjs stuff"));
        assert!(content.contains("BEGIN:nextjs-agent-rules"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_claude_md_conflict_when_block_edited_locally() {
        let dir = temp_dir("claudemd-conflict");
        let ClaudeMdOutcome::Written(sha) =
            merge_claude_md_managed_block(&dir, Some("v1"), None, false).unwrap()
        else {
            panic!("expected Written");
        };
        let path = dir.join("CLAUDE.md");
        let edited = fs::read_to_string(&path).unwrap().replace("v1", "my edit");
        fs::write(&path, &edited).unwrap();

        // New block arrives, local block edited since the lock: keep it.
        let outcome = merge_claude_md_managed_block(&dir, Some("v2"), Some(&sha), false).unwrap();
        assert_eq!(outcome, ClaudeMdOutcome::Conflict);
        assert_eq!(fs::read_to_string(&path).unwrap(), edited);

        // --force replaces it.
        let outcome = merge_claude_md_managed_block(&dir, Some("v2"), Some(&sha), true).unwrap();
        assert!(matches!(outcome, ClaudeMdOutcome::Written(_)));
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("v2"));
        assert!(!content.contains("my edit"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_claude_md_updates_unedited_block_and_records_new_hash() {
        let dir = temp_dir("claudemd-update");
        let ClaudeMdOutcome::Written(sha) =
            merge_claude_md_managed_block(&dir, Some("v1"), None, false).unwrap()
        else {
            panic!("expected Written");
        };
        let outcome = merge_claude_md_managed_block(&dir, Some("v2"), Some(&sha), false).unwrap();
        assert_eq!(
            outcome,
            ClaudeMdOutcome::Written(nexus_core::hash::sha256_hex(&claude_md_desired_block_text(
                "v2"
            )))
        );
        let content = fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        assert_eq!(
            claude_md_block_text(&content),
            Some(claude_md_desired_block_text("v2").as_str())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_claude_hooks_appends_matcher_and_command() {
        let dir = temp_dir("hooks-merge-basic");
        let adapter = ClaudeHookAdapter {
            plugin_name: "session-guard".to_string(),
            target_path: ".claude/hooks/nexus-session-guard.mjs".to_string(),
            body: String::new(),
            hook_events: vec![nexus_core::api::ClaudeHookEvent {
                event: "PostToolUse".to_string(),
                matcher: Some("Edit|Write|Bash".to_string()),
                timeout: None,
            }],
        };

        let (appended, _removed) =
            merge_claude_hooks(&dir, std::slice::from_ref(&adapter), None).unwrap();
        assert_eq!(appended, 1);

        let settings = fs::read_to_string(dir.join(".claude/settings.json")).unwrap();
        assert!(settings.contains("\"PostToolUse\""));
        assert!(settings.contains("\"matcher\": \"Edit|Write|Bash\""));
        assert!(settings.contains("nexus-session-guard.mjs"));
        assert!(settings.contains("post-tool-use"));
        assert!(settings.contains("${CLAUDE_PROJECT_DIR}"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_claude_hooks_two_plugins_same_event_both_kept() {
        // Confirmed live against a real Claude Code session (dispatch
        // 2d5017f7): multiple plugins may register independent array
        // entries under the same event key.
        let dir = temp_dir("hooks-merge-multi");
        let headroom = ClaudeHookAdapter {
            plugin_name: "headroom-intercept".to_string(),
            target_path: ".claude/hooks/nexus-headroom-intercept.mjs".to_string(),
            body: String::new(),
            hook_events: vec![nexus_core::api::ClaudeHookEvent {
                event: "Stop".to_string(),
                matcher: None,
                timeout: None,
            }],
        };
        let cost_control = ClaudeHookAdapter {
            plugin_name: "cost-control".to_string(),
            target_path: ".claude/hooks/nexus-cost-control.mjs".to_string(),
            body: String::new(),
            hook_events: vec![nexus_core::api::ClaudeHookEvent {
                event: "Stop".to_string(),
                matcher: None,
                timeout: None,
            }],
        };

        let (appended, _removed) =
            merge_claude_hooks(&dir, &[headroom, cost_control], None).unwrap();
        assert_eq!(appended, 2);

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        let stop_entries = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop_entries.len(), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_claude_hooks_is_idempotent_on_rerun() {
        let dir = temp_dir("hooks-merge-idempotent");
        let adapter = ClaudeHookAdapter {
            plugin_name: "session-guard".to_string(),
            target_path: ".claude/hooks/nexus-session-guard.mjs".to_string(),
            body: String::new(),
            hook_events: vec![nexus_core::api::ClaudeHookEvent {
                event: "PostToolUse".to_string(),
                matcher: Some("Edit|Write".to_string()),
                timeout: None,
            }],
        };

        let (first, _removed) =
            merge_claude_hooks(&dir, std::slice::from_ref(&adapter), None).unwrap();
        assert_eq!(first, 1);
        let (second, _removed) =
            merge_claude_hooks(&dir, std::slice::from_ref(&adapter), None).unwrap();
        assert_eq!(
            second, 0,
            "re-running with the same adapter must not duplicate the entry"
        );

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            settings["hooks"]["PostToolUse"].as_array().unwrap().len(),
            1
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_claude_hooks_preserves_operator_customization() {
        // Must never touch/remove an entry the operator hand-edited, even
        // for events/plugins nexus-cli also wants to register into.
        let dir = temp_dir("hooks-merge-preserve");
        fs::create_dir_all(dir.join(".claude")).unwrap();
        fs::write(
            dir.join(".claude/settings.json"),
            r#"{
  "permissions": {},
  "hooks": {
    "PostToolUse": [
      { "matcher": "MyCustomTool", "hooks": [{ "type": "command", "command": "echo custom" }] }
    ]
  }
}
"#,
        )
        .unwrap();

        let adapter = ClaudeHookAdapter {
            plugin_name: "session-guard".to_string(),
            target_path: ".claude/hooks/nexus-session-guard.mjs".to_string(),
            body: String::new(),
            hook_events: vec![nexus_core::api::ClaudeHookEvent {
                event: "PostToolUse".to_string(),
                matcher: Some("Edit|Write".to_string()),
                timeout: None,
            }],
        };

        merge_claude_hooks(&dir, &[adapter], None).unwrap();

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        let entries = settings["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(
            entries.len(),
            2,
            "operator entry must be preserved, new one appended"
        );
        assert!(entries
            .iter()
            .any(|e| e["matcher"] == "MyCustomTool" && e["hooks"][0]["command"] == "echo custom"));
        assert!(entries.iter().any(|e| e["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("nexus-session-guard.mjs")));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_claude_hooks_noop_when_no_adapters() {
        let dir = temp_dir("hooks-merge-empty");
        let (appended, _removed) = merge_claude_hooks(&dir, &[], None).unwrap();
        assert_eq!(appended, 0);
        assert!(!dir.join(".claude/settings.json").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    // ── merge_claude_hooks removal (NEXUS-APP dispatch 99f335e8
    // follow-up: hook adapter removal e.g. when nexus-core takes over) ────

    fn hook_lock_entry(
        plugin_name: &str,
        target_path: &str,
        body: &str,
        events: &[(&str, Option<&str>)],
    ) -> (String, ccx::CcxLockHookEntry) {
        (
            target_path.to_string(),
            ccx::CcxLockHookEntry {
                plugin_name: plugin_name.to_string(),
                target_path: target_path.to_string(),
                file_sha256: nexus_core::hash::sha256_hex(body),
                registrations: events
                    .iter()
                    .map(|(event, matcher)| ccx::CcxLockHookRegistration {
                        event: (*event).to_string(),
                        matcher: matcher.map(|s| s.to_string()),
                        command: format!(
                            "node \"${{CLAUDE_PROJECT_DIR}}/{}\" {}",
                            target_path,
                            kebab_case_event(event)
                        ),
                    })
                    .collect(),
            },
        )
    }

    #[test]
    fn test_merge_claude_hooks_removes_adapter_no_longer_present() {
        let dir = temp_dir("hooks-removal-basic");
        let body = "// headroom hook script";
        fs::create_dir_all(dir.join(".claude/hooks")).unwrap();
        fs::write(dir.join(".claude/hooks/nexus-headroom-intercept.mjs"), body).unwrap();

        // Pre-seed settings.json exactly as merge_claude_hooks would have
        // written it originally.
        fs::write(
            dir.join(".claude/settings.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "hooks": {
                    "Stop": [{
                        "hooks": [{
                            "type": "command",
                            "command": "node \"${CLAUDE_PROJECT_DIR}/.claude/hooks/nexus-headroom-intercept.mjs\" stop"
                        }]
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let previous = std::collections::BTreeMap::from([hook_lock_entry(
            "headroom-intercept",
            ".claude/hooks/nexus-headroom-intercept.mjs",
            body,
            &[("Stop", None)],
        )]);

        // adapters is now empty: nexus-core took over, no adapters sent.
        let (appended, removed) = merge_claude_hooks(&dir, &[], Some(&previous)).unwrap();
        assert_eq!(appended, 0);
        assert_eq!(removed, vec!["headroom-intercept".to_string()]);

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            settings["hooks"]["Stop"].as_array().unwrap().len(),
            0,
            "the Nexus-written Stop entry must be removed"
        );
        assert!(
            !dir.join(".claude/hooks/nexus-headroom-intercept.mjs")
                .exists(),
            "unmodified hook file must be deleted"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_claude_hooks_removal_preserves_operator_customized_file() {
        // "Orphaned" rule: if the local hook file no longer matches what
        // Nexus last wrote, it must NOT be deleted -- the operator has
        // customized it, and deleting a file they edited would lose work.
        let dir = temp_dir("hooks-removal-preserve-file");
        let original_body = "// original nexus body";
        fs::create_dir_all(dir.join(".claude/hooks")).unwrap();
        fs::write(
            dir.join(".claude/hooks/nexus-headroom-intercept.mjs"),
            "// operator customized this file",
        )
        .unwrap();
        fs::write(dir.join(".claude/settings.json"), r#"{"hooks": {}}"#).unwrap();

        let previous = std::collections::BTreeMap::from([hook_lock_entry(
            "headroom-intercept",
            ".claude/hooks/nexus-headroom-intercept.mjs",
            original_body,
            &[("Stop", None)],
        )]);

        merge_claude_hooks(&dir, &[], Some(&previous)).unwrap();

        assert!(
            dir.join(".claude/hooks/nexus-headroom-intercept.mjs")
                .exists(),
            "operator-modified hook file must survive"
        );
        let content =
            fs::read_to_string(dir.join(".claude/hooks/nexus-headroom-intercept.mjs")).unwrap();
        assert_eq!(content, "// operator customized this file");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_claude_hooks_removal_preserves_operator_settings_entries() {
        // Only the exact entry Nexus wrote (matching command) is removed;
        // an operator's own entry for the same event must survive.
        let dir = temp_dir("hooks-removal-preserve-settings");
        let body = "// headroom hook script";
        fs::create_dir_all(dir.join(".claude/hooks")).unwrap();
        fs::write(dir.join(".claude/hooks/nexus-headroom-intercept.mjs"), body).unwrap();
        fs::write(
            dir.join(".claude/settings.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "hooks": {
                    "Stop": [
                        {
                            "hooks": [{
                                "type": "command",
                                "command": "node \"${CLAUDE_PROJECT_DIR}/.claude/hooks/nexus-headroom-intercept.mjs\" stop"
                            }]
                        },
                        {
                            "hooks": [{"type": "command", "command": "echo operator-own-hook"}]
                        }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let previous = std::collections::BTreeMap::from([hook_lock_entry(
            "headroom-intercept",
            ".claude/hooks/nexus-headroom-intercept.mjs",
            body,
            &[("Stop", None)],
        )]);

        merge_claude_hooks(&dir, &[], Some(&previous)).unwrap();

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        let stop_entries = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(
            stop_entries.len(),
            1,
            "operator's own Stop entry must survive"
        );
        assert_eq!(
            stop_entries[0]["hooks"][0]["command"],
            "echo operator-own-hook"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_claude_hooks_still_managed_adapter_is_not_removed() {
        let dir = temp_dir("hooks-removal-still-managed");
        let adapter = ClaudeHookAdapter {
            plugin_name: "headroom-intercept".to_string(),
            target_path: ".claude/hooks/nexus-headroom-intercept.mjs".to_string(),
            body: "// v2".to_string(),
            hook_events: vec![nexus_core::api::ClaudeHookEvent {
                event: "Stop".to_string(),
                matcher: None,
                timeout: None,
            }],
        };
        let previous = std::collections::BTreeMap::from([hook_lock_entry(
            "headroom-intercept",
            ".claude/hooks/nexus-headroom-intercept.mjs",
            "// v1",
            &[("Stop", None)],
        )]);

        let (appended, removed) = merge_claude_hooks(&dir, &[adapter], Some(&previous)).unwrap();
        assert_eq!(appended, 1);
        assert!(
            removed.is_empty(),
            "still-managed adapter must not be reported as removed"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_baseline_permissions_creates_file_and_appends_all_entries() {
        let dir = temp_dir("permissions-merge-basic");
        let appended = merge_claude_baseline_permissions(&dir).unwrap();
        assert_eq!(appended, BASELINE_MCP_PERMISSIONS.len());

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        assert_eq!(allow.len(), BASELINE_MCP_PERMISSIONS.len());
        for entry in BASELINE_MCP_PERMISSIONS {
            assert!(
                allow.iter().any(|v| v.as_str() == Some(*entry)),
                "missing baseline entry: {entry}"
            );
        }
        // Explicitly confirm nothing destructive is pre-approved.
        assert!(!allow
            .iter()
            .any(|v| v.as_str().is_some_and(|s| s.contains("delete")
                || s.contains("adr_decide")
                || s.contains("dispatch_resolve")
                || s.contains("sk_update"))));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_baseline_permissions_is_idempotent_on_rerun() {
        let dir = temp_dir("permissions-merge-idempotent");
        let first = merge_claude_baseline_permissions(&dir).unwrap();
        assert_eq!(first, BASELINE_MCP_PERMISSIONS.len());
        let second = merge_claude_baseline_permissions(&dir).unwrap();
        assert_eq!(
            second, 0,
            "re-running against an already-merged file must not duplicate entries"
        );

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        assert_eq!(allow.len(), BASELINE_MCP_PERMISSIONS.len());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_baseline_permissions_preserves_operator_additions() {
        // An operator who already broadened their own allowlist (e.g. via
        // Claude Code's own "don't ask again" flow writing into
        // settings.json directly, or a hand edit) must keep every entry
        // they added -- this only ever raises the floor, never resets it.
        let dir = temp_dir("permissions-merge-preserve");
        fs::create_dir_all(dir.join(".claude")).unwrap();
        fs::write(
            dir.join(".claude/settings.json"),
            r#"{
  "permissions": {
    "allow": ["mcp__nexus__task_create", "Bash(npm run *)"]
  },
  "hooks": {}
}
"#,
        )
        .unwrap();

        let appended = merge_claude_baseline_permissions(&dir).unwrap();
        assert_eq!(appended, BASELINE_MCP_PERMISSIONS.len());

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        assert!(allow.iter().any(|v| v == "mcp__nexus__task_create"));
        assert!(allow.iter().any(|v| v == "Bash(npm run *)"));
        assert_eq!(allow.len(), BASELINE_MCP_PERMISSIONS.len() + 2);
        assert!(settings["hooks"].is_object());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_baseline_permissions_does_not_duplicate_operator_added_baseline_entry() {
        // If the operator already has one of the baseline entries (e.g.
        // Claude Code itself already wrote it into settings.local.json and
        // the operator copied it up, or they added it by hand), merging
        // must recognize it as already-present rather than duplicating it.
        let dir = temp_dir("permissions-merge-no-dup");
        fs::create_dir_all(dir.join(".claude")).unwrap();
        fs::write(
            dir.join(".claude/settings.json"),
            r#"{ "permissions": { "allow": ["mcp__nexus__session_list"] } }"#,
        )
        .unwrap();

        let appended = merge_claude_baseline_permissions(&dir).unwrap();
        assert_eq!(appended, BASELINE_MCP_PERMISSIONS.len() - 1);

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
                .unwrap();
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        let session_list_count = allow
            .iter()
            .filter(|v| v.as_str() == Some("mcp__nexus__session_list"))
            .count();
        assert_eq!(session_list_count, 1, "must not duplicate the entry");
        assert_eq!(allow.len(), BASELINE_MCP_PERMISSIONS.len());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_render_claude_projection_writes_hook_adapters_and_registers_hooks() {
        let dir = temp_dir("full-render-hook-adapters");
        let adapter = ClaudeHookAdapter {
            plugin_name: "routing-guard".to_string(),
            target_path: ".claude/hooks/nexus-routing-guard.mjs".to_string(),
            body: "// routing-guard adapter".to_string(),
            hook_events: vec![nexus_core::api::ClaudeHookEvent {
                event: "SessionStart".to_string(),
                matcher: None,
                timeout: None,
            }],
        };

        render_claude_projection(
            &dir,
            "Test Project",
            ".nexus",
            &[],
            &[],
            &[],
            None,
            &[adapter],
            None,
            None,
            None,
            false,
        )
        .unwrap();

        assert!(dir.join(".claude/hooks/nexus-routing-guard.mjs").exists());
        let settings = fs::read_to_string(dir.join(".claude/settings.json")).unwrap();
        assert!(settings.contains("\"SessionStart\""));
        assert!(settings.contains("session-start"));

        let _ = fs::remove_dir_all(&dir);
    }
}
