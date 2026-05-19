//! iMessage notifications via `osascript` Messages.app automation.
//!
//! This is the **only** driver that still spawns a subprocess. Apple does
//! not expose any public API to send iMessages — `osascript` is the
//! supported automation surface. Keeping it as a built-in driver (rather
//! than a separate plugin) removes the plugin protocol fork without
//! pretending the iMessage limitation went away.
//!
//! Rate-limit state lives at
//! `$DOTAGENT_HOME/state/notify/imessage/<slug>.json`, mirroring the
//! legacy `dotagent-plugin-notify-imessage` path so existing state files
//! continue to work.

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::warn;

use crate::{Notifier, NotifyContext, NotifyError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImessageConfig {
    pub to: String,
    /// Skip-if-recent: `now - last_send < rate_limit_minutes` ⇒ skip. `None`
    /// or `0` disables. Mirrors the legacy WARP-alert hourly cadence.
    #[serde(default)]
    pub rate_limit_minutes: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RateState {
    last_send_at: i64,
    last_send_iso: String,
    to: String,
}

#[async_trait]
impl Notifier for ImessageConfig {
    fn driver_name(&self) -> &'static str {
        "imessage"
    }

    async fn send(&self, ctx: &NotifyContext<'_>) -> Result<()> {
        if !cfg!(target_os = "macos") {
            return Err(NotifyError::UnsupportedPlatform { driver: "imessage" });
        }
        if self.to.is_empty() {
            return Err(NotifyError::Config("imessage: `to` is required".into()));
        }

        let now = Local::now();

        // Rate-limit gate.
        if let Some(rl) = self.rate_limit_minutes.filter(|n| *n > 0) {
            if let Some(state) = read_rate_state(&self.to) {
                if let Some(last) =
                    DateTime::from_timestamp(state.last_send_at, 0).map(|t| t.with_timezone(&Local))
                {
                    let elapsed_min = (now - last).num_minutes();
                    if (elapsed_min as u32) < rl {
                        let next = last + chrono::Duration::minutes(rl as i64);
                        return Err(NotifyError::Skipped {
                            reason: format!(
                                "rate_limited until {}",
                                next.format("%Y-%m-%dT%H:%M:%S%z")
                            ),
                        });
                    }
                }
            }
        }

        let script = format!(
            r#"tell application "Messages"
                set targetService to 1st account whose service type = iMessage
                set targetBuddy to participant "{to}" of targetService
                send "{msg}" to targetBuddy
            end tell"#,
            to = escape_applescript(&self.to),
            msg = escape_applescript(ctx.message),
        );

        let status = Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .await?;

        if !status.success() {
            return Err(NotifyError::Backend(format!(
                "osascript exited {}",
                status.code().unwrap_or(-1)
            )));
        }

        if let Some(rl) = self.rate_limit_minutes.filter(|n| *n > 0) {
            let _ = rl;
            if let Err(e) = write_rate_state(
                &self.to,
                &RateState {
                    last_send_at: now.timestamp(),
                    last_send_iso: now.format("%Y-%m-%dT%H:%M:%S%z").to_string(),
                    to: self.to.clone(),
                },
            ) {
                warn!(error = %e, "imessage: failed to persist rate_limit state");
            }
        }

        Ok(())
    }
}

fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ---- Rate-limit state I/O ---------------------------------------------

fn state_path(to: &str) -> Option<PathBuf> {
    let slug: String = to
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    Some(
        dotagent_home()?
            .join("state/notify/imessage")
            .join(format!("{slug}.json")),
    )
}

/// Resolve `$DOTAGENT_HOME` (default `~/.config/dotagent`). Kept in-crate so
/// this crate stays decoupled from `dotagent-state`.
fn dotagent_home() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DOTAGENT_HOME") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    Some(dirs::home_dir()?.join(".config/dotagent"))
}

fn read_rate_state(to: &str) -> Option<RateState> {
    let path = state_path(to)?;
    if !path.exists() {
        return None;
    }
    let raw = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_rate_state(to: &str, state: &RateState) -> Result<()> {
    let Some(path) = state_path(to) else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}
