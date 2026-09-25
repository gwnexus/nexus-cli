//! `nexus env` backend contract (NEXUS-APP dispatch b5f7bfb0):
//! `GET`/`PATCH /api/mcp/projects/{id}/settings` against a local stub.

use nexus_core::api::{NexusClient, SettingsPatchOutcome};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// Serve exactly one HTTP response; returns the base URL and a receiver for
/// the raw request (request line, headers and body).
async fn stub(status: &'static str, body: &'static str) -> (String, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut req = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            req.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&req).to_string();
            if let Some(head_end) = text.find("\r\n\r\n") {
                let len = text[..head_end]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                if req.len() >= head_end + 4 + len {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        let resp = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(resp.as_bytes()).await.unwrap();
        let _ = tx.send(String::from_utf8_lossy(&req).to_string());
    });
    (format!("http://127.0.0.1:{}", addr.port()), rx)
}

const SETTINGS: &str = r#"{
  "project_id": "p1", "revision": "rev-1",
  "settings": {"executioner": "claude-cli", "claude.hud": "engineering", "git.gh.user": null},
  "run_target": {"tool": "claude", "workspace": "zellij", "layout": ".nexus/claude/nexus-claude.kdl"},
  "claude_experience_explicit": false, "can_write": true,
  "schema": [{"key": "claude.hud", "type": "enum", "values": ["off", "minimal", "engineering"], "description": "HUD"}],
  "changes": [{"key": "claude.hud", "from": "engineering", "to": "minimal"}],
  "dry_run": false
}"#;

fn set_hud() -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    m.insert("claude.hud".into(), serde_json::json!("minimal"));
    m
}

#[tokio::test]
async fn test_get_settings() {
    let (url, req) = stub("200 OK", SETTINGS).await;
    let client = NexusClient::new(&url, Some("nxs_pat_test".into())).unwrap();
    let resp = client.get_project_settings("p1").await.unwrap();
    assert_eq!(resp.revision, "rev-1");
    assert_eq!(resp.schema[0].key, "claude.hud");
    let req = req.await.unwrap();
    assert!(
        req.starts_with("GET /api/mcp/projects/p1/settings "),
        "{req}"
    );
    assert!(req
        .to_lowercase()
        .contains("authorization: bearer nxs_pat_test"));
}

#[tokio::test]
async fn test_patch_settings_applied_sends_contract_body() {
    let (url, req) = stub("200 OK", SETTINGS).await;
    let client = NexusClient::new(&url, Some("t".into())).unwrap();
    match client
        .patch_project_settings("p1", &set_hud(), true, Some("rev-1"))
        .await
        .unwrap()
    {
        SettingsPatchOutcome::Applied(resp) => {
            assert_eq!(resp.changes[0].to, serde_json::json!("minimal"));
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    let req = req.await.unwrap();
    assert!(
        req.starts_with("PATCH /api/mcp/projects/p1/settings "),
        "{req}"
    );
    let body: serde_json::Value =
        serde_json::from_str(req.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(
        body,
        serde_json::json!({
            "set": {"claude.hud": "minimal"}, "dry_run": true, "expected_revision": "rev-1"
        })
    );
}

#[tokio::test]
async fn test_patch_settings_400_details() {
    let (url, _) = stub(
        "400 Bad Request",
        r#"{"error": "Validation failed", "details": [{"field": "claude.hud", "message": "must be one of off, minimal"}]}"#,
    )
    .await;
    let client = NexusClient::new(&url, Some("t".into())).unwrap();
    match client
        .patch_project_settings("p1", &set_hud(), false, None)
        .await
        .unwrap()
    {
        SettingsPatchOutcome::Invalid { error, details } => {
            assert_eq!(error, "Validation failed");
            assert_eq!(
                details,
                vec![(
                    "claude.hud".to_string(),
                    "must be one of off, minimal".to_string()
                )]
            );
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[tokio::test]
async fn test_patch_settings_403() {
    let (url, _) = stub(
        "403 Forbidden",
        r#"{"error": "Changing project settings requires project admin rights"}"#,
    )
    .await;
    let client = NexusClient::new(&url, Some("t".into())).unwrap();
    match client
        .patch_project_settings("p1", &set_hud(), false, None)
        .await
        .unwrap()
    {
        SettingsPatchOutcome::Forbidden(msg) => assert!(msg.contains("admin rights")),
        other => panic!("expected Forbidden, got {other:?}"),
    }
}

#[tokio::test]
async fn test_patch_settings_409_revision() {
    let (url, _) = stub(
        "409 Conflict",
        r#"{"error": "Settings changed since they were read", "revision": "rev-2"}"#,
    )
    .await;
    let client = NexusClient::new(&url, Some("t".into())).unwrap();
    match client
        .patch_project_settings("p1", &set_hud(), false, Some("rev-1"))
        .await
        .unwrap()
    {
        SettingsPatchOutcome::Conflict { revision, .. } => {
            assert_eq!(revision.as_deref(), Some("rev-2"))
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
}
