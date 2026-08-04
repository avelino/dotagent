//! Inbound Telegram — the other half of [`crate::telegram`].
//!
//! Long-polls `getUpdates` and hands back plain messages. Deliberately dumb
//! about policy: it does not know about allowlists, rate limits or agents. The
//! daemon owns those decisions because it owns the audit log, and keeping this
//! module to transport makes both halves testable without a network.
//!
//! ## Why long-poll and not a webhook
//!
//! `getUpdates` with a held-open connection reaches near-instant delivery
//! without a public IP, TLS termination or a tunnel. dotagent runs on laptops
//! behind NAT; a webhook would make the common case the hard one.
//!
//! ## Offset
//!
//! Telegram redelivers every update until you acknowledge it by asking for a
//! higher `offset`. That offset is persisted, so a daemon restart resumes
//! instead of replaying the backlog — which for a bot that runs agents would
//! mean re-running whatever the last messages asked for.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::telegram::{client, expand_env, sanitize_reqwest_err, send_message};
use crate::{NotifyError, Result};

/// Telegram's hard cap on a single message body.
const MAX_MESSAGE_CHARS: usize = 4096;

/// One accepted inbound message, flattened to what a trigger needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundMessage {
    pub update_id: i64,
    /// Numeric sender id. The only identity worth trusting — `@username` is
    /// changeable and therefore not an authorization input.
    pub user_id: i64,
    /// Conversation to answer in.
    pub chat_id: i64,
    /// The message being answered. Replies quote it, so a chat with several
    /// questions in flight shows which answer belongs to which.
    pub message_id: i64,
    pub text: String,
    /// Text of the message this one replies to, when the sender used
    /// Telegram's reply.
    ///
    /// A bare "sim" carries no meaning on its own. Quoting the assistant's
    /// question makes the intent explicit instead of leaving it to be inferred
    /// from conversation history, which breaks the moment someone answers an
    /// older message out of order.
    pub reply_to_text: Option<String>,
}

// ---------------------------------------------------------------------
// Bot API wire types (only the fields we use)
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GetUpdatesResponse {
    ok: bool,
    #[serde(default)]
    result: Vec<Update>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    #[serde(default)]
    message: Option<Message>,
}

/// Every field is optional on purpose, even the ones Telegram always sends.
///
/// A required field that a single update happens to lack fails
/// `serde_json::from_*` for the **whole batch**, which in a long-poll means the
/// offset never advances and that batch is redelivered forever. Optional plus
/// a filter in [`extract`] drops the one bad update and keeps the rest.
#[derive(Debug, Deserialize)]
struct Message {
    #[serde(default)]
    message_id: Option<i64>,
    #[serde(default)]
    from: Option<User>,
    #[serde(default)]
    chat: Option<Chat>,
    #[serde(default)]
    text: Option<String>,
    /// Boxed because `Message` would otherwise be infinitely sized.
    #[serde(default)]
    reply_to_message: Option<Box<Message>>,
}

#[derive(Debug, Deserialize)]
struct User {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
}

/// Extract the messages we can act on. Updates without a sender, a chat or
/// text (edits, joins, stickers, channel posts) are skipped — they carry no
/// instruction, and guessing at intent from a sticker is not a feature.
fn extract(updates: Vec<Update>) -> Vec<InboundMessage> {
    updates
        .into_iter()
        .filter_map(|u| {
            let msg = u.message?;
            let from = msg.from?;
            let chat = msg.chat?;
            let text = msg.text?;
            // No message_id means nothing to quote, and quoting id 0 would
            // point at a message that does not exist.
            let message_id = msg.message_id?;
            if text.trim().is_empty() {
                return None;
            }
            let reply_to_text = msg
                .reply_to_message
                .and_then(|r| r.text)
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty());
            Some(InboundMessage {
                update_id: u.update_id,
                user_id: from.id,
                chat_id: chat.id,
                message_id,
                text,
                reply_to_text,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------
// Offset persistence
// ---------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct OffsetFile {
    offset: i64,
}

fn read_offset(path: &Path) -> i64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<OffsetFile>(&raw).ok())
        .map(|f| f.offset)
        .unwrap_or(0)
}

/// Persist via tmp+rename so a crash mid-write cannot leave a truncated file
/// that parses as offset 0 and replays the whole backlog.
fn write_offset(path: &Path, offset: i64) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string(&OffsetFile { offset })?;
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------
// Poller
// ---------------------------------------------------------------------

/// Long-poll loop state.
#[derive(Debug)]
pub struct Poller {
    bot_token: String,
    timeout_seconds: u32,
    offset_path: PathBuf,
    offset: i64,
}

impl Poller {
    /// `offset_path` is where the acknowledged update id is persisted.
    pub fn new(bot_token: impl Into<String>, timeout_seconds: u32, offset_path: PathBuf) -> Self {
        let offset = read_offset(&offset_path);
        Self {
            bot_token: bot_token.into(),
            // Telegram rejects anything above 50.
            timeout_seconds: timeout_seconds.min(50),
            offset_path,
            offset,
        }
    }

    /// Hold one `getUpdates` call open and return whatever arrived.
    ///
    /// Advances and persists the offset before returning, so a message is
    /// delivered at most once even if handling it panics later. For a bot that
    /// runs agents, at-most-once beats at-least-once: a redelivered message
    /// would re-run whatever it asked for.
    pub async fn poll(&mut self) -> Result<Vec<InboundMessage>> {
        let token = expand_env(&self.bot_token).map_err(NotifyError::Config)?;
        let url = format!("https://api.telegram.org/bot{token}/getUpdates");
        let body = serde_json::json!({
            "offset": self.offset,
            "timeout": self.timeout_seconds,
            "allowed_updates": ["message"],
        });

        let res = match client().post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => return Err(NotifyError::Backend(sanitize_reqwest_err(&e))),
        };
        if !res.status().is_success() {
            return Err(NotifyError::Backend(format!(
                "telegram getUpdates returned {}",
                res.status()
            )));
        }
        let parsed: GetUpdatesResponse = match res.json().await {
            Ok(p) => p,
            Err(e) => return Err(NotifyError::Backend(sanitize_reqwest_err(&e))),
        };
        if !parsed.ok {
            return Err(NotifyError::Backend(format!(
                "telegram getUpdates rejected: {}",
                parsed.description.unwrap_or_else(|| "no reason".into())
            )));
        }

        if let Some(highest) = parsed.result.iter().map(|u| u.update_id).max() {
            self.offset = highest + 1;
            if let Err(e) = write_offset(&self.offset_path, self.offset) {
                // Losing the offset means replaying on restart, which is worth
                // a loud log but not worth dropping messages we already have.
                tracing::warn!(error = %e, "could not persist telegram offset");
            }
        }
        Ok(extract(parsed.result))
    }
}

/// Answer a conversation, quoting the message being answered.
///
/// Sent as plain text on purpose: the body is agent output, and any parse mode
/// would make an unescaped backtick or underscore in a log line turn into a
/// Bot API 400. Correctness of delivery beats formatting.
///
/// `reply_to` quotes the original message. Runs are asynchronous and a chat can
/// have several questions in flight, so without the quote there is no telling
/// which answer belongs to which.
pub async fn reply(bot_token: &str, chat_id: i64, reply_to: Option<i64>, text: &str) -> Result<()> {
    send_message(
        bot_token,
        &chat_id.to_string(),
        &truncate(text),
        None,
        false,
        reply_to,
    )
    .await
}

/// How often to re-send the typing indicator.
///
/// Telegram clears it after ~5s, so a run that takes 15 seconds needs the
/// action repeated or the chat looks idle halfway through. Four seconds leaves
/// margin without hammering the API.
pub const TYPING_REFRESH: std::time::Duration = std::time::Duration::from_secs(4);

/// Show "typing…" in the chat.
///
/// Best-effort by design: a failed indicator is cosmetic, and letting it
/// interrupt the run would trade a working answer for a spinner.
pub async fn typing(bot_token: &str, chat_id: i64) -> Result<()> {
    let token = expand_env(bot_token).map_err(NotifyError::Config)?;
    let url = format!("https://api.telegram.org/bot{token}/sendChatAction");
    let body = serde_json::json!({ "chat_id": chat_id, "action": "typing" });
    match client().post(&url).json(&body).send().await {
        Ok(r) if r.status().is_success() => Ok(()),
        Ok(r) => Err(NotifyError::Backend(format!(
            "telegram sendChatAction returned {}",
            r.status()
        ))),
        Err(e) => Err(NotifyError::Backend(sanitize_reqwest_err(&e))),
    }
}

/// Trim to Telegram's limit, saying so rather than silently losing the tail.
fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_MESSAGE_CHARS {
        return text.to_string();
    }
    const NOTICE: &str = "\n[truncated]";
    let keep = MAX_MESSAGE_CHARS - NOTICE.chars().count();
    let head: String = text.chars().take(keep).collect();
    format!("{head}{NOTICE}")
}

// ---------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------

/// Per-sender sliding window.
///
/// In memory on purpose: a daemon restart clearing the window is acceptable,
/// while persisting it would add a write to every inbound message. The bot is
/// reachable from anywhere, so this is the backstop that keeps one sender from
/// occupying the daemon indefinitely.
#[derive(Debug, Default)]
pub struct RateLimiter {
    per_minute: u32,
    hits: Vec<(i64, std::time::Instant)>,
}

impl RateLimiter {
    pub fn new(per_minute: u32) -> Self {
        Self {
            per_minute,
            hits: Vec::new(),
        }
    }

    /// Record an attempt and report whether it is allowed.
    pub fn check(&mut self, user_id: i64) -> bool {
        self.check_at(user_id, std::time::Instant::now())
    }

    /// Injectable clock so the window is testable without sleeping.
    fn check_at(&mut self, user_id: i64, now: std::time::Instant) -> bool {
        let window = std::time::Duration::from_secs(60);
        self.hits.retain(|(_, t)| now.duration_since(*t) < window);
        let count = self.hits.iter().filter(|(u, _)| *u == user_id).count();
        if count as u32 >= self.per_minute {
            return false;
        }
        self.hits.push((user_id, now));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn updates_json(raw: &str) -> Vec<Update> {
        serde_json::from_str::<GetUpdatesResponse>(raw)
            .unwrap()
            .result
    }

    #[test]
    fn extracts_a_plain_text_message() {
        let raw = r#"{"ok":true,"result":[
            {"update_id":10,"message":{"message_id":7,"from":{"id":42},"chat":{"id":99},"text":"hello"}}
        ]}"#;
        let msgs = extract(updates_json(raw));
        assert_eq!(
            msgs,
            vec![InboundMessage {
                update_id: 10,
                user_id: 42,
                chat_id: 99,
                message_id: 7,
                text: "hello".into(),
                reply_to_text: None
            }]
        );
    }

    #[test]
    fn carries_the_quoted_text_when_the_sender_replied() {
        // "sim" on its own is meaningless. This is what makes it resolvable
        // without relying on conversation history.
        let raw = r#"{"ok":true,"result":[
            {"update_id":1,"message":{"message_id":9,"from":{"id":1},"chat":{"id":2},
             "text":"sim",
             "reply_to_message":{"message_id":8,"from":{"id":99},"chat":{"id":2},
              "text":"Confirmando: criar pagina. Posso criar?"}}}
        ]}"#;
        let m = &extract(updates_json(raw))[0];
        assert_eq!(m.text, "sim");
        assert_eq!(
            m.reply_to_text.as_deref(),
            Some("Confirmando: criar pagina. Posso criar?")
        );
    }

    #[test]
    fn a_message_without_a_reply_has_no_quoted_text() {
        let raw = r#"{"ok":true,"result":[
            {"update_id":1,"message":{"message_id":9,"from":{"id":1},"chat":{"id":2},"text":"oi"}}
        ]}"#;
        assert!(extract(updates_json(raw))[0].reply_to_text.is_none());
    }

    #[test]
    fn a_reply_to_a_photo_carries_no_quoted_text() {
        // reply_to_message exists but has no text (sticker, photo, voice).
        // An empty quote is worse than none: it would read as context.
        let raw = r#"{"ok":true,"result":[
            {"update_id":1,"message":{"message_id":9,"from":{"id":1},"chat":{"id":2},
             "text":"sim",
             "reply_to_message":{"message_id":8,"chat":{"id":2}}}}
        ]}"#;
        assert!(extract(updates_json(raw))[0].reply_to_text.is_none());
    }

    #[test]
    fn carries_the_message_id_for_quoting() {
        // Runs are async and a chat can have several questions in flight, so
        // the reply has to quote what it answers.
        let raw = r#"{"ok":true,"result":[
            {"update_id":10,"message":{"message_id":4242,"from":{"id":42},"chat":{"id":99},"text":"hi"}}
        ]}"#;
        assert_eq!(extract(updates_json(raw))[0].message_id, 4242);
    }

    #[test]
    fn an_update_without_message_id_is_skipped_not_defaulted() {
        // Defaulting to 0 would make the reply quote a message that does not
        // exist. Skipping is the honest answer.
        let raw = r#"{"ok":true,"result":[
            {"update_id":1,"message":{"from":{"id":42},"chat":{"id":99},"text":"hi"}}
        ]}"#;
        assert!(extract(updates_json(raw)).is_empty());
    }

    #[test]
    fn one_malformed_update_does_not_drop_the_batch() {
        // The failure this guards is nastier than a lost message: if parsing
        // the batch fails, poll() errors, the offset never advances, and
        // Telegram redelivers the same batch forever.
        let raw = r#"{"ok":true,"result":[
            {"update_id":1,"message":{"from":{"id":42},"text":"no chat"}},
            {"update_id":2,"message":{"message_id":9,"from":{"id":7},"chat":{"id":8},"text":"ok"}}
        ]}"#;
        let msgs = extract(updates_json(raw));
        assert_eq!(msgs.len(), 1, "the healthy update must survive");
        assert_eq!(msgs[0].message_id, 9);
    }

    #[test]
    fn skips_updates_without_text() {
        let raw = r#"{"ok":true,"result":[
            {"update_id":1,"message":{"message_id":1,"from":{"id":42},"chat":{"id":99}}}
        ]}"#;
        assert!(extract(updates_json(raw)).is_empty());
    }

    #[test]
    fn skips_updates_without_a_sender() {
        // Channel posts have no `from`, so there is no identity to authorize.
        let raw = r#"{"ok":true,"result":[
            {"update_id":1,"message":{"message_id":1,"chat":{"id":99},"text":"hi"}}
        ]}"#;
        assert!(extract(updates_json(raw)).is_empty());
    }

    #[test]
    fn skips_whitespace_only_text() {
        let raw = r#"{"ok":true,"result":[
            {"update_id":1,"message":{"message_id":1,"from":{"id":42},"chat":{"id":99},"text":"   "}}
        ]}"#;
        assert!(extract(updates_json(raw)).is_empty());
    }

    #[test]
    fn skips_unknown_update_kinds_without_failing_the_batch() {
        let raw = r#"{"ok":true,"result":[
            {"update_id":1,"edited_message":{"message_id":1,"from":{"id":42},"chat":{"id":99},"text":"x"}},
            {"update_id":2,"message":{"message_id":2,"from":{"id":7},"chat":{"id":8},"text":"ok"}}
        ]}"#;
        let msgs = extract(updates_json(raw));
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].user_id, 7);
    }

    #[test]
    fn offset_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("offset.json");
        assert_eq!(read_offset(&path), 0, "missing file reads as zero");
        write_offset(&path, 137).unwrap();
        assert_eq!(read_offset(&path), 137);
    }

    #[test]
    fn corrupt_offset_file_reads_as_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("offset.json");
        std::fs::write(&path, "{ truncated").unwrap();
        assert_eq!(read_offset(&path), 0);
    }

    #[test]
    fn poller_clamps_timeout_to_telegram_max() {
        let dir = tempfile::tempdir().unwrap();
        let p = Poller::new("t", 5000, dir.path().join("offset.json"));
        assert_eq!(p.timeout_seconds, 50);
    }

    #[test]
    fn poller_resumes_from_persisted_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("offset.json");
        write_offset(&path, 500).unwrap();
        let p = Poller::new("t", 30, path);
        assert_eq!(p.offset, 500);
    }

    #[test]
    fn truncate_leaves_short_text_alone() {
        assert_eq!(truncate("hi"), "hi");
    }

    #[test]
    fn truncate_marks_long_text_and_respects_the_cap() {
        let long = "x".repeat(MAX_MESSAGE_CHARS + 500);
        let out = truncate(&long);
        assert!(out.ends_with("[truncated]"));
        assert!(out.chars().count() <= MAX_MESSAGE_CHARS);
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        // Multi-byte input must not be cut mid-character nor over-trimmed.
        let long = "é".repeat(MAX_MESSAGE_CHARS + 10);
        let out = truncate(&long);
        assert!(out.chars().count() <= MAX_MESSAGE_CHARS);
    }

    // --- rate limiter: deny-side coverage outweighs allow-side ---

    #[test]
    fn rate_limiter_allows_up_to_the_cap() {
        let mut rl = RateLimiter::new(3);
        let t = std::time::Instant::now();
        assert!(rl.check_at(1, t));
        assert!(rl.check_at(1, t));
        assert!(rl.check_at(1, t));
    }

    #[test]
    fn rate_limiter_denies_past_the_cap() {
        let mut rl = RateLimiter::new(2);
        let t = std::time::Instant::now();
        assert!(rl.check_at(1, t));
        assert!(rl.check_at(1, t));
        assert!(!rl.check_at(1, t), "third hit in the window must be denied");
        assert!(!rl.check_at(1, t));
    }

    #[test]
    fn rate_limiter_denies_zero_cap_entirely() {
        let mut rl = RateLimiter::new(0);
        assert!(!rl.check_at(1, std::time::Instant::now()));
    }

    #[test]
    fn rate_limiter_is_per_user() {
        let mut rl = RateLimiter::new(1);
        let t = std::time::Instant::now();
        assert!(rl.check_at(1, t));
        assert!(!rl.check_at(1, t));
        assert!(rl.check_at(2, t), "a busy sender must not block another");
    }

    #[test]
    fn rate_limiter_forgets_after_the_window() {
        let mut rl = RateLimiter::new(1);
        let t = std::time::Instant::now();
        assert!(rl.check_at(1, t));
        assert!(!rl.check_at(1, t));
        let later = t + std::time::Duration::from_secs(61);
        assert!(rl.check_at(1, later));
    }
}
