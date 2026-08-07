//! Slack notifications via Incoming Webhooks.
//!
//! Replaces `dotagent-plugin-notify-slack`. The HTTP call is the same; the
//! difference is that the daemon now does it in-process instead of
//! spawning a plugin binary for every notification.
//!
//! The webhook URL **is** the credential here — there is no separate token, so
//! `hooks.slack.com/services/T…/B…/<secret>` is the whole authentication story.
//! That makes every place the URL could be rendered a disclosure: transport
//! errors, response bodies, and `Debug`. See [`crate::redact`].
//!
//! It also makes the URL a thing that must not be written into `agent.toml`,
//! which lives in a dotfiles repo — so `webhook_url` accepts `${VAR}` and is
//! resolved at send time against `~/.config/dotagent/secrets.env`. See
//! [`crate::secrets`].

use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::limits::{truncate_chars, TRUNCATION_NOTICE};
use crate::redact::{sanitize_reqwest_err, scrub_detail};
use crate::secrets::expand_env;
use crate::{Notifier, NotifyContext, NotifyError, Result};

/// Manifest key the credential is configured under — the label an operator
/// sees in a `${VAR}` expansion error.
const WEBHOOK_FIELD: &str = "slack.webhook_url";

/// Slack's cap on the `text` field of a webhook payload.
///
/// Roomy compared to the other drivers, but not infinite: a paste of a failing
/// agent's stdout clears it easily, and Slack answers `invalid_payload` for the
/// whole message rather than trimming it.
pub(crate) const MAX_MESSAGE_CHARS: usize = 40_000;

#[derive(Clone, Serialize, Deserialize)]
pub struct SlackConfig {
    pub webhook_url: String,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub icon_emoji: Option<String>,
}

/// Hand-written so the URL — the credential — cannot reach a log through a
/// `{:?}` on this struct or on any `NotifierEntry` that contains it.
impl fmt::Debug for SlackConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlackConfig")
            .field("webhook_url", &"<redacted>")
            .field("channel", &self.channel)
            .field("username", &self.username)
            .field("icon_emoji", &self.icon_emoji)
            .finish()
    }
}

impl SlackConfig {
    /// Resolve `webhook_url` to the URL that will actually be requested.
    ///
    /// Expansion runs **before** validation on purpose: `"${SLACK_WEBHOOK}"`
    /// does not start with `http`, so validating the raw field would reject
    /// every interpolated config with a message about URL schemes instead of
    /// the one thing the operator needs to know — which variable is missing.
    fn resolve_webhook(&self) -> Result<String> {
        let url = expand_env(WEBHOOK_FIELD, &self.webhook_url).map_err(NotifyError::Config)?;
        let url = url.trim().to_string();
        if !url.starts_with("http") {
            // The value is a credential even when it is malformed, so the
            // message describes it rather than quoting it.
            return Err(NotifyError::Config(
                "slack: webhook_url must be an http(s) URL".into(),
            ));
        }
        Ok(url)
    }
}

#[async_trait]
impl Notifier for SlackConfig {
    fn driver_name(&self) -> &'static str {
        "slack"
    }

    async fn send(&self, ctx: &NotifyContext<'_>) -> Result<Option<i64>> {
        let url = self.resolve_webhook()?;
        let url = url.as_str();
        let mut body = json!({
            "text": truncate_chars(ctx.message, MAX_MESSAGE_CHARS, TRUNCATION_NOTICE),
        });
        if let Some(c) = &self.channel {
            body["channel"] = json!(c);
        }
        if let Some(u) = &self.username {
            body["username"] = json!(u);
        }
        if let Some(i) = &self.icon_emoji {
            body["icon_emoji"] = json!(i);
        }
        // `reqwest::Error`'s `Display` carries the request URL, and the URL is
        // the secret — convert here, before the error can become a
        // `NotifyError` and reach `warn!(error = %e)` in the runner.
        let res = match reqwest::Client::new().post(url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => return Err(NotifyError::Backend(sanitize_reqwest_err("slack", &e))),
        };
        let status = res.status();
        if !status.is_success() {
            // Slack says *why* in the body (`invalid_payload`, `no_service`,
            // `channel_not_found`), which is worth far more than the status
            // alone — but the body is server-controlled and a proxy in front of
            // it may echo the request URL back, so it is scrubbed either way.
            let detail = match res.text().await {
                Ok(raw) => scrub_detail(&raw, &[url]),
                Err(_) => None,
            };
            return Err(NotifyError::Backend(match detail {
                Some(d) => format!("slack webhook returned {status}: {d}"),
                None => format!("slack webhook returned {status}"),
            }));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like a real incoming-webhook secret, and distinctive enough that
    /// a substring match cannot pass by accident.
    const SECRET: &str = "aBcDeF1234567890gHiJkLmN";

    fn webhook(host: &str) -> String {
        format!("http://{host}/services/T00000000/B00000000/{SECRET}")
    }

    fn cfg(url: &str) -> SlackConfig {
        SlackConfig {
            webhook_url: url.into(),
            channel: None,
            username: None,
            icon_emoji: None,
        }
    }

    fn ctx<'a>(message: &'a str) -> NotifyContext<'a> {
        NotifyContext {
            agent: "disk-alert",
            schedule: "hourly",
            event: "given_up",
            message,
        }
    }

    /// Answer exactly one request with `status_line` and `body`, then close.
    /// Returns the port it is listening on.
    async fn one_shot(status_line: &'static str, body: String) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 {status_line}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        port
    }

    // --- deny: the webhook secret must not survive any error path ----------

    #[tokio::test]
    async fn transport_error_never_carries_the_webhook_secret() {
        // Port 1 refuses instantly: a real transport failure, no network.
        let err = cfg(&webhook("127.0.0.1:1"))
            .send(&ctx("m"))
            .await
            .unwrap_err();
        let rendered = format!("{err}");
        assert!(
            !rendered.contains(SECRET),
            "leaked webhook secret: {rendered}"
        );
        assert!(!rendered.contains("hooks.slack.com"), "{rendered}");
        assert!(rendered.contains("slack transport error"), "{rendered}");
    }

    #[tokio::test]
    async fn status_error_never_carries_the_webhook_secret() {
        // A proxy in front of Slack answering 404 with the request URL in the
        // body is the realistic way the secret comes back to us.
        let body = format!("no_service for {}", webhook("hooks.slack.com"));
        let port = one_shot("404 Not Found", body).await;
        let err = cfg(&webhook(&format!("127.0.0.1:{port}")))
            .send(&ctx("m"))
            .await
            .unwrap_err();
        let rendered = format!("{err}");
        assert!(
            !rendered.contains(SECRET),
            "leaked webhook secret: {rendered}"
        );
        assert!(rendered.contains("404"), "status must survive: {rendered}");
        assert!(
            rendered.contains("no_service"),
            "reason must survive: {rendered}"
        );
    }

    #[tokio::test]
    async fn status_error_survives_an_unreadable_body() {
        // Server promises 200 bytes and sends none: reading the body fails, and
        // that must not cost us the status too.
        let port = one_shot("500 Internal Server Error", String::new()).await;
        let err = cfg(&webhook(&format!("127.0.0.1:{port}")))
            .send(&ctx("m"))
            .await
            .unwrap_err();
        let rendered = format!("{err}");
        assert!(!rendered.contains(SECRET), "{rendered}");
        assert!(rendered.contains("500"), "{rendered}");
    }

    #[test]
    fn debug_redacts_the_webhook_url() {
        let dbg = format!("{:?}", cfg(&webhook("hooks.slack.com")));
        assert!(!dbg.contains(SECRET), "debug leaked webhook: {dbg}");
        assert!(dbg.contains("<redacted>"), "{dbg}");
    }

    #[test]
    fn debug_of_the_enclosing_entry_redacts_too() {
        // The runner logs `NotifierEntry`, not `SlackConfig`.
        let toml_str = format!(
            r#"
            driver = "slack"
            webhook_url = "{}"
            "#,
            webhook("hooks.slack.com")
        );
        let entry: crate::NotifierEntry = toml::from_str(&toml_str).unwrap();
        let dbg = format!("{entry:?}");
        assert!(!dbg.contains(SECRET), "entry debug leaked webhook: {dbg}");
    }

    // --- deny: malformed / empty / absent config ---------------------------

    #[tokio::test]
    async fn send_rejects_an_empty_webhook_url() {
        let err = cfg("").send(&ctx("m")).await.unwrap_err();
        match err {
            NotifyError::Config(msg) => assert!(msg.contains("webhook_url"), "{msg}"),
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_rejects_a_non_http_webhook_url() {
        for bad in ["file:///etc/passwd", "hooks.slack.com/services/x", "   "] {
            let err = cfg(bad).send(&ctx("m")).await.unwrap_err();
            assert!(
                matches!(err, NotifyError::Config(_)),
                "{bad} should be rejected, got {err:?}"
            );
        }
    }

    // --- deny: `${VAR}` that does not resolve --------------------------------

    #[tokio::test]
    async fn send_rejects_an_unset_webhook_var() {
        let err = cfg("${DOTAGENT_TEST_SLACK_UNSET_QQQ}")
            .send(&ctx("m"))
            .await
            .unwrap_err();
        match err {
            NotifyError::Config(msg) => {
                // The scheme complaint would be the wrong diagnosis: the field
                // is well-formed, the variable behind it is not set.
                assert!(msg.contains("unset"), "{msg}");
                assert!(msg.contains("DOTAGENT_TEST_SLACK_UNSET_QQQ"), "{msg}");
                assert!(msg.contains("slack.webhook_url"), "{msg}");
                assert!(!msg.contains("http(s) URL"), "wrong diagnosis: {msg}");
            }
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn resolution_rejects_a_var_that_expands_to_a_non_url() {
        // A `secrets.env` line pointing at the wrong value must not become a
        // POST to something that is not Slack.
        let out = crate::secrets::with_store(&[("DOTAGENT_TEST_SLACK_BAD", "not-a-url")], || {
            cfg("${DOTAGENT_TEST_SLACK_BAD}").resolve_webhook()
        });
        assert!(matches!(out, Err(NotifyError::Config(_))), "{out:?}");
    }

    #[test]
    fn resolution_rejects_a_var_that_expands_to_empty() {
        let out = crate::secrets::with_store(&[("DOTAGENT_TEST_SLACK_EMPTY", "   ")], || {
            cfg("${DOTAGENT_TEST_SLACK_EMPTY}").resolve_webhook()
        });
        assert!(matches!(out, Err(NotifyError::Config(_))), "{out:?}");
    }

    #[test]
    fn a_resolution_error_never_quotes_the_resolved_value() {
        let out = crate::secrets::with_store(
            &[(
                "DOTAGENT_TEST_SLACK_BAD2",
                "ftp://host/aBcDeF1234567890gHiJkLmN",
            )],
            || cfg("${DOTAGENT_TEST_SLACK_BAD2}").resolve_webhook(),
        );
        let Err(NotifyError::Config(msg)) = out else {
            panic!("expected Config error, got {out:?}");
        };
        assert!(!msg.contains(SECRET), "leaked the resolved value: {msg}");
    }

    // --- allow: `${VAR}` that resolves --------------------------------------

    #[test]
    fn resolution_expands_the_webhook_from_the_secrets_store() {
        let url = webhook("hooks.slack.com");
        let out = crate::secrets::with_store(&[("DOTAGENT_TEST_SLACK_OK", url.as_str())], || {
            cfg("${DOTAGENT_TEST_SLACK_OK}").resolve_webhook()
        })
        .unwrap();
        assert_eq!(out, url);
    }

    #[test]
    fn resolution_leaves_a_literal_webhook_alone() {
        // Backwards compatibility: configs that predate `${VAR}` keep working.
        let url = webhook("hooks.slack.com");
        assert_eq!(cfg(&url).resolve_webhook().unwrap(), url);
    }

    // --- allow: normal payloads still work ---------------------------------

    #[test]
    fn truncates_a_body_past_slacks_cap() {
        let out = truncate_chars(
            &"x".repeat(MAX_MESSAGE_CHARS * 2),
            MAX_MESSAGE_CHARS,
            TRUNCATION_NOTICE,
        );
        assert!(out.chars().count() <= MAX_MESSAGE_CHARS);
        assert!(out.ends_with("[truncated]"));
    }

    #[test]
    fn leaves_an_ordinary_body_untouched() {
        assert_eq!(
            truncate_chars("disk at 95%", MAX_MESSAGE_CHARS, TRUNCATION_NOTICE),
            "disk at 95%"
        );
    }
}
