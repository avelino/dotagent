//! Delivery sinks: where a conversation's answer goes.
//!
//! A sink is the "any client becomes just another transport" half of the
//! gateway: the worker shapes a reply and hands it to whatever sink the
//! submitter provided. Sinks are transport-only — they hold no conversation
//! state, know nothing about agents, and never interpret the session id
//! beyond passing it along.
//!
//! # Error contract
//!
//! Methods return `()` on purpose: a delivery failure is the sink's own
//! business, logged inside the implementation (the same posture as
//! the old direct-delivery path). A sink must never panic — a panic unwinds into
//! the conversation worker and loses the reply it was delivering.

use std::future::Future;
use std::pin::Pin;
#[cfg(test)]
use std::sync::Arc;

use tracing::{debug, warn};

/// The future returned by each asynchronous [`ReplySink`] delivery method.
pub type SinkFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Where a gateway worker delivers typing indicators, streamed lines and the
/// final reply.
///
/// `session` is the trigger's opaque `session_id` when present. A sink with
/// no concept of sessions (Telegram delivers to a fixed chat, fixed at
/// construction) may ignore it.
pub trait ReplySink: Send + Sync {
    /// Notify the transport that admission and trigger auditing succeeded.
    /// This is synchronous so the gateway can emit it before enqueueing the
    /// job whose output follows it.
    fn started(&self, session: Option<&str>, agent: &str);
    /// Deliver the final reply for one run.
    fn reply<'a>(&'a self, session: Option<&'a str>, text: &'a str) -> SinkFuture<'a>;
    /// Show a "working on it" indicator.
    fn typing<'a>(&'a self, session: Option<&'a str>) -> SinkFuture<'a>;
    /// One raw stdout line, in arrival order. The line is raw on purpose —
    /// the client interprets it (assistant protocol frames included);
    /// store-and-forward transports ignore it.
    fn delta<'a>(&'a self, session: Option<&'a str>, line: &'a str) -> SinkFuture<'a>;
}

/// Telegram transport.
///
/// Replies are plain text (no parse mode: agent output with a stray backtick
/// would turn into a Bot API 400) and quote the message being answered, so a
/// chat with several questions in flight shows which answer belongs to
/// which. Typing maps to the chat action. Deltas are a no-op — the Bot API
/// has no streaming, so phase 1 delivers only the final reply.
pub struct TelegramSink {
    bot_token: String,
    chat_id: i64,
    message_id: Option<i64>,
}

impl TelegramSink {
    /// `message_id` is the inbound message a reply quotes (`None` sends
    /// unquoted rather than dropping the answer).
    pub fn new(bot_token: impl Into<String>, chat_id: i64, message_id: Option<i64>) -> Self {
        Self {
            bot_token: bot_token.into(),
            chat_id,
            message_id,
        }
    }
}

impl ReplySink for TelegramSink {
    fn started(&self, _session: Option<&str>, _agent: &str) {}

    fn reply<'a>(&'a self, _session: Option<&'a str>, text: &'a str) -> SinkFuture<'a> {
        let token = self.bot_token.clone();
        let chat_id = self.chat_id;
        let message_id = self.message_id;
        let text = text.to_string();
        Box::pin(async move {
            if let Err(e) =
                dotagent_notify::telegram_inbound::reply(&token, chat_id, message_id, &text).await
            {
                // Already token-sanitized by the transport layer.
                warn!(error = %e, chat_id, "gateway: telegram reply failed");
            }
        })
    }

    fn typing<'a>(&'a self, _session: Option<&'a str>) -> SinkFuture<'a> {
        let token = self.bot_token.clone();
        let chat_id = self.chat_id;
        Box::pin(async move {
            if let Err(e) = dotagent_notify::telegram_inbound::typing(&token, chat_id).await {
                // Cosmetic by design: a failed indicator must never surface.
                debug!(error = %e, chat_id, "gateway: telegram typing failed");
            }
        })
    }

    fn delta<'a>(&'a self, _session: Option<&'a str>, _line: &'a str) -> SinkFuture<'a> {
        Box::pin(std::future::ready(()))
    }
}

/// Fan one conversation out to several sinks (e.g. Telegram plus a local
/// connection sink).
///
/// Sinks are called in registration order and one sink's internal failure —
/// logged inside it, per the trait contract — never reaches the others.
#[cfg(test)]
pub struct FanoutSink {
    sinks: Vec<Arc<dyn ReplySink>>,
}

#[cfg(test)]
impl FanoutSink {
    pub fn new(sinks: Vec<Arc<dyn ReplySink>>) -> Self {
        Self { sinks }
    }
}

#[cfg(test)]
impl ReplySink for FanoutSink {
    fn started(&self, session: Option<&str>, agent: &str) {
        for sink in &self.sinks {
            sink.started(session, agent);
        }
    }

    fn reply<'a>(&'a self, session: Option<&'a str>, text: &'a str) -> SinkFuture<'a> {
        let sinks = self.sinks.clone();
        let session = session.map(str::to_string);
        let text = text.to_string();
        Box::pin(async move {
            for sink in &sinks {
                sink.reply(session.as_deref(), &text).await;
            }
        })
    }

    fn typing<'a>(&'a self, session: Option<&'a str>) -> SinkFuture<'a> {
        let sinks = self.sinks.clone();
        let session = session.map(str::to_string);
        Box::pin(async move {
            for sink in &sinks {
                sink.typing(session.as_deref()).await;
            }
        })
    }

    fn delta<'a>(&'a self, session: Option<&'a str>, line: &'a str) -> SinkFuture<'a> {
        let sinks = self.sinks.clone();
        let session = session.map(str::to_string);
        let line = line.to_string();
        Box::pin(async move {
            for sink in &sinks {
                sink.delta(session.as_deref(), &line).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::testutil::{FailingSink, RecordingSink};

    #[tokio::test]
    async fn fanout_delivers_to_every_sink() {
        let a = Arc::new(RecordingSink::default());
        let b = Arc::new(RecordingSink::default());
        let fan = FanoutSink::new(vec![a.clone(), b.clone()]);

        fan.reply(Some("s1"), "hi").await;
        fan.typing(Some("s1")).await;
        fan.delta(Some("s1"), "line").await;
        fan.started(Some("s1"), "agent");

        assert_eq!(a.events().len(), 4);
        assert_eq!(b.events().len(), 4);
    }

    #[tokio::test]
    async fn fanout_survives_a_sink_that_fails() {
        let healthy = Arc::new(RecordingSink::default());
        let failing = Arc::new(FailingSink::default());
        let fan = FanoutSink::new(vec![failing.clone(), healthy.clone()]);

        fan.reply(None, "hi").await;

        assert_eq!(failing.attempts(), 1, "the failing sink was tried");
        assert_eq!(
            healthy.replies(),
            1,
            "the healthy sink must still be served"
        );
    }

    #[tokio::test]
    async fn telegram_delta_is_a_noop() {
        let sink = TelegramSink::new("token", 1, Some(10));
        // No network, no panic: phase 1 Telegram does not stream.
        sink.delta(None, "streamed line").await;
    }
}
