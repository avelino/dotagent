//! Dispatch built-in notifiers (`[[notifiers]]` array on the manifest).
//!
//! Sits next to `hooks.rs`. `hooks.rs` still drives the legacy
//! `[[on_failure]]` / `[[on_success]]` plugin protocol path. This module
//! handles the new in-process dispatch — zero subprocess for desktop /
//! Slack / ntfy / Pushover; only iMessage still forks (`osascript`).
//!
//! Failure of an individual notifier does **not** propagate to the run's
//! exit code — the run already happened. Errors are logged and audited.

use dotagent_core::{audit::AuditEvent, manifest::AgentManifest};
use dotagent_notify::{NotifierConfig, NotifierEntry, NotifyContext, NotifyError};
use dotagent_plugin::{InvokePayload, PluginClient, PluginKind};
use dotagent_state::AuditLog;
use tracing::{debug, warn};

/// Fire every `[[notifiers]]` entry that matches `event`. Plugin escape-hatch
/// entries (`driver = "plugin"`) are dispatched through `PluginClient`; all
/// other drivers run in-process.
pub async fn fire_notifiers(
    manifest: &AgentManifest,
    schedule_id: &str,
    event: &str,
    message: &str,
    plugins: Option<&PluginClient>,
    audit: Option<&AuditLog>,
) {
    fire_notifier_entries(
        &manifest.agent.name,
        &manifest.notifiers,
        schedule_id,
        event,
        message,
        plugins,
        audit,
    )
    .await
}

/// Same dispatch, for entries that do not come from a manifest.
///
/// The daemon's daily summary is not an agent — it has no `agent.toml`, no
/// schedule and no run — but it is delivered by the same drivers, and it wants
/// the same audit trail and the same reply correlation. Taking the entries
/// directly is what lets it reuse this without fabricating a manifest that no
/// file backs.
#[allow(clippy::too_many_arguments)]
pub async fn fire_notifier_entries(
    agent: &str,
    entries: &[NotifierEntry],
    schedule_id: &str,
    event: &str,
    message: &str,
    plugins: Option<&PluginClient>,
    audit: Option<&AuditLog>,
) {
    let ctx = NotifyContext {
        agent,
        schedule: schedule_id,
        event,
        message,
    };

    for entry in entries {
        if !entry.matches_event(event) {
            continue;
        }
        let driver = entry.driver_name();

        // Plugin escape hatch — falls back to the legacy plugin protocol.
        if let NotifierConfig::Plugin(p) = &entry.config {
            let Some(client) = plugins else {
                warn!(driver, plugin = %p.name, "plugin notifier but no PluginClient — skipping");
                continue;
            };
            let payload = InvokePayload {
                kind: PluginKind::Notify,
                agent: agent.to_string(),
                schedule: schedule_id.to_string(),
                event: event.into(),
                message: Some(message.into()),
                config: p.config.clone(),
            };
            let ok = match client.invoke(&p.name, &payload).await {
                Ok(r) => r.ok,
                Err(e) => {
                    warn!(driver, plugin = %p.name, error = %e, "plugin notifier failed");
                    false
                }
            };
            if let Some(log) = audit {
                let _ = log.append(AuditEvent::PluginInvoked {
                    agent: agent.to_string(),
                    plugin: p.name.clone(),
                    plugin_kind: "notify".into(),
                    ok,
                });
            }
            continue;
        }

        // Built-in driver — build and dispatch in-process.
        let driver_box = match entry.build_driver() {
            Ok(Some(d)) => d,
            Ok(None) => continue, // shouldn't happen — plugin branch handled above
            Err(e) => {
                warn!(driver, error = %e, "failed to build notifier");
                audit_notifier(audit, agent, driver, false);
                continue;
            }
        };
        let outcome = driver_box.send(&ctx).await;

        // Remember what this notification was about, when the transport gave
        // it an id. A failure alert is the message someone replies to, and
        // without this the reply arrives as prose that has to be parsed to
        // guess the agent — the text format is not even consistent across
        // events, so that guess would be wrong sooner or later.
        //
        // Best-effort by design: correlation is a convenience on top of a
        // notification that was already delivered.
        if let Ok(Some(message_id)) = &outcome {
            let store = dotagent_state::SentMessageStore::from_home();
            if let Err(e) = store.record(
                *message_id,
                dotagent_state::SentMessage {
                    agent: ctx.agent.to_string(),
                    schedule: ctx.schedule.to_string(),
                    event: ctx.event.to_string(),
                    at: chrono::Local::now().timestamp(),
                },
            ) {
                warn!(driver, error = %e, "could not record notification for reply correlation");
            }
        }

        let (ok, skipped) = match &outcome {
            Ok(_) => (true, false),
            Err(NotifyError::Skipped { reason }) => {
                debug!(driver, reason = %reason, "notifier skipped (rate-limit / dedup)");
                (true, true)
            }
            Err(e) => {
                warn!(driver, error = %e, "notifier failed");
                (false, false)
            }
        };
        audit_notifier(audit, agent, driver, ok);
        let _ = skipped;
    }
}

fn audit_notifier(audit: Option<&AuditLog>, agent: &str, driver: &'static str, ok: bool) {
    if let Some(log) = audit {
        let _ = log.append(AuditEvent::PluginInvoked {
            agent: agent.to_string(),
            plugin: format!("notifier:{driver}"),
            plugin_kind: "notify".into(),
            ok,
        });
    }
}
