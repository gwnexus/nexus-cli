//! A string that never shows up in `Debug` output.
//!
//! Wire types that carry tokens (`af_export` echoing the API token, runner
//! registration, inference tokens) hold them as [`Secret`], so a `{:?}` of
//! the whole response in a log or panic message prints `[REDACTED]`.

use serde::{Deserialize, Serialize};

/// A secret value: serializes as the plain string, debugs as `[REDACTED]`.
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret value; only for the place that uses it (a header, a file).
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// `url` for display: a user name or password in it is replaced by `***`.
/// An input that does not parse as a URL with a host but contains `@` (for
/// example `user:pw@host` without a scheme) is hidden completely.
pub fn redact_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut parsed) if !parsed.username().is_empty() || parsed.password().is_some() => {
            let _ = parsed.set_username("***");
            let _ = parsed.set_password(None);
            parsed.to_string()
        }
        Ok(parsed) if parsed.cannot_be_a_base() && url.contains('@') => "<invalid URL>".into(),
        Ok(_) => url.to_string(),
        Err(_) if url.contains('@') => "<invalid URL>".into(),
        Err(_) => url.to_string(),
    }
}

/// Whether `url` carries a user name or password (or cannot be parsed and
/// contains `@`, which may hide one).
pub fn url_has_credentials(url: &str) -> bool {
    match reqwest::Url::parse(url) {
        Ok(parsed) => {
            !parsed.username().is_empty()
                || parsed.password().is_some()
                || (parsed.cannot_be_a_base() && url.contains('@'))
        }
        Err(_) => url.contains('@'),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debug_hides_and_serde_keeps_the_value() {
        let s = Secret::new("nxs_run_abcdef");
        assert_eq!(format!("{s:?}"), "[REDACTED]");
        assert_eq!(format!("{:?}", Some(&s)), "Some([REDACTED])");
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"nxs_run_abcdef\"");
        let back: Secret = serde_json::from_str("\"nxs_run_abcdef\"").unwrap();
        assert_eq!(back.expose(), "nxs_run_abcdef");
    }

    #[test]
    fn test_redact_url() {
        let shown = redact_url("https://alice:pw@api.example.test/x");
        assert!(!shown.contains("alice") && !shown.contains("pw"), "{shown}");
        assert_eq!(
            redact_url("https://api.example.test"),
            "https://api.example.test"
        );
        assert_eq!(redact_url("alice:pw@api.example.test"), "<invalid URL>");
        assert!(url_has_credentials("https://alice@api.example.test"));
        assert!(url_has_credentials("alice:pw@api.example.test"));
        assert!(!url_has_credentials("https://api.example.test/a@b"));
    }
}
