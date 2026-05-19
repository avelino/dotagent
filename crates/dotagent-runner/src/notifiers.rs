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
use dotagent_notify::{NotifierConfig, NotifyContext, NotifyError};
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
    let ctx = NotifyContext {
        agent: &manifest.agent.name,
        schedule: schedule_id,
        event,
        message,
    };

    for entry in &manifest.notifiers {
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
                agent: manifest.agent.name.clone(),
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
                    agent: manifest.agent.name.clone(),
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
                audit_notifier(audit, manifest, driver, false);
                continue;
            }
        };
        let outcome = driver_box.send(&ctx).await;
        let (ok, skipped) = match &outcome {
            Ok(()) => (true, false),
            Err(NotifyError::Skipped { reason }) => {
                debug!(driver, reason = %reason, "notifier skipped (rate-limit / dedup)");
                (true, true)
            }
            Err(e) => {
                warn!(driver, error = %e, "notifier failed");
                (false, false)
            }
        };
        audit_notifier(audit, manifest, driver, ok);
        let _ = skipped;
    }
}

fn audit_notifier(
    audit: Option<&AuditLog>,
    manifest: &AgentManifest,
    driver: &'static str,
    ok: bool,
) {
    if let Some(log) = audit {
        let _ = log.append(AuditEvent::PluginInvoked {
            agent: manifest.agent.name.clone(),
            plugin: format!("notifier:{driver}"),
            plugin_kind: "notify".into(),
            ok,
        });
    }
}
