//! Built-in notification drivers for dotagent.
//!
//! Notifications used to be plugins (`dotagent-plugin-notify-*`) spawned as
//! subprocesses speaking JSON-over-stdio. That worked, but added a fork per
//! notification, dependency on OS CLI tools (`osascript`, `notify-send`),
//! and friction for the most common path (notify-on-failure).
//!
//! Now notifications live in-process. The daemon links this crate directly
//! and calls a `Notifier` trait — zero subprocess for desktop / HTTP-based
//! drivers. The plugin protocol stays alive for `sink` / `preflight` and
//! third-party notifiers (`driver = "plugin"`).
//!
//! ```toml
//! [[notifiers]]
//! driver = "desktop"
//! title  = "dotagent"
//!
//! [[notifiers]]
//! driver = "slack"
//! webhook_url = "https://hooks.slack.com/..."
//! events = ["given_up", "recovered"]
//! ```
//!
//! ## Drivers
//!
//! | driver       | underlying transport                                      |
//! |--------------|-----------------------------------------------------------|
//! | `desktop`    | `notify-rust` — NSUserNotification (macOS) / D-Bus (Linux) |
//! | `slack`      | HTTPS POST (Incoming Webhooks)                            |
//! | `ntfy`       | HTTPS POST (`ntfy.sh` or self-hosted)                     |
//! | `pushover`   | HTTPS POST (`api.pushover.net`)                           |
//! | `imessage`   | `osascript` (no native API exists; macOS only)            |
//! | `plugin`     | falls back to the legacy plugin protocol                   |
//!
//! Only `imessage` keeps a subprocess — Apple does not expose any Messages
//! API. Every other driver is fully native.

pub mod desktop;
pub mod imessage;
pub mod ntfy;
pub mod pushover;
pub mod slack;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, NotifyError>;

#[derive(Debug, Error)]
pub enum NotifyError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("desktop: {0}")]
    Desktop(String),
    #[error("config: {0}")]
    Config(String),
    #[error("skipped: {reason}")]
    Skipped { reason: String },
    #[error("unsupported platform for driver {driver}")]
    UnsupportedPlatform { driver: &'static str },
    #[error("invalid driver: {0}")]
    InvalidDriver(String),
    #[error("backend failed: {0}")]
    Backend(String),
}

/// Context passed to every notifier when an event fires.
#[derive(Debug, Clone, Serialize)]
pub struct NotifyContext<'a> {
    pub agent: &'a str,
    pub schedule: &'a str,
    /// Lifecycle event — matches the same set the plugin protocol used:
    /// `attempt_failed`, `given_up`, `recovered`, `success`, `preflight`,
    /// `timed_out`.
    pub event: &'a str,
    pub message: &'a str,
}

/// Trait every built-in driver implements. `send` may short-circuit (return
/// `Err(NotifyError::Skipped)`) for rate-limiting / dedup — the caller treats
/// that as an `ok` outcome rather than a failure.
#[async_trait]
pub trait Notifier: Send + Sync {
    async fn send(&self, ctx: &NotifyContext<'_>) -> Result<()>;
    fn driver_name(&self) -> &'static str;
}

/// Manifest representation of a notifier. Discriminated by `driver`.
///
/// Each variant carries the driver-specific config inline — no nested `config = {...}`
/// like the legacy `[[plugins.notify]]` shape, because there's no plugin layer
/// in the middle anymore.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "driver", rename_all = "snake_case")]
pub enum NotifierConfig {
    Desktop(desktop::DesktopConfig),
    Slack(slack::SlackConfig),
    Ntfy(ntfy::NtfyConfig),
    Pushover(pushover::PushoverConfig),
    Imessage(imessage::ImessageConfig),
    /// Escape hatch for third-party notifiers — falls back to the plugin protocol.
    Plugin(PluginNotifierConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginNotifierConfig {
    /// Short name (without `dotagent-plugin-` prefix). Resolved via the plugin client.
    pub name: String,
    #[serde(default)]
    pub config: serde_json::Value,
    /// Optional event filter, identical semantics to `NotifierEntry::events`.
    #[serde(default)]
    pub events: Vec<String>,
}

/// Top-level entry on a manifest: the driver-specific config + the events it
/// fires on. Empty `events` means "all events".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifierEntry {
    #[serde(flatten)]
    pub config: NotifierConfig,
    #[serde(default)]
    pub events: Vec<String>,
}

impl NotifierEntry {
    /// Should this entry fire for the given event name?
    pub fn matches_event(&self, event: &str) -> bool {
        let extra_events = match &self.config {
            NotifierConfig::Plugin(p) => p.events.as_slice(),
            _ => &[],
        };
        if !self.events.is_empty() {
            return self.events.iter().any(|e| e == event);
        }
        if !extra_events.is_empty() {
            return extra_events.iter().any(|e| e == event);
        }
        true
    }

    /// Build a concrete driver. `Plugin` returns `Ok(None)` — the caller is
    /// expected to dispatch via the plugin client.
    pub fn build_driver(&self) -> Result<Option<Box<dyn Notifier>>> {
        Ok(Some(match &self.config {
            NotifierConfig::Desktop(c) => Box::new(c.clone()),
            NotifierConfig::Slack(c) => Box::new(c.clone()),
            NotifierConfig::Ntfy(c) => Box::new(c.clone()),
            NotifierConfig::Pushover(c) => Box::new(c.clone()),
            NotifierConfig::Imessage(c) => Box::new(c.clone()),
            NotifierConfig::Plugin(_) => return Ok(None),
        }))
    }

    /// Driver discriminator for logging / audit.
    pub fn driver_name(&self) -> &'static str {
        match &self.config {
            NotifierConfig::Desktop(_) => "desktop",
            NotifierConfig::Slack(_) => "slack",
            NotifierConfig::Ntfy(_) => "ntfy",
            NotifierConfig::Pushover(_) => "pushover",
            NotifierConfig::Imessage(_) => "imessage",
            NotifierConfig::Plugin(_) => "plugin",
        }
    }

    /// If this entry is a `plugin` escape-hatch, return its plugin reference.
    pub fn as_plugin(&self) -> Option<&PluginNotifierConfig> {
        match &self.config {
            NotifierConfig::Plugin(p) => Some(p),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_desktop_minimal() {
        let toml_str = r#"
            driver = "desktop"
        "#;
        let entry: NotifierEntry = toml::from_str(toml_str).unwrap();
        assert_eq!(entry.driver_name(), "desktop");
        assert!(entry.matches_event("given_up"));
    }

    #[test]
    fn deserialize_slack_with_events() {
        let toml_str = r#"
            driver = "slack"
            webhook_url = "https://hooks.slack.com/x"
            events = ["given_up", "recovered"]
        "#;
        let entry: NotifierEntry = toml::from_str(toml_str).unwrap();
        assert!(entry.matches_event("given_up"));
        assert!(!entry.matches_event("success"));
    }

    #[test]
    fn deserialize_plugin_escape_hatch() {
        let toml_str = r#"
            driver = "plugin"
            name = "notify-custom"
            events = ["given_up"]
            [config]
            foo = "bar"
        "#;
        let entry: NotifierEntry = toml::from_str(toml_str).unwrap();
        let plugin = entry.as_plugin().unwrap();
        assert_eq!(plugin.name, "notify-custom");
        assert_eq!(plugin.config["foo"], "bar");
    }

    #[test]
    fn event_filter_empty_means_all() {
        let toml_str = r#"
            driver = "ntfy"
            topic = "alerts"
        "#;
        let entry: NotifierEntry = toml::from_str(toml_str).unwrap();
        assert!(entry.matches_event("anything"));
        assert!(entry.matches_event("success"));
    }
}
