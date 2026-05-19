//! Adaptive daemon loop.
//!
//! One launchd plist (`run.avelino.dotagent`) keeps this process alive. The
//! loop:
//!
//!   1. Discover manifests + detect drift / phantom agents.
//!   2. For each (agent, schedule), check whether its current cron window
//!      has already succeeded. If not, dispatch the run.
//!   3. Compute the next event across all schedules.
//!   4. Sleep until `min(next_event, now + max_sleep)` — wake-up early on
//!      SIGHUP (reload) or SIGTERM (graceful exit).
//!
//! No polling. The safety net `max_sleep = 30min` exists to (a) re-check
//! the filesystem if a new manifest was dropped and (b) bound how stale
//! the loaded state can get.

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone};
use dotagent_core::{audit::AuditEvent, AgentManifest, Heartbeat, Schedule, WindowState};
use dotagent_plugin::PluginClient;
use dotagent_runner::{run_with_hooks, RunContext, RunSpec};
use dotagent_scheduler::{
    compute_next_event, expected_at, is_stale, should_retry, AgentSchedulePair, ResolvedPolicy,
};
use dotagent_state::{
    audit::AuditLog,
    manifest_cache::{hash_manifest_file, KnownManifest, ManifestCache},
    slug_from_args, StateStore,
};
use tokio::signal::unix::{signal, SignalKind};
use tracing::{info, warn};

use crate::discovery::{self, DiscoveredAgent};

/// Hard upper bound on a single sleep cycle. After this, the daemon
/// re-discovers manifests even if no event fires — covers the case where a
/// fresh manifest was dropped into `~/.config/dotagent/agents/`.
const MAX_SLEEP_MINUTES: i64 = 30;

/// PID file location (used by `dotagent reload` / `status` to find the daemon).
pub fn pidfile_path() -> Option<std::path::PathBuf> {
    Some(dotagent_state::paths::daemon_pid_file())
}

fn write_pidfile() -> Result<()> {
    let path = pidfile_path().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, std::process::id().to_string())?;
    Ok(())
}

/// Auto-cleanup pidfile on daemon exit.
struct PidGuard;
impl Drop for PidGuard {
    fn drop(&mut self) {
        if let Some(path) = pidfile_path() {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub async fn run() -> Result<()> {
    let state = StateStore::from_home().context("opening state store")?;
    let audit = AuditLog::from_home().context("opening audit log")?;
    let plugins = PluginClient::from_environment();
    let cache = ManifestCache::from_home().context("opening manifest cache")?;

    // Write our PID so `dotagent reload` / `dotagent status` can find us.
    write_pidfile()?;
    let _pid_guard = PidGuard;

    audit.append(AuditEvent::DaemonStarted {
        version: env!("CARGO_PKG_VERSION").into(),
        pid: std::process::id(),
    })?;

    // Verify the existing chain at startup; emit `AuditChainBroken` (which
    // itself becomes a chained entry) if tampered.
    if let Ok(Some(brk)) = audit.verify_chain() {
        warn!(position = brk.position, "audit chain broken");
        let _ = audit.append(AuditEvent::AuditChainBroken {
            position: brk.position,
            expected_prev_hash: brk.expected,
            actual_prev_hash: brk.actual,
        });
    }

    let mut sighup = signal(SignalKind::hangup()).context("registering SIGHUP")?;
    let mut sigterm = signal(SignalKind::terminate()).context("registering SIGTERM")?;
    let mut sigint = signal(SignalKind::interrupt()).context("registering SIGINT")?;

    info!("daemon started");
    let app_config =
        dotagent_core::Config::load(dotagent_state::paths::config_file()).unwrap_or_default();

    let mut last_summary_date: Option<chrono::NaiveDate> = None;
    let mut last_retention_date: Option<chrono::NaiveDate> = None;
    let exit_reason = loop {
        let cycle_start = Local::now();
        let TickResult { next_event, .. } =
            tick_once(&state, &audit, &plugins, &cache, cycle_start).await;

        // Daily summary at 22:45 local time. Fires once per day; the
        // `last_summary_date` guard avoids double-fire when the daemon
        // re-enters the window (e.g., user runs `dotagent reload`).
        if should_run_daily_summary(cycle_start, last_summary_date) {
            if let Err(e) = crate::commands::daily_summary::run(false).await {
                warn!(error = %e, "daily-summary delivery failed");
            }
            last_summary_date = Some(cycle_start.date_naive());
        }

        // Log retention sweep: runs once per day at 03:00 (chosen so it
        // happens during natural quiet hours and never fights with the
        // 22:45 summary).
        if should_run_retention(cycle_start, last_retention_date) {
            let stats = dotagent_telemetry::retention::sweep_all(&app_config.logging);
            info!(
                compressed = stats.compressed,
                deleted = stats.deleted,
                scanned = stats.scanned,
                "log retention sweep completed"
            );
            last_retention_date = Some(cycle_start.date_naive());
        }

        let sleep_target = compute_sleep_target(cycle_start, next_event);
        let sleep_for = (sleep_target - Local::now())
            .to_std()
            .unwrap_or(Duration::from_secs(60));
        info!(
            "sleeping until {} ({}s)",
            sleep_target.format("%Y-%m-%dT%H:%M:%S%z"),
            sleep_for.as_secs()
        );

        tokio::select! {
            _ = tokio::time::sleep(sleep_for) => { continue; }
            _ = sighup.recv() => {
                info!("SIGHUP — reloading on next tick");
                let _ = audit.append(AuditEvent::ConfigReloaded { reason: "SIGHUP".into() });
                continue;
            }
            _ = sigterm.recv() => break "SIGTERM",
            _ = sigint.recv()  => break "SIGINT",
        }
    };

    info!(reason = exit_reason, "daemon stopping");
    audit.append(AuditEvent::DaemonStopped {
        reason: exit_reason.into(),
    })?;
    Ok(())
}

/// Output of one tick iteration.
#[derive(Debug, Clone)]
pub struct TickResult {
    pub agents_scanned: u32,
    pub runs_dispatched: u32,
    pub next_event: Option<DateTime<Local>>,
}

/// Run one iteration: discover, cache-check, dispatch retries, compute next.
/// Used by the daemon loop AND by `dotagent tick`.
pub async fn tick_once(
    state: &StateStore,
    audit: &AuditLog,
    plugins: &PluginClient,
    cache: &ManifestCache,
    now: DateTime<Local>,
) -> TickResult {
    let agents = match discovery::discover_all() {
        Ok(a) => a,
        Err(e) => {
            warn!(error = ?e, "discovery failed");
            vec![]
        }
    };

    if let Err(e) = check_cache(&agents, cache, audit) {
        warn!(error = ?e, "manifest cache check failed");
    }

    let _ = audit.append(AuditEvent::TickStarted {
        agents_scanned: agents.len() as u32,
    });

    let runs_dispatched = dispatch_due_runs(&agents, state, audit, plugins, now).await;
    let next_event = compute_next_event_from_agents(&agents, state, now);

    let _ = audit.append(AuditEvent::TickCompleted {
        agents_scanned: agents.len() as u32,
        runs_dispatched,
        next_event_iso: next_event.map(|t| t.format("%Y-%m-%dT%H:%M:%S%z").to_string()),
    });

    TickResult {
        agents_scanned: agents.len() as u32,
        runs_dispatched,
        next_event,
    }
}

/// Dry-run variant: reports what `tick_once` *would* do without dispatching
/// or writing to the audit log.
pub async fn tick_dry_run(state: &StateStore, now: DateTime<Local>) -> TickResult {
    let agents = discovery::discover_all().unwrap_or_default();
    let mut would_dispatch = 0u32;

    for agent in &agents {
        if !agent.manifest.agent.monitor {
            continue;
        }
        for sched in &agent.manifest.schedules {
            let last_success = last_success_for(&agent.manifest, sched, state);
            let Some(expected) = expected_at(sched, now, last_success) else {
                continue;
            };
            if expected > now {
                continue;
            }
            if last_success.is_some_and(|ls| ls >= expected) {
                continue;
            }
            let policy = ResolvedPolicy::resolve(&agent.manifest, sched);
            if is_stale(expected, policy.stale_after_minutes, now) {
                continue;
            }
            let slug = slug_from_args(sched.args());
            let window = state
                .read_window(&agent.manifest.agent.name, &slug, expected)
                .ok()
                .flatten()
                .unwrap_or_default();
            if window.given_up {
                continue;
            }
            if window.attempts >= policy.max_retries {
                continue;
            }
            let last_attempt = window
                .last_attempt_at
                .and_then(|t| Local.timestamp_opt(t, 0).single());
            if should_retry(
                window.attempts,
                last_attempt,
                &policy.retry_backoff_minutes,
                now,
            ) {
                println!(
                    "would dispatch {}/{}  attempt {}/{}",
                    agent.manifest.agent.name,
                    sched.id(),
                    window.attempts + 1,
                    policy.max_retries
                );
                would_dispatch += 1;
            }
        }
    }
    let next_event = compute_next_event_from_agents(&agents, state, now);
    TickResult {
        agents_scanned: agents.len() as u32,
        runs_dispatched: would_dispatch,
        next_event,
    }
}

/// Log retention runs once per day at 03:00 ± 30min.
fn should_run_retention(now: DateTime<Local>, last_date: Option<chrono::NaiveDate>) -> bool {
    use chrono::Timelike;
    let in_window = now.hour() == 3 && now.minute() < 30;
    if !in_window {
        return false;
    }
    match last_date {
        Some(d) => d != now.date_naive(),
        None => true,
    }
}

/// True if we're inside the 22:45 window AND haven't fired today yet.
/// Window is `[22:45, 23:15)` to forgive late wake-ups (laptop closed,
/// daemon was still sleeping past 22:45).
fn should_run_daily_summary(
    now: DateTime<Local>,
    last_summary_date: Option<chrono::NaiveDate>,
) -> bool {
    use chrono::Timelike;
    let in_window =
        (now.hour() == 22 && now.minute() >= 45) || (now.hour() == 23 && now.minute() < 15);
    if !in_window {
        return false;
    }
    match last_summary_date {
        Some(d) => d != now.date_naive(),
        None => true,
    }
}

/// Returns `now + min(MAX_SLEEP, next_event)`. Falls back to `now + MAX_SLEEP`
/// when there's no event in sight.
fn compute_sleep_target(
    now: DateTime<Local>,
    next_event: Option<DateTime<Local>>,
) -> DateTime<Local> {
    let safety_cap = now + chrono::Duration::minutes(MAX_SLEEP_MINUTES);
    match next_event {
        Some(t) if t > now && t < safety_cap => t,
        _ => safety_cap,
    }
}

fn compute_next_event_from_agents(
    agents: &[DiscoveredAgent],
    state: &StateStore,
    now: DateTime<Local>,
) -> Option<DateTime<Local>> {
    let pairs: Vec<AgentSchedulePair> = agents
        .iter()
        .filter(|a| a.manifest.agent.monitor)
        .flat_map(|a| {
            a.manifest.schedules.iter().map(move |s| AgentSchedulePair {
                agent_name: &a.manifest.agent.name,
                schedule: s,
                last_success: last_success_for(&a.manifest, s, state),
            })
        })
        .collect();
    compute_next_event(pairs, now)
}

fn last_success_for(
    manifest: &AgentManifest,
    schedule: &Schedule,
    state: &StateStore,
) -> Option<DateTime<Local>> {
    let slug = slug_from_args(schedule.args());
    let hb: Heartbeat = state
        .read_heartbeat(&manifest.agent.name, &slug)
        .ok()
        .flatten()?;
    let ts = hb.last_success_at?;
    Local.timestamp_opt(ts, 0).single()
}

pub(crate) async fn dispatch_due_runs(
    agents: &[DiscoveredAgent],
    state: &StateStore,
    audit: &AuditLog,
    plugins: &PluginClient,
    now: DateTime<Local>,
) -> u32 {
    let mut dispatched = 0u32;
    for agent in agents {
        if !agent.manifest.agent.monitor {
            continue;
        }
        for sched in &agent.manifest.schedules {
            if dispatch_one(agent, sched, state, audit, plugins, now).await {
                dispatched += 1;
            }
        }
    }
    dispatched
}

/// Returns `true` if a run was dispatched (regardless of outcome).
async fn dispatch_one(
    agent: &DiscoveredAgent,
    sched: &Schedule,
    state: &StateStore,
    audit: &AuditLog,
    plugins: &PluginClient,
    now: DateTime<Local>,
) -> bool {
    let last_success = last_success_for(&agent.manifest, sched, state);
    let Some(expected) = expected_at(sched, now, last_success) else {
        return false;
    };
    if expected > now {
        return false;
    }
    if let Some(ls) = last_success {
        if ls >= expected {
            return false; // already succeeded in this window
        }
    }

    let policy = ResolvedPolicy::resolve(&agent.manifest, sched);

    // 1. Skip if the window is too old to bother retrying. Matches the
    //    legacy orchestrator's `stale_after_minutes` semantics.
    if is_stale(expected, policy.stale_after_minutes, now) {
        return false;
    }

    let slug = slug_from_args(sched.args());
    let mut window = state
        .read_window(&agent.manifest.agent.name, &slug, expected)
        .ok()
        .flatten()
        .unwrap_or_else(|| WindowState {
            agent: agent.manifest.agent.name.clone(),
            schedule_id: sched.id().to_string(),
            expected_at: expected.timestamp(),
            ..Default::default()
        });

    if window.given_up {
        return false;
    }

    // 2. Backoff gate. If we've already attempted ≥1 and the wait hasn't
    //    elapsed yet, skip.
    let last_attempt = window
        .last_attempt_at
        .and_then(|t| Local.timestamp_opt(t, 0).single());
    if !should_retry(
        window.attempts,
        last_attempt,
        &policy.retry_backoff_minutes,
        now,
    ) {
        return false;
    }

    // 3. max_retries gate. If we've already burned them, mark given_up and
    //    fire on_failure(given_up).
    if window.attempts >= policy.max_retries {
        give_up(agent, sched, &mut window, audit, plugins, &slug, expected).await;
        return false;
    }

    // 4. Dispatch.
    info!(
        agent = %agent.manifest.agent.name,
        schedule = %sched.id(),
        attempt = window.attempts + 1,
        max_retries = policy.max_retries,
        expected = %expected.format("%Y-%m-%dT%H:%M:%S%z"),
        "dispatching run"
    );
    let args: Vec<String> = sched.args().to_vec();
    let manifest_path = agent.dir.join("agent.toml");
    let manifest_sha256 = hash_manifest_file(&manifest_path).ok();

    let attempts_before = window.attempts;
    let spec = RunSpec {
        manifest: &agent.manifest,
        manifest_dir: &agent.dir,
        schedule_id: sched.id(),
        args: &args,
        dry_run: false,
        manifest_sha256,
    };
    let ctx = RunContext {
        state,
        plugins: Some(plugins),
        audit: Some(audit),
    };

    let outcome = match run_with_hooks(spec, &ctx).await {
        Ok(o) => o,
        Err(e) => {
            warn!(
                agent = %agent.manifest.agent.name,
                error = %e,
                "run_with_hooks failed"
            );
            return false;
        }
    };

    // 5. Update window state from outcome.
    window.attempts += 1;
    window.last_attempt_at = Some(now.timestamp());

    match outcome {
        dotagent_runner::OrchestratedOutcome::PreflightFailed { plugin, .. } => {
            window.last_attempt_exit_code = Some(-1);
            window.last_attempt_stderr = Some(format!("preflight {plugin} failed"));
        }
        dotagent_runner::OrchestratedOutcome::Ran(ref ro) => {
            window.last_attempt_exit_code = Some(ro.exit_code);
            window.last_attempt_stderr = Some(ro.stderr_tail.clone());

            if ro.exit_code == 0 && attempts_before > 0 {
                // recovered after at least one failure
                let _ = audit.append(AuditEvent::AgentRecovered {
                    agent: agent.manifest.agent.name.clone(),
                    schedule: sched.id().to_string(),
                    attempts: window.attempts,
                });
                fire_on_failure_event(
                    &agent.manifest,
                    sched.id(),
                    "recovered",
                    &format!(
                        "agent {} recovered on attempt {}",
                        agent.manifest.agent.name, window.attempts
                    ),
                    plugins,
                    audit,
                )
                .await;
            } else if ro.exit_code != 0 && window.attempts >= policy.max_retries {
                give_up(agent, sched, &mut window, audit, plugins, &slug, expected).await;
                return true;
            }
        }
    }

    if let Err(e) = state.write_window(&window, &slug, expected) {
        warn!(error = %e, "writing window state failed");
    }
    true
}

async fn give_up(
    agent: &DiscoveredAgent,
    sched: &Schedule,
    window: &mut WindowState,
    audit: &AuditLog,
    plugins: &PluginClient,
    slug: &str,
    expected: DateTime<Local>,
) {
    window.given_up = true;
    window.given_up_at = Some(Local::now().timestamp());

    let _ = audit.append(AuditEvent::AgentGivenUp {
        agent: agent.manifest.agent.name.clone(),
        schedule: sched.id().to_string(),
        attempts: window.attempts,
        last_exit: window.last_attempt_exit_code.unwrap_or(-1),
        stderr_tail: window.last_attempt_stderr.clone().unwrap_or_default(),
    });

    let message = format!(
        "🚨 {}/{} gave up after {} attempts (exit {})\n{}",
        agent.manifest.agent.name,
        sched.id(),
        window.attempts,
        window.last_attempt_exit_code.unwrap_or(-1),
        window.last_attempt_stderr.clone().unwrap_or_default()
    );
    fire_on_failure_event(
        &agent.manifest,
        sched.id(),
        "given_up",
        &message,
        plugins,
        audit,
    )
    .await;

    if let Ok(store) = StateStore::from_home() {
        if let Err(e) = store.write_window(window, slug, expected) {
            warn!(error = %e, "writing given_up window state failed");
        }
    }
}

async fn fire_on_failure_event(
    manifest: &AgentManifest,
    schedule_id: &str,
    event: &str,
    message: &str,
    plugins: &PluginClient,
    audit: &AuditLog,
) {
    dotagent_runner::hooks::fire_on_failure(
        manifest,
        schedule_id,
        event,
        message,
        plugins,
        Some(audit),
    )
    .await;
    dotagent_runner::notifiers::fire_notifiers(
        manifest,
        schedule_id,
        event,
        message,
        Some(plugins),
        Some(audit),
    )
    .await;
}

/// Update the manifest cache. Emits `phantom_agent_detected` for unseen
/// names, `manifest_drift_detected` for hash changes, and
/// `manifest_loaded` on first sight.
fn check_cache(agents: &[DiscoveredAgent], cache: &ManifestCache, audit: &AuditLog) -> Result<()> {
    let mut known = cache.load().unwrap_or_default();
    let now = Local::now().format("%Y-%m-%dT%H:%M:%S%z").to_string();
    let mut changed = false;

    for agent in agents {
        let manifest_path = agent.dir.join("agent.toml");
        let Ok(sha) = hash_manifest_file(&manifest_path) else {
            continue;
        };
        match known.entries.get_mut(&agent.manifest.agent.name) {
            Some(entry) if entry.sha256 != sha => {
                let _ = audit.append(AuditEvent::ManifestDriftDetected {
                    agent: agent.manifest.agent.name.clone(),
                    path: manifest_path.display().to_string(),
                    expected_sha256: entry.sha256.clone(),
                    actual_sha256: sha.clone(),
                });
                entry.sha256 = sha;
                entry.last_seen_at_iso = now.clone();
                entry.path = manifest_path;
                changed = true;
            }
            Some(entry) => {
                entry.last_seen_at_iso = now.clone();
            }
            None => {
                let _ = audit.append(AuditEvent::PhantomAgentDetected {
                    agent: agent.manifest.agent.name.clone(),
                    path: manifest_path.display().to_string(),
                    sha256: sha.clone(),
                });
                let _ = audit.append(AuditEvent::ManifestLoaded {
                    agent: agent.manifest.agent.name.clone(),
                    path: manifest_path.display().to_string(),
                    sha256: sha.clone(),
                });
                known.entries.insert(
                    agent.manifest.agent.name.clone(),
                    KnownManifest {
                        path: manifest_path,
                        sha256: sha,
                        first_seen_at_iso: now.clone(),
                        last_seen_at_iso: now.clone(),
                    },
                );
                changed = true;
            }
        }
    }
    if changed {
        cache.save(&known)?;
    }
    Ok(())
}
