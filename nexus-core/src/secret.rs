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
}
