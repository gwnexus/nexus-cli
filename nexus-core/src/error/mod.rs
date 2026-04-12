//! Nexus Core error types.
//!
//! Provides a unified error enum for the entire library crate,
//! with automatic conversions from common error sources.

use thiserror::Error;

/// Unified error type for nexus-core operations.
#[derive(Debug, Error)]
pub enum Error {
    /// HTTP transport error from reqwest.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// Authentication failure (invalid or expired token).
    #[error("Authentication error: {0}")]
    Auth(String),

    /// Configuration loading or parsing error.
    #[error("Configuration error: {0}")]
    Config(String),

    /// API-level error returned by the Nexus server.
    #[error("API error: {0}")]
    Api(String),

    /// Resource not found (HTTP 404).
    #[error("Not found: {0}")]
    NotFound(String),

    /// Unauthorized (HTTP 401).
    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    /// Forbidden (HTTP 403).
    #[error("Forbidden: {0}")]
    Forbidden(String),

    /// Filesystem I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// TOML serialization/deserialization error.
    #[error("TOML error: {0}")]
    Toml(String),

    /// Generic catch-all error.
    #[error("{0}")]
    Other(String),
}

impl From<toml::de::Error> for Error {
    fn from(err: toml::de::Error) -> Self {
        Error::Toml(err.to_string())
    }
}

impl From<toml::ser::Error> for Error {
    fn from(err: toml::ser::Error) -> Self {
        Error::Toml(err.to_string())
    }
}
