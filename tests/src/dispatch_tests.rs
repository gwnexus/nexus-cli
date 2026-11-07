//! Dispatch list contract: `dispatch_inbox` / `dispatch_outbox` against a local stub.

use nexus_core::api::{NexusClient, DISPATCH_LIST_LIMIT};

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
        .list_dispatches("dispatch_inbox", "p1", &["open", "blocked"])
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
    let d = &resp.dispatches[0];
    assert_eq!(
        (d.kind.as_str(), d.status.as_str(), d.blocking),
        ("bug_report", "open", true)
    );
    assert_eq!(d.updated_at.as_deref(), Some("2026-11-06T12:40:00+00:00"));
    assert!(d.resolved_at.is_none());
}
