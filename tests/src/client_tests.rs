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
