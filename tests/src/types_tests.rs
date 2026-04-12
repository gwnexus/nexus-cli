//! Tests for nexus_core::api::types module.

use nexus_core::api::types::*;

#[test]
fn test_auth_status_deserialize() {
    let json = r#"{
        "user_id": "83a37012-8add-428f-8bc3-d56c84291671",
        "email": "test@example.com",
        "display_name": "Test User",
        "platform_role": "platform_admin"
    }"#;

    let status: AuthStatus = serde_json::from_str(json).unwrap();
    assert_eq!(status.user_id, "83a37012-8add-428f-8bc3-d56c84291671");
    assert_eq!(status.email, "test@example.com");
    assert_eq!(status.display_name.as_deref(), Some("Test User"));
    assert_eq!(status.platform_role, "platform_admin");
}

#[test]
fn test_auth_status_without_display_name() {
    let json = r#"{
        "user_id": "test-id",
        "email": "test@example.com",
        "display_name": null,
        "platform_role": "viewer"
    }"#;

    let status: AuthStatus = serde_json::from_str(json).unwrap();
    assert!(status.display_name.is_none());
}

#[test]
fn test_auth_status_response_wrapper() {
    let json = r#"{
        "user": {
            "user_id": "test-id",
            "email": "test@example.com",
            "display_name": null,
            "platform_role": "editor"
        }
    }"#;

    let resp: AuthStatusResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.user.email, "test@example.com");
    assert_eq!(resp.user.platform_role, "editor");
}

#[test]
fn test_project_summary_deserialize() {
    let json = r#"{
        "id": "proj-id",
        "name": "Test Project",
        "description": "A test project",
        "status": "active",
        "created_at": "2026-01-01T00:00:00Z"
    }"#;

    let project: ProjectSummary = serde_json::from_str(json).unwrap();
    assert_eq!(project.id, "proj-id");
    assert_eq!(project.name, "Test Project");
    assert_eq!(project.description.as_deref(), Some("A test project"));
}

#[test]
fn test_project_summary_without_description() {
    let json = r#"{
        "id": "proj-id",
        "name": "Minimal",
        "description": null,
        "status": "active",
        "created_at": "2026-01-01T00:00:00Z"
    }"#;

    let project: ProjectSummary = serde_json::from_str(json).unwrap();
    assert!(project.description.is_none());
}

#[test]
fn test_project_list_response() {
    let json = r#"{
        "projects": [
            {
                "id": "p1",
                "name": "Project 1",
                "description": null,
                "status": "active",
                "created_at": "2026-01-01T00:00:00Z"
            },
            {
                "id": "p2",
                "name": "Project 2",
                "description": "Second project",
                "status": "archived",
                "created_at": "2026-02-01T00:00:00Z"
            }
        ]
    }"#;

    let resp: ProjectListResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.projects.len(), 2);
    assert_eq!(resp.projects[0].name, "Project 1");
    assert_eq!(resp.projects[1].status, "archived");
}

#[test]
fn test_api_error_display_with_message() {
    let err = ApiError {
        error: Some("bad_request".to_string()),
        message: Some("Missing required field".to_string()),
    };
    assert_eq!(err.to_string(), "Missing required field");
}

#[test]
fn test_api_error_display_with_error_only() {
    let err = ApiError {
        error: Some("internal_error".to_string()),
        message: None,
    };
    assert_eq!(err.to_string(), "internal_error");
}

#[test]
fn test_api_error_display_empty() {
    let err = ApiError {
        error: None,
        message: None,
    };
    assert_eq!(err.to_string(), "unknown API error");
}

#[test]
fn test_api_error_deserialize() {
    let json = r#"{"error": "not_found", "message": "Project does not exist"}"#;
    let err: ApiError = serde_json::from_str(json).unwrap();
    assert_eq!(err.error.as_deref(), Some("not_found"));
    assert_eq!(err.message.as_deref(), Some("Project does not exist"));
}

#[test]
fn test_auth_status_serialize_roundtrip() {
    let status = AuthStatus {
        user_id: "user-123".to_string(),
        email: "test@test.com".to_string(),
        display_name: Some("Test".to_string()),
        platform_role: "platform_admin".to_string(),
    };

    let json = serde_json::to_string(&status).unwrap();
    let deserialized: AuthStatus = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.user_id, status.user_id);
    assert_eq!(deserialized.email, status.email);
}
