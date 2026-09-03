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
            {"project_id": "fdc7a78c-d0b9-46fd-8206-9fc57301de2d", "role": "owner"}
        ],
        "agentAssignments": [
            {"project_id": "fdc7a78c-d0b9-46fd-8206-9fc57301de2d", "agent_id": "nexus-app-agent", "agent_owner": null}
        ]
    }"#;

    let identity: IdentityResponse = serde_json::from_str(json).unwrap();
    assert_eq!(identity.user_id, "83a37012-8add-428f-8bc3-d56c84291671");
    assert_eq!(identity.email, "patrick@example.com");
    assert_eq!(identity.display_name.as_deref(), Some("Patrick"));
    assert!(identity.is_platform_admin);
    assert!(identity.is_platform_owner);
    assert_eq!(
        identity.tenant_id.as_deref(),
        Some("20c72e35-d4d8-4e40-a7be-efff14d8eaff")
    );
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
    assert_eq!(
        resp.skills[0].command_slug.as_deref(),
        Some("nexus-git-commit")
    );
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

// ---------------------------------------------------------------------------
// Directive export types
// ---------------------------------------------------------------------------

#[test]
fn test_exported_directive_deserialize() {
    let json = r#"{
        "id": "0a0df02f-edc8-4f1a-a1f7-43be3cbca2f9",
        "title": "Use HTTPS everywhere",
        "body": "All production endpoints must use HTTPS.",
        "category": "security",
        "priority": "high"
    }"#;

    let d: ExportedDirective = serde_json::from_str(json).unwrap();
    assert_eq!(d.id, "0a0df02f-edc8-4f1a-a1f7-43be3cbca2f9");
    assert_eq!(d.title, "Use HTTPS everywhere");
    assert_eq!(
        d.body.as_deref(),
        Some("All production endpoints must use HTTPS.")
    );
    assert_eq!(d.category, "security");
    assert_eq!(d.priority, "high");
}

#[test]
fn test_exported_directive_null_body() {
    let json = r#"{
        "id": "abc-123",
        "title": "Simple rule",
        "body": null,
        "category": "general",
        "priority": "medium"
    }"#;

    let d: ExportedDirective = serde_json::from_str(json).unwrap();
    assert!(d.body.is_none());
    assert_eq!(d.category, "general");
    assert_eq!(d.priority, "medium");
}

#[test]
fn test_directive_export_response_deserialize() {
    let json = r#"{
        "action": "directive_export",
        "project": {
            "id": "fdc7a78c-d0b9-46fd-8206-9fc57301de2d",
            "slug": "nexus-app",
            "name": "NEXUS-APP"
        },
        "directives": [
            {
                "id": "d1",
                "title": "Green healthcheck",
                "body": null,
                "category": "deployment",
                "priority": "high"
            },
            {
                "id": "d2",
                "title": "Run migrations locally",
                "body": "Use makefile targets.",
                "category": "migration",
                "priority": "medium"
            }
        ],
        "count": 2
    }"#;

    let resp: DirectiveExportResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.action, "directive_export");
    assert_eq!(resp.project.slug, "nexus-app");
    assert_eq!(resp.project.name, "NEXUS-APP");
    assert_eq!(resp.count, 2);
    assert_eq!(resp.directives.len(), 2);

    assert_eq!(resp.directives[0].title, "Green healthcheck");
    assert_eq!(resp.directives[0].priority, "high");
    assert!(resp.directives[0].body.is_none());

    assert_eq!(resp.directives[1].title, "Run migrations locally");
    assert_eq!(
        resp.directives[1].body.as_deref(),
        Some("Use makefile targets.")
    );
}

#[test]
fn test_directive_export_response_empty_directives() {
    let json = r#"{
        "action": "directive_export",
        "project": {
            "id": "abc",
            "slug": "test",
            "name": "Test"
        },
        "directives": [],
        "count": 0
    }"#;

    let resp: DirectiveExportResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.count, 0);
    assert!(resp.directives.is_empty());
}

#[test]
fn test_exported_directive_serialize_roundtrip() {
    let d = ExportedDirective {
        id: "test-id".into(),
        title: "Test".into(),
        body: Some("Body text".into()),
        category: "general".into(),
        priority: "low".into(),
    };

    let json = serde_json::to_string(&d).unwrap();
    let d2: ExportedDirective = serde_json::from_str(&json).unwrap();
    assert_eq!(d.id, d2.id);
    assert_eq!(d.title, d2.title);
    assert_eq!(d.body, d2.body);
    assert_eq!(d.category, d2.category);
    assert_eq!(d.priority, d2.priority);
}

// ---------------------------------------------------------------------------
// Agent file export types
// ---------------------------------------------------------------------------

#[test]
fn test_exported_agent_file_deserialize() {
    let json = r##"{
        "file_key": "agents-md",
        "target_path": "AGENTS.md",
        "name": "AGENTS.md",
        "description": "Agent role definitions",
        "category": "agent",
        "version": 1,
        "body": "---\ntype: agent-policy\n---\n# Test"
    }"##;

    let af: ExportedAgentFile = serde_json::from_str(json).unwrap();
    assert_eq!(af.file_key, "agents-md");
    assert_eq!(af.target_path, "AGENTS.md");
    assert_eq!(af.name, "AGENTS.md");
    assert_eq!(af.description.as_deref(), Some("Agent role definitions"));
    assert_eq!(af.category, "agent");
    assert_eq!(af.version, 1);
    assert!(af.body.contains("agent-policy"));
}

#[test]
fn test_exported_agent_file_null_description() {
    let json = r##"{
        "file_key": "claude-md",
        "target_path": ".claude/CLAUDE.md",
        "name": "CLAUDE.md",
        "description": null,
        "category": "agent",
        "version": 2,
        "body": "# Bootstrap"
    }"##;

    let af: ExportedAgentFile = serde_json::from_str(json).unwrap();
    assert!(af.description.is_none());
    assert_eq!(af.version, 2);
}

#[test]
fn test_agent_file_export_response_deserialize() {
    let json = r##"{
        "project_id": "fdc7a78c-d0b9-46fd-8206-9fc57301de2d",
        "project_name": "NEXUS-APP",
        "agent_files": [
            {
                "file_key": "agents-md",
                "target_path": "AGENTS.md",
                "name": "AGENTS.md",
                "description": null,
                "category": "agent",
                "version": 1,
                "body": "# Agents"
            },
            {
                "file_key": "claude-md",
                "target_path": ".claude/CLAUDE.md",
                "name": "CLAUDE.md",
                "description": "Bootstrap file",
                "category": "agent",
                "version": 3,
                "body": "# Bootstrap"
            }
        ],
        "count": 2
    }"##;

    let resp: AgentFileExportResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.project_id, "fdc7a78c-d0b9-46fd-8206-9fc57301de2d");
    assert_eq!(resp.project_name, "NEXUS-APP");
    assert_eq!(resp.count, 2);
    assert_eq!(resp.agent_files.len(), 2);
    assert_eq!(resp.agent_files[0].file_key, "agents-md");
    assert_eq!(resp.agent_files[1].file_key, "claude-md");
    assert_eq!(resp.agent_files[1].version, 3);
}

#[test]
fn test_agent_file_export_response_empty() {
    let json = r#"{
        "project_id": "abc",
        "project_name": "Test",
        "agent_files": [],
        "count": 0
    }"#;

    let resp: AgentFileExportResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.count, 0);
    assert!(resp.agent_files.is_empty());
}

#[test]
fn test_exported_agent_file_serialize_roundtrip() {
    let af = ExportedAgentFile {
        file_key: "cursorrules".into(),
        target_path: ".cursorrules".into(),
        name: ".cursorrules".into(),
        description: Some("Cursor IDE rules".into()),
        category: "ide".into(),
        version: 1,
        body: "# Cursor Rules\nFollow these rules.".into(),
        content_hash: None,
        agent_file_id: None,
    };

    let json = serde_json::to_string(&af).unwrap();
    let af2: ExportedAgentFile = serde_json::from_str(&json).unwrap();
    assert_eq!(af.file_key, af2.file_key);
    assert_eq!(af.target_path, af2.target_path);
    assert_eq!(af.body, af2.body);
    assert_eq!(af.description, af2.description);
    assert_eq!(af.category, af2.category);
    assert_eq!(af.version, af2.version);
}

// ═══════════════════════════════════════════════════════════════════════
// ProviderConfig tests (opaque JSON pass-through)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_provider_config_deserialize_opencode_format() {
    let json = r#"{
        "npm": "@ai-sdk/openai-compatible",
        "name": "DGX Spark (HomeLab)",
        "options": {
            "baseURL": "http://10.0.10.121/v1"
        },
        "models": {
            "nexus-coder-main": {
                "name": "Nexus Coder Main"
            }
        }
    }"#;

    let cfg: ProviderConfig = serde_json::from_str(json).unwrap();
    assert_eq!(cfg["npm"], "@ai-sdk/openai-compatible");
    assert_eq!(cfg["name"], "DGX Spark (HomeLab)");
    assert_eq!(cfg["options"]["baseURL"], "http://10.0.10.121/v1");
    assert_eq!(
        cfg["models"]["nexus-coder-main"]["name"],
        "Nexus Coder Main"
    );
}

#[test]
fn test_provider_config_serialize_roundtrip() {
    let cfg: ProviderConfig = serde_json::json!({
        "npm": "@ai-sdk/openai-compatible",
        "name": "DGX Spark (HomeLab)",
        "options": {
            "baseURL": "http://10.0.10.121/v1"
        },
        "models": {
            "nexus-coder-main": {
                "name": "Nexus Coder Main"
            }
        }
    });

    let json = serde_json::to_string(&cfg).unwrap();
    let cfg2: ProviderConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(cfg, cfg2);
}

#[test]
fn test_agent_file_export_response_with_provider() {
    let json = r##"{
        "project_id": "fdc7a78c-d0b9-46fd-8206-9fc57301de2d",
        "project_name": "NEXUS-APP",
        "agent_files": [],
        "count": 0,
        "provider": {
            "dgx-spark": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "DGX Spark (HomeLab)",
                "options": {
                    "baseURL": "http://10.0.10.121/v1"
                },
                "models": {
                    "nexus-coder-main": {
                        "name": "Nexus Coder Main"
                    }
                }
            }
        }
    }"##;

    let resp: AgentFileExportResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.provider.len(), 1);
    let spark = resp.provider.get("dgx-spark").unwrap();
    assert_eq!(spark["npm"], "@ai-sdk/openai-compatible");
    assert_eq!(spark["options"]["baseURL"], "http://10.0.10.121/v1");
}

#[test]
fn test_agent_file_export_response_providers_alias() {
    // The old API key "providers" must still work via serde alias
    let json = r##"{
        "project_id": "abc",
        "project_name": "Test",
        "agent_files": [],
        "count": 0,
        "providers": {
            "dgx-spark": {
                "npm": "@ai-sdk/openai-compatible"
            }
        }
    }"##;

    let resp: AgentFileExportResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.provider.len(), 1);
}

#[test]
fn test_agent_file_export_response_providers_default_empty() {
    let json = r#"{
        "project_id": "abc",
        "project_name": "Test",
        "agent_files": [],
        "count": 0
    }"#;

    let resp: AgentFileExportResponse = serde_json::from_str(json).unwrap();
    assert!(resp.provider.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════
// auth_token + prerequisites (v0.7.1)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_agent_file_export_response_auth_token_present() {
    let json = r#"{
        "project_id": "abc",
        "project_name": "Test",
        "agent_files": [],
        "count": 0,
        "auth_token": "nxs_pat_testtoken123"
    }"#;

    let resp: AgentFileExportResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.auth_token, Some("nxs_pat_testtoken123".to_string()));
}

#[test]
fn test_agent_file_export_response_auth_token_absent_defaults_none() {
    let json = r#"{
        "project_id": "abc",
        "project_name": "Test",
        "agent_files": [],
        "count": 0
    }"#;

    let resp: AgentFileExportResponse = serde_json::from_str(json).unwrap();
    assert!(resp.auth_token.is_none());
}

#[test]
fn test_agent_file_export_response_prerequisites_present() {
    let json = r#"{
        "project_id": "abc",
        "project_name": "Test",
        "agent_files": [],
        "count": 0,
        "plugins": ["rtk", "headroom"],
        "prerequisites": [
            {
                "tool": "rtk",
                "check_command": "rtk --version",
                "install_hint": "Install RTK: https://www.rtk-ai.app/#install",
                "required_by": "RTK output filtering (codebase: rust, docker)"
            },
            {
                "tool": "headroom",
                "check_command": "headroom --version",
                "install_hint": "pip install headroom-ai[mcp]",
                "required_by": "Headroom context compression MCP"
            }
        ]
    }"#;

    let resp: AgentFileExportResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.prerequisites.len(), 2);

    let rtk = &resp.prerequisites[0];
    assert_eq!(rtk.tool, "rtk");
    assert_eq!(rtk.check_command, "rtk --version");
    assert!(rtk.install_hint.contains("rtk-ai.app"));
    assert!(rtk.required_by.contains("rust"));

    let headroom = &resp.prerequisites[1];
    assert_eq!(headroom.tool, "headroom");
    assert_eq!(headroom.check_command, "headroom --version");
}

#[test]
fn test_agent_file_export_response_prerequisites_absent_defaults_empty() {
    let json = r#"{
        "project_id": "abc",
        "project_name": "Test",
        "agent_files": [],
        "count": 0
    }"#;

    let resp: AgentFileExportResponse = serde_json::from_str(json).unwrap();
    assert!(resp.prerequisites.is_empty());
}

#[test]
fn test_prerequisite_serialize_roundtrip() {
    let prereq = Prerequisite {
        tool: "rtk".to_string(),
        check_command: "rtk --version".to_string(),
        install_hint: "Install RTK: https://www.rtk-ai.app/#install".to_string(),
        required_by: "RTK output filtering".to_string(),
    };

    let json = serde_json::to_string(&prereq).unwrap();
    let prereq2: Prerequisite = serde_json::from_str(&json).unwrap();
    assert_eq!(prereq.tool, prereq2.tool);
    assert_eq!(prereq.check_command, prereq2.check_command);
    assert_eq!(prereq.install_hint, prereq2.install_hint);
    assert_eq!(prereq.required_by, prereq2.required_by);
}

// ---------------------------------------------------------------------------
// WorkspacePushResponse deserialization (v0.14.0)
// ---------------------------------------------------------------------------

#[test]
fn test_workspace_push_response_deserialize() {
    let json = r#"{
        "action": "ws_push",
        "project_id": "fdc7a78c-d0b9-46fd-8206-9fc57301de2d",
        "fork_id": "aaaa-bbbb-cccc",
        "fork_name": "push-2026-08-23T10:30:00Z",
        "version": 1,
        "previous_fork_id": "dddd-eeee-ffff",
        "previous_fork_name": "default",
        "files_pushed": ["devbox.json", "scripts/devbox/dbx_init.sh"]
    }"#;

    let resp: WorkspacePushResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.action, "ws_push");
    assert_eq!(resp.project_id, "fdc7a78c-d0b9-46fd-8206-9fc57301de2d");
    assert_eq!(resp.fork_name, "push-2026-08-23T10:30:00Z");
    assert_eq!(resp.version, 1);
    assert_eq!(resp.previous_fork_name, "default");
    assert_eq!(resp.files_pushed.len(), 2);
    assert_eq!(resp.files_pushed[0], "devbox.json");
}

#[test]
fn test_workspace_push_response_roundtrip() {
    let resp = WorkspacePushResponse {
        action: "ws_push".to_string(),
        project_id: "test-id".to_string(),
        fork_id: "fork-1".to_string(),
        fork_name: "my-fork".to_string(),
        version: 3,
        previous_fork_id: "fork-0".to_string(),
        previous_fork_name: "old-fork".to_string(),
        files_pushed: vec!["devbox.json".to_string()],
    };

    let json = serde_json::to_string(&resp).unwrap();
    let resp2: WorkspacePushResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(resp.fork_name, resp2.fork_name);
    assert_eq!(resp.version, resp2.version);
    assert_eq!(resp.files_pushed, resp2.files_pushed);
}

// ---------------------------------------------------------------------------
// FileStatusResponse deserialization (v0.14.0)
// ---------------------------------------------------------------------------

#[test]
fn test_file_status_response_deserialize_full() {
    let json = r#"{
        "action": "af_status",
        "project_id": "test-proj",
        "modified": [{
            "path": "devbox.json",
            "local_hash": "abc123",
            "remote_hash": "def456",
            "category": "workspace"
        }],
        "new_local": [{"path": "scripts/devbox/custom.sh"}],
        "deleted_local": [{
            "path": "scripts/devbox/old.sh",
            "remote_hash": "ghi789",
            "category": "workspace"
        }],
        "unchanged": [{
            "path": ".nexus/skills/nx-init/SKILL.md",
            "category": "skill"
        }],
        "server_file_count": 5
    }"#;

    let resp: FileStatusResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.action, "af_status");
    assert_eq!(resp.modified.len(), 1);
    assert_eq!(resp.modified[0].path, "devbox.json");
    assert_eq!(resp.modified[0].category, "workspace");
    assert_eq!(resp.new_local.len(), 1);
    assert_eq!(resp.new_local[0].path, "scripts/devbox/custom.sh");
    assert_eq!(resp.deleted_local.len(), 1);
    assert_eq!(resp.unchanged.len(), 1);
    assert_eq!(resp.server_file_count, 5);
}

#[test]
fn test_file_status_response_deserialize_empty() {
    let json = r#"{
        "action": "af_status",
        "project_id": "test-proj",
        "modified": [],
        "new_local": [],
        "deleted_local": [],
        "unchanged": []
    }"#;

    let resp: FileStatusResponse = serde_json::from_str(json).unwrap();
    assert!(resp.modified.is_empty());
    assert!(resp.new_local.is_empty());
    assert!(resp.deleted_local.is_empty());
    assert!(resp.unchanged.is_empty());
    assert_eq!(resp.server_file_count, 0); // default
}

// -- Project inference tokens (nxs_proj_*) ----------------------------------

#[test]
fn test_inference_token_response_deserialize() {
    let json = r#"{
        "token": "nxs_proj_abcdefghijklmnop",
        "token_id": "11111111-2222-3333-4444-555555555555",
        "token_prefix": "nxs_proj_abcd",
        "runtime_id": "developer-workstation",
        "expires_at": "2027-01-01T00:00:00Z",
        "warning": "no expiry recommended"
    }"#;
    let resp: InferenceTokenResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.token, "nxs_proj_abcdefghijklmnop");
    assert_eq!(resp.token_id, "11111111-2222-3333-4444-555555555555");
    assert_eq!(resp.token_prefix.as_deref(), Some("nxs_proj_abcd"));
    assert_eq!(resp.runtime_id.as_deref(), Some("developer-workstation"));
    assert_eq!(resp.expires_at.as_deref(), Some("2027-01-01T00:00:00Z"));
    assert_eq!(resp.warning.as_deref(), Some("no expiry recommended"));
}

#[test]
fn test_inference_token_response_minimal() {
    // Only the mandatory fields are present.
    let json = r#"{ "token": "nxs_proj_x", "token_id": "abc" }"#;
    let resp: InferenceTokenResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.token, "nxs_proj_x");
    assert!(resp.token_prefix.is_none());
    assert!(resp.expires_at.is_none());
    assert!(resp.warning.is_none());
}

#[test]
fn test_inference_token_list_deserialize() {
    let json = r#"{
        "tokens": [
            {
                "token_id": "t1",
                "token_prefix": "nxs_proj_aaaa",
                "runtime_id": "ci-production",
                "status": "active",
                "created_at": "2026-09-01T00:00:00Z",
                "last_used_at": "2026-09-02T00:00:00Z",
                "expires_at": null
            }
        ]
    }"#;
    let resp: InferenceTokenListResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.tokens.len(), 1);
    let t = &resp.tokens[0];
    assert_eq!(t.token_id, "t1");
    assert_eq!(t.status.as_deref(), Some("active"));
    assert!(t.expires_at.is_none());
}

#[test]
fn test_inference_token_issue_request_skips_none() {
    let req = InferenceTokenIssueRequest {
        runtime_id: "developer-workstation".to_string(),
        capabilities: None,
        profile_ceiling: None,
        budget_ceiling_ref: None,
        expires_at: None,
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(json.contains("\"runtime_id\":\"developer-workstation\""));
    assert!(!json.contains("capabilities"));
    assert!(!json.contains("profile_ceiling"));
    assert!(!json.contains("budget_ceiling_ref"));
    assert!(!json.contains("expires_at"));
}

#[test]
fn test_inference_token_issue_request_with_ceiling() {
    let req = InferenceTokenIssueRequest {
        runtime_id: "ci-production".to_string(),
        capabilities: None,
        profile_ceiling: Some(ProfileCeiling {
            mode: "restrict".to_string(),
            profiles: vec!["coding".to_string(), "reasoning".to_string()],
        }),
        budget_ceiling_ref: None,
        expires_at: Some("2027-01-01T00:00:00Z".to_string()),
    };
    let value: serde_json::Value = serde_json::to_value(&req).unwrap();
    assert_eq!(value["profile_ceiling"]["mode"], "restrict");
    assert_eq!(value["profile_ceiling"]["profiles"][0], "coding");
    assert_eq!(value["expires_at"], "2027-01-01T00:00:00Z");
}
