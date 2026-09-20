//! Shared display helpers for project-scoped commands.
//!
//! Every command that talks to a specific Nexus project (`pull`, `push`,
//! `init`, `sync status`, `project status`, etc.) should make two things
//! visible up front:
//!
//! 1. Which API backend it is targeting (`api_url`) — a workspace silently
//!    pointed at the wrong environment (e.g. staging vs. prod) is otherwise
//!    only diagnosable via `nexus config show` or `nexus status`.
//! 2. The project's human-readable name, not just its opaque UUID — the
//!    UUID remains visible in parentheses for exact identification, but the
//!    name is what a human actually recognizes at a glance.
use console::style;
use nexus_core::api::NexusClient;

/// Print the standard "API: ... / Project: Name (uuid)" banner used by all
/// project-scoped commands.
///
/// `project_name` is `None` when the display name could not be resolved
/// (e.g. offline, or the project no longer exists at `api_url`) -- in that
/// case only the UUID is shown, matching previous behavior.
pub fn print_project_banner(api_url: &str, project_id: &str, project_name: Option<&str>) {
    println!("   API:     {}", style(api_url).dim());
    match project_name {
        Some(name) => println!(
            "   Project: {} {}",
            style(name).bold(),
            style(format!("({})", project_id)).dim()
        ),
        None => println!("   Project: {}", style(project_id).dim()),
    }
}

/// Resolve a project's display name for banner output.
///
/// Prefers the locally linked `.nexus/config.toml` entry when it matches
/// `project_id` (no network round trip). Falls back to a live
/// `GET /api/mcp/projects/{id}` lookup via `client`, which is the same
/// call most of these commands already make elsewhere. Returns `None`
/// (never an error) if neither source can resolve a name -- callers should
/// fall back to showing the raw UUID via [`print_project_banner`].
pub async fn resolve_project_display_name(
    client: &NexusClient,
    project_id: &str,
    workspace: Option<&std::path::Path>,
) -> Option<String> {
    if let Ok(Some(info)) = nexus_core::config::load_linked_project(workspace) {
        if info.id == project_id && !info.name.is_empty() {
            return Some(info.name);
        }
    }
    client
        .get_project(project_id)
        .await
        .ok()
        .map(|detail| detail.project.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::config::{save_project_config, ProjectConfig, ProjectInfo};

    /// Regression test for the "nexus pull only shows an opaque UUID"
    /// feedback: when a project is already linked locally, resolving its
    /// display name must not require a network round trip.
    #[tokio::test]
    async fn resolves_name_from_local_link_without_network_call() {
        let workspace = tempfile::tempdir().unwrap();
        let project_id = "3273a55a-1e2f-43d7-9c2d-75035e902959";

        save_project_config(
            Some(workspace.path()),
            &ProjectConfig {
                project: Some(ProjectInfo {
                    id: project_id.to_string(),
                    name: "NEXUS-CLI".to_string(),
                    slug: "nexus-cli".to_string(),
                }),
                ..Default::default()
            },
        )
        .unwrap();

        // Use an unroutable base URL: if this test ever fell through to a
        // network call it would hang/fail, proving the local-link fast
        // path was actually taken.
        let client = NexusClient::new("http://127.0.0.1:0", None).unwrap();

        let name = resolve_project_display_name(&client, project_id, Some(workspace.path())).await;
        assert_eq!(name.as_deref(), Some("NEXUS-CLI"));
    }

    #[tokio::test]
    async fn returns_none_when_no_local_link_and_lookup_fails() {
        let workspace = tempfile::tempdir().unwrap();
        let client = NexusClient::new("http://127.0.0.1:0", None).unwrap();

        let name = resolve_project_display_name(
            &client,
            "00000000-0000-0000-0000-000000000000",
            Some(workspace.path()),
        )
        .await;
        assert!(name.is_none());
    }
}
