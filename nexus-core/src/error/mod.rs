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

    /// Conflict (HTTP 409), e.g. a runner cursor that would move backwards.
    #[error("Conflict: {0}")]
    Conflict(String),

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

impl Error {
    /// The request failed in transport (connect or send), before any
    /// HTTP status came back; a retry may succeed.
    pub fn is_transport(&self) -> bool {
        matches!(self, Error::Http(e) if e.is_connect() || e.is_request())
    }
}

impl From<toml::de::Error> for Error {
    /// Only the message, with every quoted part redacted: the `Display`
    /// text quotes the offending source line, and value errors quote the
    /// value itself (``unknown variant `...` ``), either of which may be a
    /// token.
    fn from(err: toml::de::Error) -> Self {
        Error::Toml(redact_quoted(err.message()))
    }
}

/// `text` with the content of each backtick, double or single quoted
/// part replaced by `...`; an unclosed quote redacts the rest.
fn redact_quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut open: Option<char> = None;
    for c in text.chars() {
        match open {
            Some(q) if c == q => {
                out.push_str("...");
                out.push(c);
                open = None;
            }
            Some(_) => {}
            None if matches!(c, '`' | '"' | '\'') => {
                out.push(c);
                open = Some(c);
            }
            None => out.push(c),
        }
    }
    if open.is_some() {
        out.push_str("...");
    }
    out
}

impl From<toml::ser::Error> for Error {
    fn from(err: toml::ser::Error) -> Self {
        Error::Toml(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_toml_error_omits_the_source_line() {
        let raw = "token = \"tok-SYNTHETIC-123\" junk\n";
        let err: Error = toml::from_str::<toml::Value>(raw).unwrap_err().into();
        let text = err.to_string();
        assert!(!text.contains("tok-SYNTHETIC-123"), "{text}");
    }

    #[test]
    fn test_toml_value_error_omits_the_value() {
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        enum Format {
            Text,
            Json,
        }
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        struct Cfg {
            default_output: Format,
            limit: Option<u64>,
        }
        for raw in [
            "default_output = \"tok-SYNTHETIC-456\"\n",
            "default_output = \"Text\"\nlimit = \"tok-SYNTHETIC-456\"\n",
        ] {
            let err: Error = toml::from_str::<Cfg>(raw).unwrap_err().into();
            let text = err.to_string();
            assert!(!text.contains("tok-SYNTHETIC-456"), "{text}");
        }
        assert_eq!(
            redact_quoted("a `x` b \"y\" c 'z"),
            "a `...` b \"...\" c '..."
        );
    }
}
