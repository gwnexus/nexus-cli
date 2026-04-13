//! Tests for nexus_core::api types module.

use nexus_core::api::*;

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

// ---------------------------------------------------------------------------
// New type tests: IdentityResponse, SkillExportResponse
// ---------------------------------------------------------------------------

#[test]
fn test_identity_response_deserialize() {
    let json = r#"{
        "userId": "83a37012-8add-428f-8bc3-d56c84291671",
        "email": "patrick@example.com",
        "displayName": "Patrick",
        "isPlatformAdmin": true,
        "isPlatformOwner": true,
        "tenantId": "20c72e35-d4d8-4e40-a7be-efff14d8eaff",
        "memberships": [
            {"projectId": "fdc7a78c-d0b9-46fd-8206-9fc57301de2d", "role": "owner"}
        ],
        "agentAssignments": [
            {"projectId": "fdc7a78c-d0b9-46fd-8206-9fc57301de2d", "agentId": "nexus-app-agent", "agentOwner": null}
        ]
    }"#;

    let identity: IdentityResponse = serde_json::from_str(json).unwrap();
    assert_eq!(identity.user_id, "83a37012-8add-428f-8bc3-d56c84291671");
    assert_eq!(identity.email, "patrick@example.com");
    assert_eq!(identity.display_name.as_deref(), Some("Patrick"));
    assert!(identity.is_platform_admin);
    assert!(identity.is_platform_owner);
    assert_eq!(identity.tenant_id.as_deref(), Some("20c72e35-d4d8-4e40-a7be-efff14d8eaff"));
    assert_eq!(identity.memberships.len(), 1);
    assert_eq!(identity.memberships[0].role, "owner");
    assert_eq!(identity.agent_assignments.len(), 1);
    assert_eq!(identity.agent_assignments[0].agent_id, "nexus-app-agent");
}

#[test]
fn test_identity_response_minimal() {
    let json = r#"{
        "userId": "test-id",
        "email": "test@test.com",
        "displayName": null,
        "isPlatformAdmin": false,
        "isPlatformOwner": false,
        "tenantId": null,
        "memberships": [],
        "agentAssignments": []
    }"#;

    let identity: IdentityResponse = serde_json::from_str(json).unwrap();
    assert!(!identity.is_platform_admin);
    assert!(identity.display_name.is_none());
    assert!(identity.memberships.is_empty());
}

#[test]
fn test_identity_to_auth_status_owner() {
    let identity = IdentityResponse {
        user_id: "u1".to_string(),
        email: "owner@example.com".to_string(),
        display_name: Some("Owner".to_string()),
        is_platform_admin: true,
        is_platform_owner: true,
        tenant_id: None,
        memberships: vec![],
        agent_assignments: vec![],
    };

    let status = AuthStatus::from(&identity);
    assert_eq!(status.platform_role, "platform_owner");
    assert_eq!(status.email, "owner@example.com");
}

#[test]
fn test_identity_to_auth_status_admin() {
    let identity = IdentityResponse {
        user_id: "u2".to_string(),
        email: "admin@example.com".to_string(),
        display_name: None,
        is_platform_admin: true,
        is_platform_owner: false,
        tenant_id: None,
        memberships: vec![],
        agent_assignments: vec![],
    };

    let status = AuthStatus::from(&identity);
    assert_eq!(status.platform_role, "platform_admin");
}

#[test]
fn test_identity_to_auth_status_member() {
    let identity = IdentityResponse {
        user_id: "u3".to_string(),
        email: "member@example.com".to_string(),
        display_name: None,
        is_platform_admin: false,
        is_platform_owner: false,
        tenant_id: None,
        memberships: vec![],
        agent_assignments: vec![],
    };

    let status = AuthStatus::from(&identity);
    assert_eq!(status.platform_role, "member");
}

#[test]
fn test_skill_export_response_deserialize() {
    let json = r#"{
        "action": "sk_export",
        "project": {
            "id": "fdc7a78c-d0b9-46fd-8206-9fc57301de2d",
            "slug": "nexus-app",
            "name": "NEXUS-APP"
        },
        "skills": [
            {
                "skill_id": "nx-git-commit",
                "name": "Git Commit",
                "description": "Commit helper skill",
                "version": 1,
                "body": "Follow commit rules...",
                "command_slug": "nexus-git-commit",
                "pinned": false
            },
            {
                "skill_id": "nx-session-close",
                "name": "Session Close",
                "description": null,
                "version": 2,
                "body": "Close the session...",
                "command_slug": "nexus-session-close",
                "pinned": true
            }
        ],
        "count": 2
    }"#;

    let resp: SkillExportResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.action, "sk_export");
    assert_eq!(resp.project.slug, "nexus-app");
    assert_eq!(resp.project.name, "NEXUS-APP");
    assert_eq!(resp.count, 2);
    assert_eq!(resp.skills.len(), 2);

    assert_eq!(resp.skills[0].skill_id, "nx-git-commit");
    assert_eq!(resp.skills[0].name, "Git Commit");
    assert_eq!(resp.skills[0].command_slug.as_deref(), Some("nexus-git-commit"));
    assert!(!resp.skills[0].pinned);

    assert_eq!(resp.skills[1].skill_id, "nx-session-close");
    assert_eq!(resp.skills[1].version, 2);
    assert!(resp.skills[1].pinned);
}

#[test]
fn test_exported_skill_without_body() {
    let json = r#"{
        "skill_id": "nx-empty",
        "name": "Empty Skill",
        "description": null,
        "version": 1,
        "body": null,
        "command_slug": null,
        "pinned": false
    }"#;

    let skill: ExportedSkill = serde_json::from_str(json).unwrap();
    assert!(skill.body.is_none());
    assert!(skill.command_slug.is_none());
    assert!(skill.description.is_none());
}
