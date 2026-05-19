//! Desktop notifications via `notify-rust`.
//!
//! Native APIs, no subprocess:
//! - **macOS**: `NSUserNotification` / `NSUserNotificationCenter`
//! - **Linux**: D-Bus to `org.freedesktop.Notifications`
//! - **Windows**: Toast notifications via WinRT (out of scope here, but
//!   compiles)
//!
//! Replaces the legacy `notify-desktop` plugin, which shelled out to
//! `osascript` (macOS) or `notify-send` (Linux). Both required the user
//! to have those binaries installed and added one fork per notification.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::{Notifier, NotifyContext, NotifyError, Result};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopConfig {
    /// Notification title. Defaults to the agent name when omitted.
    pub title: Option<String>,
    /// macOS subtitle (ignored on Linux).
    pub subtitle: Option<String>,
    /// Play system sound. macOS only.
    pub sound: Option<bool>,
    /// Linux urgency. One of `low | normal | critical`.
    pub urgency: Option<String>,
    /// Linux icon name (or absolute path).
    pub icon: Option<String>,
    /// Linux only: how long the notification is visible (ms). 0 = persistent.
    pub expire_ms: Option<u32>,
}

#[async_trait]
impl Notifier for DesktopConfig {
    fn driver_name(&self) -> &'static str {
        "desktop"
    }

    async fn send(&self, ctx: &NotifyContext<'_>) -> Result<()> {
        // notify-rust is sync. Use spawn_blocking so we don't stall the
        // tokio runtime when the D-Bus call has to wait on the user's
        // session bus.
        let title = self.title.clone().unwrap_or_else(|| ctx.agent.to_string());
        let body = ctx.message.to_string();
        let cfg = self.clone();

        let result = tokio::task::spawn_blocking(move || {
            let mut n = notify_rust::Notification::new();
            n.summary(&title).body(&body).appname("dotagent");

            #[cfg(target_os = "macos")]
            {
                if let Some(sub) = cfg.subtitle.as_deref() {
                    n.subtitle(sub);
                }
                if cfg.sound.unwrap_or(false) {
                    n.sound_name("Submarine");
                }
            }

            #[cfg(target_os = "linux")]
            {
                if let Some(icon) = cfg.icon.as_deref() {
                    n.icon(icon);
                }
                if let Some(urgency) = cfg.urgency.as_deref() {
                    n.urgency(match urgency {
                        "low" => notify_rust::Urgency::Low,
                        "critical" => notify_rust::Urgency::Critical,
                        _ => notify_rust::Urgency::Normal,
                    });
                }
                if let Some(ms) = cfg.expire_ms {
                    n.timeout(notify_rust::Timeout::Milliseconds(ms));
                }
            }

            // Touch unused fields on platforms that don't use them so the
            // compiler stops complaining about dead config.
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            {
                let _ = (
                    cfg.subtitle,
                    cfg.sound,
                    cfg.urgency,
                    cfg.icon,
                    cfg.expire_ms,
                );
            }

            n.show()
                .map(|_| ())
                .map_err(|e| NotifyError::Desktop(e.to_string()))
        })
        .await
        .map_err(|e| NotifyError::Desktop(format!("spawn_blocking join: {e}")))?;

        debug!(
            driver = "desktop",
            agent = ctx.agent,
            "desktop notification sent"
        );
        result
    }
}
