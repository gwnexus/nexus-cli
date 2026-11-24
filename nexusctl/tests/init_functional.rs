//! Functional tests for `nexus init`: a new workspace is linked and
//! filled from the backend, and a second init never destroys local work.

mod common;
use common::*;

/// `init --project-id` links the workspace and pulls the project content.
#[test]
fn init_links_and_pulls() {
    let api = MockApi::start();
    full_backend().install(&api);
    let ws = Workspace::unlinked();
    ws.nexus(
        &api,
        &[
            "init",
            "--project-id",
            PROJECT_ID,
            "--name",
            "demo",
            "--trust",
        ],
    )
    .assert_ok();
    let config = ws.read(".nexus/config.toml");
    assert!(config.contains(PROJECT_ID), "project not linked:\n{config}");
    assert!(ws.exists(".nexus/AGENTS.md"));
    assert!(ws.exists(".nexus/directives.md"));
    assert!(ws.read(".nexus/directives.md").contains("Be precise"));
}

/// A second init without --force stops and changes nothing.
#[test]
fn init_twice_without_force_changes_nothing() {
    let api = MockApi::start();
    full_backend().install(&api);
    let ws = Workspace::unlinked();
    ws.nexus(
        &api,
        &[
            "init",
            "--project-id",
            PROJECT_ID,
            "--name",
            "demo",
            "--trust",
        ],
    )
    .assert_ok();
    ws.write(".nexus/notes.md", "mine\n");
    let before = snapshot(&ws);
    let run = ws.nexus(
        &api,
        &["init", "--project-id", PROJECT_ID, "--name", "demo"],
    );
    assert!(!run.ok, "second init went on:\n{}", run.all());
    assert_eq!(snapshot(&ws), before);
}

/// A name with quotes and a line break stays one TOML value; the linked
/// project id is still the one written.
#[test]
fn init_project_name_cannot_break_the_config() {
    let api = MockApi::start();
    full_backend().install(&api);
    let ws = Workspace::unlinked();
    let name = "a \"b\"\nid = \"evil\"";
    ws.nexus(
        &api,
        &[
            "init",
            "--project-id",
            PROJECT_ID,
            "--name",
            name,
            "--trust",
        ],
    )
    .assert_ok();
    let config: toml::Table = toml::from_str(&ws.read(".nexus/config.toml"))
        .unwrap_or_else(|e| panic!("config.toml does not parse: {e}"));
    assert_eq!(config["project"]["id"].as_str(), Some(PROJECT_ID));
    assert!(config.get("id").is_none());
}
