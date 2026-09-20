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
use nexus_core::api::{ExportedActorFile, ExportedAgentFile, ExportedSkill};

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

/// Write a single skill as a native Claude Code project skill:
/// `.claude/skills/<canonical-id>/SKILL.md` (+ any resource files).
/// The directory name becomes the `/<canonical-id>` slash-command in
/// Claude Code (project skill directory names are command names).
pub fn write_claude_skill(target: &Path, skill: &ExportedSkill) -> anyhow::Result<()> {
    let canonical_id = canonical_claude_skill_id(&skill.skill_id);
    let skill_dir = target.join(".claude").join("skills").join(&canonical_id);
    fs::create_dir_all(&skill_dir)?;

    let body = skill
        .body
        .as_deref()
        .unwrap_or("<!-- No skill body defined -->");

    let content = format!(
        r#"---
skill_id: {skill_id}
name: {name}
version: {version}
source: nexus-platform
---

{body}
"#,
        skill_id = canonical_id,
        name = skill.name,
        version = skill.version,
        body = body,
    );

    fs::write(skill_dir.join("SKILL.md"), content)?;

    for res in &skill.resources {
        // Sanitize filename: prevent directory traversal.
        let filename = res.filename.replace(['/', '\\'], "_");
        if filename.is_empty() || filename == "SKILL.md" {
            continue;
        }
        fs::write(skill_dir.join(&filename), &res.body)?;
    }

    Ok(())
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
    let mut by_slug: BTreeMap<String, String> = BTreeMap::new();

    for actor in actors {
        by_slug.insert(actor.slug.clone(), actor.body.clone());
    }
    for af in agent_files {
        if let Some(slug) = actor_slug_from_agent_file(&af.target_path) {
            by_slug.entry(slug).or_insert_with(|| af.body.clone());
        }
    }

    if by_slug.is_empty() {
        return Ok(0);
    }

    let agents_dir = target.join(".claude").join("agents");
    fs::create_dir_all(&agents_dir)?;

    let mut written = 0;
    for (slug, body) in &by_slug {
        fs::write(agents_dir.join(format!("{}.md", slug)), body)?;
        written += 1;
    }
    Ok(written)
}

/// Write `.claude/settings.json` if it does not already exist (user-managed
/// once created, never overwritten). Contains Claude runtime behavior only —
/// no secrets, no OpenCode-specific statements (ADR-C04 "CLAUDE.md design").
/// Returns `true` if the file was created.
pub fn write_claude_settings(target: &Path) -> anyhow::Result<bool> {
    let path = target.join(".claude").join("settings.json");
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = r#"{
  "$schema": "https://json.schemastore.org/claude-code-settings.json",
  "permissions": {}
}
"#;
    fs::write(&path, content)?;
    Ok(true)
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
    let content = format!(
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
    );
    fs::write(&path, content)?;
    Ok(true)
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
#[allow(clippy::too_many_arguments)]
pub fn render_claude_projection(
    target: &Path,
    project_name: &str,
    agentic_root: &str,
    skills: &[ExportedSkill],
    actors: &[ExportedActorFile],
    agent_files: &[ExportedAgentFile],
) -> anyhow::Result<()> {
    let mut skills_written = 0;
    for skill in skills {
        write_claude_skill(target, skill)?;
        skills_written += 1;
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

    if write_claude_settings(target)? {
        println!("   {} .claude/settings.json", style("+").bold().green());
    }

    if write_claude_root_md(target, project_name, agentic_root)? {
        println!("   {} CLAUDE.md", style("+").bold().green());
    }

    Ok(())
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

        let created = write_claude_settings(&dir).unwrap();
        assert!(created);
        assert!(dir.join(".claude/settings.json").exists());

        // Simulate user edit, then re-run: must not overwrite.
        fs::write(dir.join(".claude/settings.json"), "user-edited").unwrap();
        let created_again = write_claude_settings(&dir).unwrap();
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
        write_claude_settings(&dir).unwrap();
        let content = fs::read_to_string(dir.join(".claude/settings.json")).unwrap();
        assert!(!content.contains("NEXUS_PRIVATE_TOKEN"));
        assert!(!content.contains("nxs_pat_"));
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

        render_claude_projection(&dir, "Test Project", ".nexus", &skills, &actors, &[]).unwrap();

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
}
