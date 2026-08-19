//! Wire types for the assistant stdout protocol (`assistant-v1`).
//!
//! An agent whose manifest declares `[run] protocol = "assistant-v1"` does not
//! answer with an exit code alone: it streams what it is doing over stdout,
//! one JSON object per line, and the daemon forwards those lines to the client
//! that asked. This module is the parse side of that stream — pure, no IO,
//! and lenient on purpose: a non-protocol line (a banner, a stray log) is
//! `None`, never an error and never a panic.

use serde::{Deserialize, Serialize};

/// The only stdout protocol dotagent understands today.
pub const ASSISTANT_PROTOCOL_V1: &str = "assistant-v1";

/// One line of an assistant agent's stdout.
///
/// Unknown fields on a frame are ignored, so the protocol can grow without
/// breaking older daemons mid-stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantEvent {
    /// A chunk of the answer as it is being composed (streaming).
    Delta { text: String },
    /// The final answer.
    Reply { text: String },
    /// Bookkeeping, emitted at most once per run: which assistant session did
    /// the work, and how much transcript it produced.
    Session {
        claude_session: String,
        transcript_bytes: u64,
    },
}

/// Parse one stdout line into an [`AssistantEvent`].
///
/// `None` for anything that is not a protocol frame: empty lines, non-JSON,
/// or JSON with a missing/unknown `type` or missing required fields. Never
/// panics on arbitrary input.
pub fn parse_line(line: &str) -> Option<AssistantEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str(trimmed).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_frame_kind() {
        assert_eq!(
            parse_line(r#"{"type":"delta","text":"he"}"#),
            Some(AssistantEvent::Delta { text: "he".into() })
        );
        assert_eq!(
            parse_line(r#"{"type":"reply","text":"the answer"}"#),
            Some(AssistantEvent::Reply {
                text: "the answer".into()
            })
        );
        assert_eq!(
            parse_line(r#"{"type":"session","claude_session":"s-1","transcript_bytes":42}"#),
            Some(AssistantEvent::Session {
                claude_session: "s-1".into(),
                transcript_bytes: 42,
            })
        );
    }

    #[test]
    fn extra_fields_are_ignored() {
        assert_eq!(
            parse_line(r#"{"type":"delta","text":"a","extra":1}"#),
            Some(AssistantEvent::Delta { text: "a".into() })
        );
    }

    #[test]
    fn non_protocol_lines_are_none() {
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("   \t "), None);
        assert_eq!(parse_line("plain text"), None);
        assert_eq!(parse_line("{not json"), None);
        assert_eq!(parse_line(r#"{"type":"log","text":"x"}"#), None);
        assert_eq!(parse_line(r#"{"text":"no type"}"#), None);
        assert_eq!(parse_line(r#"["not","an","object"]"#), None);
    }

    #[test]
    fn frames_with_missing_fields_are_none() {
        assert_eq!(parse_line(r#"{"type":"delta"}"#), None);
        assert_eq!(
            parse_line(r#"{"type":"session","claude_session":"s"}"#),
            None
        );
        assert_eq!(
            parse_line(r#"{"type":"session","claude_session":"s","transcript_bytes":"big"}"#),
            None
        );
    }

    #[test]
    fn frames_round_trip_through_serde() {
        for event in [
            AssistantEvent::Delta {
                text: "chunk".into(),
            },
            AssistantEvent::Reply {
                text: "final".into(),
            },
            AssistantEvent::Session {
                claude_session: "s-9".into(),
                transcript_bytes: u64::MAX,
            },
        ] {
            let line = serde_json::to_string(&event).unwrap();
            assert_eq!(parse_line(&line), Some(event));
        }
    }

    #[test]
    fn the_protocol_constant_is_the_documented_string() {
        assert_eq!(ASSISTANT_PROTOCOL_V1, "assistant-v1");
    }
}
