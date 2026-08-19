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
        let (outcome, canonical_chat_id) = match &entry.config {
            NotifierConfig::Telegram(config) => {
                let result = config.send_with_result(&ctx).await;
                let canonical_chat_id = match &result {
                    Ok(Some(result)) => result.chat_id.map(|chat_id| chat_id.to_string()),
                    _ => None,
                };
                let outcome = result.map(|result| result.map(|result| result.message_id));
                (outcome, canonical_chat_id)
            }
            _ => (driver_box.send(&ctx).await, None),
        };

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
            if let Err(e) = record_sent_message(
                &store,
                entry,
                *message_id,
                canonical_chat_id.as_deref(),
                &ctx,
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

fn record_sent_message(
    store: &dotagent_state::SentMessageStore,
    entry: &NotifierEntry,
    message_id: i64,
    canonical_chat_id: Option<&str>,
    ctx: &NotifyContext<'_>,
) -> dotagent_state::Result<()> {
    let message = dotagent_state::SentMessage {
        chat_id: None,
        agent: ctx.agent.to_string(),
        schedule: ctx.schedule.to_string(),
        event: ctx.event.to_string(),
        at: chrono::Local::now().timestamp(),
    };

    match &entry.config {
        NotifierConfig::Telegram(config) => {
            let chat_id = if let Some(chat_id) = canonical_chat_id {
                chat_id
            } else if config.chat_id.parse::<i64>().is_ok() {
                // Numeric configuration was already the canonical inbound id.
                &config.chat_id
            } else {
                // A username can send successfully but cannot scope an inbound
                // reply when Telegram omitted the canonical id.
                warn!(
                    chat_id = %config.chat_id,
                    message_id,
                    "Telegram notification cannot be recorded for reply correlation: response omitted canonical chat id"
                );
                return Ok(());
            };
            store.record_for_chat(chat_id, message_id, message)
        }
        _ => store.record(message_id, message),
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

#[cfg(test)]
mod tests {
    use super::*;
    use dotagent_notify::telegram::{TelegramConfig, TelegramSendResult};

    fn telegram_entry(chat_id: &str) -> NotifierEntry {
        NotifierEntry {
            config: NotifierConfig::Telegram(TelegramConfig {
                bot_token: "token".into(),
                chat_id: chat_id.into(),
                parse_mode: None,
                disable_notification: None,
            }),
            events: Vec::new(),
        }
    }

    #[test]
    fn telegram_correlation_is_recorded_for_its_chat() {
        let dir = tempfile::tempdir().unwrap();
        let store = dotagent_state::SentMessageStore::new(dir.path().join("sent.json"));
        let entry = telegram_entry("-1001");
        let ctx = NotifyContext {
            agent: "agent",
            schedule: "daily",
            event: "given_up",
            message: "failed",
        };

        record_sent_message(&store, &entry, 42, None, &ctx).unwrap();

        let found = store.resolve_for_chat("-1001", 42).expect("scoped record");
        assert_eq!(found.chat_id.as_deref(), Some("-1001"));
        assert_eq!(found.agent, "agent");
        assert!(store.resolve_for_chat("chat-b", 42).is_none());
        assert!(store.resolve(42).is_none());
    }

    #[test]
    fn telegram_username_correlation_uses_canonical_chat_id_from_send_message() {
        let dir = tempfile::tempdir().unwrap();
        let store = dotagent_state::SentMessageStore::new(dir.path().join("sent.json"));
        let entry = telegram_entry("@channel_username");
        let ctx = NotifyContext {
            agent: "agent",
            schedule: "daily",
            event: "given_up",
            message: "failed",
        };
        // Mock the successful sendMessage response; no Telegram request is made.
        let response = serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 42,
                "chat": {"id": -1001234567890i64}
            }
        });
        let send_result = TelegramSendResult {
            message_id: response["result"]["message_id"]
                .as_i64()
                .expect("sendMessage must return message_id"),
            chat_id: response["result"]["chat"]["id"].as_i64(),
        };
        let inbound_chat_id = send_result.chat_id.unwrap().to_string();

        record_sent_message(
            &store,
            &entry,
            send_result.message_id,
            Some(&inbound_chat_id),
            &ctx,
        )
        .unwrap();

        let found = store
            .resolve_for_chat(&inbound_chat_id, send_result.message_id)
            .expect("inbound numeric chat id must resolve the record");
        assert_eq!(found.agent, "agent");
        assert!(store
            .resolve_for_chat("@channel_username", send_result.message_id)
            .is_none());
    }
}
