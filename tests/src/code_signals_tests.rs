//! `nexus code-report` backend contract:
//! `POST /api/mcp/projects/{id}/code-signals` against a local stub.

use std::collections::BTreeMap;

use nexus_core::api::{CodeSignals, NexusClient};
use nexus_core::Error;
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

fn sample() -> CodeSignals {
    let mut extensions = BTreeMap::new();
    extensions.insert("rs".to_string(), 180);
    extensions.insert("toml".to_string(), 12);
    CodeSignals {
        source: "cli".into(),
        cli_version: "0.0.0".into(),
        scanned_at: "2026-01-01T00:00:00Z".into(),
        file_count: 192,
        empty: false,
        markers: vec!["Cargo.toml".into(), "crates/core/Cargo.toml".into()],
        extensions,
    }
}

#[tokio::test]
async fn test_report_code_signals_posts_the_payload() {
    let (url, req) = stub("200 OK", r#"{"stored": true, "suggestions": 2}"#).await;
    let client = NexusClient::new(&url, Some("nxs_pat_test".into())).unwrap();
    let resp = client.report_code_signals("p1", &sample()).await.unwrap();
    assert!(resp.stored);
    assert_eq!(resp.suggestions, 2);

    let req = req.await.unwrap();
    assert!(
        req.starts_with("POST /api/mcp/projects/p1/code-signals "),
        "{req}"
    );
    assert!(req
        .to_lowercase()
        .contains("authorization: bearer nxs_pat_test"));
    let body = &req[req.find("\r\n\r\n").unwrap() + 4..];
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(v["source"], "cli");
    assert_eq!(v["file_count"], 192);
    assert_eq!(v["empty"], false);
    assert_eq!(v["markers"][1], "crates/core/Cargo.toml");
    assert_eq!(v["extensions"]["rs"], 180);
    assert_eq!(v.as_object().unwrap().len(), 7, "only the agreed fields");
}

#[tokio::test]
async fn test_report_code_signals_tolerates_a_bare_answer() {
    let (url, _req) = stub("200 OK", "{}").await;
    let client = NexusClient::new(&url, Some("nxs_pat_test".into())).unwrap();
    let resp = client.report_code_signals("p1", &sample()).await.unwrap();
    assert_eq!(resp.suggestions, 0);
}

#[tokio::test]
async fn test_report_code_signals_on_an_older_hub_is_not_found() {
    let (url, _req) = stub("404 Not Found", r#"{"error": "not found"}"#).await;
    let client = NexusClient::new(&url, Some("nxs_pat_test".into())).unwrap();
    let err = client
        .report_code_signals("p1", &sample())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");
}
