//! Pushover notifications via `api.pushover.net`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{Notifier, NotifyContext, NotifyError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushoverConfig {
    pub token: String,
    pub user: String,
    #[serde(default)]
    pub priority: Option<i32>,
    #[serde(default)]
    pub title: Option<String>,
}

#[async_trait]
impl Notifier for PushoverConfig {
    fn driver_name(&self) -> &'static str {
        "pushover"
    }

    async fn send(&self, ctx: &NotifyContext<'_>) -> Result<()> {
        if self.token.is_empty() || self.user.is_empty() {
            return Err(NotifyError::Config(
                "pushover: token and user are required".into(),
            ));
        }
        let mut form = vec![
            ("token", self.token.clone()),
            ("user", self.user.clone()),
            ("message", ctx.message.to_string()),
        ];
        if let Some(p) = self.priority {
            form.push(("priority", p.to_string()));
        }
        if let Some(t) = &self.title {
            form.push(("title", t.clone()));
        } else {
            form.push(("title", ctx.agent.to_string()));
        }
        let res = reqwest::Client::new()
            .post("https://api.pushover.net/1/messages.json")
            .form(&form)
            .send()
            .await?;
        if !res.status().is_success() {
            return Err(NotifyError::Backend(format!(
                "pushover returned {}",
                res.status()
            )));
        }
        Ok(())
    }
}
