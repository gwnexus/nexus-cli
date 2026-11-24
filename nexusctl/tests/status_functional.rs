//! Functional tests for `nexus status`: it reports what `nexus pull` would
//! do, with the same export fallbacks, and never writes.

mod common;
use common::*;
use serde_json::json;

/// The fork export fails (not "no fork"): the legacy workspace export is
/// what pull applies, so status compares against it.
#[test]
fn status_uses_the_legacy_workspace_export() {
    let (api, ws) = setup(&full_backend());
    api.route(
        "POST",
        "/api/mcp/agent-files#ws_export",
        500,
        json!({"error": "boom"}),
    );
    api.route(
        "POST",
        "/api/mcp/workspace-files#wf_export",
        200,
        json!({"action": "wf_export", "project_id": PROJECT_ID,
               "workspace": {"target_path": "devbox.json", "body": "{\"packages\": [\"v1\"]}\n",
                             "provider": "devbox"},
               "scripts": []}),
    );
    let before = snapshot(&ws);
    let run = ws.nexus(&api, &["status"]);
    assert!(
        run.all().contains("devbox.json"),
        "v1 file not reported:\n{}",
        run.all()
    );
    assert_eq!(snapshot(&ws), before, "status wrote");
}

/// A locally edited script whose backend version cannot be fetched is not
/// diffed against an empty file.
#[test]
fn status_diff_without_backend_version() {
    let (api, ws) = setup(&full_backend());
    ws.nexus(&api, &["pull", "--trust"]).assert_ok();
    ws.write("scripts/devbox/setup.sh", "#!/bin/sh\necho mine\n");
    api.route(
        "POST",
        "/api/mcp/agent-files#ws_export",
        500,
        json!({"error": "boom"}),
    );
    api.route(
        "POST",
        "/api/mcp/workspace-files#wf_export",
        500,
        json!({"error": "boom"}),
    );
    let run = ws.nexus(&api, &["diff", "scripts/devbox/setup.sh"]);
    assert!(
        run.all().contains("backend version unavailable"),
        "{}",
        run.all()
    );
    assert!(!run.all().contains("+echo mine"), "{}", run.all());
}
