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

/// SHA-256 of agent-file content without the `generated_at:` field of its
/// front matter, which the backend re-stamps on every export even when
/// nothing else changed. Only the front matter (between a leading `---`
/// line and the next `---` line) counts: a `generated_at:` line in the
/// document body is content. Byte-identical to [`sha256_hex`] for content
/// without such a field.
pub fn sha256_hex_normalized(content: &str) -> String {
    let mut lines = content.split_inclusive('\n');
    let is_fence = |l: &str| l.trim_end_matches(['\n', '\r']) == "---";
    match lines.next() {
        Some(first) if is_fence(first) => {}
        _ => return sha256_hex(content),
    }
    let mut kept = String::with_capacity(content.len());
    let mut in_front = true;
    let mut stripped = false;
    for (i, line) in content.split_inclusive('\n').enumerate() {
        if i > 0 && in_front && is_fence(line) {
            in_front = false;
        } else if i > 0 && in_front && line.starts_with("generated_at:") {
            stripped = true;
            continue;
        }
        kept.push_str(line);
    }
    // An unterminated front matter is no front matter.
    if !stripped || in_front {
        return sha256_hex(content);
    }
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
/// freshly pushed `devbox.json` as changed.
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
    // Both raw and normalized: a re-stamped `generated_at` and a changed
    // final newline together are still no edit.
    ["", "\n", "\r\n", "\n\n"].iter().any(|end| {
        let variant = format!("{trimmed}{end}");
        recorded == sha256_hex(&variant) || recorded == sha256_hex_normalized(&variant)
    })
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
    fn test_normalized_keeps_generated_at_lines_in_the_body() {
        let a = "---\ntitle: x\n---\ngenerated_at: keep\n";
        let b = "---\ntitle: x\n---\ngenerated_at: change\n";
        assert_ne!(sha256_hex_normalized(a), sha256_hex_normalized(b));
        // Without front matter the line is content too.
        assert_eq!(
            sha256_hex_normalized("generated_at: 1\nbody\n"),
            sha256_hex("generated_at: 1\nbody\n")
        );
        // An unterminated front matter is not stripped.
        let open = "---\ngenerated_at: 1\nbody\n";
        assert_eq!(sha256_hex_normalized(open), sha256_hex(open));
    }

    #[test]
    fn test_text_match_tolerates_restamp_and_final_newline_together() {
        let recorded = sha256_hex_normalized("---\ngenerated_at: old\n---\nbody\n");
        assert!(hash_matches_text(
            &recorded,
            "---\ngenerated_at: new\n---\nbody"
        ));
        assert!(!hash_matches_text(
            &recorded,
            "---\ngenerated_at: new\n---\nother"
        ));
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
