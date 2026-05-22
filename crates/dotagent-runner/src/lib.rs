//! Agent runner — spawn the agent process with timeout, capture stdio, inject
//! environment variables, and update the heartbeat before/after execution.
//!
//! Replaces the Perl-based `lib/run-with-timeout.fish` wrapper with native
//! `tokio::process` and the per-agent Fish init+exit handlers in
//! `lib/agent.fish`.

pub mod hooks;
pub mod notifiers;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use chrono::Local;
use dotagent_core::{audit::AuditEvent, AgentManifest, Heartbeat};
use dotagent_plugin::PluginClient;
use dotagent_state::{slug_from_args, AuditLog, StateStore};
use dotagent_supervisor::{ProcessKind, ProcessOwner, SpawnSpec, Supervisor, SupervisorError};
use serde::Serialize;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::info;

pub type Result<T> = std::result::Result<T, RunnerError>;

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("state: {0}")]
    State(#[from] dotagent_state::StateError),
    #[error("spawn failed: {0}")]
    Spawn(String),
}

/// Outcome of a single agent run.
#[derive(Debug, Clone, Serialize)]
pub struct RunOutcome {
    pub exit_code: i32,
    pub timed_out: bool,
    pub duration_seconds: i64,
    pub stdout_tail: String,
    pub stderr_tail: String,
    /// Number of stdout lines dropped before the tail (0 = full output kept).
    #[serde(default)]
    pub stdout_truncated_lines: usize,
    /// Number of stderr lines dropped before the tail (0 = full output kept).
    #[serde(default)]
    pub stderr_truncated_lines: usize,
}

/// What the runner needs to execute one agent.
pub struct RunSpec<'a> {
    pub manifest: &'a AgentManifest,
    pub manifest_dir: &'a Path,
    pub schedule_id: &'a str,
    pub args: &'a [String],
    pub dry_run: bool,
    /// sha256 of the manifest text — recorded in the audit log so forensics
    /// can correlate runs with a specific manifest revision.
    pub manifest_sha256: Option<String>,
}

// stdout_tail is consumed by:
// - sink plugins (need the full Roam-formatted output — root + children)
// - notify plugins (need a short summary)
// Bumping to 500 covers typical agent outputs (linkedin: ~10 lines, dora
// standup: ~50, finops: ~100) without ballooning memory. A proper split
// (full stdout for sinks, tail for notifies) is a future refactor.
const TAIL_LINES: usize = 500;
const SIGKILL_GRACE_SECONDS: u64 = 5;
const TIMED_OUT_EXIT_CODE: i32 = 124;

/// Aggregate context for `run_with_hooks`. Caller passes the orchestrator's
/// shared state (audit log + plugin client) so the runner can fire lifecycle
/// hooks and emit audit events without owning them.
pub struct RunContext<'a> {
    pub state: &'a StateStore,
    pub plugins: Option<&'a PluginClient>,
    pub audit: Option<&'a AuditLog>,
    /// Shared subprocess supervisor. When `None`, the runner creates a
    /// per-call supervisor — convenient for ad-hoc `dotagent run`, but the
    /// daemon should always pass its singleton so `status`/`doctor` can see
    /// the live agent.
    pub supervisor: Option<&'a Supervisor>,
}

/// Outcome variants produced by `run_with_hooks`.
#[derive(Debug, Clone, Serialize)]
pub enum OrchestratedOutcome {
    /// Preflight aborted the run before spawn.
    PreflightFailed {
        plugin: String,
        suggest: Option<String>,
    },
    /// Agent process ran (success or failure).
    Ran(RunOutcome),
}

/// Run preflight → spawn → success/failure hooks. Emits audit events
/// (`agent_run`, `preflight_failed`, `plugin_invoked`) when an `AuditLog` is
/// provided. Plugin client absent ⇒ no hooks fire (used by `dotagent run`
/// for ad-hoc foreground runs).
pub async fn run_with_hooks(
    spec: RunSpec<'_>,
    ctx: &RunContext<'_>,
) -> Result<OrchestratedOutcome> {
    // Hold on to references that outlive the `spec` move into `run()` below.
    let manifest_ref: &AgentManifest = spec.manifest;
    let schedule_id = spec.schedule_id.to_string();
    let args_slug = slug_from_args(spec.args);
    let manifest_sha256 = spec.manifest_sha256.clone().unwrap_or_default();

    // 1) Preflight (only if plugins are wired up)
    if let Some(plugins) = ctx.plugins {
        let outcome = hooks::run_preflight(manifest_ref, &schedule_id, plugins, ctx.audit).await;
        if !outcome.passed {
            let plugin = outcome.failed_plugin.clone().unwrap_or_default();
            let suggest = outcome.suggest.clone();
            let message = format!(
                "preflight aborted by plugin {plugin}{}",
                suggest
                    .as_ref()
                    .map(|s| format!(": {s}"))
                    .unwrap_or_default()
            );
            hooks::fire_on_failure(
                manifest_ref,
                &schedule_id,
                "preflight",
                &message,
                plugins,
                ctx.audit,
            )
            .await;
            notifiers::fire_notifiers(
                manifest_ref,
                &schedule_id,
                "preflight",
                &message,
                ctx.plugins,
                ctx.audit,
            )
            .await;
            return Ok(OrchestratedOutcome::PreflightFailed { plugin, suggest });
        }
    }

    // 2) Spawn (consumes `spec`, but we kept the refs we still need)
    let outcome = run(spec, ctx.state, ctx.supervisor).await?;

    // 3) Audit
    if let Some(log) = ctx.audit {
        let _ = log.append(AuditEvent::AgentRun {
            agent: manifest_ref.agent.name.clone(),
            schedule: schedule_id.clone(),
            slug: args_slug,
            manifest_sha256,
            exit_code: outcome.exit_code,
            duration_seconds: outcome.duration_seconds,
            timed_out: outcome.timed_out,
        });
    }

    // 4) on_success / on_failure (legacy plugin hooks) + built-in notifiers
    let (event, message) = if outcome.exit_code == 0 {
        ("success", outcome.stdout_tail.clone())
    } else {
        let ev = if outcome.timed_out {
            "timed_out"
        } else {
            "attempt_failed"
        };
        let msg = if outcome.stderr_tail.is_empty() {
            format!(
                "{} exited {} (tail empty)",
                manifest_ref.agent.name, outcome.exit_code
            )
        } else {
            format!(
                "{} exited {}\n{}",
                manifest_ref.agent.name, outcome.exit_code, outcome.stderr_tail
            )
        };
        (ev, msg)
    };
    if let Some(plugins) = ctx.plugins {
        if event == "success" {
            hooks::fire_on_success(manifest_ref, &schedule_id, &message, plugins, ctx.audit).await;
        } else {
            hooks::fire_on_failure(
                manifest_ref,
                &schedule_id,
                event,
                &message,
                plugins,
                ctx.audit,
            )
            .await;
        }
    }
    notifiers::fire_notifiers(
        manifest_ref,
        &schedule_id,
        event,
        &message,
        ctx.plugins,
        ctx.audit,
    )
    .await;

    Ok(OrchestratedOutcome::Ran(outcome))
}

/// Run the agent with timeout, stdio capture, heartbeat lifecycle. Returns the
/// outcome — the caller is responsible for deciding what notifications to
/// emit.
///
/// When `supervisor` is `None`, a one-shot supervisor is created for this
/// call. Pass the daemon's singleton to make the agent visible in
/// `dotagent status`/`doctor` and to share the kill-on-shutdown machinery.
pub async fn run(
    spec: RunSpec<'_>,
    state: &StateStore,
    supervisor: Option<&Supervisor>,
) -> Result<RunOutcome> {
    let name = spec.manifest.agent.name.clone();
    let slug = slug_from_args(spec.args);

    // Heartbeat start
    let start = Local::now();
    let heartbeat_path = state.heartbeat_path(&name, &slug);

    if !spec.dry_run {
        let prev = state.read_heartbeat(&name, &slug)?;
        let hb = Heartbeat {
            name: name.clone(),
            slug: slug.clone(),
            args: spec.args.to_vec(),
            started_at: start.timestamp(),
            started_at_iso: start.format("%Y-%m-%dT%H:%M:%S%z").to_string(),
            finished_at: None,
            finished_at_iso: None,
            exit_code: None,
            duration_seconds: None,
            last_success_at: prev.as_ref().and_then(|p| p.last_success_at),
            last_success_at_iso: prev.as_ref().and_then(|p| p.last_success_at_iso.clone()),
        };
        state.write_heartbeat(&hb)?;
    }

    // Tmpdir (auto-cleanup when this scope ends)
    let tmpdir = tempfile::tempdir()?;

    // Build command
    let working_dir = spec
        .manifest
        .run
        .working_dir
        .clone()
        .map(|p| spec.manifest_dir.join(p))
        .unwrap_or_else(|| spec.manifest_dir.to_path_buf());

    let mut cmd = Command::new(&spec.manifest.run.command);
    cmd.args(&spec.manifest.run.args);
    cmd.args(spec.args);
    cmd.current_dir(&working_dir);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    // Env injection
    apply_env(
        &mut cmd,
        &spec,
        &name,
        &slug,
        &start,
        tmpdir.path(),
        &heartbeat_path,
    );

    info!(agent = %name, schedule = %spec.schedule_id, slug = %slug, "running agent");

    let timeout_sec = spec.manifest.agent.timeout_seconds;
    let owned_supervisor;
    let sup = match supervisor {
        Some(s) => s,
        None => {
            owned_supervisor = Supervisor::with_grace(Duration::from_secs(SIGKILL_GRACE_SECONDS));
            &owned_supervisor
        }
    };
    let spawn_spec = SpawnSpec {
        kind: ProcessKind::Agent,
        owner: ProcessOwner {
            agent: name.clone(),
            schedule: Some(spec.schedule_id.to_string()),
            hook_event: None,
            plugin: None,
        },
        deadline: Duration::from_secs(timeout_sec),
        label: format!("{name}.{}", spec.schedule_id),
    };
    let mut handle = sup
        .spawn_supervised(cmd, spawn_spec)
        .await
        .map_err(|e| RunnerError::Spawn(e.to_string()))?;
    let mut stdout = handle.take_stdout().expect("piped stdout");
    let mut stderr = handle.take_stderr().expect("piped stderr");

    // Per-agent log file: tee everything stdout+stderr writes into
    // `$DOTAGENT_HOME/logs/agents/<name>/<name>.log.YYYY-MM-DD`. Keeping
    // the full output (not just the 5-line tail) makes `dotagent logs
    // <agent>` and forensics actually useful.
    let log_dir_result = std::fs::create_dir_all(dotagent_state::paths::agent_logs_dir(&name));
    let log_file = match log_dir_result {
        Ok(()) => {
            let path = dotagent_state::paths::agent_logs_dir(&name).join(format!("{name}.log"));
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok()
        }
        Err(_) => None,
    };
    if let Some(ref f) = log_file {
        use std::io::Write;
        let _ = writeln!(
            &*f,
            "\n=== {} run started · schedule={} · slug={} ===",
            start.format("%Y-%m-%dT%H:%M:%S%z"),
            spec.schedule_id,
            slug
        );
    }
    let log_for_stdout = log_file.as_ref().and_then(|f| f.try_clone().ok());
    let log_for_stderr = log_file;

    // Drain stdio in background tasks so the OS pipe buffer never fills up.
    let stdout_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf).await;
        if let Some(mut f) = log_for_stdout {
            use std::io::Write;
            let _ = f.write_all(buf.as_bytes());
        }
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf).await;
        if let Some(mut f) = log_for_stderr {
            use std::io::Write;
            let _ = writeln!(f, "--- stderr ---");
            let _ = f.write_all(buf.as_bytes());
        }
        buf
    });

    let (status_opt, timed_out) = match handle.wait_status().await {
        Ok((status, timed_out)) => (Some(status), timed_out),
        Err(SupervisorError::Io(e)) => return Err(RunnerError::Io(e)),
        Err(e) => return Err(RunnerError::Spawn(e.to_string())),
    };

    let stdout_buf = stdout_task.await.unwrap_or_default();
    let stderr_buf = stderr_task.await.unwrap_or_default();

    let finish = Local::now();
    let duration = (finish - start).num_seconds();
    let exit_code = status_opt.and_then(|s| s.code()).unwrap_or(if timed_out {
        TIMED_OUT_EXIT_CODE
    } else {
        -1
    });

    if !spec.dry_run {
        let mut hb = state
            .read_heartbeat(&name, &slug)?
            .expect("heartbeat written above");
        hb.finished_at = Some(finish.timestamp());
        hb.finished_at_iso = Some(finish.format("%Y-%m-%dT%H:%M:%S%z").to_string());
        hb.exit_code = Some(exit_code);
        hb.duration_seconds = Some(duration);
        if exit_code == 0 {
            hb.last_success_at = Some(finish.timestamp());
            hb.last_success_at_iso = Some(finish.format("%Y-%m-%dT%H:%M:%S%z").to_string());
        }
        state.write_heartbeat(&hb)?;
    }

    let (stdout_tail, stdout_truncated_lines) = tail_lines(&stdout_buf, TAIL_LINES);
    let (stderr_tail, stderr_truncated_lines) = tail_lines(&stderr_buf, TAIL_LINES);
    Ok(RunOutcome {
        exit_code,
        timed_out,
        duration_seconds: duration,
        stdout_tail,
        stderr_tail,
        stdout_truncated_lines,
        stderr_truncated_lines,
    })
}

fn apply_env(
    cmd: &mut Command,
    spec: &RunSpec<'_>,
    name: &str,
    slug: &str,
    start: &chrono::DateTime<Local>,
    tmpdir: &Path,
    heartbeat: &Path,
) {
    if let Some(env_cfg) = &spec.manifest.env {
        if !env_cfg.inherit {
            cmd.env_clear();
        }
        for (k, v) in &env_cfg.extra {
            cmd.env(k, v);
        }
    }
    cmd.env("AGENT_NAME", name);
    cmd.env("AGENT_HOME", spec.manifest_dir);
    cmd.env("AGENT_TMPDIR", tmpdir);
    cmd.env("AGENT_DRY_RUN", if spec.dry_run { "true" } else { "false" });
    cmd.env("AGENT_SCHEDULE_ID", spec.schedule_id);
    cmd.env("AGENT_START_EPOCH", start.timestamp().to_string());
    cmd.env("AGENT_SLUG", slug);
    if !spec.dry_run {
        cmd.env("AGENT_HEARTBEAT_FILE", heartbeat);
    }
    let argv_json = serde_json::to_string(spec.args).unwrap_or_else(|_| "[]".into());
    cmd.env("AGENT_ARGV", argv_json);
}

fn tail_lines(s: &str, n: usize) -> (String, usize) {
    let lines: Vec<&str> = s.lines().collect();
    let total = lines.len();
    let start = total.saturating_sub(n);
    (lines[start..].join("\n"), start)
}
