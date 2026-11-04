//! `nexus classification` backend contract:
//! `GET`/`PATCH /api/mcp/projects/{id}/classification` against a local stub.

use nexus_core::api::{ClassificationPatchOutcome, NexusClient};
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

const CLASSIFICATION: &str = r#"{
  "project_level": "strictly-confidential",
  "customer": {"id": "c1", "name": "Acme", "level": "internal"},
  "effective_level": "strictly-confidential", "effective_source": "project",
  "gateway_classification": "restricted", "severity": "medium", "mapping_version": 1,
  "blocked_actions": ["textract_extraction"], "revision": "rev-1",
  "levels": [{"value": "confidential", "label": "Confidential", "group": "standard", "gateway_classification": "confidential"}]
}"#;

const PLAN: &str = r#"{
  "applied": false,
  "before": {"project_level": "strictly-confidential", "effective_level": "strictly-confidential", "gateway_classification": "restricted"},
  "after": {"project_level": "confidential", "effective_level": "confidential", "gateway_classification": "confidential"},
  "impact": {"gateway_classification_changed": true, "downgrade": true, "delegates": "rendered",
             "newly_allowed_actions": ["textract_extraction"], "newly_blocked_actions": []},
  "warnings": [], "revision": "rev-2"
}"#;

#[tokio::test]
async fn test_get_classification() {
    let (url, req) = stub("200 OK", CLASSIFICATION).await;
    let client = NexusClient::new(&url, Some("nxs_pat_test".into())).unwrap();
    let c = client.get_project_classification("p1").await.unwrap();
    assert_eq!(c.gateway_classification, "restricted");
    assert_eq!(c.customer.unwrap().level.as_deref(), Some("internal"));
    assert_eq!(
        c.levels[0].gateway_classification.as_deref(),
        Some("confidential")
    );
    let req = req.await.unwrap();
    assert!(
        req.starts_with("GET /api/mcp/projects/p1/classification "),
        "{req}"
    );
}

#[tokio::test]
async fn test_patch_classification_plan_sends_level_reason_revision() {
    let (url, req) = stub("200 OK", PLAN).await;
    let client = NexusClient::new(&url, Some("nxs_pat_test".into())).unwrap();
    let outcome = client
        .patch_project_classification("p1", "confidential", Some("lane test"), true, "rev-1")
        .await
        .unwrap();
    let ClassificationPatchOutcome::Done(plan) = outcome else {
        panic!("expected a plan, got {outcome:?}");
    };
    assert!(plan.impact.downgrade);
    assert_eq!(plan.revision.as_deref(), Some("rev-2"));
    let req = req.await.unwrap();
    assert!(req.starts_with("PATCH /api/mcp/projects/p1/classification "));
    let body: serde_json::Value =
        serde_json::from_str(&req[req.find("\r\n\r\n").unwrap() + 4..]).unwrap();
    assert_eq!(body["level"], "confidential");
    assert_eq!(body["reason"], "lane test");
    assert_eq!(body["dry_run"], true);
    assert_eq!(body["revision"], "rev-1");
}

#[tokio::test]
async fn test_patch_classification_conflict_and_reason_required() {
    let (url, _) = stub("409 Conflict", r#"{"error": "revision mismatch"}"#).await;
    let client = NexusClient::new(&url, Some("nxs_pat_test".into())).unwrap();
    assert_eq!(
        client
            .patch_project_classification("p1", "public", None, false, "rev-1")
            .await
            .unwrap(),
        ClassificationPatchOutcome::Conflict("revision mismatch".into())
    );
    let (url, _) = stub(
        "422 Unprocessable Entity",
        r#"{"error": "reason required"}"#,
    )
    .await;
    let client = NexusClient::new(&url, Some("nxs_pat_test".into())).unwrap();
    assert_eq!(
        client
            .patch_project_classification("p1", "public", None, false, "rev-1")
            .await
            .unwrap(),
        ClassificationPatchOutcome::ReasonRequired("reason required".into())
    );
    let (url, _) = stub("403 Forbidden", r#"{"error": "project.manage required"}"#).await;
    let client = NexusClient::new(&url, Some("nxs_pat_test".into())).unwrap();
    assert!(matches!(
        client
            .patch_project_classification("p1", "public", None, false, "rev-1")
            .await
            .unwrap(),
        ClassificationPatchOutcome::Forbidden(_)
    ));
}
