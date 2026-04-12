//! Tests for nexus_core::auth module.

use nexus_core::auth::{Credentials, TOKEN_PREFIX};

#[test]
fn test_credentials_path_uses_xdg_style() {
    let path = Credentials::path().unwrap();
    let path_str = path.to_string_lossy();

    // Should use ~/.config/nexus/, NOT platform-specific directories
    assert!(
        path_str.contains(".config/nexus/credentials.toml"),
        "expected XDG-style path, got: {}",
        path_str
    );

    // Should NOT use macOS Library path
    assert!(
        !path_str.contains("Library/Application Support"),
        "should not use macOS Library path"
    );
}

#[test]
fn test_token_prefix() {
    assert_eq!(TOKEN_PREFIX, "nxs_pat_");
}

#[test]
fn test_validate_token_format_valid() {
    let token = "nxs_pat_abc1234567890abcdef";
    assert!(Credentials::validate_token_format(token).is_ok());
}

#[test]
fn test_validate_token_format_wrong_prefix() {
    let token = "gws_pat_abc1234567890abcdef";
    let result = Credentials::validate_token_format(token);
    assert!(result.is_err());
}

#[test]
fn test_validate_token_format_too_short() {
    let token = "nxs_pat_ab";
    let result = Credentials::validate_token_format(token);
    assert!(result.is_err());
}

#[test]
fn test_validate_token_format_empty() {
    let result = Credentials::validate_token_format("");
    assert!(result.is_err());
}

#[test]
fn test_credentials_toml_roundtrip() {
    let creds = Credentials {
        token: "nxs_pat_test-token-for-roundtrip-test".to_string(),
        expires_at: Some("2026-12-31T23:59:59Z".to_string()),
    };

    let serialized = toml::to_string_pretty(&creds).unwrap();
    let deserialized: Credentials = toml::from_str(&serialized).unwrap();

    assert_eq!(deserialized.token, creds.token);
    assert_eq!(deserialized.expires_at, creds.expires_at);
}

#[test]
fn test_credentials_toml_without_expires() {
    let creds = Credentials {
        token: "nxs_pat_test-token-no-expiry-roundtrip".to_string(),
        expires_at: None,
    };

    let serialized = toml::to_string_pretty(&creds).unwrap();
    // expires_at should not be in the output (skip_serializing_if)
    assert!(!serialized.contains("expires_at"));

    let deserialized: Credentials = toml::from_str(&serialized).unwrap();
    assert_eq!(deserialized.token, creds.token);
    assert!(deserialized.expires_at.is_none());
}

#[test]
fn test_credentials_deserialize_minimal() {
    let toml_str = r#"token = "nxs_pat_minimal-test-token-1234""#;
    let creds: Credentials = toml::from_str(toml_str).unwrap();
    assert_eq!(creds.token, "nxs_pat_minimal-test-token-1234");
    assert!(creds.expires_at.is_none());
}
