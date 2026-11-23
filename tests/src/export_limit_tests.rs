//! Export responses: the 8 MiB cap and the `executables` field.

use nexus_core::api::{NexusClient, MAX_RESPONSE_BYTES};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve one response; `chunked` sends the body without a Content-Length.
async fn stub(status: &'static str, body: String, chunked: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 65536];
        let mut req = Vec::new();
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            req.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&req);
            if let Some(end) = text.find("\r\n\r\n") {
                let len = text[..end]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                if req.len() >= end + 4 + len {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        let head = if chunked {
            format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")
        } else {
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
        };
        let _ = sock.write_all(head.as_bytes()).await;
        if chunked {
            for part in body.as_bytes().chunks(1 << 20) {
                let _ = sock
                    .write_all(format!("{:x}\r\n", part.len()).as_bytes())
                    .await;
                let _ = sock.write_all(part).await;
                let _ = sock.write_all(b"\r\n").await;
            }
            let _ = sock.write_all(b"0\r\n\r\n").await;
        } else {
            let _ = sock.write_all(body.as_bytes()).await;
        }
    });
    format!("http://127.0.0.1:{}", addr.port())
}

fn oversized() -> String {
    format!(
        r#"{{"action":"sk_export","project":{{"id":"p","slug":"p","name":"P"}},"skills":[],"count":0,"pad":"{}"}}"#,
        "x".repeat(MAX_RESPONSE_BYTES)
    )
}

#[tokio::test]
async fn test_export_over_cap_is_refused() {
    for chunked in [false, true] {
        let url = stub("200 OK", oversized(), chunked).await;
        let client = NexusClient::new(&url, Some("nxs_pat_test".into())).unwrap();
        let err = client.export_skills("p").await.unwrap_err().to_string();
        assert!(err.contains("exceeds 8 MiB"), "chunked={chunked}: {err}");
    }
}

#[tokio::test]
async fn test_server_413_is_reported() {
    let url = stub(
        "413 Payload Too Large",
        r#"{"error":"too large"}"#.into(),
        false,
    )
    .await;
    let client = NexusClient::new(&url, Some("nxs_pat_test".into())).unwrap();
    let err = client
        .export_agent_files("p")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("exceeds 8 MiB"), "{err}");
}

#[tokio::test]
async fn test_export_executables_are_parsed() {
    let body = r#"{"action":"sk_export","project":{"id":"p","slug":"p","name":"P"},"skills":[],"count":0,
        "executables":[{"kind":"skill_script","id":"nx-a/run.sh","target_path":".claude/skills/nx-a/run.sh","command":null,"sha256":"ab"}]}"#;
    let url = stub("200 OK", body.into(), true).await;
    let client = NexusClient::new(&url, Some("nxs_pat_test".into())).unwrap();
    let export = client.export_skills("p").await.unwrap();
    assert_eq!(export.executables.len(), 1);
    assert_eq!(export.executables[0].kind, "skill_script");
    assert_eq!(export.executables[0].command, None);
}
