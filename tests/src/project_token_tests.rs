//! Tests for project inference token storage and API types (`nxs_proj_*`).

use nexus_core::auth::{
    validate_project_token_format, ProjectTokenEntry, ProjectTokenStore, PROJECT_TOKEN_PREFIX,
};

fn sample_entry(token_id: &str) -> ProjectTokenEntry {
    ProjectTokenEntry {
        token: format!("{}deadbeefcafe0123", PROJECT_TOKEN_PREFIX),
        token_id: token_id.to_string(),
        token_prefix: Some("nxs_proj_dead".to_string()),
        runtime_id: "developer-workstation".to_string(),
        expires_at: None,
        created_at: Some("2026-09-03T00:00:00Z".to_string()),
        previous_token_id: None,
    }
}

#[test]
fn test_store_set_get_remove() {
    let mut store = ProjectTokenStore::default();
    let pid = "fdc7a78c-d0b9-46fd-8206-9fc57301de2d";
    assert!(store.get(pid).is_none());

    store.set(pid, sample_entry("tok-1"));
    assert_eq!(store.get(pid).unwrap().token_id, "tok-1");

    // Replace overwrites in place.
    store.set(pid, sample_entry("tok-2"));
    assert_eq!(store.get(pid).unwrap().token_id, "tok-2");

    let removed = store.remove(pid).expect("entry removed");
    assert_eq!(removed.token_id, "tok-2");
    assert!(store.get(pid).is_none());
}

#[test]
fn test_store_toml_roundtrip() {
    let mut store = ProjectTokenStore::default();
    store.set("proj-a", sample_entry("tok-a"));
    let mut rotated = sample_entry("tok-b");
    rotated.previous_token_id = Some("tok-a".to_string());
    rotated.expires_at = Some("2027-01-01T00:00:00Z".to_string());
    store.set("proj-b", rotated);

    let serialized = toml::to_string_pretty(&store).unwrap();
    let parsed: ProjectTokenStore = toml::from_str(&serialized).unwrap();

    assert_eq!(parsed.projects.len(), 2);
    assert_eq!(parsed.get("proj-a").unwrap().token_id, "tok-a");
    let b = parsed.get("proj-b").unwrap();
    assert_eq!(b.previous_token_id.as_deref(), Some("tok-a"));
    assert_eq!(b.expires_at.as_deref(), Some("2027-01-01T00:00:00Z"));
}

#[test]
fn test_store_debug_redacts_and_lists_projects() {
    let mut store = ProjectTokenStore::default();
    store.set("proj-a", sample_entry("tok-a"));
    let dbg = format!("{:?}", store);
    assert!(dbg.contains("proj-a"));
    assert!(!dbg.contains("deadbeefcafe"), "raw token must not leak");

    let entry_dbg = format!("{:?}", sample_entry("tok-a"));
    assert!(entry_dbg.contains("[REDACTED]"));
    assert!(!entry_dbg.contains("deadbeefcafe"));
}

#[test]
fn test_validate_project_token_format() {
    assert!(validate_project_token_format("nxs_proj_abcdefghij123").is_ok());
    assert!(validate_project_token_format("nxs_pat_abcdefghij").is_err());
    assert!(validate_project_token_format("nxs_proj_short").is_err());
}

#[test]
fn test_store_path_uses_config_dir() {
    let path = ProjectTokenStore::path().unwrap();
    let s = path.to_string_lossy();
    assert!(
        s.ends_with(".config/nexus/project-tokens.toml"),
        "got {}",
        s
    );
}
