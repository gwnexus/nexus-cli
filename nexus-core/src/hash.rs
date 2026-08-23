//! Shared hashing utilities for the Nexus CLI.

use sha2::{Digest, Sha256};

/// Compute a lowercase hex SHA-256 hash of the given content.
///
/// Matches the server-side `computeContentHash` function used by
/// `af_status` and `ws_push` endpoints.
pub fn sha256_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deterministic() {
        assert_eq!(sha256_hex("hello"), sha256_hex("hello"));
    }

    #[test]
    fn test_hex_length() {
        assert_eq!(sha256_hex("hello").len(), 64);
    }

    #[test]
    fn test_known_value() {
        assert_eq!(
            sha256_hex("hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_different_inputs() {
        assert_ne!(sha256_hex("hello"), sha256_hex("world"));
    }
}
