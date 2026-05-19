//! Slack notifications via Incoming Webhooks.
//!
//! Replaces `dotagent-plugin-notify-slack`. The HTTP call is the same; the
//! difference is that the daemon now does it in-process instead of
//! spawning a plugin binary for every notification.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{Notifier, NotifyContext, NotifyError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlackConfig {
    pub webhook_url: String,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub icon_emoji: Option<String>,
}

#[async_trait]
impl Notifier for SlackConfig {
    fn driver_name(&self) -> &'static str {
        "slack"
    }

    async fn send(&self, ctx: &NotifyContext<'_>) -> Result<()> {
        if !self.webhook_url.starts_with("http") {
            return Err(NotifyError::Config(
                "slack: webhook_url must be an http(s) URL".into(),
            ));
        }
        let mut body = json!({ "text": ctx.message });
        if let Some(c) = &self.channel {
            body["channel"] = json!(c);
        }
        if let Some(u) = &self.username {
            body["username"] = json!(u);
        }
        if let Some(i) = &self.icon_emoji {
            body["icon_emoji"] = json!(i);
        }
        let res = reqwest::Client::new()
            .post(&self.webhook_url)
            .json(&body)
            .send()
            .await?;
        if !res.status().is_success() {
            return Err(NotifyError::Backend(format!(
                "slack webhook returned {}",
                res.status()
            )));
        }
        Ok(())
    }
}
