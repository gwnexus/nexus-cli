//! Dispatch list contract: `dispatch_inbox` / `dispatch_outbox` against a local stub.

use nexus_core::api::{NexusClient, DISPATCH_LIST_LIMIT};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::sync_tests::stub;

#[tokio::test]
async fn test_list_dispatches_sends_filter_and_limit() {
    let (url, req) = stub(
        "200 OK",
        r#"{"action":"dispatch_inbox","project_id":"p1","count":1,"dispatches":[{"id":"d1","source_project_id":"p2","target_project_id":"p1","title":"t","type":"bug_report","priority":"high","status":"open","blocking":true,"created_at":"2026-11-06T12:39:21.306961+00:00","updated_at":"2026-11-06T12:40:00+00:00"}],"letters":[]}"#,
    )
    .await;
    let client = NexusClient::new(&url, Some("nxs_pat_test".into())).unwrap();
    let resp = client
        .list_dispatches("dispatch_inbox", "p1", &["open", "blocked"], None)
        .await
        .unwrap();
    let raw = req.await.unwrap();
    assert!(raw.starts_with("POST /api/mcp/dispatches "));
    let body: serde_json::Value =
        serde_json::from_str(&raw[raw.find("\r\n\r\n").unwrap() + 4..]).unwrap();
    assert_eq!(body["action"], "dispatch_inbox");
    assert_eq!(body["project_id"], "p1");
    assert_eq!(
        body["status_filter"],
        serde_json::json!(["open", "blocked"])
    );
    assert_eq!(body["limit"], DISPATCH_LIST_LIMIT);
    // Without a scope the parameter is left out, so older hubs see no change.
    assert!(body.get("project_scope").is_none());
    assert!(resp.projects.is_empty());
    let d = &resp.dispatches[0];
    assert_eq!(
        (d.kind.as_str(), d.status.as_str(), d.blocking),
        ("bug_report", "open", true)
    );
    assert_eq!(d.updated_at.as_deref(), Some("2026-11-06T12:40:00+00:00"));
    assert!(d.resolved_at.is_none());
}

/// A connection the server closes without answering (as with an idle
/// connection it dropped) is retried once on a new connection.
#[tokio::test]
async fn test_list_dispatches_retries_a_dropped_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        let (mut first, _) = listener.accept().await.unwrap();
        let _ = first.read(&mut buf).await;
        drop(first);
        let (mut second, _) = listener.accept().await.unwrap();
        let _ = second.read(&mut buf).await;
        let body = r#"{"action":"dispatch_inbox","project_id":"p1","count":0,"dispatches":[]}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        second.write_all(resp.as_bytes()).await.unwrap();
    });
    let client = NexusClient::new(
        &format!("http://127.0.0.1:{}", addr.port()),
        Some("nxs_pat_test".into()),
    )
    .unwrap();
    let resp = client
        .list_dispatches("dispatch_inbox", "p1", &["open"], None)
        .await
        .unwrap();
    assert!(resp.dispatches.is_empty());
}

#[tokio::test]
async fn test_list_dispatches_linked_scope() {
    let (url, req) = stub(
        "200 OK",
        r#"{"action":"dispatch_outbox","project_id":"p1","project_scope":"linked","projects":["p1","p2"],"count":0,"dispatches":[]}"#,
    )
    .await;
    let client = NexusClient::new(&url, Some("nxs_pat_test".into())).unwrap();
    let resp = client
        .list_dispatches("dispatch_outbox", "p1", &["open"], Some("linked"))
        .await
        .unwrap();
    let raw = req.await.unwrap();
    let body: serde_json::Value =
        serde_json::from_str(&raw[raw.find("\r\n\r\n").unwrap() + 4..]).unwrap();
    assert_eq!(body["project_scope"], "linked");
    assert_eq!(resp.projects, ["p1", "p2"]);
}
