//! Conversation registry: the pure decision core.
//!
//! A registry record is a set of pointers about one conversation — which
//! model session last served it, how large its transcript was, which toolkit
//! hash it was created under. The daemon persists records under
//! `state/assistant/`; this module decides *what should change* given a new
//! session frame, a new toolkit hash, or a retirement ceiling. No IO here.

use serde::{Deserialize, Serialize};

/// What applying new information did to the record.
///
/// The daemon logs and reinjects based on this — a `FreshSession` means the
/// next trigger must NOT receive the old session pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryChange {
    /// Nothing about the session pointer changed.
    Unchanged,
    /// The model session pointer was recorded for the first time or replaced.
    SessionRecorded,
    /// The transcript passed the retirement ceiling: generation bumped,
    /// pointer cleared. The next run starts a fresh session.
    Retired,
    /// The toolkit hash changed: pointer cleared (a resumed session would
    /// not see the new tools), generation kept.
    ToolkitChanged,
}

/// One conversation's pointers, as persisted by the daemon.
///
/// Deliberately excludes any message content: a record that could hold chat
/// text would turn a registry leak into a transcript leak.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryRecord {
    /// Trigger source the conversation arrived on (`telegram`, `local`).
    pub source: String,
    /// Opaque conversation key the gateway already routes on.
    pub session_id: String,
    /// Model-side session pointer (e.g. a claude session id), if one was
    /// reported and is still valid.
    #[serde(default)]
    pub model_session: Option<String>,
    /// Monotonic counter, bumped on retirement. Agents derive stable
    /// per-generation session ids from it.
    #[serde(default)]
    pub generation: u64,
    /// Toolkit hash the current pointer was created under.
    #[serde(default)]
    pub toolkit_hash: Option<String>,
    /// Last reported transcript size in bytes.
    #[serde(default)]
    pub transcript_bytes: u64,
}

/// Default transcript retirement ceiling (400 KiB), matching the measured
/// knee where replies degrade from seconds to tens of seconds.
pub const DEFAULT_TRANSCRIPT_BYTES_MAX: u64 = 409_600;

impl RegistryRecord {
    pub fn new(source: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            session_id: session_id.into(),
            model_session: None,
            generation: 0,
            toolkit_hash: None,
            transcript_bytes: 0,
        }
    }

    /// Record the toolkit hash the conversation should run under.
    ///
    /// A changed hash invalidates the model session pointer: `--resume`
    /// freezes the MCP config at creation time, so a resumed session would
    /// be blind to the new tools. The generation is kept — retirement and
    /// toolkit rotation are different lifecycles.
    pub fn note_toolkit_hash(&mut self, hash: &str) -> RegistryChange {
        if self.toolkit_hash.as_deref() == Some(hash) {
            return RegistryChange::Unchanged;
        }
        self.toolkit_hash = Some(hash.to_string());
        let had_session = self.model_session.take().is_some();
        if had_session {
            RegistryChange::ToolkitChanged
        } else {
            RegistryChange::Unchanged
        }
    }

    /// Apply a session frame reported by the agent.
    ///
    /// If the reported transcript size passes `ceiling`, the record retires:
    /// generation bumps and the pointer is dropped, so the next trigger
    /// starts fresh while durable facts survive in memory.
    pub fn apply_session_frame(
        &mut self,
        model_session: &str,
        transcript_bytes: u64,
        ceiling: u64,
    ) -> RegistryChange {
        if transcript_bytes > ceiling {
            self.generation += 1;
            self.model_session = None;
            self.transcript_bytes = 0;
            return RegistryChange::Retired;
        }
        let changed = self.model_session.as_deref() != Some(model_session);
        self.model_session = Some(model_session.to_string());
        self.transcript_bytes = transcript_bytes;
        if changed {
            RegistryChange::SessionRecorded
        } else {
            RegistryChange::Unchanged
        }
    }

    /// Drop the session pointer on request (`/novo`), keeping the record.
    ///
    /// Same shape as a transcript retirement — generation bumps, pointer
    /// clears — so agents that derive per-generation ids see a new one and
    /// the next trigger starts a fresh conversation while durable facts
    /// survive in memory.
    pub fn reset(&mut self) {
        self.generation += 1;
        self.model_session = None;
        self.transcript_bytes = 0;
    }

    /// The pointer to reinject on the next trigger, if any.
    pub fn session_pointer(&self) -> Option<&str> {
        self.model_session.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> RegistryRecord {
        RegistryRecord::new("telegram", "chat-1")
    }

    #[test]
    fn new_record_has_no_pointers() {
        let r = record();
        assert_eq!(r.session_pointer(), None);
        assert_eq!(r.generation, 0);
        assert_eq!(r.transcript_bytes, 0);
    }

    #[test]
    fn session_frame_records_pointer() {
        let mut r = record();
        assert_eq!(
            r.apply_session_frame("s-1", 1_000, DEFAULT_TRANSCRIPT_BYTES_MAX),
            RegistryChange::SessionRecorded
        );
        assert_eq!(r.session_pointer(), Some("s-1"));
        assert_eq!(r.transcript_bytes, 1_000);
    }

    #[test]
    fn same_session_twice_is_unchanged() {
        let mut r = record();
        r.apply_session_frame("s-1", 1_000, DEFAULT_TRANSCRIPT_BYTES_MAX);
        assert_eq!(
            r.apply_session_frame("s-1", 1_200, DEFAULT_TRANSCRIPT_BYTES_MAX),
            RegistryChange::Unchanged
        );
        assert_eq!(r.transcript_bytes, 1_200);
    }

    #[test]
    fn transcript_ceiling_retires() {
        let mut r = record();
        r.apply_session_frame("s-1", 1_000, DEFAULT_TRANSCRIPT_BYTES_MAX);
        assert_eq!(
            r.apply_session_frame(
                "s-1",
                DEFAULT_TRANSCRIPT_BYTES_MAX + 1,
                DEFAULT_TRANSCRIPT_BYTES_MAX
            ),
            RegistryChange::Retired
        );
        assert_eq!(r.generation, 1);
        assert_eq!(r.session_pointer(), None);
        assert_eq!(r.transcript_bytes, 0);
    }

    #[test]
    fn exactly_at_ceiling_still_answers() {
        let mut r = record();
        assert_eq!(
            r.apply_session_frame(
                "s-1",
                DEFAULT_TRANSCRIPT_BYTES_MAX,
                DEFAULT_TRANSCRIPT_BYTES_MAX
            ),
            RegistryChange::SessionRecorded
        );
        assert_eq!(r.session_pointer(), Some("s-1"));
    }

    #[test]
    fn retirement_then_new_session_increments_pointer_only() {
        let mut r = record();
        r.apply_session_frame("s-1", 500_000, DEFAULT_TRANSCRIPT_BYTES_MAX);
        assert_eq!(
            r.apply_session_frame("s-2", 1_000, DEFAULT_TRANSCRIPT_BYTES_MAX),
            RegistryChange::SessionRecorded
        );
        assert_eq!(r.generation, 1, "generation survives into the new session");
    }

    #[test]
    fn toolkit_change_clears_pointer_keeps_generation() {
        let mut r = record();
        r.note_toolkit_hash("hash-a");
        r.apply_session_frame("s-1", 1_000, DEFAULT_TRANSCRIPT_BYTES_MAX);
        assert_eq!(
            r.note_toolkit_hash("hash-b"),
            RegistryChange::ToolkitChanged
        );
        assert_eq!(r.session_pointer(), None);
        assert_eq!(r.generation, 0);
        assert_eq!(r.toolkit_hash.as_deref(), Some("hash-b"));
    }

    #[test]
    fn same_toolkit_hash_is_unchanged_even_with_session() {
        let mut r = record();
        r.note_toolkit_hash("hash-a");
        r.apply_session_frame("s-1", 1_000, DEFAULT_TRANSCRIPT_BYTES_MAX);
        assert_eq!(r.note_toolkit_hash("hash-a"), RegistryChange::Unchanged);
        assert_eq!(r.session_pointer(), Some("s-1"));
    }

    #[test]
    fn first_toolkit_hash_without_session_is_unchanged() {
        let mut r = record();
        assert_eq!(r.note_toolkit_hash("hash-a"), RegistryChange::Unchanged);
    }

    #[test]
    fn record_round_trips_through_serde_without_content_fields() {
        let mut r = record();
        r.note_toolkit_hash("hash-a");
        r.apply_session_frame("s-1", 4_000, DEFAULT_TRANSCRIPT_BYTES_MAX);
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("messages"), "no room for chat text");
        assert!(!json.contains("transcript_text"));
        let back: RegistryRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn manual_reset_drops_the_pointer_and_bumps_generation() {
        let mut r = record();
        r.note_toolkit_hash("hash-a");
        r.apply_session_frame("s-1", 1_000, DEFAULT_TRANSCRIPT_BYTES_MAX);
        r.reset();
        assert_eq!(r.session_pointer(), None);
        assert_eq!(r.generation, 1);
        assert_eq!(r.transcript_bytes, 0);
        // The toolkit hash survives: the next session still wants the tools.
        assert_eq!(r.toolkit_hash.as_deref(), Some("hash-a"));
    }

    #[test]
    fn older_record_without_optionals_deserializes() {
        // Forward compatibility: a record persisted before a field existed
        // (all optional fields serde-default) still loads.
        let back: RegistryRecord =
            serde_json::from_str(r#"{"source":"telegram","session_id":"chat-1"}"#).unwrap();
        assert_eq!(back, record());
    }
}
