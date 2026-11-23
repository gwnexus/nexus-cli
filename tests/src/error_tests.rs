//! Tests for nexus_core::error module.

use nexus_core::Error;

#[test]
fn test_error_display_auth() {
    let err = Error::Auth("invalid token".to_string());
    assert_eq!(err.to_string(), "Authentication error: invalid token");
}

#[test]
fn test_error_display_config() {
    let err = Error::Config("missing key".to_string());
    assert_eq!(err.to_string(), "Configuration error: missing key");
}

#[test]
fn test_error_display_api() {
    let err = Error::Api("rate limit exceeded".to_string());
    assert_eq!(err.to_string(), "API error: rate limit exceeded");
}

#[test]
fn test_error_display_not_found() {
    let err = Error::NotFound("project abc".to_string());
    assert_eq!(err.to_string(), "Not found: project abc");
}

#[test]
fn test_error_display_unauthorized() {
    let err = Error::Unauthorized("expired token".to_string());
    assert_eq!(err.to_string(), "Unauthorized: expired token");
}

#[test]
fn test_error_display_forbidden() {
    let err = Error::Forbidden("insufficient permissions".to_string());
    assert_eq!(err.to_string(), "Forbidden: insufficient permissions");
}

#[test]
fn test_error_display_other() {
    let err = Error::Other("something went wrong".to_string());
    assert_eq!(err.to_string(), "something went wrong");
}

#[test]
fn test_error_display_toml() {
    let err = Error::Toml("unexpected character".to_string());
    assert_eq!(err.to_string(), "TOML error: unexpected character");
}

#[test]
fn test_error_from_io() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
    let err: Error = io_err.into();
    assert!(matches!(err, Error::Io(_)));
    assert!(err.to_string().contains("file not found"));
}

#[test]
fn test_error_from_serde_json() {
    let json_err = serde_json::from_str::<String>("not-json").unwrap_err();
    let err: Error = json_err.into();
    assert!(matches!(err, Error::Json(_)));
}

#[test]
fn test_error_from_toml_de() {
    let toml_err = toml::from_str::<String>("not valid toml {{{").unwrap_err();
    let err: Error = toml_err.into();
    assert!(matches!(err, Error::Toml(_)));
}
