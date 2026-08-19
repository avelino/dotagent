//! Pure wire types for the local API socket.
//!
//! No IO lives here — [`super::server`] owns the transport; these shapes are
//! the contract between it, the daemon's gateway integration, and any client
//! (TUI or otherwise). Parsing is lenient where the protocol allows it
//! (request ids may be strings or numbers) and strict where safety demands
//! it (session ids, text bounds).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use dotagent_core::slug::is_valid_session_id;

/// Ceiling for the `text` field of a `message.send`. 32 KB is far beyond a
/// human-sized prompt and far below anything that could pressure the daemon.
pub const MAX_TEXT_BYTES: usize = 32 * 1024;

/// Concurrent client connections the server will actually serve.
pub const MAX_CONNECTIONS: usize = 16;

/// Per-connection budget for events queued but not yet written to the
/// client. A consumer this far behind is disconnected, not buffered forever.
pub const MAX_EVENT_QUEUE_BYTES: usize = 1024 * 1024;

/// Requests per minute, per connection (sliding window).
pub const RATE_PER_MINUTE: u32 = 30;

/// Session used when a `message.send` carries no explicit one.
pub const DEFAULT_SESSION_ID: &str = "default";

/// Error codes on the wire. The set is closed: a client can branch on these
/// without a default arm lying about it.
pub mod error_code {
    /// Unparseable line, unknown method, bad params, oversized text.
    pub const INVALID_REQUEST: &str = "invalid_request";
    /// The connection exceeded its per-minute request budget.
    pub const RATE_LIMITED: &str = "rate_limited";
    /// `session_id` failed validation (charset `[A-Za-z0-9_-]`, 1..=64).
    pub const SESSION_ID_INVALID: &str = "session_id_invalid";
    /// Sent to connections accepted above [`MAX_CONNECTIONS`].
    pub const TOO_MANY_CONNECTIONS: &str = "too_many_connections";
    /// The handler could not do its job. Deliberately vague on the wire.
    pub const INTERNAL: &str = "internal";
}

/// Accept a request id as a string or a number, normalized to a string.
///
/// TUIs like auto-incrementing integers; humans like names. Both are
/// correlation tokens, nothing more, so the wire takes either and the
/// response always echoes a string.
fn deserialize_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Value::deserialize(deserializer)? {
        Value::String(s) => Ok(s),
        Value::Number(n) => Ok(n.to_string()),
        other => Err(serde::de::Error::custom(format!(
            "id must be a string or a number, got: {other}"
        ))),
    }
}

/// One request line from a client.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientMessage {
    #[serde(deserialize_with = "deserialize_id")]
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

/// Best-effort id recovery from a line that failed typed parsing, so an
/// error response can still be correlated. Anything that is not a string or
/// a number yields `""`.
pub fn salvage_id(raw: &Value) -> String {
    match raw.get("id") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// The error half of a response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServerError {
    pub code: String,
    pub message: String,
}

impl ServerError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// One response line to a client. Exactly one of `result` / `error` is set;
/// the other is absent from the wire.
#[derive(Debug, Serialize)]
pub struct ServerResponse {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ServerError>,
}

impl ServerResponse {
    pub fn ok(id: impl Into<String>, result: Value) -> Self {
        Self {
            id: id.into(),
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: impl Into<String>, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            result: None,
            error: Some(ServerError::new(code, message)),
        }
    }
}

/// One server-initiated event line. Constructors cover the four kinds the
/// harness emits; extra fields stay off the wire when unset.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServerEvent {
    pub event: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl ServerEvent {
    /// The run behind this session started thinking. Cosmetic, but it is
    /// what keeps a TUI from looking dead during a long agent run.
    pub fn typing(session_id: impl Into<String>) -> Self {
        Self {
            event: "typing",
            session_id: Some(session_id.into()),
            agent: None,
            line: None,
            text: None,
        }
    }

    /// An agent run was dispatched for this session.
    pub fn run_started(session_id: impl Into<String>, agent: impl Into<String>) -> Self {
        Self {
            event: "run.started",
            session_id: Some(session_id.into()),
            agent: Some(agent.into()),
            line: None,
            text: None,
        }
    }

    /// One line of streaming output.
    pub fn reply_delta(session_id: impl Into<String>, line: impl Into<String>) -> Self {
        Self {
            event: "reply.delta",
            session_id: Some(session_id.into()),
            agent: None,
            line: Some(line.into()),
            text: None,
        }
    }

    /// The complete reply. Terminal for a request's event stream.
    pub fn reply(session_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            event: "reply",
            session_id: Some(session_id.into()),
            agent: None,
            line: None,
            text: Some(text.into()),
        }
    }
}

/// Params of `message.send`.
#[derive(Debug, Clone, Deserialize)]
pub struct MessageSendParams {
    #[serde(default)]
    pub session_id: Option<String>,
    pub text: String,
}

impl MessageSendParams {
    /// The session a trigger belongs to: the explicit one, or `"default"`.
    pub fn effective_session_id(&self) -> &str {
        self.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID)
    }

    /// Enforce the bounds the threat model cares about before the text
    /// travels any further: non-empty, bounded, and keyed to a session id
    /// that cannot smuggle path or traversal characters into a filename, a
    /// label or a log line.
    pub fn validate(&self) -> Result<(), ServerError> {
        if self.text.trim().is_empty() {
            return Err(ServerError::new(
                error_code::INVALID_REQUEST,
                "text must not be empty",
            ));
        }
        if self.text.len() > MAX_TEXT_BYTES {
            return Err(ServerError::new(
                error_code::INVALID_REQUEST,
                format!("text exceeds {MAX_TEXT_BYTES} bytes"),
            ));
        }
        if !is_valid_session_id(self.effective_session_id()) {
            return Err(ServerError::new(
                error_code::SESSION_ID_INVALID,
                format!(
                    "session_id must match ^[A-Za-z0-9_-]{{1,64}}$, got: {:?}",
                    self.effective_session_id()
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_numeric_id_is_normalized_to_a_string() {
        let msg: ClientMessage =
            serde_json::from_str(r#"{"id":42,"method":"commands.list"}"#).unwrap();
        assert_eq!(msg.id, "42");
        assert_eq!(msg.method, "commands.list");
        assert!(msg.params.is_none());
    }

    #[test]
    fn a_string_id_survives_verbatim() {
        let msg: ClientMessage =
            serde_json::from_str(r#"{"id":"u1","method":"status.get","params":{"x":1}}"#).unwrap();
        assert_eq!(msg.id, "u1");
        assert_eq!(msg.params, Some(json!({ "x": 1 })));
    }

    #[test]
    fn an_id_that_is_neither_string_nor_number_is_rejected() {
        let raw = r#"{"id":true,"method":"commands.list"}"#;
        assert!(serde_json::from_str::<ClientMessage>(raw).is_err());
    }

    #[test]
    fn a_request_without_an_id_or_method_is_rejected() {
        assert!(serde_json::from_str::<ClientMessage>(r#"{"method":"m"}"#).is_err());
        assert!(serde_json::from_str::<ClientMessage>(r#"{"id":"u1"}"#).is_err());
    }

    #[test]
    fn salvage_recovers_string_and_numeric_ids_but_not_garbage() {
        assert_eq!(salvage_id(&json!({ "id": "u9" })), "u9");
        assert_eq!(salvage_id(&json!({ "id": 7 })), "7");
        assert_eq!(salvage_id(&json!({ "id": null })), "");
        assert_eq!(salvage_id(&json!({ "method": "m" })), "");
    }

    #[test]
    fn response_omits_the_unset_half() {
        let ok = serde_json::to_value(ServerResponse::ok("u1", json!({"accepted": true}))).unwrap();
        assert_eq!(ok, json!({"id": "u1", "result": {"accepted": true}}));

        let err =
            serde_json::to_value(ServerResponse::err("u2", error_code::INTERNAL, "boom")).unwrap();
        assert_eq!(
            err,
            json!({"id": "u2", "error": {"code": "internal", "message": "boom"}})
        );
    }

    #[test]
    fn event_constructors_emit_exactly_the_documented_fields() {
        assert_eq!(
            serde_json::to_value(ServerEvent::typing("default")).unwrap(),
            json!({"event": "typing", "session_id": "default"})
        );
        assert_eq!(
            serde_json::to_value(ServerEvent::run_started("s1", "disk-alert")).unwrap(),
            json!({"event": "run.started", "session_id": "s1", "agent": "disk-alert"})
        );
        assert_eq!(
            serde_json::to_value(ServerEvent::reply_delta("s1", "half a line")).unwrap(),
            json!({"event": "reply.delta", "session_id": "s1", "line": "half a line"})
        );
        assert_eq!(
            serde_json::to_value(ServerEvent::reply("s1", "all done")).unwrap(),
            json!({"event": "reply", "session_id": "s1", "text": "all done"})
        );
    }

    #[test]
    fn session_defaults_to_default_when_absent() {
        let params: MessageSendParams = serde_json::from_str(r#"{"text":"hi"}"#).unwrap();
        assert_eq!(params.effective_session_id(), "default");

        let params: MessageSendParams =
            serde_json::from_str(r#"{"session_id":"chat-9_a","text":"hi"}"#).unwrap();
        assert_eq!(params.effective_session_id(), "chat-9_a");
    }

    #[test]
    fn validate_rejects_empty_and_whitespace_text() {
        for text in ["", "   ", "\t\n"] {
            let params: MessageSendParams =
                serde_json::from_value(json!({ "text": text })).unwrap();
            let err = params.validate().unwrap_err();
            assert_eq!(err.code, error_code::INVALID_REQUEST, "{text:?}");
        }
    }

    #[test]
    fn validate_rejects_oversized_text() {
        let params: MessageSendParams =
            serde_json::from_value(json!({ "text": "a".repeat(MAX_TEXT_BYTES + 1) })).unwrap();
        assert_eq!(
            params.validate().unwrap_err().code,
            error_code::INVALID_REQUEST
        );
    }

    #[test]
    fn validate_rejects_a_traversal_shaped_session_id() {
        let params: MessageSendParams =
            serde_json::from_value(json!({ "session_id": "../../etc/passwd", "text": "hi" }))
                .unwrap();
        assert_eq!(
            params.validate().unwrap_err().code,
            error_code::SESSION_ID_INVALID
        );
    }

    #[test]
    fn validate_rejects_an_overlong_session_id() {
        let params: MessageSendParams = serde_json::from_value(json!({
            "session_id": "a".repeat(65),
            "text": "hi"
        }))
        .unwrap();
        assert_eq!(
            params.validate().unwrap_err().code,
            error_code::SESSION_ID_INVALID
        );
    }

    #[test]
    fn validate_accepts_the_boring_good_case() {
        let params: MessageSendParams =
            serde_json::from_value(json!({ "session_id": "chat-9_a", "text": "status?" })).unwrap();
        assert!(params.validate().is_ok());
    }
}
