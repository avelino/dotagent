//! Wire format for persistent agents — one JSON object per line, both ways.
//!
//! A one-shot agent needs no protocol: stdout is the answer and exit code is
//! the verdict, because the process ending is the boundary. A process that
//! does not end has to say where one answer stops, and this is that.
//!
//! Two properties are deliberate:
//!
//! - **stdout is the channel, stderr is the log.** Same split the plugin
//!   protocol already uses, so an agent author has one rule to remember.
//! - **The reader is tolerant.** A line that does not parse, or that carries
//!   an `id` nobody is waiting for, is dropped rather than fatal. A stray
//!   `echo` on stdout is a mistake somebody will make, and it should cost a
//!   debug line rather than the conversation.
//!
//! See `docs/reference/persistent-protocol.md`.

use serde::{Deserialize, Serialize};

/// Protocol version carried in every frame. Bumped only for a breaking
/// change; a persistent agent may refuse a version it does not know.
pub const PROTOCOL_VERSION: u32 = 1;

/// Opening frame, written once per instance right after spawn.
///
/// Its answer (`ready`) is what tells the pool the process is up and warm.
/// Without a handshake, an agent that dies during startup fails every message
/// by timeout instead of failing once, immediately, at the spawn.
#[derive(Debug, Clone, Serialize)]
pub struct HelloFrame {
    pub v: u32,
    pub kind: &'static str,
    pub agent: String,
    /// Which slice of the world this instance answers for — the resolved
    /// `[lifecycle] key`, or `"default"`.
    pub key: String,
    pub schedule: String,
}

impl HelloFrame {
    pub fn new(
        agent: impl Into<String>,
        key: impl Into<String>,
        schedule: impl Into<String>,
    ) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            kind: "hello",
            agent: agent.into(),
            key: key.into(),
            schedule: schedule.into(),
        }
    }
}

/// One request. The agent answers with a [`ResponseFrame`] carrying the same
/// `id`.
#[derive(Debug, Clone, Serialize)]
pub struct RequestFrame {
    pub v: u32,
    pub kind: &'static str,
    pub id: String,
    pub agent: String,
    pub schedule: String,
    pub args: Vec<String>,
    /// How long the pool will wait before giving up and recycling the
    /// instance. Passed on so a well-behaved agent can bail first and say
    /// something useful instead of being killed mid-sentence.
    pub deadline_seconds: u64,
    /// Trigger context. Present when the run came from a message or a tool
    /// call, absent when it came from a clock.
    ///
    /// This is the field that replaces the per-request `AGENT_TRIGGER_*` and
    /// `AGENT_SESSION_ID` environment block: those variables are fixed at
    /// spawn, and a persistent process is spawned once for many messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<TriggerFrame>,
}

/// Trigger context for one request.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TriggerFrame {
    pub source: String,
    /// Opaque conversation/session identifier for this request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

impl TriggerFrame {
    /// Rebuild the trigger from the per-invocation environment the caller
    /// already assembled.
    ///
    /// The daemon flattens a `TriggerRequest` into `AGENT_TRIGGER_*` and
    /// `AGENT_SESSION_ID` for one-shot agents; rather than thread a second
    /// representation through `RunSpec`, the pool reads that same block back.
    /// One producer, so the two paths can never describe different triggers.
    ///
    /// Returns `None` when there is no trigger — a scheduled run of a
    /// persistent agent is legal and simply has no context to carry.
    pub fn from_env(extra_env: &[(String, String)]) -> Option<Self> {
        let get = |key: &str| {
            extra_env
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        let source = get("AGENT_TRIGGER_SOURCE")?;
        Some(Self {
            source,
            session_id: get("AGENT_SESSION_ID"),
            actor: get("AGENT_TRIGGER_ACTOR"),
            reply_to: get("AGENT_TRIGGER_REPLY_TO"),
            payload: get("AGENT_TRIGGER_PAYLOAD").and_then(|raw| serde_json::from_str(&raw).ok()),
        })
    }
}

/// Is this a per-request trigger variable?
///
/// The pool strips them from the spawn environment: they describe one message
/// and the process outlives every message. Leaving them in would freeze the
/// first request's context into every later one, which is worse than absent —
/// an agent reading `AGENT_TRIGGER_PAYLOAD` or `AGENT_SESSION_ID` would get
/// stale data that looks perfectly valid.
pub fn is_trigger_env(key: &str) -> bool {
    key.starts_with("AGENT_TRIGGER_") || key == "AGENT_SESSION_ID"
}

/// Anything the agent writes on stdout, parsed leniently.
///
/// Every field is optional because this type has to survive a half-written
/// frame from a process that is about to be recycled anyway.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InboundFrame {
    #[serde(default)]
    pub v: u32,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default = "default_ok")]
    pub ok: bool,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// Optional. Absent means `0` when `ok`, `1` otherwise — so a minimal
    /// agent never has to think about exit codes.
    #[serde(default)]
    pub exit_code: Option<i32>,
}

fn default_ok() -> bool {
    true
}

impl InboundFrame {
    /// Parse one line. `None` for anything that is not a JSON object — a log
    /// line that went to the wrong stream, a banner from a runtime, a partial
    /// write.
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if !line.starts_with('{') {
            return None;
        }
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        let object = value.as_object()?;
        let has_response_field = ["id", "output", "error", "exit_code"]
            .iter()
            .any(|field| object.contains_key(*field));
        match object.get("kind") {
            Some(kind) => match kind.as_str() {
                Some("hello" | "ready" | "response") => {}
                _ => return None,
            },
            None if !has_response_field => return None,
            None => {}
        }
        serde_json::from_value(value).ok()
    }

    pub fn is_ready(&self) -> bool {
        self.kind.as_deref() == Some("ready")
    }

    /// Is this the answer to the request `id`?
    ///
    /// A frame with no `id` counts when one is outstanding: the simplest
    /// possible agent answers without echoing it back, and there is only ever
    /// one request in flight per instance.
    pub fn answers(&self, id: &str) -> bool {
        match self.kind.as_deref() {
            Some("response") | None => {}
            Some(_) => return false,
        }
        match self.id.as_deref() {
            Some(got) => got == id,
            None => true,
        }
    }

    /// Exit code this frame implies.
    pub fn resolved_exit_code(&self) -> i32 {
        match self.exit_code {
            Some(code) => code,
            None if self.ok => 0,
            None => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_line_on_stdout_is_dropped_not_fatal() {
        assert!(InboundFrame::parse("starting up…").is_none());
        assert!(InboundFrame::parse("").is_none());
        assert!(InboundFrame::parse("{not json}").is_none());
    }

    #[test]
    fn a_minimal_answer_needs_only_output() {
        let f = InboundFrame::parse(r#"{"output":"hi"}"#).expect("frame");
        assert!(f.ok, "ok defaults to true so a minimal agent can omit it");
        assert_eq!(f.output.as_deref(), Some("hi"));
        assert_eq!(f.resolved_exit_code(), 0);
        assert!(
            f.answers("7"),
            "no id means it answers whatever is in flight"
        );
    }

    #[test]
    fn progress_only_frame_is_dropped() {
        assert!(InboundFrame::parse(r#"{"progress":50}"#).is_none());
    }

    #[test]
    fn progress_with_id_is_dropped() {
        assert!(InboundFrame::parse(r#"{"kind":"progress","id":"7"}"#).is_none());
    }

    #[test]
    fn response_kind_is_enough_to_recognize_a_frame() {
        let f = InboundFrame::parse(r#"{"kind":"response"}"#).expect("frame");
        assert!(f.answers("7"));
    }

    #[test]
    fn unknown_kind_without_response_fields_is_dropped() {
        assert!(InboundFrame::parse(r#"{"kind":"progress"}"#).is_none());
    }

    #[test]
    fn unknown_kind_with_output_is_dropped() {
        assert!(InboundFrame::parse(r#"{"kind":"custom","output":"not an answer"}"#).is_none());
    }

    #[test]
    fn extra_fields_do_not_invalidate_a_response() {
        let f = InboundFrame::parse(r#"{"output":"hi","progress":50}"#).expect("frame");
        assert_eq!(f.output.as_deref(), Some("hi"));
    }

    #[test]
    fn a_failure_without_an_exit_code_is_one() {
        let f = InboundFrame::parse(r#"{"ok":false,"error":"nope"}"#).expect("frame");
        assert_eq!(f.resolved_exit_code(), 1);
        let f = InboundFrame::parse(r#"{"ok":false,"exit_code":42}"#).expect("frame");
        assert_eq!(f.resolved_exit_code(), 42);
    }

    #[test]
    fn a_stale_id_does_not_answer_the_current_request() {
        let f = InboundFrame::parse(r#"{"kind":"response","id":"1","output":"old"}"#).unwrap();
        assert!(!f.answers("2"));
        assert!(f.answers("1"));
    }

    #[test]
    fn ready_is_never_mistaken_for_an_answer() {
        let f = InboundFrame::parse(r#"{"kind":"ready","ok":true}"#).unwrap();
        assert!(f.is_ready());
        assert!(!f.answers("1"));
    }

    #[test]
    fn ready_with_id_is_never_mistaken_for_an_answer() {
        let f = InboundFrame::parse(r#"{"kind":"ready","id":"1","output":"warm"}"#)
            .expect("handshake frame");
        assert!(f.is_ready());
        assert!(!f.answers("1"));
    }

    #[test]
    fn response_with_id_answers_matching_request() {
        let f = InboundFrame::parse(r#"{"kind":"response","id":"1","output":"ok"}"#)
            .expect("response frame");
        assert!(f.answers("1"));
    }

    #[test]
    fn response_without_id_answers_the_request_in_flight() {
        let f =
            InboundFrame::parse(r#"{"kind":"response","output":"ok"}"#).expect("response frame");
        assert!(f.answers("1"));
    }

    #[test]
    fn trigger_is_rebuilt_from_the_env_block() {
        let env = vec![
            ("AGENT_TRIGGER_SOURCE".to_string(), "telegram".to_string()),
            ("AGENT_TRIGGER_ACTOR".to_string(), "123".to_string()),
            (
                "AGENT_TRIGGER_PAYLOAD".to_string(),
                r#"{"text":"oi","chat_id":9}"#.to_string(),
            ),
            ("AGENT_SESSION_ID".to_string(), "chat-9".to_string()),
        ];
        let t = TriggerFrame::from_env(&env).expect("trigger");
        assert_eq!(t.source, "telegram");
        assert_eq!(t.session_id.as_deref(), Some("chat-9"));
        assert_eq!(t.actor.as_deref(), Some("123"));
        assert_eq!(t.payload.unwrap()["chat_id"], 9);
    }

    #[test]
    fn a_scheduled_run_carries_no_trigger() {
        assert!(TriggerFrame::from_env(&[]).is_none());
    }

    #[test]
    fn a_trigger_without_a_session_id_keeps_it_absent() {
        let env = vec![("AGENT_TRIGGER_SOURCE".to_string(), "mcp".to_string())];
        let trigger = TriggerFrame::from_env(&env).expect("trigger");
        assert!(trigger.session_id.is_none());
    }

    #[test]
    fn an_unparseable_payload_still_yields_a_trigger() {
        // The source is attested by the daemon; a broken body should not
        // erase who asked.
        let env = vec![
            ("AGENT_TRIGGER_SOURCE".to_string(), "mcp".to_string()),
            ("AGENT_TRIGGER_PAYLOAD".to_string(), "{{{".to_string()),
        ];
        let t = TriggerFrame::from_env(&env).expect("trigger");
        assert!(t.payload.is_none());
    }

    #[test]
    fn trigger_env_is_recognised() {
        assert!(is_trigger_env("AGENT_TRIGGER_PAYLOAD"));
        assert!(is_trigger_env("AGENT_SESSION_ID"));
        assert!(!is_trigger_env("AGENT_NAME"));
        assert!(!is_trigger_env("TELEGRAM_ASSISTANT_MODEL"));
    }

    #[test]
    fn request_frame_carries_session_id_from_env_and_omits_absent() {
        let with_session = vec![
            ("AGENT_TRIGGER_SOURCE".to_string(), "telegram".to_string()),
            ("AGENT_SESSION_ID".to_string(), "chat-9".to_string()),
        ];
        let without_session = vec![("AGENT_TRIGGER_SOURCE".to_string(), "telegram".to_string())];
        let mut req = RequestFrame {
            v: PROTOCOL_VERSION,
            kind: "request",
            id: "1".into(),
            agent: "x".into(),
            schedule: "trigger".into(),
            args: vec![],
            deadline_seconds: 60,
            trigger: TriggerFrame::from_env(&with_session),
        };

        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["trigger"]["session_id"], "chat-9");

        req.trigger = TriggerFrame::from_env(&without_session);
        let value = serde_json::to_value(&req).unwrap();
        assert!(value["trigger"].get("session_id").is_none());
    }

    #[test]
    fn frames_serialize_as_one_line() {
        let req = RequestFrame {
            v: PROTOCOL_VERSION,
            kind: "request",
            id: "1".into(),
            agent: "x".into(),
            schedule: "trigger".into(),
            args: vec![],
            deadline_seconds: 60,
            trigger: None,
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(!line.contains('\n'), "a frame must be one line: {line}");
        assert!(line.contains(r#""kind":"request""#));
        assert!(
            !line.contains(r#""trigger":"#),
            "an absent trigger is omitted entirely: {line}"
        );
    }
}
