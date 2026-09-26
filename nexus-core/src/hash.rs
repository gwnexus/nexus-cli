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

/// `content` without trailing line breaks (`\n` / `\r\n`).
///
/// Text workspace files (`devbox.json`, `scripts/devbox/**`) are compared
/// modulo trailing newlines: the backend delivers some of them without a
/// final newline, while repos running e.g. pre-commit's end-of-file-fixer
/// commit them with exactly one.
pub fn trim_trailing_newlines(content: &str) -> &str {
    content.trim_end_matches(['\n', '\r'])
}

/// `content` ending with exactly one `\n` (empty content stays empty).
pub fn with_single_trailing_newline(content: &str) -> String {
    let trimmed = trim_trailing_newlines(content);
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

/// Whether two text files differ at most in their trailing newlines, or
/// are the same JSON document. The backend stores `devbox.json` as JSON and
/// returns it with its own key order, so a byte comparison reported every
/// freshly pushed `devbox.json` as changed (NEXUS-APP dispatch 95d81511).
pub fn text_equivalent(a: &str, b: &str) -> bool {
    let (a, b) = (trim_trailing_newlines(a), trim_trailing_newlines(b));
    a == b || json_equivalent(a, b)
}

/// Whether both texts parse as JSON objects/arrays with equal values
/// (object key order is not significant).
fn json_equivalent(a: &str, b: &str) -> bool {
    let looks_json = |s: &str| matches!(s.trim_start().as_bytes().first(), Some(b'{' | b'['));
    if !looks_json(a) || !looks_json(b) {
        return false;
    }
    match (
        serde_json::from_str::<serde_json::Value>(a),
        serde_json::from_str::<serde_json::Value>(b),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// [`hash_matches`] that also accepts `content` with its trailing newlines
/// removed or normalized to exactly one, so a hash recorded for the
/// backend's copy still matches a local file whose only difference is the
/// final newline.
pub fn hash_matches_text(recorded: &str, content: &str) -> bool {
    if hash_matches(recorded, content) {
        return true;
    }
    let trimmed = trim_trailing_newlines(content);
    ["", "\n", "\r\n", "\n\n"]
        .iter()
        .any(|end| recorded == sha256_hex(&format!("{trimmed}{end}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_equivalent_ignores_trailing_newlines_only() {
        assert!(text_equivalent("{}", "{}\n"));
        assert!(text_equivalent("{}\n\n", "{}\r\n"));
        assert!(!text_equivalent("a b", "a  b"));
        assert!(text_equivalent("{}", "{ }"));
        assert!(!text_equivalent("a\nb", "a\n\nb"));
        assert!(text_equivalent(
            "{\"a\": 1, \"b\": [1, 2]}",
            "{\n  \"b\": [1, 2],\n  \"a\": 1\n}\n"
        ));
        assert!(!text_equivalent("{\"b\": [1, 2]}", "{\"b\": [2, 1]}"));
        assert!(!text_equivalent("{\"a\": 1}", "{\"a\": 1, \"b\": 2}"));
        assert_eq!(with_single_trailing_newline("x"), "x\n");
        assert_eq!(with_single_trailing_newline("x\n\n"), "x\n");
        assert_eq!(with_single_trailing_newline(""), "");
    }

    #[test]
    fn test_hash_matches_text_accepts_newline_variants() {
        let server = "{\"a\":1}";
        let recorded = sha256_hex(server);
        assert!(hash_matches_text(&recorded, "{\"a\":1}\n"));
        assert!(hash_matches_text(&recorded, "{\"a\":1}\n\n"));
        assert!(hash_matches_text(&sha256_hex("x\n"), "x"));
        // Pull writes one `\n` but records the backend body's hash.
        assert!(hash_matches_text(&sha256_hex("x\r\n"), "x\n"));
        assert!(hash_matches_text(&sha256_hex("x\n\n"), "x\n"));
        assert!(!hash_matches_text(&recorded, "{\"a\":2}\n"));
    }

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
