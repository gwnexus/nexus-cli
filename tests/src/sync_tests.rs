//! Agent-file sync contract: `af_sync` push against a local stub.

use nexus_core::api::NexusClient;
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

#[tokio::test]
async fn test_sync_push_sends_local_body_and_reads_content_hash() {
    let (url, req) = stub(
        "200 OK",
        r#"{"file_key": ".cursorrules", "sync_status": "synced", "content_hash": "abc", "direction": "push"}"#,
    )
    .await;
    let client = NexusClient::new(&url, Some("nxs_pat_test".into())).unwrap();
    let resp = client
        .sync_file("p1", ".cursorrules", "push", Some("rules\n"), Some("abc"))
        .await
        .unwrap();
    assert_eq!(resp.new_hash.as_deref(), Some("abc"));
    assert_eq!(resp.sync_status.as_deref(), Some("synced"));
    let req = req.await.unwrap();
    let body: serde_json::Value =
        serde_json::from_str(&req[req.find("\r\n\r\n").unwrap() + 4..]).unwrap();
    assert_eq!(body["action"], "af_sync");
    assert_eq!(body["direction"], "push");
    assert_eq!(body["local_body"], "rules\n");
    assert_eq!(body["local_hash"], "abc");
    assert!(body.get("body").is_none());
}

#[tokio::test]
async fn test_sync_push_rejection_is_an_error() {
    let (url, _) = stub(
        "400 Bad Request",
        r#"{"error": "local_body is required for push"}"#,
    )
    .await;
    let client = NexusClient::new(&url, Some("nxs_pat_test".into())).unwrap();
    let err = client
        .sync_file("p1", ".cursorrules", "push", Some("x"), None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("local_body is required"), "{err}");
}
