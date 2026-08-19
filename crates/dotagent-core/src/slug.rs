//! Sanitizing arbitrary identifiers into filename- and label-safe slugs.
//!
//! The rules are copied verbatim from `dotagent-runner`'s persistent pool
//! (which keys live processes by a chat payload field): the same decision must
//! decide which heartbeat file a triggered run writes, so the logic lives in
//! core where everyone can share it. Runner is expected to migrate onto this
//! copy; until then the two must stay byte-for-byte in sync.

/// Longest a sanitized key may be. Keys land in filenames, process labels and
/// audit entries; a 4 KB chat field should not.
pub const MAX_KEY_LEN: usize = 64;

/// Reduce an arbitrary value to something safe to put in a filename, a process
/// label, a log line and an audit entry.
///
/// A value that is already a plain `[A-Za-z0-9_-]` identifier within
/// [`MAX_KEY_LEN`] is kept verbatim. Anything else — dirty, empty, or
/// overlong — becomes a stable digest of itself. There is deliberately no
/// truncation: a truncated identifier could collide two values into one key,
/// while the digest is bounded (17 chars) and distinct inputs stay distinct.
pub fn sanitize_key(raw: &str) -> String {
    let clean: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if clean.is_empty() || clean.len() != raw.len() || clean.len() > MAX_KEY_LEN {
        // Anything that is not already a plain identifier becomes a stable
        // digest of itself. Two different values never collapse to one key,
        // and no chat text ever reaches a label or a filename.
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        raw.hash(&mut hasher);
        format!("k{:016x}", hasher.finish())
    } else {
        clean
    }
}

/// Is `id` a session id the harness may pass through verbatim?
///
/// The daemon does not persist sessions — an id is an opaque string handed
/// between components — but it must stay bounded and free of path/traversal
/// characters before it reaches a filename, a label, or a log line. Charset
/// and length mirror [`sanitize_key`]'s notion of a plain identifier, but as
/// a *predicate* (accept/reject), not a rewrite: callers that need a safe
/// fallback key should use [`sanitize_key`] instead.
pub fn is_valid_session_id(id: &str) -> bool {
    (1..=MAX_KEY_LEN).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_identifier_is_kept_verbatim() {
        assert_eq!(sanitize_key("12345"), "12345");
        assert_eq!(sanitize_key("chat-9_a"), "chat-9_a");
    }

    #[test]
    fn anything_else_becomes_a_stable_digest() {
        let a = sanitize_key("../../etc/passwd");
        assert!(a.starts_with('k'), "{a}");
        assert_eq!(a, sanitize_key("../../etc/passwd"), "must be stable");
        assert_ne!(a, sanitize_key("../../etc/shadow"), "must not collide");
        // No path separator, no whitespace, nothing a label would have to quote.
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn path_like_values_never_produce_path_separators() {
        for raw in ["../etc/passwd", "/abs/path", "a/b", "..", ".", "a b"] {
            let s = sanitize_key(raw);
            assert!(!s.contains('/'), "{raw} -> {s}");
            assert!(!s.contains('.'), "{raw} -> {s}");
            assert!(!s.contains(' '), "{raw} -> {s}");
        }
    }

    #[test]
    fn an_overlong_key_is_hashed_rather_than_truncated() {
        // Truncating would let two different chats share one file.
        let long = "a".repeat(MAX_KEY_LEN + 1);
        let longer = "a".repeat(MAX_KEY_LEN + 2);
        assert_ne!(sanitize_key(&long), sanitize_key(&longer));
        assert!(sanitize_key(&long).len() <= MAX_KEY_LEN);
    }

    #[test]
    fn an_empty_key_never_yields_an_empty_label() {
        assert!(!sanitize_key("").is_empty());
    }

    #[test]
    fn output_is_always_bounded_and_filename_safe() {
        for raw in ["ok", "", "x".repeat(5000).as_str(), "\u{00e9}\u{1f600}"] {
            let s = sanitize_key(raw);
            assert!(!s.is_empty());
            assert!(s.len() <= MAX_KEY_LEN, "{raw} -> {s}");
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{raw} -> {s}"
            );
        }
    }

    #[test]
    fn session_id_accepts_plain_identifiers() {
        assert!(is_valid_session_id("a"));
        assert!(is_valid_session_id("12345"));
        assert!(is_valid_session_id("chat-9_a"));
    }

    #[test]
    fn session_id_accepts_exactly_64_chars_but_not_65() {
        assert!(is_valid_session_id(&"a".repeat(MAX_KEY_LEN)));
        assert!(!is_valid_session_id(&"a".repeat(MAX_KEY_LEN + 1)));
    }

    #[test]
    fn session_id_rejects_paths_and_traversal() {
        for bad in ["../x", "/abs", "a/b", "..", "."] {
            assert!(!is_valid_session_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn session_id_rejects_empty_and_unicode() {
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("café"));
        assert!(!is_valid_session_id("\u{1f600}"));
    }
}
