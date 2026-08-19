//! Inbound trigger types — a run caused by something other than the clock.
//!
//! Every run in dotagent used to originate from a `[[schedules]]` window. A
//! trigger is the other cause: a chat message arrived, an operator asked, an
//! agent finished and wants to start the next one.
//!
//! The daemon owns a channel of these and serializes them against its own tick
//! loop, so a triggered run gets the same supervisor, heartbeat, retry policy
//! and audit trail as a scheduled one.
//!
//! ## Trust
//!
//! A `TriggerRequest` can originate from untrusted input (a Telegram message
//! from the public internet). `agent` is therefore a **selector, not a
//! command**: the daemon resolves it against the manifests already on disk and
//! refuses anything it cannot find. Nothing in this struct is ever passed to a
//! shell. See `docs/security/threat-model.md`.

use serde::{Deserialize, Serialize};

/// Schedule id given to an agent that declares no `[[schedules]]` of its own —
/// one that exists only to be asked for.
pub const TRIGGER_SCHEDULE_ID: &str = "trigger";

/// Where a trigger came from. Recorded in the audit log so a run can always be
/// traced back to what caused it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerSource {
    /// An inbound Telegram message.
    Telegram,
    /// An operator on the same machine.
    Cli,
    /// An MCP `tools/call`.
    Mcp,
    /// A local client over the assistant API (`assistant-v1`).
    Local,
}

impl TriggerSource {
    /// Slug for the heartbeat and window files of runs from this source.
    ///
    /// Triggered runs never share a slug with the scheduled history of the
    /// same agent — otherwise an on-demand run would overwrite
    /// `last_success_at` for a cron window that never actually ran, and the
    /// scheduler would stop retrying a genuinely missed run.
    pub fn slug(&self) -> String {
        format!("trigger-{self}")
    }
}

impl std::fmt::Display for TriggerSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TriggerSource::Telegram => "telegram",
            TriggerSource::Cli => "cli",
            TriggerSource::Mcp => "mcp",
            TriggerSource::Local => "local",
        })
    }
}

/// A request to run an agent now, outside any schedule window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerRequest {
    pub source: TriggerSource,
    /// Agent name, matched against `agent.name` in a discovered manifest.
    pub agent: String,
    /// Schedule id to borrow args and policy from. `None` ⇒ the manifest's
    /// first schedule, matching `run-now` behavior.
    #[serde(default)]
    pub schedule: Option<String>,
    /// Extra argv appended after the manifest's own `run.args`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Structured payload handed to the agent as `AGENT_TRIGGER_PAYLOAD`,
    /// never as argv — quoting and shell metacharacters in a message body stay
    /// out of the picture that way. A source with payloads large enough to
    /// approach `ARG_MAX` should write a file into `AGENT_TMPDIR` instead of
    /// growing this.
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
    /// Who asked, in whatever form the source can attest. For Telegram this is
    /// the numeric user id — never a `@username`, which is spoofable.
    #[serde(default)]
    pub actor: Option<String>,
    /// Where the answer goes back to, opaque to the daemon. For Telegram this
    /// is the chat id of the conversation that asked.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Local conversation id, when the source is one. Scopes the slug — and
    /// therefore the heartbeat file — per conversation, so two parallel
    /// sessions keep separate run histories instead of clobbering each other.
    /// Sanitized before it reaches a filename; see [`crate::slug`].
    #[serde(default)]
    pub session_id: Option<String>,
}

impl TriggerRequest {
    /// Slug used for heartbeat and window files. See [`TriggerSource::slug`].
    ///
    /// A `session_id` scopes it further, so each conversation gets its own
    /// history instead of sharing one `trigger-local` file with every other.
    /// Without one, the slug is exactly what it has always been.
    pub fn slug(&self) -> String {
        match &self.session_id {
            Some(session) => {
                format!(
                    "{}-{}",
                    self.source.slug(),
                    crate::slug::sanitize_key(session)
                )
            }
            None => self.source.slug(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(source: TriggerSource, session_id: Option<&str>) -> TriggerRequest {
        TriggerRequest {
            source,
            agent: "a".into(),
            schedule: None,
            args: vec![],
            payload: None,
            actor: None,
            reply_to: None,
            session_id: session_id.map(str::to_string),
        }
    }

    #[test]
    fn slug_is_namespaced_by_source() {
        let req = TriggerRequest {
            source: TriggerSource::Telegram,
            agent: "disk-alert".into(),
            schedule: None,
            args: vec![],
            payload: None,
            actor: None,
            reply_to: None,
            session_id: None,
        };
        assert_eq!(req.slug(), "trigger-telegram");
    }

    #[test]
    fn slug_without_a_session_is_exactly_what_it_always_was() {
        // Backcompat: these exact strings are already on disk as heartbeat
        // filenames. A session-less trigger must keep writing to them.
        for (source, want) in [
            (TriggerSource::Telegram, "trigger-telegram"),
            (TriggerSource::Cli, "trigger-cli"),
            (TriggerSource::Mcp, "trigger-mcp"),
            (TriggerSource::Local, "trigger-local"),
        ] {
            assert_eq!(req(source, None).slug(), want);
        }
    }

    #[test]
    fn slug_never_collides_across_sources() {
        assert_ne!(
            req(TriggerSource::Telegram, None).slug(),
            req(TriggerSource::Cli, None).slug()
        );
        assert_ne!(
            req(TriggerSource::Cli, None).slug(),
            req(TriggerSource::Mcp, None).slug()
        );
    }

    #[test]
    fn a_session_namespaces_the_slug() {
        assert_eq!(
            req(TriggerSource::Local, Some("chat-9_a")).slug(),
            "trigger-local-chat-9_a"
        );
        assert_eq!(
            req(TriggerSource::Telegram, Some("12345")).slug(),
            "trigger-telegram-12345"
        );
    }

    #[test]
    fn a_dirty_session_yields_a_stable_safe_slug() {
        let a = req(TriggerSource::Local, Some("../../etc/passwd")).slug();
        assert!(a.starts_with("trigger-local-k"), "{a}");
        assert_eq!(
            a,
            req(TriggerSource::Local, Some("../../etc/passwd")).slug(),
            "must be stable"
        );
        assert_ne!(
            a,
            req(TriggerSource::Local, Some("../../etc/shadow")).slug(),
            "must not collide"
        );
        // No path separator ever reaches a filename.
        assert!(!a.contains('/'), "{a}");
    }

    #[test]
    fn distinct_sessions_never_share_a_slug() {
        let first = req(TriggerSource::Local, Some("s-1")).slug();
        let second = req(TriggerSource::Local, Some("s-2")).slug();
        assert_ne!(first, second);
    }

    #[test]
    fn deserializes_with_only_required_fields() {
        let req: TriggerRequest =
            serde_json::from_str(r#"{"source":"telegram","agent":"disk-alert"}"#).unwrap();
        assert_eq!(req.agent, "disk-alert");
        assert!(req.schedule.is_none());
        assert!(req.args.is_empty());
        assert!(req.reply_to.is_none());
        assert!(req.session_id.is_none());
    }

    #[test]
    fn local_round_trips_through_serde() {
        let req: TriggerRequest =
            serde_json::from_str(r#"{"source":"local","agent":"a","session_id":"s1"}"#).unwrap();
        assert_eq!(req.source, TriggerSource::Local);
        assert_eq!(req.session_id.as_deref(), Some("s1"));
        assert_eq!(req.slug(), "trigger-local-s1");
    }
}
