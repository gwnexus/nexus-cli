//! `nexus env get | keys | set`: the project's backend settings
//! (NEXUS-APP dispatch b5f7bfb0), via `GET`/`PATCH
//! /api/mcp/projects/{id}/settings`. Keys, types and allowed values come
//! from the backend `schema`; nothing is hardcoded. `nexus env` never writes
//! local files except through `--pull`.

use console::style;
use nexus_core::api::{NexusClient, ProjectSettingsResponse, SettingSchema, SettingsPatchOutcome};
use nexus_core::auth::resolve_token;
use nexus_core::config;

fn connect(api_url: &str) -> anyhow::Result<(String, NexusClient)> {
    let workspace = std::env::current_dir()?;
    let project_id = config::resolve_project_id(None, Some(&workspace))?;
    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;
    Ok((project_id, NexusClient::new(api_url, Some(token))?))
}

/// Fetch the settings, turning a 404 into a clear "backend too old" hint.
async fn fetch(client: &NexusClient, project_id: &str) -> anyhow::Result<ProjectSettingsResponse> {
    match client.get_project_settings(project_id).await {
        Err(nexus_core::Error::NotFound(_)) => anyhow::bail!(
            "this Nexus backend does not offer project settings yet (GET /api/mcp/projects/{{id}}/settings); \
             change them in the Nexus dashboard"
        ),
        other => Ok(other?),
    }
}

fn display(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "(unset)".to_string(),
        other => other.to_string(),
    }
}

/// `nexus env get [key]`.
pub async fn get(api_url: &str, key: Option<&str>, json: bool) -> anyhow::Result<()> {
    let (project_id, client) = connect(api_url)?;
    let resp = fetch(&client, &project_id).await?;
    if let Some(key) = key {
        let value = resp
            .settings
            .get(key)
            .ok_or_else(|| unknown_key(key, &resp.schema))?;
        if json {
            println!("{}", serde_json::to_string_pretty(value)?);
        } else {
            println!("{}", display(value));
        }
        return Ok(());
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "revision": resp.revision,
                "settings": resp.settings,
                "run_target": resp.run_target,
                "can_write": resp.can_write,
            }))?
        );
        return Ok(());
    }
    let width = resp.settings.keys().map(String::len).max().unwrap_or(0);
    for (k, v) in &resp.settings {
        println!("  {:<width$}  {}", k, display(v));
    }
    if !resp.can_write {
        println!();
        println!(
            "  {} Read-only: changing settings requires project admin rights.",
            style("i").bold().blue()
        );
    }
    Ok(())
}

/// `nexus env keys`: the settings schema.
pub async fn keys(api_url: &str, json: bool) -> anyhow::Result<()> {
    let (project_id, client) = connect(api_url)?;
    let resp = fetch(&client, &project_id).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&resp.schema)?);
        return Ok(());
    }
    for s in &resp.schema {
        let values = if s.values.is_empty() {
            s.kind.clone()
        } else {
            s.values.iter().map(display).collect::<Vec<_>>().join(" | ")
        };
        println!(
            "  {:<28} {}{}",
            style(&s.key).bold(),
            values,
            if s.nullable { " | unset" } else { "" }
        );
        if let Some(ref d) = s.description {
            println!("  {:<28} {}", "", style(d).dim());
        }
    }
    Ok(())
}

fn unknown_key(key: &str, schema: &[SettingSchema]) -> anyhow::Error {
    anyhow::anyhow!(
        "unknown setting '{}'. Known keys: {}",
        key,
        schema
            .iter()
            .map(|s| s.key.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Parse a command-line value into the JSON value the schema expects.
fn parse_value(schema: &SettingSchema, raw: &str) -> anyhow::Result<serde_json::Value> {
    if matches!(raw, "null" | "unset") {
        if schema.nullable {
            return Ok(serde_json::Value::Null);
        }
        anyhow::bail!("'{}' cannot be unset", schema.key);
    }
    match schema.kind.as_str() {
        "bool" => match raw.to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" => Ok(serde_json::Value::Bool(true)),
            "false" | "off" | "no" => Ok(serde_json::Value::Bool(false)),
            _ => anyhow::bail!("'{}' expects true or false, got '{}'", schema.key, raw),
        },
        "enum" => {
            let value = serde_json::Value::String(raw.to_string());
            if !schema.values.is_empty() && !schema.values.contains(&value) {
                anyhow::bail!(
                    "'{}' must be one of: {}",
                    schema.key,
                    schema
                        .values
                        .iter()
                        .map(display)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            Ok(value)
        }
        _ => Ok(serde_json::Value::String(raw.to_string())),
    }
}

/// `nexus env set <key> <value> [--dry-run] [--pull]`. Returns `true` when
/// something changed (so the caller can chain `nexus pull`).
pub async fn set(
    api_url: &str,
    key: &str,
    raw_value: &str,
    dry_run: bool,
    assume_yes: bool,
) -> anyhow::Result<bool> {
    let (project_id, client) = connect(api_url)?;
    let mut current = fetch(&client, &project_id).await?;
    let schema = current
        .schema
        .iter()
        .find(|s| s.key == key)
        .cloned()
        .ok_or_else(|| unknown_key(key, &current.schema))?;
    let value = parse_value(&schema, raw_value)?;
    let mut set = serde_json::Map::new();
    set.insert(key.to_string(), value);

    // One retry after a concurrent change (409), on confirmation.
    for attempt in 0..2 {
        match client
            .patch_project_settings(&project_id, &set, dry_run, Some(&current.revision))
            .await?
        {
            SettingsPatchOutcome::Applied(resp) => {
                print_changes(&resp, dry_run);
                let claude_key_on_opencode = key.starts_with("claude.")
                    && resp.settings.get("executioner").and_then(|v| v.as_str())
                        != Some("claude-cli");
                if claude_key_on_opencode {
                    println!(
                        "   {} Stored; claude.* settings only take effect with the claude-cli executioner.",
                        style("i").bold().blue()
                    );
                }
                return Ok(!resp.dry_run && !resp.changes.is_empty());
            }
            SettingsPatchOutcome::Invalid { error, details } => {
                println!("   {} {}", style("!").bold().red(), error);
                for (field, message) in &details {
                    println!("      {field}: {message}");
                }
                anyhow::bail!("setting rejected by the backend");
            }
            SettingsPatchOutcome::Forbidden(error) => anyhow::bail!(error),
            SettingsPatchOutcome::Conflict { error, .. } => {
                if attempt > 0 {
                    anyhow::bail!(error);
                }
                let fresh = fetch(&client, &project_id).await?;
                println!(
                    "   {} {} Changed since it was read:",
                    style("!").bold().yellow(),
                    error
                );
                for (k, v) in &fresh.settings {
                    let before = current.settings.get(k);
                    if before != Some(v) {
                        println!(
                            "      {k}: {} -> {}",
                            before.map(display).unwrap_or_default(),
                            display(v)
                        );
                    }
                }
                if !assume_yes && !confirm("Apply your change on top of it?")? {
                    anyhow::bail!("not applied");
                }
                current = fresh;
            }
        }
    }
    unreachable!("the loop returns or bails on its second attempt")
}

fn print_changes(resp: &ProjectSettingsResponse, dry_run: bool) {
    if resp.changes.is_empty() {
        println!("   {} No change (already set).", style("=").dim());
        return;
    }
    for c in &resp.changes {
        println!(
            "   {} {}: {} -> {}",
            style("~").bold().cyan(),
            c.key,
            display(&c.from),
            display(&c.to)
        );
    }
    for line in agent_file_lines(resp, dry_run || resp.dry_run) {
        println!("{line}");
    }
    if dry_run || resp.dry_run {
        println!("   {} Dry run, nothing applied.", style("i").bold().blue());
    }
}

/// The agent files a change assigns / unassigns (e.g. an executioner
/// switch), as reported by the backend.
fn agent_file_lines(resp: &ProjectSettingsResponse, dry_run: bool) -> Vec<String> {
    let Some(delta) = resp.agent_files.as_ref() else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    let mut section = |keys: &[String], verb: &str, sign: console::StyledObject<&str>| {
        if keys.is_empty() {
            return;
        }
        lines.push(format!(
            "   {} {} agent file(s) {}:",
            sign,
            keys.len(),
            if dry_run {
                format!("would be {verb}")
            } else {
                verb.to_string()
            }
        ));
        for key in keys {
            lines.push(format!("      {key}"));
        }
    };
    section(&delta.assign, "assigned", style("+").bold().green());
    section(&delta.unassign, "unassigned", style("-").bold().red());
    lines
}

fn confirm(question: &str) -> anyhow::Result<bool> {
    use std::io::Write as _;
    print!("   {} {} [y/N] ", style("?").bold().cyan(), question);
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(kind: &str, values: &[&str], nullable: bool) -> SettingSchema {
        SettingSchema {
            key: "k".into(),
            kind: kind.into(),
            values: values.iter().map(|v| serde_json::json!(v)).collect(),
            nullable,
            description: None,
        }
    }

    #[test]
    fn test_parse_bool() {
        let s = schema("bool", &[], false);
        assert_eq!(parse_value(&s, "true").unwrap(), serde_json::json!(true));
        assert_eq!(parse_value(&s, "off").unwrap(), serde_json::json!(false));
        assert!(parse_value(&s, "maybe").is_err());
        assert!(parse_value(&s, "unset").is_err());
    }

    #[test]
    fn test_parse_enum_from_schema_values() {
        let s = schema("enum", &["off", "minimal"], false);
        assert_eq!(
            parse_value(&s, "minimal").unwrap(),
            serde_json::json!("minimal")
        );
        let err = parse_value(&s, "loud").unwrap_err().to_string();
        assert!(err.contains("off, minimal"), "{err}");
    }

    #[test]
    fn test_parse_nullable_string() {
        let s = schema("string", &[], true);
        assert_eq!(parse_value(&s, "null").unwrap(), serde_json::Value::Null);
        assert_eq!(parse_value(&s, "unset").unwrap(), serde_json::Value::Null);
        assert_eq!(parse_value(&s, "octo").unwrap(), serde_json::json!("octo"));
    }

    #[test]
    fn test_unknown_key_lists_schema_keys() {
        let err = unknown_key("nope", &[schema("bool", &[], false)]).to_string();
        assert!(err.contains("Known keys: k"));
    }

    #[test]
    fn test_settings_response_parses_backend_contract() {
        let resp: ProjectSettingsResponse = serde_json::from_value(serde_json::json!({
            "project_id": "p", "revision": "3c65583cc9101a4e",
            "settings": {"executioner": "claude-cli", "claude.hud": "engineering", "git.signing-key": null},
            "run_target": {"tool": "claude", "workspace": "zellij", "layout": ".nexus/claude/nexus-claude.kdl"},
            "claude_experience_explicit": false, "can_write": true,
            "schema": [{"key": "claude.hud", "type": "enum", "values": ["off", "minimal"], "description": "HUD"}],
            "changes": [{"key": "claude.hud", "from": "engineering", "to": "minimal"}],
            "dry_run": false
        }))
        .unwrap();
        assert_eq!(resp.schema[0].kind, "enum");
        assert_eq!(resp.changes[0].to, serde_json::json!("minimal"));
        assert_eq!(
            resp.run_target.unwrap().workspace.as_deref(),
            Some("zellij")
        );
    }

    #[test]
    fn test_agent_files_of_executioner_switch() {
        let mut resp: ProjectSettingsResponse = serde_json::from_value(serde_json::json!({
            "revision": "r", "settings": {"executioner": "claude-cli"},
            "changes": [{"key": "executioner", "from": "opencode", "to": "claude-cli"}],
            "dry_run": true,
            "agent_files": {"assign": ["ccx-rule-base", "claude-md"], "unassign": ["opencode-json"]}
        }))
        .unwrap();
        let lines: Vec<String> = agent_file_lines(&resp, true)
            .iter()
            .map(|l| console::strip_ansi_codes(l).into_owned())
            .collect();
        assert_eq!(
            lines,
            vec![
                "   + 2 agent file(s) would be assigned:",
                "      ccx-rule-base",
                "      claude-md",
                "   - 1 agent file(s) would be unassigned:",
                "      opencode-json",
            ]
        );
        assert!(agent_file_lines(&resp, false)[0].contains("assigned:"));
        // Older backends: nothing shown.
        resp.agent_files = None;
        assert!(agent_file_lines(&resp, false).is_empty());
    }
}
