//! End-of-day health summary delivered via the configured notification
//! plugin.
//!
//! The daemon fires this internally at `daily_summary_time` (default 22:45)
//! once per day. Standalone invocation is also supported for testing:
//! `dotagent daily-summary --dry-run`.

use anyhow::{Context, Result};
use chrono::Local;
use dotagent_plugin::{InvokePayload, PluginClient, PluginKind};
use dotagent_scheduler::{health_state, HealthState, ResolvedPolicy};
use dotagent_state::{slug_from_args, StateStore};
use serde_json::json;
use tracing::warn;

use crate::discovery;

/// Default target. In production this should be read from
/// `~/.config/dotagent/config.toml` (TODO once that file lands).
const DEFAULT_NOTIFY_PLUGIN: &str = "notify-imessage";
const DEFAULT_NOTIFY_TO: &str = "+5511999999999";

pub async fn run(dry_run: bool) -> Result<()> {
    let state = StateStore::from_home().context("opening state store")?;
    let agents = discovery::discover_all()?;
    let now = Local::now();

    let mut ok = 0;
    let mut degraded: Vec<String> = vec![];
    let mut failing: Vec<String> = vec![];
    let mut stale: Vec<String> = vec![];

    for agent in &agents {
        if !agent.manifest.agent.monitor {
            continue;
        }
        for sched in &agent.manifest.schedules {
            let policy = ResolvedPolicy::resolve(&agent.manifest, sched);
            let slug = slug_from_args(sched.args());
            let hb = state
                .read_heartbeat(&agent.manifest.agent.name, &slug)
                .ok()
                .flatten();
            let expected = hb
                .as_ref()
                .and_then(|h| h.last_success_at)
                .and_then(|s| chrono::TimeZone::timestamp_opt(&Local, s, 0).single());
            let window = expected.and_then(|e| {
                state
                    .read_window(&agent.manifest.agent.name, &slug, e)
                    .ok()
                    .flatten()
            });
            let (s, reason) = health_state(sched, &policy, hb.as_ref(), window.as_ref(), now);
            let label = format!("{}/{} — {}", agent.manifest.agent.name, sched.id(), reason);
            match s {
                HealthState::Ok => ok += 1,
                HealthState::Degraded => degraded.push(label),
                HealthState::Failing => failing.push(label),
                HealthState::Stale => stale.push(label),
            }
        }
    }

    let total = ok + degraded.len() + failing.len() + stale.len();
    let mut body = format!("📊 Agents · {}\n", now.format("%Y-%m-%d"));
    body.push_str(&format!("{ok}/{total} ok\n"));
    if !failing.is_empty() {
        body.push_str("\n❌ Falhando:\n");
        for f in &failing {
            body.push_str(&format!("  · {f}\n"));
        }
    }
    if !degraded.is_empty() {
        body.push_str("\n⚠️ Degradado:\n");
        for d in &degraded {
            body.push_str(&format!("  · {d}\n"));
        }
    }
    if !stale.is_empty() {
        body.push_str("\n🕑 Stale:\n");
        for s in &stale {
            body.push_str(&format!("  · {s}\n"));
        }
    }

    if dry_run {
        println!("{body}");
        return Ok(());
    }

    let plugins = PluginClient::from_environment();
    let payload = InvokePayload {
        kind: PluginKind::Notify,
        agent: "dotagent".into(),
        schedule: "daily-summary".into(),
        event: "daily_summary".into(),
        message: Some(body),
        config: json!({ "to": DEFAULT_NOTIFY_TO }),
    };
    match plugins.invoke(DEFAULT_NOTIFY_PLUGIN, &payload).await {
        Ok(r) if r.ok => Ok(()),
        Ok(r) => {
            warn!("daily-summary delivery returned ok=false: {r:?}");
            Ok(())
        }
        Err(e) => {
            warn!(error = %e, "daily-summary delivery failed");
            Ok(())
        }
    }
}
