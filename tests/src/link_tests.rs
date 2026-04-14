//! Tests for the `nexus link` / `nexus unlink` project-config logic.
//!
//! These tests exercise the `nexus_core::config` functions that underpin
//! `nexus link` and `nexus unlink`: saving, loading, removing, and resolving
//! project information from `.nexus/config.toml`.

use nexus_core::config::{
    load_project_config, remove_project_section, resolve_project_id, save_project_config,
    ProjectConfig, ProjectInfo,
};
use std::fs;
use std::path::PathBuf;

/// Helper: create a unique temp directory for each test.
fn temp_dir(suffix: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("nexus-link-test-{}-{}", std::process::id(), suffix));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// save / load roundtrip
// ---------------------------------------------------------------------------

#[test]
fn test_save_and_load_project_config() {
    let dir = temp_dir("save-load");

    let config = ProjectConfig {
        project: Some(ProjectInfo {
            id: "fdc7a78c-d0b9-46fd-8206-9fc57301de2d".to_string(),
            name: "My Nexus Project".to_string(),
            slug: "my-nexus-project".to_string(),
        }),
        mcp: None,
    };

    // Save
    save_project_config(Some(&dir), &config).unwrap();

    // File should exist
    let config_path = dir.join(".nexus/config.toml");
    assert!(config_path.exists(), ".nexus/config.toml must be created");

    // Load back
    let loaded = load_project_config(Some(&dir))
        .unwrap()
        .expect("config should be Some");

    let project = loaded.project.expect("project section should be present");
    assert_eq!(project.id, "fdc7a78c-d0b9-46fd-8206-9fc57301de2d");
    assert_eq!(project.name, "My Nexus Project");
    assert_eq!(project.slug, "my-nexus-project");

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_save_project_config_creates_nexus_dir() {
    let dir = temp_dir("creates-dir");

    // .nexus/ does not exist yet
    assert!(!dir.join(".nexus").exists());

    let config = ProjectConfig {
        project: Some(ProjectInfo {
            id: "abc".to_string(),
            name: "Test".to_string(),
            slug: "test".to_string(),
        }),
        mcp: None,
    };

    save_project_config(Some(&dir), &config).unwrap();
    assert!(dir.join(".nexus").is_dir());
    assert!(dir.join(".nexus/config.toml").exists());

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_load_project_config_returns_none_when_missing() {
    let dir = temp_dir("load-missing");

    let result = load_project_config(Some(&dir)).unwrap();
    assert!(
        result.is_none(),
        "should return None when file does not exist"
    );

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// remove_project_section
// ---------------------------------------------------------------------------

#[test]
fn test_remove_project_section() {
    let dir = temp_dir("remove-section");

    // First, save a config with project info
    let config = ProjectConfig {
        project: Some(ProjectInfo {
            id: "to-be-removed".to_string(),
            name: "Removal Target".to_string(),
            slug: "removal-target".to_string(),
        }),
        mcp: None,
    };
    save_project_config(Some(&dir), &config).unwrap();

    // Remove the project section
    let removed = remove_project_section(Some(&dir)).unwrap();
    assert!(removed, "should return true when project was removed");

    // Load again — project should be gone
    let loaded = load_project_config(Some(&dir))
        .unwrap()
        .expect("file should still exist");
    assert!(
        loaded.project.is_none(),
        "project section should be None after removal"
    );

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_remove_project_section_returns_false_when_no_project() {
    let dir = temp_dir("remove-no-project");

    // Save a config WITHOUT a project section
    let config = ProjectConfig {
        project: None,
        mcp: None,
    };
    save_project_config(Some(&dir), &config).unwrap();

    let removed = remove_project_section(Some(&dir)).unwrap();
    assert!(
        !removed,
        "should return false when no project section existed"
    );

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_remove_project_section_returns_false_when_no_file() {
    let dir = temp_dir("remove-no-file");

    // No .nexus/config.toml at all
    let removed = remove_project_section(Some(&dir)).unwrap();
    assert!(
        !removed,
        "should return false when config file does not exist"
    );

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// resolve_project_id
// ---------------------------------------------------------------------------

#[test]
fn test_resolve_project_id_from_config() {
    let dir = temp_dir("resolve-config");

    // Write a project config
    let config = ProjectConfig {
        project: Some(ProjectInfo {
            id: "fdc7a78c-d0b9-46fd-8206-9fc57301de2d".to_string(),
            name: "Test Project".to_string(),
            slug: "test-project".to_string(),
        }),
        mcp: None,
    };
    save_project_config(Some(&dir), &config).unwrap();

    // Resolve without CLI override — should read from config
    let resolved = resolve_project_id(None, Some(&dir)).unwrap();
    assert_eq!(resolved, "fdc7a78c-d0b9-46fd-8206-9fc57301de2d");

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_resolve_project_id_with_flag_override() {
    let dir = temp_dir("resolve-flag");

    // Write a project config with one ID
    let config = ProjectConfig {
        project: Some(ProjectInfo {
            id: "config-id-111".to_string(),
            name: "Config Project".to_string(),
            slug: "config-proj".to_string(),
        }),
        mcp: None,
    };
    save_project_config(Some(&dir), &config).unwrap();

    // CLI flag should override config
    let resolved = resolve_project_id(Some("cli-id-999"), Some(&dir)).unwrap();
    assert_eq!(resolved, "cli-id-999");

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_resolve_project_id_errors_when_nothing_set() {
    let dir = temp_dir("resolve-nothing");

    // No config, no CLI flag
    let result = resolve_project_id(None, Some(&dir));
    assert!(
        result.is_err(),
        "should error when no project ID is available"
    );

    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("No project ID found"),
        "error should mention missing project ID, got: {}",
        err_msg
    );

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_resolve_project_id_skips_empty_cli_flag() {
    let dir = temp_dir("resolve-empty-flag");

    // Config has a real ID
    let config = ProjectConfig {
        project: Some(ProjectInfo {
            id: "real-id".to_string(),
            name: "Real".to_string(),
            slug: "real".to_string(),
        }),
        mcp: None,
    };
    save_project_config(Some(&dir), &config).unwrap();

    // Empty string CLI flag should fall through to config
    let resolved = resolve_project_id(Some(""), Some(&dir)).unwrap();
    assert_eq!(resolved, "real-id");

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_resolve_project_id_skips_empty_config_id() {
    let dir = temp_dir("resolve-empty-config");

    // Config has an empty ID
    let config = ProjectConfig {
        project: Some(ProjectInfo {
            id: "".to_string(),
            name: "Empty".to_string(),
            slug: "empty".to_string(),
        }),
        mcp: None,
    };
    save_project_config(Some(&dir), &config).unwrap();

    // No CLI flag, empty config ID — should error
    let result = resolve_project_id(None, Some(&dir));
    assert!(result.is_err(), "empty config ID should not resolve");

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Config file content verification
// ---------------------------------------------------------------------------

#[test]
fn test_project_config_toml_content() {
    let dir = temp_dir("toml-content");

    let config = ProjectConfig {
        project: Some(ProjectInfo {
            id: "uuid-123".to_string(),
            name: "Content Check".to_string(),
            slug: "content-check".to_string(),
        }),
        mcp: None,
    };
    save_project_config(Some(&dir), &config).unwrap();

    // Read raw TOML and verify structure
    let raw = fs::read_to_string(dir.join(".nexus/config.toml")).unwrap();
    assert!(
        raw.contains("[project]"),
        "should contain [project] section"
    );
    assert!(raw.contains("uuid-123"), "should contain the project ID");
    assert!(
        raw.contains("Content Check"),
        "should contain the project name"
    );

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_save_overwrites_existing_config() {
    let dir = temp_dir("overwrite");

    // Write first config
    let config1 = ProjectConfig {
        project: Some(ProjectInfo {
            id: "first-id".to_string(),
            name: "First".to_string(),
            slug: "first".to_string(),
        }),
        mcp: None,
    };
    save_project_config(Some(&dir), &config1).unwrap();

    // Overwrite with second config
    let config2 = ProjectConfig {
        project: Some(ProjectInfo {
            id: "second-id".to_string(),
            name: "Second".to_string(),
            slug: "second".to_string(),
        }),
        mcp: None,
    };
    save_project_config(Some(&dir), &config2).unwrap();

    // Should load the second config
    let loaded = load_project_config(Some(&dir))
        .unwrap()
        .expect("config should exist");
    let project = loaded.project.expect("project should exist");
    assert_eq!(project.id, "second-id");
    assert_eq!(project.name, "Second");

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}
