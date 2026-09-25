//! Shared hashing utilities for the Nexus CLI.

use sha2::{Digest, Sha256};

/// Compute a lowercase hex SHA-256 hash of the given content.
///
/// Matches the server-side `computeContentHash` function used by
/// `af_status` and `ws_push` endpoints.
pub fn sha256_hex(content: &str) -> String {
    sha256_hex_bytes(content.as_bytes())
}

/// Compute a lowercase hex SHA-256 hash of raw bytes (no normalization),
/// for hashing files exactly as they exist on disk.
pub fn sha256_hex_bytes(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

/// SHA-256 of agent-file content without its `generated_at:` lines, which
/// the backend re-stamps on every export even when nothing else changed.
/// Byte-identical to [`sha256_hex`] for content without such a line.
pub fn sha256_hex_normalized(content: &str) -> String {
    if !content.lines().any(|l| l.starts_with("generated_at:")) {
        return sha256_hex(content);
    }
    let kept: String = content
        .split_inclusive('\n')
        .filter(|l| !l.starts_with("generated_at:"))
        .collect();
    sha256_hex(&kept)
}

/// Whether a hash recorded for a file (raw, or normalized per
/// [`sha256_hex_normalized`]) matches `content`.
pub fn hash_matches(recorded: &str, content: &str) -> bool {
    recorded == sha256_hex(content) || recorded == sha256_hex_normalized(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalized_ignores_generated_at_only() {
        let a = "---\ngenerated_at: 1\n---\nbody\n";
        let b = "---\ngenerated_at: 2\n---\nbody\n";
        assert_eq!(sha256_hex_normalized(a), sha256_hex_normalized(b));
        assert_ne!(
            sha256_hex_normalized(a),
            sha256_hex_normalized("---\n---\nother\n")
        );
        // Unchanged bytes (incl. CRLF, no trailing newline) hash like raw.
        for c in ["x\r\ny", "no newline", "a\n\n"] {
            assert_eq!(sha256_hex_normalized(c), sha256_hex(c));
        }
    }

    #[test]
    fn test_hash_matches_raw_or_normalized() {
        let c = "---\ngenerated_at: 1\n---\nbody\n";
        assert!(hash_matches(&sha256_hex(c), c));
        assert!(hash_matches(
            &sha256_hex_normalized("---\ngenerated_at: 9\n---\nbody\n"),
            c
        ));
        assert!(!hash_matches(&sha256_hex("other"), c));
    }

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
