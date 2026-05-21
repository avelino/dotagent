//! Telegram notifications via the Bot API (`api.telegram.org`).
//!
//! Sends a `sendMessage` POST with the formatted body. `bot_token`
//! supports `${ENV_VAR}` interpolation so secrets stay out of the
//! manifest file — declare `bot_token = "${TELEGRAM_BOT_TOKEN}"` and
//! export the variable in the daemon's env.
//!
//! Tokens never reach `tracing` output: errors are logged with the
//! HTTP status only, and `Debug` redacts the token explicitly.

use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

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

/// Expand `${VAR}` references against `std::env`. Returns `Err` if a
/// referenced variable is unset — failing fast beats sending requests
/// authenticated as the literal string `"${TELEGRAM_BOT_TOKEN}"`.
fn expand_env(input: &str) -> std::result::Result<String, String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err("unterminated `${…}` in bot_token".into());
        };
        let name = &after[..end];
        if name.is_empty() {
            return Err("empty `${}` placeholder".into());
        }
        match std::env::var(name) {
            Ok(v) => out.push_str(&v),
            Err(_) => return Err(format!("env var ${{{name}}} is unset")),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Escape per Telegram MarkdownV2 reserved characters.
/// Source: <https://core.telegram.org/bots/api#markdownv2-style>.
pub(crate) fn escape_markdown_v2(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if matches!(
            c,
            '_' | '*'
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

#[async_trait]
impl Notifier for TelegramConfig {
    fn driver_name(&self) -> &'static str {
        "telegram"
    }

    async fn send(&self, ctx: &NotifyContext<'_>) -> Result<()> {
        if self.chat_id.trim().is_empty() {
            return Err(NotifyError::Config("telegram: chat_id is required".into()));
        }
        if self.bot_token.trim().is_empty() {
            return Err(NotifyError::Config(
                "telegram: bot_token is required".into(),
            ));
        }
        let token = expand_env(&self.bot_token).map_err(NotifyError::Config)?;
        if token.trim().is_empty() {
            return Err(NotifyError::Config(
                "telegram: bot_token resolved to empty string".into(),
            ));
        }

        let body_text = match self.parse_mode {
            Some(ParseMode::MarkdownV2) => escape_markdown_v2(ctx.message),
            _ => ctx.message.to_string(),
        };

        let mut payload = json!({
            "chat_id": self.chat_id,
            "text": body_text,
        });
        if let Some(mode) = self.parse_mode {
            let mode_str = match mode {
                ParseMode::MarkdownV2 => "MarkdownV2",
                ParseMode::Html => "HTML",
                ParseMode::MarkdownLegacy => "Markdown",
            };
            payload["parse_mode"] = json!(mode_str);
        }
        if let Some(true) = self.disable_notification {
            payload["disable_notification"] = json!(true);
        }

        // URL embeds the token; never log it. We log status only on failure.
        let url = format!("https://api.telegram.org/bot{token}/sendMessage");
        let res = reqwest::Client::new()
            .post(&url)
            .json(&payload)
            .send()
            .await?;
        let status = res.status();
        if !status.is_success() {
            return Err(NotifyError::Backend(format!("telegram returned {status}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn expand_env_replaces_var() {
        // SAFETY: tests run single-threaded for env mutation isolation only
        // because this var is unique to this test. Cargo's default test
        // harness is multi-threaded but each name is scoped per-test here.
        unsafe { std::env::set_var("DOTAGENT_TEST_TG_TOKEN_A", "abc123") };
        let out = expand_env("${DOTAGENT_TEST_TG_TOKEN_A}").unwrap();
        assert_eq!(out, "abc123");
        unsafe { std::env::remove_var("DOTAGENT_TEST_TG_TOKEN_A") };
    }

    #[test]
    fn expand_env_preserves_literal() {
        let out = expand_env("plain-literal-token").unwrap();
        assert_eq!(out, "plain-literal-token");
    }

    #[test]
    fn expand_env_errors_on_missing_var() {
        let err = expand_env("${THIS_DOES_NOT_EXIST_DOTAGENT_XYZ}").unwrap_err();
        assert!(err.contains("unset"), "{err}");
    }

    #[test]
    fn expand_env_errors_on_unterminated() {
        let err = expand_env("${UNCLOSED").unwrap_err();
        assert!(err.contains("unterminated"), "{err}");
    }

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
            NotifyError::Config(msg) => assert!(msg.contains("unset"), "{msg}"),
            other => panic!("expected Config error, got {other:?}"),
        }
    }
}
