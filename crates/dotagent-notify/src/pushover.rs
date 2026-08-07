//! Pushover notifications via `api.pushover.net`.
//!
//! Both credentials (`token`, `user`) travel in the form body, and the URL is a
//! fixed constant — so `reqwest::Error`'s URL-bearing `Display` has nothing to
//! disclose here. Transport errors are still converted rather than propagated,
//! for the same reason the other drivers do it: it keeps "a `?` here is safe"
//! from being a judgement each new driver has to make correctly.

use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::limits::{truncate_chars, ELLIPSIS_NOTICE, TRUNCATION_NOTICE};
use crate::redact::{sanitize_reqwest_err, scrub_detail};
use crate::secrets::expand_env;
use crate::{Notifier, NotifyContext, NotifyError, Result};

/// Manifest keys the credentials are configured under — the labels an operator
/// sees in a `${VAR}` expansion error.
const TOKEN_FIELD: &str = "pushover.token";
const USER_FIELD: &str = "pushover.user";

/// Pushover's cap on `message`. The tightest limit of any driver here by an
/// order of magnitude — a few lines of an agent's stderr clears it, and
/// Pushover answers 400 for the whole request rather than trimming.
pub(crate) const MAX_MESSAGE_CHARS: usize = 1024;

/// Pushover's cap on `title`. Defaults to the agent name, which is short, but
/// an operator-supplied title is not bounded by anything else.
pub(crate) const MAX_TITLE_CHARS: usize = 250;

const ENDPOINT: &str = "https://api.pushover.net/1/messages.json";

#[derive(Clone, Serialize, Deserialize)]
pub struct PushoverConfig {
    pub token: String,
    pub user: String,
    #[serde(default)]
    pub priority: Option<i32>,
    #[serde(default)]
    pub title: Option<String>,
}

/// Hand-written: `token` is the application credential and `user` is the
/// delivery key. Neither belongs in a log line reached via `{:?}`.
impl fmt::Debug for PushoverConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PushoverConfig")
            .field("token", &"<redacted>")
            .field("user", &"<redacted>")
            .field("priority", &self.priority)
            .field("title", &self.title)
            .finish()
    }
}

impl PushoverConfig {
    /// Resolve `(token, user)` to the values that will actually be sent.
    ///
    /// Both are credentials, both accept `${VAR}`, and expansion runs before
    /// the emptiness check — otherwise an unset variable would be reported as
    /// "token and user are required" for a field that is very much filled in.
    fn resolve_credentials(&self) -> Result<(String, String)> {
        let token = expand_env(TOKEN_FIELD, &self.token).map_err(NotifyError::Config)?;
        let user = expand_env(USER_FIELD, &self.user).map_err(NotifyError::Config)?;
        if token.trim().is_empty() || user.trim().is_empty() {
            return Err(NotifyError::Config(
                "pushover: token and user are required".into(),
            ));
        }
        Ok((token, user))
    }
}

#[async_trait]
impl Notifier for PushoverConfig {
    fn driver_name(&self) -> &'static str {
        "pushover"
    }

    async fn send(&self, ctx: &NotifyContext<'_>) -> Result<Option<i64>> {
        let (token, user) = self.resolve_credentials()?;
        let title = self.title.as_deref().unwrap_or(ctx.agent);
        let mut form = vec![
            ("token", token.clone()),
            ("user", user.clone()),
            (
                "message",
                truncate_chars(ctx.message, MAX_MESSAGE_CHARS, TRUNCATION_NOTICE),
            ),
            (
                "title",
                truncate_chars(title, MAX_TITLE_CHARS, ELLIPSIS_NOTICE),
            ),
        ];
        if let Some(p) = self.priority {
            form.push(("priority", p.to_string()));
        }
        let res = match reqwest::Client::new()
            .post(ENDPOINT)
            .form(&form)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return Err(NotifyError::Backend(sanitize_reqwest_err("pushover", &e))),
        };
        let status = res.status();
        if !status.is_success() {
            // Pushover answers `{"errors":["application token is invalid"]}`,
            // which is the difference between a config bug and an outage.
            // The *resolved* credentials, not the `${VAR}` templates — those
            // are what an echoing error body would contain.
            let detail = match res.text().await {
                Ok(raw) => scrub_detail(&raw, &[&token, &user]),
                Err(_) => None,
            };
            return Err(NotifyError::Backend(match detail {
                Some(d) => format!("pushover returned {status}: {d}"),
                None => format!("pushover returned {status}"),
            }));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "azGDORePK8gMaC0QOYAMyEEuzJnyUi";
    const USER: &str = "uQiRzpo4DXghDmr9QzzfQu27cmVRsG";

    fn cfg() -> PushoverConfig {
        PushoverConfig {
            token: TOKEN.into(),
            user: USER.into(),
            priority: None,
            title: None,
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

    // --- deny --------------------------------------------------------------

    #[test]
    fn debug_redacts_both_credentials() {
        let dbg = format!("{:?}", cfg());
        assert!(!dbg.contains(TOKEN), "debug leaked token: {dbg}");
        assert!(!dbg.contains(USER), "debug leaked user key: {dbg}");
    }

    #[test]
    fn debug_of_the_enclosing_entry_redacts_too() {
        let toml_str = format!(
            r#"
            driver = "pushover"
            token = "{TOKEN}"
            user = "{USER}"
            "#
        );
        let entry: crate::NotifierEntry = toml::from_str(&toml_str).unwrap();
        let dbg = format!("{entry:?}");
        assert!(!dbg.contains(TOKEN), "{dbg}");
        assert!(!dbg.contains(USER), "{dbg}");
    }

    #[test]
    fn an_error_body_echoing_the_token_is_scrubbed() {
        let body = format!(r#"{{"errors":["token {TOKEN} is invalid"],"status":0}}"#);
        let detail = scrub_detail(&body, &[TOKEN, USER]).unwrap();
        assert!(!detail.contains(TOKEN), "{detail}");
        assert!(detail.contains("is invalid"), "{detail}");
    }

    #[tokio::test]
    async fn send_rejects_missing_credentials() {
        for (token, user) in [("", USER), (TOKEN, ""), ("", ""), ("   ", "   ")] {
            let c = PushoverConfig {
                token: token.into(),
                user: user.into(),
                priority: None,
                title: None,
            };
            let err = c.send(&ctx("m")).await.unwrap_err();
            assert!(
                matches!(err, NotifyError::Config(_)),
                "({token:?}, {user:?}) should be rejected, got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn send_rejects_an_unset_credential_var() {
        for (token, user) in [
            ("${DOTAGENT_TEST_PO_UNSET_QQQ}", USER),
            (TOKEN, "${DOTAGENT_TEST_PO_UNSET_QQQ}"),
        ] {
            let c = PushoverConfig {
                token: token.into(),
                user: user.into(),
                priority: None,
                title: None,
            };
            let err = c.send(&ctx("m")).await.unwrap_err();
            match err {
                NotifyError::Config(msg) => {
                    assert!(msg.contains("unset"), "{msg}");
                    assert!(msg.contains("DOTAGENT_TEST_PO_UNSET_QQQ"), "{msg}");
                    assert!(msg.contains("pushover."), "must name the field: {msg}");
                    assert!(
                        !msg.contains("are required"),
                        "wrong diagnosis for a set-but-unresolvable field: {msg}"
                    );
                }
                other => panic!("expected Config error, got {other:?}"),
            }
        }
    }

    #[test]
    fn resolution_rejects_a_var_that_expands_to_empty() {
        let out = crate::secrets::with_store(&[("DOTAGENT_TEST_PO_EMPTY", "  ")], || {
            PushoverConfig {
                token: "${DOTAGENT_TEST_PO_EMPTY}".into(),
                user: USER.into(),
                priority: None,
                title: None,
            }
            .resolve_credentials()
        });
        assert!(matches!(out, Err(NotifyError::Config(_))), "{out:?}");
    }

    #[test]
    fn a_resolution_error_never_quotes_the_sibling_credential() {
        // `user` resolves, `token` does not — the live user key must not ride
        // along in the message.
        let out = crate::secrets::with_store(&[("DOTAGENT_TEST_PO_USER", USER)], || {
            PushoverConfig {
                token: "${DOTAGENT_TEST_PO_UNSET_QQQ}".into(),
                user: "${DOTAGENT_TEST_PO_USER}".into(),
                priority: None,
                title: None,
            }
            .resolve_credentials()
        });
        let Err(NotifyError::Config(msg)) = out else {
            panic!("expected Config error, got {out:?}");
        };
        assert!(!msg.contains(USER), "leaked the user key: {msg}");
    }

    #[test]
    fn resolution_expands_both_credentials_from_the_secrets_store() {
        let out = crate::secrets::with_store(
            &[
                ("DOTAGENT_TEST_PO_TOKEN", TOKEN),
                ("DOTAGENT_TEST_PO_USER2", USER),
            ],
            || {
                PushoverConfig {
                    token: "${DOTAGENT_TEST_PO_TOKEN}".into(),
                    user: "${DOTAGENT_TEST_PO_USER2}".into(),
                    priority: None,
                    title: None,
                }
                .resolve_credentials()
            },
        )
        .unwrap();
        assert_eq!(out, (TOKEN.to_string(), USER.to_string()));
    }

    #[test]
    fn resolution_leaves_literal_credentials_alone() {
        assert_eq!(
            cfg().resolve_credentials().unwrap(),
            (TOKEN.to_string(), USER.to_string())
        );
    }

    #[test]
    fn the_endpoint_carries_no_credential() {
        // The premise of this driver's threat model: unlike Slack, the URL is
        // a constant, so a URL-bearing error has nothing to disclose.
        assert!(!ENDPOINT.contains(TOKEN));
        assert!(!ENDPOINT.contains(USER));
        assert_eq!(ENDPOINT, "https://api.pushover.net/1/messages.json");
    }

    // --- the 1024-char cap -------------------------------------------------

    #[test]
    fn a_long_message_is_trimmed_to_the_cap() {
        let out = truncate_chars(&"x".repeat(5_000), MAX_MESSAGE_CHARS, TRUNCATION_NOTICE);
        assert!(out.chars().count() <= MAX_MESSAGE_CHARS);
        assert!(out.ends_with("[truncated]"));
    }

    #[test]
    fn an_accented_message_is_trimmed_by_characters() {
        // Pushover counts characters, so 2000 accented chars must come back as
        // 1024 characters — not 1024 bytes.
        let out = truncate_chars(&"ç".repeat(2_000), MAX_MESSAGE_CHARS, TRUNCATION_NOTICE);
        assert_eq!(out.chars().count(), MAX_MESSAGE_CHARS);
    }

    #[test]
    fn a_long_title_is_trimmed_with_an_ellipsis() {
        let out = truncate_chars(&"t".repeat(1_000), MAX_TITLE_CHARS, ELLIPSIS_NOTICE);
        assert!(out.chars().count() <= MAX_TITLE_CHARS);
        assert!(out.ends_with('…'));
        // A title is one line; the body's "\n[truncated]" would break it.
        assert!(!out.contains('\n'));
    }

    // --- allow -------------------------------------------------------------

    #[test]
    fn an_ordinary_message_and_title_are_left_alone() {
        assert_eq!(
            truncate_chars("disk at 95%", MAX_MESSAGE_CHARS, TRUNCATION_NOTICE),
            "disk at 95%"
        );
        assert_eq!(
            truncate_chars("disk-alert", MAX_TITLE_CHARS, ELLIPSIS_NOTICE),
            "disk-alert"
        );
    }
}
