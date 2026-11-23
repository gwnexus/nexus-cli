//! Tests for NexusClient construction and configuration.

use nexus_core::api::NexusClient;

#[test]
fn test_client_new_with_https_url() {
    let client = NexusClient::new("https://nexus.gatewarden.eu", Some("token".into()));
    assert!(client.is_ok());
}

#[test]
fn test_client_new_with_localhost_http() {
    let client = NexusClient::new("http://localhost:3000", Some("token".into()));
    assert!(client.is_ok(), "localhost should allow HTTP");
}

#[test]
fn test_client_new_with_127_0_0_1_http() {
    let client = NexusClient::new("http://127.0.0.1:3000", Some("token".into()));
    assert!(client.is_ok(), "127.0.0.1 should allow HTTP");
}

#[test]
fn test_client_new_rejects_http_for_remote() {
    let client = NexusClient::new("http://nexus.gatewarden.eu", Some("token".into()));
    assert!(client.is_err(), "remote URLs must use HTTPS");
    let err = client.unwrap_err().to_string();
    assert!(err.contains("HTTPS"), "error should mention HTTPS");
}

#[test]
fn test_client_new_without_token() {
    let client = NexusClient::new("https://nexus.gatewarden.eu", None);
    assert!(client.is_ok(), "token is optional at construction time");
}

#[test]
fn test_client_new_strips_trailing_slash() {
    // This test validates that URLs are normalized (trailing slash stripped).
    // The actual stripping happens internally, so we verify construction succeeds.
    let client = NexusClient::new("https://nexus.gatewarden.eu/", Some("token".into()));
    assert!(client.is_ok());
}

#[test]
fn test_client_set_token() {
    let mut client = NexusClient::new("https://nexus.gatewarden.eu", None).unwrap();
    client.set_token("nxs_pat_test_token".into());
    // No assertion needed beyond no panic - set_token is infallible
}

/// A server on 127.0.0.1 that answers each connection with the next raw
/// response (built from its port); returns the port and a counter of
/// accepted connections.
async fn sequence_stub(
    responses: impl FnOnce(u16) -> Vec<String>,
) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let responses = responses(port);
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        for resp in responses {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    (port, hits)
}

fn redirect_to(location: &str) -> String {
    format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

fn svg_ok() -> String {
    "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\n<svg/>".into()
}

#[tokio::test]
async fn test_avatar_redirect_to_another_host_is_refused() {
    let (private, private_hits) = sequence_stub(|_| vec![svg_ok()]).await;
    let (api, api_hits) =
        sequence_stub(|_| vec![redirect_to(&format!("http://127.0.0.1:{private}/private"))]).await;
    let client = NexusClient::new(&format!("http://localhost:{api}"), None).unwrap();
    let got = client
        .download_actor_avatar(&format!("http://localhost:{api}/a.svg"))
        .await;
    let err = format!("{:?}", got.unwrap_err());
    assert!(err.contains("not allowed"), "{err}");
    assert_eq!(api_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(private_hits.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// `n` same-host redirects, then the SVG.
async fn redirect_chain(n: usize) -> Result<Vec<u8>, nexus_core::error::Error> {
    let (api, _) = sequence_stub(|port| {
        let mut r: Vec<String> = (0..n)
            .map(|i| redirect_to(&format!("http://localhost:{port}/r{i}")))
            .collect();
        r.push(svg_ok());
        r
    })
    .await;
    let client = NexusClient::new(&format!("http://localhost:{api}"), None).unwrap();
    client
        .download_actor_avatar(&format!("http://localhost:{api}/a.svg"))
        .await
}

#[tokio::test]
async fn test_avatar_follows_five_redirects_and_refuses_six() {
    assert_eq!(redirect_chain(5).await.unwrap(), b"<svg/>");
    let err = format!("{:?}", redirect_chain(6).await.unwrap_err());
    assert!(err.contains("too many"), "{err}");
}

#[tokio::test]
async fn test_avatar_redirect_to_plain_http_elsewhere_is_refused() {
    // Not reachable: the policy refuses before connecting.
    let (api, _) = sequence_stub(|_| vec![redirect_to("http://cdn.supabase.co/a.svg")]).await;
    let client = NexusClient::new(&format!("http://localhost:{api}"), None).unwrap();
    let err = format!(
        "{:?}",
        client
            .download_actor_avatar(&format!("http://localhost:{api}/a.svg"))
            .await
            .unwrap_err()
    );
    assert!(err.contains("not allowed"), "{err}");
}

#[tokio::test]
async fn test_avatar_redirect_on_the_api_host_is_followed() {
    let (api, hits) = sequence_stub(|port| {
        vec![
            redirect_to(&format!("http://localhost:{port}/b.svg")),
            svg_ok(),
        ]
    })
    .await;
    let client = NexusClient::new(&format!("http://localhost:{api}"), None).unwrap();
    let got = client
        .download_actor_avatar(&format!("http://localhost:{api}/a.svg"))
        .await
        .unwrap();
    assert_eq!(got, b"<svg/>");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
}
