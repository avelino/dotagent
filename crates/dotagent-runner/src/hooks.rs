//! Plugin lifecycle hooks: preflight + on_success + on_failure.
//!
//! The runner asks this module to invoke plugins at the right moments:
//!
//! - **Preflight** runs BEFORE spawn. If any plugin returns `ok=false`, the
//!   run is aborted and `on_failure` fires with `event = "preflight"`.
//! - **on_success** runs AFTER a successful spawn (exit 0).
//! - **on_failure** runs AFTER a failed spawn (non-zero or timeout), AND
//!   when preflight aborts.
//!
//! Failures of hook plugins themselves are logged as audit `plugin_invoked`
//! events with `ok=false`, but they do not propagate to the run's exit
//! code — the run already happened (or was already aborted), so plugin
//! failure can't undo it.

use dotagent_core::{
    audit::AuditEvent,
    manifest::{AgentManifest, PluginRef},
};
use dotagent_plugin::{InvokePayload, PluginClient, PluginKind, PluginResponse};
use dotagent_state::AuditLog;
use serde_json::json;
use tracing::warn;

/// Result of preflight aggregation.
pub struct PreflightOutcome {
    pub passed: bool,
    /// If any check failed, the first failure's plugin name + suggest.
    pub failed_plugin: Option<String>,
    pub suggest: Option<String>,
}

/// Run every preflight plugin in declaration order. Short-circuits on
/// first failure. Each invocation emits a `plugin_invoked` audit event.
pub async fn run_preflight(
    manifest: &AgentManifest,
    schedule_id: &str,
    client: &PluginClient,
    audit: Option<&AuditLog>,
) -> PreflightOutcome {
    for pref in &manifest.preflight {
        let payload = InvokePayload {
            kind: PluginKind::Preflight,
            agent: manifest.agent.name.clone(),
            schedule: schedule_id.to_string(),
            event: "preflight".into(),
            message: None,
            config: pref.config.clone(),
        };
        let result = client.invoke(&pref.plugin, &payload).await;
        let (ok, response) = match &result {
            Ok(r) => (r.ok, Some(r)),
            Err(e) => {
                warn!(plugin = %pref.plugin, error = %e, "preflight plugin invocation failed");
                (false, None)
            }
        };
        if let Some(log) = audit {
            let _ = log.append(AuditEvent::PluginInvoked {
                agent: manifest.agent.name.clone(),
                plugin: pref.plugin.clone(),
                plugin_kind: "preflight".into(),
                ok,
            });
        }
        if !ok {
            let suggest = response
                .and_then(|r| r.extra.get("suggest"))
                .and_then(|v| v.as_str())
                .map(String::from);
            if let Some(log) = audit {
                let _ = log.append(AuditEvent::PreflightFailed {
                    agent: manifest.agent.name.clone(),
                    schedule: schedule_id.to_string(),
                    plugin: pref.plugin.clone(),
                    suggest: suggest.clone(),
                });
            }
            return PreflightOutcome {
                passed: false,
                failed_plugin: Some(pref.plugin.clone()),
                suggest,
            };
        }
    }
    PreflightOutcome {
        passed: true,
        failed_plugin: None,
        suggest: None,
    }
}

/// Invoke every plugin in `[[on_success]]` (filtered by `event` if declared).
pub async fn fire_on_success(
    manifest: &AgentManifest,
    schedule_id: &str,
    message: &str,
    client: &PluginClient,
    audit: Option<&AuditLog>,
) {
    fire_hooks(
        &manifest.on_success,
        manifest,
        schedule_id,
        "success",
        message,
        PluginKind::Sink,
        client,
        audit,
    )
    .await;
}

/// Invoke every plugin in `[[on_failure]]`. `event` is the failure event
/// name (`attempt_failed`, `given_up`, `preflight`).
pub async fn fire_on_failure(
    manifest: &AgentManifest,
    schedule_id: &str,
    event: &str,
    message: &str,
    client: &PluginClient,
    audit: Option<&AuditLog>,
) {
    fire_hooks(
        &manifest.on_failure,
        manifest,
        schedule_id,
        event,
        message,
        PluginKind::Notify,
        client,
        audit,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn fire_hooks(
    refs: &[PluginRef],
    manifest: &AgentManifest,
    schedule_id: &str,
    event: &str,
    message: &str,
    kind: PluginKind,
    client: &PluginClient,
    audit: Option<&AuditLog>,
) {
    for hook in refs {
        if !event_matches(&hook.events, event) {
            continue;
        }
        let payload = InvokePayload {
            kind,
            agent: manifest.agent.name.clone(),
            schedule: schedule_id.to_string(),
            event: event.into(),
            message: Some(message.into()),
            config: hook.config.clone(),
        };
        let resp: PluginResponse = match client.invoke(&hook.plugin, &payload).await {
            Ok(r) => r,
            Err(e) => {
                warn!(plugin = %hook.plugin, error = %e, "hook plugin invocation failed");
                PluginResponse {
                    ok: false,
                    error: Some(e.to_string()),
                    extra: serde_json::Map::from_iter([("invoke_error".into(), json!(true))]),
                }
            }
        };
        if let Some(log) = audit {
            let _ = log.append(AuditEvent::PluginInvoked {
                agent: manifest.agent.name.clone(),
                plugin: hook.plugin.clone(),
                plugin_kind: format!("{kind:?}").to_lowercase(),
                ok: resp.ok,
            });
        }
    }
}

fn event_matches(filter: &[String], event: &str) -> bool {
    filter.is_empty() || filter.iter().any(|e| e == event)
}
