//! ntfy.sh / self-hosted ntfy notifications.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{Notifier, NotifyContext, NotifyError, Result};

fn default_base_url() -> String {
    "https://ntfy.sh".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtfyConfig {
    #[serde(default = "default_base_url")]
    pub base_url: String,
    pub topic: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub priority: Option<u8>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[async_trait]
impl Notifier for NtfyConfig {
    fn driver_name(&self) -> &'static str {
        "ntfy"
    }

    async fn send(&self, ctx: &NotifyContext<'_>) -> Result<()> {
        if self.topic.is_empty() {
            return Err(NotifyError::Config("ntfy: topic is required".into()));
        }
        let url = format!("{}/{}", self.base_url.trim_end_matches('/'), self.topic);
        let mut req = reqwest::Client::new()
            .post(&url)
            .body(ctx.message.to_string());
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        if let Some(p) = self.priority {
            req = req.header("X-Priority", p.to_string());
        }
        if let Some(t) = &self.title {
            req = req.header("X-Title", t.clone());
        } else {
            req = req.header("X-Title", ctx.agent.to_string());
        }
        if !self.tags.is_empty() {
            req = req.header("X-Tags", self.tags.join(","));
        }
        let res = req.send().await?;
        if !res.status().is_success() {
            return Err(NotifyError::Backend(format!(
                "ntfy returned {}",
                res.status()
            )));
        }
        Ok(())
    }
}
