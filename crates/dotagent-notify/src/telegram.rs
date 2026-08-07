//! Telegram notifications via the Bot API (`api.telegram.org`).
//!
//! Sends a `sendMessage` POST with the formatted body. `bot_token`
//! supports `${VAR}` interpolation so secrets stay out of the manifest
//! file — declare `bot_token = "${TELEGRAM_BOT_TOKEN}"` and put the
//! actual value in `~/.config/dotagent/secrets.env` (preferred) or
//! export it into the daemon's environment.
//!
//! Resolution order at send time:
//! 1. [`dotagent_secrets::lookup`] — the daemon-loaded secrets store.
//! 2. `std::env::var` — fallback for operators who wired vars into the
//!    plist / systemd unit directly.
//!
//! Tokens never reach `tracing` output: transport errors are sanitized to
//! a kind + status, the API's own error description is redacted before it is
//! surfaced, and `Debug` redacts the token explicitly.
//!
//! Message bodies are escaped for the configured parse mode and then trimmed
//! to Telegram's 4096-char cap — in that order, see [`render_body`].

use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::limits::{truncate_chars_with, TRUNCATION_NOTICE};
use crate::redact::scrub_detail;
use crate::secrets::expand_env;
use crate::{Notifier, NotifyContext, NotifyError, Result};

/// Telegram Bot API parse mode. Defaults to plain text.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ParseMode {
    #[serde(rename = "MarkdownV2")]
    MarkdownV2,
    #[serde(rename = "HTML")]
    Html,
    /// Legacy "Markdown" (V1). Discouraged by Telegram, kept for completeness.
    #[serde(rename = "Markdown")]
    MarkdownLegacy,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    /// Bot token. Accepts `${VAR}` for env interpolation (the env var is
    /// resolved at send time, not at TOML parse time).
    pub bot_token: String,
    /// Numeric chat id (`-100…` for channels/groups) or `@channel_username`.
    /// Telegram accepts both; serialized as a string either way.
    pub chat_id: String,
    /// Optional `parse_mode`. When set to `MarkdownV2`, message bodies are
    /// auto-escaped per Telegram's strict V2 rules.
    #[serde(default)]
    pub parse_mode: Option<ParseMode>,
    /// Optional `disable_notification` (silent message).
    #[serde(default)]
    pub disable_notification: Option<bool>,
}

impl fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("bot_token", &"<redacted>")
            .field("chat_id", &self.chat_id)
            .field("parse_mode", &self.parse_mode)
            .field("disable_notification", &self.disable_notification)
            .finish()
    }
}

/// Manifest key this driver's credential is configured under. Reaches the
/// operator verbatim in `${VAR}` expansion errors, so it has to match what is
/// actually written in `agent.toml`.
pub(crate) const BOT_TOKEN_FIELD: &str = "telegram.bot_token";

/// Convert a `reqwest::Error` into a short, URL-free message. Our URL embeds
/// the bot token (`api.telegram.org/bot<token>/sendMessage`), so the raw error
/// must never reach `tracing` — see [`crate::redact`].
pub(crate) fn sanitize_reqwest_err(e: &reqwest::Error) -> String {
    crate::redact::sanitize_reqwest_err("telegram", e)
}

/// Escape per Telegram MarkdownV2 reserved characters.
/// Source: <https://core.telegram.org/bots/api#markdownv2-style>.
///
/// `\` is escaped along with the documented set even though the spec does not
/// list it: a raw backslash in agent output opens an escape sequence Telegram
/// then cannot close, and the message dies with `can't parse entities`. Any
/// char in 1..=126 may be escaped, so `\\` is always legal.
pub(crate) fn escape_markdown_v2(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '_'
                | '*'
                | '['
                | ']'
                | '('
                | ')'
                | '~'
                | '`'
                | '>'
                | '#'
                | '+'
                | '-'
                | '='
                | '|'
                | '{'
                | '}'
                | '.'
                | '!'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Telegram's hard cap on a single message body.
pub(crate) const MAX_MESSAGE_CHARS: usize = 4096;

/// Trim to Telegram's limit, saying so rather than silently losing the tail.
///
/// `parse_mode` is the mode `text` was *already* rendered for — see
/// [`render_body`] for why the order is escape-then-truncate. The cut itself
/// is [`crate::limits::truncate_chars_with`]; what is Telegram-specific is the
/// notice needing the same escaping as the body, and the dangling-escape guard.
pub(crate) fn truncate(text: &str, parse_mode: Option<ParseMode>) -> String {
    let markdown_v2 = matches!(parse_mode, Some(ParseMode::MarkdownV2));
    // The notice is part of the body and obeys the same rules as the body:
    // `[` and `]` are reserved, so an unescaped notice is itself a 400.
    let notice = if markdown_v2 {
        escape_markdown_v2(TRUNCATION_NOTICE)
    } else {
        TRUNCATION_NOTICE.to_string()
    };
    truncate_chars_with(text, MAX_MESSAGE_CHARS, &notice, |head| {
        if !markdown_v2 {
            return;
        }
        // The cut can land between a backslash and the char it escapes, and
        // Telegram rejects the whole message over one dangling escape. Every
        // backslash in escaped output is either an opener or the escaped half
        // of `\\`, so an odd trailing run means the pair lost its partner.
        let trailing = head.chars().rev().take_while(|c| *c == '\\').count();
        if trailing % 2 == 1 {
            head.pop();
        }
    })
}

/// Render the wire body: escape for the parse mode, then trim to the cap.
///
/// Order matters, and it is escape-first. Escaping inserts a backslash per
/// reserved char, so it *grows* the text — a body that fits under 4096 before
/// escaping can blow past it after, which is a 400 the truncation was supposed
/// to prevent. Trimming the escaped form is the only way to bound what
/// actually goes on the wire; the cost is the dangling-escape guard inside
/// [`truncate`].
pub(crate) fn render_body(text: &str, parse_mode: Option<ParseMode>) -> String {
    let rendered = match parse_mode {
        Some(ParseMode::MarkdownV2) => escape_markdown_v2(text),
        _ => text.to_string(),
    };
    truncate(&rendered, parse_mode)
}

/// Pull a human-readable reason out of a non-2xx Bot API response.
///
/// The API answers `{"ok":false,"error_code":400,"description":"…"}`, and that
/// description is the only thing that says *why*. Logging the bare status is
/// what made 113 undelivered alerts indistinguishable from one another.
///
/// The body is API-controlled and should never carry the token, but it ends up
/// in `tracing` — so it goes through [`crate::redact::scrub_detail`] regardless,
/// which also flattens it to one bounded line so an HTML error page from a
/// proxy cannot flood the log.
pub(crate) fn describe_api_error(body: &str, token: &str) -> Option<String> {
    let raw = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("description")?.as_str().map(str::to_string))
        .unwrap_or_else(|| body.to_string());
    scrub_detail(&raw, &[token])
}

#[async_trait]
impl Notifier for TelegramConfig {
    fn driver_name(&self) -> &'static str {
        "telegram"
    }

    async fn send(&self, ctx: &NotifyContext<'_>) -> Result<Option<i64>> {
        if self.chat_id.trim().is_empty() {
            return Err(NotifyError::Config("telegram: chat_id is required".into()));
        }
        send_message(
            &self.bot_token,
            &self.chat_id,
            ctx.message,
            self.parse_mode,
            self.disable_notification.unwrap_or(false),
            // A scheduled notification answers nothing — there is no inbound
            // message to quote.
            None,
        )
        .await
    }
}

/// Shared HTTP client.
///
/// Built once: the inbound poller holds a `getUpdates` connection open for up
/// to 50 seconds per iteration, and rebuilding the TLS stack every time (which
/// is what `Client::new()` per call did) is wasted work on a hot path. The
/// timeout has to clear the longest long-poll, hence 90s rather than something
/// tighter.
pub(crate) fn client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(90))
            .build()
            .unwrap_or_default()
    })
}

/// Post one message to an arbitrary chat.
///
/// Split out of [`Notifier::send`] so inbound replies can target the
/// conversation that asked instead of the static `chat_id` in the manifest.
/// `NotifyContext` carries no destination and the `Notifier` trait has no way
/// to return one, so a reply path has to bypass the trait rather than bend it.
pub(crate) async fn send_message(
    bot_token: &str,
    chat_id: &str,
    text: &str,
    parse_mode: Option<ParseMode>,
    silent: bool,
    reply_to: Option<i64>,
) -> Result<Option<i64>> {
    if bot_token.trim().is_empty() {
        return Err(NotifyError::Config(
            "telegram: bot_token is required".into(),
        ));
    }
    let token = expand_env(BOT_TOKEN_FIELD, bot_token).map_err(NotifyError::Config)?;
    if token.trim().is_empty() {
        return Err(NotifyError::Config(
            "telegram: bot_token resolved to empty string".into(),
        ));
    }

    let body_text = render_body(text, parse_mode);

    let mut payload = json!({
        "chat_id": chat_id,
        "text": body_text,
    });
    if let Some(mode) = parse_mode {
        let mode_str = match mode {
            ParseMode::MarkdownV2 => "MarkdownV2",
            ParseMode::Html => "HTML",
            ParseMode::MarkdownLegacy => "Markdown",
        };
        payload["parse_mode"] = json!(mode_str);
    }
    if silent {
        payload["disable_notification"] = json!(true);
    }
    if let Some(message_id) = reply_to {
        // `reply_parameters` is the current shape (Bot API 7.0+);
        // `reply_to_message_id` is the deprecated spelling.
        //
        // `allow_sending_without_reply` matters: if the original message was
        // deleted while the agent ran, the answer still gets delivered instead
        // of failing with a 400 and leaving the sender waiting.
        payload["reply_parameters"] = json!({
            "message_id": message_id,
            "allow_sending_without_reply": true,
        });
    }

    // URL embeds the token; never log it. Both the URL and the
    // request-side `reqwest::Error::Display` carry the token, so we
    // convert transport errors into a sanitized `Backend` message
    // before they can reach `tracing` via `error = %e`.
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    let res = match client().post(&url).json(&payload).send().await {
        Ok(r) => r,
        Err(e) => return Err(NotifyError::Backend(sanitize_reqwest_err(&e))),
    };
    let status = res.status();
    if !status.is_success() {
        // The status alone does not say *why*; the reason lives in the body.
        // A body we cannot read is not a reason to lose the status too.
        let detail = match res.text().await {
            Ok(body) => describe_api_error(&body, &token),
            Err(_) => None,
        };
        return Err(NotifyError::Backend(match detail {
            Some(d) => format!("telegram returned {status}: {d}"),
            None => format!("telegram returned {status}"),
        }));
    }

    // The id of the message we just posted, so a later reply to it can be
    // resolved back to the run that caused it. Not being able to read it is
    // not a delivery failure: the message arrived, and the only thing lost is
    // the ability to correlate a future reply.
    let message_id = res
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|v| v.get("result")?.get("message_id")?.as_i64());
    Ok(message_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redact::MAX_ERROR_DETAIL_CHARS;

    #[test]
    fn debug_redacts_bot_token() {
        let cfg = TelegramConfig {
            bot_token: "1234567:SUPER-SECRET-VALUE".into(),
            chat_id: "-1001".into(),
            parse_mode: None,
            disable_notification: None,
        };
        let dbg = format!("{cfg:?}");
        assert!(
            !dbg.contains("SUPER-SECRET-VALUE"),
            "debug leaked token: {dbg}"
        );
        assert!(dbg.contains("<redacted>"));
    }

    // `${VAR}` expansion itself is covered in `crate::secrets` — it is shared
    // by every driver now. What stays here is the Telegram-specific wiring:
    // that `send` resolves the token before using it, and says which variable
    // was missing when it cannot.

    #[test]
    fn escape_markdown_v2_quotes_all_reserved_chars() {
        // Every char in the spec set needs to be backslash-escaped.
        let input = "_*[]()~`>#+-=|{}.!";
        let escaped = escape_markdown_v2(input);
        let expected = r"\_\*\[\]\(\)\~\`\>\#\+\-\=\|\{\}\.\!";
        assert_eq!(escaped, expected);
    }

    #[test]
    fn escape_markdown_v2_passes_through_normal_text() {
        // Letters, digits, spaces stay intact.
        let escaped = escape_markdown_v2("disk-alert ok 95 percent");
        // hyphen is reserved → escaped, but letters / digits / spaces are not.
        assert!(escaped.contains("disk\\-alert"));
        assert!(escaped.contains("95 percent"));
    }

    #[test]
    fn escape_markdown_v2_escapes_literal_backslash() {
        // A raw backslash in agent output opens an escape sequence Telegram
        // then cannot close → "can't parse entities" 400.
        assert_eq!(escape_markdown_v2(r"C:\path"), r"C:\\path");
        assert!(!has_dangling_escape(&escape_markdown_v2(r"trailing\")));
    }

    /// Walk the string the way a MarkdownV2 parser does: a backslash consumes
    /// the character after it. A backslash with nothing left to consume is what
    /// makes Telegram reject the whole message.
    fn has_dangling_escape(s: &str) -> bool {
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\\' && chars.next().is_none() {
                return true;
            }
        }
        false
    }

    // --- truncation -------------------------------------------------------

    #[test]
    fn truncate_leaves_short_text_alone() {
        assert_eq!(truncate("hi", None), "hi");
    }

    #[test]
    fn truncate_marks_long_text_and_respects_the_cap() {
        let long = "x".repeat(MAX_MESSAGE_CHARS + 500);
        let out = truncate(&long, None);
        assert!(out.ends_with("[truncated]"));
        assert!(out.chars().count() <= MAX_MESSAGE_CHARS);
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        // 2 bytes per char: a byte-based cut would slice mid-codepoint.
        let long = "é".repeat(MAX_MESSAGE_CHARS + 10);
        let out = truncate(&long, None);
        assert!(out.chars().count() <= MAX_MESSAGE_CHARS);
    }

    #[test]
    fn truncate_escapes_its_own_notice_in_markdown_v2() {
        // "[truncated]" carries reserved chars; an unescaped notice would be
        // the 400 the truncation exists to prevent.
        let long = "x".repeat(MAX_MESSAGE_CHARS + 1);
        let out = truncate(&long, Some(ParseMode::MarkdownV2));
        assert!(
            out.ends_with(r"\[truncated\]"),
            "{}",
            &out[out.len() - 40..]
        );
        assert!(out.chars().count() <= MAX_MESSAGE_CHARS);
        assert!(!has_dangling_escape(&out));
    }

    #[test]
    fn truncate_never_cuts_an_escape_pair_in_half() {
        // "a-" escapes to "a\-" (3 chars), so the cut boundary lands on the
        // backslash of a pair for this input — exactly the case that would
        // ship invalid markdown and earn another 400.
        let escaped = escape_markdown_v2(&"a-".repeat(MAX_MESSAGE_CHARS));
        let out = truncate(&escaped, Some(ParseMode::MarkdownV2));
        assert!(out.chars().count() <= MAX_MESSAGE_CHARS);
        assert!(!has_dangling_escape(&out), "cut split an escape pair");
    }

    #[test]
    fn truncate_keeps_a_whole_escaped_backslash_pair() {
        // Source backslashes escape to "\\": an even trailing run is content,
        // not a dangling escape, and must survive the cut.
        let escaped = escape_markdown_v2(&"\\".repeat(MAX_MESSAGE_CHARS));
        let out = truncate(&escaped, Some(ParseMode::MarkdownV2));
        assert!(out.chars().count() <= MAX_MESSAGE_CHARS);
        assert!(!has_dangling_escape(&out));
    }

    // --- render_body: the escape-then-truncate order ----------------------

    #[test]
    fn render_body_fits_the_cap_after_escaping() {
        // Regression for the 113 undelivered alerts: a body under the cap
        // *before* escaping can blow past it after, because every reserved
        // char grows by one. Truncating first would not have caught this.
        let text = ".".repeat(MAX_MESSAGE_CHARS - 1);
        assert!(text.chars().count() < MAX_MESSAGE_CHARS);
        let body = render_body(&text, Some(ParseMode::MarkdownV2));
        assert!(
            body.chars().count() <= MAX_MESSAGE_CHARS,
            "escaped body is {} chars",
            body.chars().count()
        );
        assert!(!has_dangling_escape(&body));
    }

    #[test]
    fn render_body_truncates_plain_text_too() {
        let body = render_body(&"x".repeat(MAX_MESSAGE_CHARS * 2), None);
        assert!(body.chars().count() <= MAX_MESSAGE_CHARS);
        assert!(body.ends_with("[truncated]"));
    }

    #[test]
    fn render_body_leaves_a_short_message_untouched() {
        assert_eq!(render_body("all good", None), "all good");
    }

    // --- API error detail --------------------------------------------------

    #[test]
    fn describe_api_error_extracts_description() {
        let body =
            r#"{"ok":false,"error_code":400,"description":"Bad Request: message is too long"}"#;
        let detail = describe_api_error(body, "12345:secret").unwrap();
        assert_eq!(detail, "Bad Request: message is too long");
    }

    #[test]
    fn describe_api_error_falls_back_to_raw_body() {
        // Telegram fronted by a proxy can answer HTML; still better than
        // nothing, which is what 113 log lines carried.
        let detail = describe_api_error("<html>502 upstream</html>", "12345:secret").unwrap();
        assert!(detail.contains("502 upstream"), "{detail}");
    }

    #[test]
    fn describe_api_error_ignores_an_empty_body() {
        assert!(describe_api_error("", "12345:secret").is_none());
        assert!(describe_api_error("   \n ", "12345:secret").is_none());
    }

    #[test]
    fn describe_api_error_redacts_the_bot_token() {
        let token = "1234567:AAH-SUPER-SECRET-VALUE";
        // Defensive: the API is not supposed to echo the token, but this
        // string reaches `tracing`, so a leak here is a leak on disk.
        let body = format!(
            r#"{{"ok":false,"description":"Not Found: https://api.telegram.org/bot{token}/sendMessage"}}"#
        );
        let detail = describe_api_error(&body, token).unwrap();
        assert!(!detail.contains("AAH-SUPER-SECRET-VALUE"), "{detail}");
        assert!(!detail.contains(token), "{detail}");
        assert!(detail.contains("<redacted>"), "{detail}");
    }

    #[test]
    fn describe_api_error_redacts_the_secret_half_alone() {
        let token = "1234567:AAH-SUPER-SECRET-VALUE";
        let detail = describe_api_error("leaked AAH-SUPER-SECRET-VALUE here", token).unwrap();
        assert!(!detail.contains("AAH-SUPER-SECRET-VALUE"), "{detail}");
    }

    #[test]
    fn describe_api_error_redacts_from_a_non_json_body() {
        let token = "1234567:AAH-SUPER-SECRET-VALUE";
        let detail = describe_api_error(&format!("<html>{token}</html>"), token).unwrap();
        assert!(!detail.contains("AAH-SUPER-SECRET-VALUE"), "{detail}");
    }

    #[test]
    fn describe_api_error_is_bounded_and_single_line() {
        let body = format!("line one\nline two\r\n{}", "z".repeat(2_000));
        let detail = describe_api_error(&body, "12345:secret").unwrap();
        assert!(!detail.contains('\n'), "detail must stay on one log line");
        assert!(detail.chars().count() <= MAX_ERROR_DETAIL_CHARS + 1);
    }

    #[test]
    fn deserialize_minimal() {
        let toml_str = r#"
            driver = "telegram"
            bot_token = "12345:abc"
            chat_id = "-1001234567890"
        "#;
        let entry: crate::NotifierEntry = toml::from_str(toml_str).unwrap();
        assert_eq!(entry.driver_name(), "telegram");
        assert!(entry.matches_event("given_up"));
    }

    #[test]
    fn deserialize_with_parse_mode_and_events() {
        let toml_str = r#"
            driver = "telegram"
            bot_token = "${TELEGRAM_BOT_TOKEN}"
            chat_id = "@my_channel"
            parse_mode = "MarkdownV2"
            disable_notification = true
            events = ["given_up", "recovered"]
        "#;
        let entry: crate::NotifierEntry = toml::from_str(toml_str).unwrap();
        let crate::NotifierConfig::Telegram(cfg) = &entry.config else {
            panic!("expected telegram variant");
        };
        assert_eq!(cfg.bot_token, "${TELEGRAM_BOT_TOKEN}");
        assert_eq!(cfg.chat_id, "@my_channel");
        assert_eq!(cfg.parse_mode, Some(ParseMode::MarkdownV2));
        assert_eq!(cfg.disable_notification, Some(true));
        assert!(entry.matches_event("given_up"));
        assert!(!entry.matches_event("success"));
    }

    #[tokio::test]
    async fn send_rejects_empty_chat_id() {
        let cfg = TelegramConfig {
            bot_token: "12345:abc".into(),
            chat_id: "  ".into(),
            parse_mode: None,
            disable_notification: None,
        };
        let ctx = NotifyContext {
            agent: "a",
            schedule: "s",
            event: "given_up",
            message: "m",
        };
        let err = cfg.send(&ctx).await.unwrap_err();
        match err {
            NotifyError::Config(msg) => assert!(msg.contains("chat_id")),
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_rejects_unset_env_token() {
        let cfg = TelegramConfig {
            bot_token: "${DOTAGENT_TEST_TG_UNSET_QQQ}".into(),
            chat_id: "-1001".into(),
            parse_mode: None,
            disable_notification: None,
        };
        let ctx = NotifyContext {
            agent: "a",
            schedule: "s",
            event: "given_up",
            message: "m",
        };
        let err = cfg.send(&ctx).await.unwrap_err();
        match err {
            NotifyError::Config(msg) => {
                assert!(msg.contains("unset"), "{msg}");
                assert!(
                    msg.contains("DOTAGENT_TEST_TG_UNSET_QQQ"),
                    "must name the variable: {msg}"
                );
                assert!(msg.contains(BOT_TOKEN_FIELD), "must name the field: {msg}");
            }
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_does_not_use_an_unresolved_placeholder_as_the_token() {
        // The failure that matters is not "it errored" but "it never went to
        // the wire": a request authenticated as the literal `${…}` string 404s
        // and reads like an outage instead of a typo.
        let err = send_message(
            "${DOTAGENT_TEST_TG_UNSET_QQQ}",
            "-1001",
            "m",
            None,
            false,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, NotifyError::Config(_)),
            "expected Config, got {err:?}"
        );
    }
}
