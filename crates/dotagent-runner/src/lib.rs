//! Agent runner — spawn the agent process with timeout, capture stdio, inject
//! environment variables, and update the heartbeat before/after execution.
//!
//! Replaces the Perl-based `lib/run-with-timeout.fish` wrapper with native
//! `tokio::process` and the per-agent Fish init+exit handlers in
//! `lib/agent.fish`.

pub mod hooks;
pub mod notifiers;
pub mod persistent;
pub mod protocol;

use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use chrono::Local;
use dotagent_core::{audit::AuditEvent, AgentManifest, Heartbeat};
use dotagent_plugin::PluginClient;
use dotagent_state::{slug_from_args, AuditLog, StateStore};
use dotagent_supervisor::{ProcessKind, ProcessOwner, SpawnSpec, Supervisor, SupervisorError};
use serde::Serialize;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::process::Command;
use tracing::{info, warn};

pub type Result<T> = std::result::Result<T, RunnerError>;

/// Metadata for a persistent request whose delivery became ambiguous after
/// crossing the process boundary.
#[derive(Debug, Clone, Error)]
#[error("persistent request may have been delivered; not retrying: {reason}")]
pub struct RequestLost {
    pub reason: String,
    /// Elapsed wall-clock time measured by the exchange before it was lost.
    pub duration_seconds: i64,
}

impl RequestLost {
    pub(crate) fn new(reason: impl Into<String>, elapsed: Duration) -> Self {
        Self {
            reason: reason.into(),
            duration_seconds: elapsed.as_secs() as i64,
        }
    }
}

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("state: {0}")]
    State(#[from] dotagent_state::StateError),
    #[error("audit: {0}")]
    Audit(#[from] dotagent_state::AuditError),
    #[error("spawn failed: {0}")]
    Spawn(String),
    #[error("{0}")]
    RequestLost(#[source] RequestLost),
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
    /// Replaces the args-derived slug for heartbeat and window files.
    ///
    /// Scheduled runs leave this `None` and key their state off the schedule's
    /// args. On-demand runs (trigger, MCP) set it so they never overwrite
    /// `last_success_at` for a cron window that never actually fired — which
    /// would make the scheduler believe a missed window had succeeded.
    pub slug_override: Option<&'a str>,
    /// Per-invocation environment, applied after the manifest's `[env.extra]`
    /// and before the `AGENT_*` block.
    ///
    /// Carries trigger context (`AGENT_TRIGGER_*`) that varies per run and
    /// therefore cannot live in the manifest. Keys starting with `AGENT_` that
    /// collide with the injected block are overwritten by it — the runner's own
    /// variables are not spoofable from here.
    pub extra_env: &'a [(String, String)],
}

impl RunSpec<'_> {
    /// Slug for this run's state files.
    fn slug(&self) -> String {
        match self.slug_override {
            Some(s) => s.to_string(),
            None => slug_from_args(self.args),
        }
    }
}

/// Callback invoked once per line of agent stdout, as the line is read from
/// the pipe — while the process may still be running.
///
/// The runner owns no protocol here: it forwards the raw line (without its
/// trailing newline) and nothing else. Callers that need to ship lines to a
/// client should `try_send` into their own channel and drop on a full
/// channel.
///
/// Contract:
/// - **synchronous and fast**: called from the stdout reader task, so a slow
///   callback delays tail capture and log tee for the whole run;
/// - **must not panic**: a panic unwinds through the reader task and the run
///   loses every stdout line after it;
/// - errors the caller wants to handle (closed channel, slow consumer) are
///   the caller's business — the runner ignores whatever happens inside.
pub type StdoutLineTap = Arc<dyn Fn(&str) + Send + Sync>;

/// Real-time streaming taps for a run.
///
/// Lives beside [`RunSpec`] instead of inside it only because every caller
/// in `crates/dotagent` builds `RunSpec` with a struct literal — a new
/// required field would break them all. A default `StreamOptions` behaves
/// exactly like the buffered runner that came before streaming.
///
/// Persistent agents (response-frame protocol, see `protocol.rs`) do not
/// honor taps yet — their output is not a plain stream. Taps apply to
/// one-shot runs.
#[derive(Clone, Default)]
pub struct StreamOptions {
    /// Tap for each stdout line, in arrival order.
    pub on_stdout_line: Option<StdoutLineTap>,
}

// stdout_tail is consumed by:
// - sink plugins (need the full Roam-formatted output — root + children)
// - notify plugins (need a short summary)
// Bumping to 500 covers typical agent outputs (a post draft: ~10 lines, a
// standup: ~50, a cost report: ~100) without ballooning memory. A proper split
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
    /// Pool of live processes for agents declaring `[lifecycle] mode =
    /// "persistent"`.
    ///
    /// `None` means "run it one-shot regardless" — which is what every caller
    /// outside a long-lived process should pass. A pool held for the duration
    /// of a single CLI invocation would spawn a process, ask it one question
    /// and kill it, paying the startup cost the mode exists to avoid.
    pub persistent: Option<&'a persistent::PersistentPool>,
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
    run_with_hooks_streaming(spec, StreamOptions::default(), ctx).await
}

/// [`run_with_hooks`] with real-time stdout taps. See [`StreamOptions`].
pub async fn run_with_hooks_streaming(
    spec: RunSpec<'_>,
    stream: StreamOptions,
    ctx: &RunContext<'_>,
) -> Result<OrchestratedOutcome> {
    // Hold on to references that outlive the `spec` move into `run()` below.
    let manifest_ref: &AgentManifest = spec.manifest;
    let schedule_id = spec.schedule_id.to_string();
    let args_slug = spec.slug();
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

    // 2) Run it. A persistent agent is handed to the pool, which delivers the
    //    request to a process that is already up; everything else spawns.
    //    `dry_run` always takes the one-shot path — a dry run must not leave a
    //    live process behind, and there is nothing to deliver anyway.
    //    Persistent dispatch ignores `stream`: its output arrives as framed
    //    responses, not as a plain stdout stream.
    let run_result = match ctx.persistent {
        Some(pool) if manifest_ref.lifecycle.is_persistent() && !spec.dry_run => {
            pool.dispatch(&spec, ctx.state, ctx.audit).await
        }
        _ => run_streaming(spec, stream, ctx.state, ctx.supervisor).await,
    };

    let outcome = match run_result {
        Ok(outcome) => outcome,
        Err(RunnerError::RequestLost(lost)) => {
            append_agent_run_audit(
                ctx.audit,
                &manifest_ref.agent.name,
                &schedule_id,
                &args_slug,
                &manifest_sha256,
                persistent::REQUEST_LOST_EXIT_CODE,
                lost.duration_seconds,
                false,
            );
            return Err(RunnerError::RequestLost(lost));
        }
        Err(error) => return Err(error),
    };

    // 3) Audit
    append_agent_run_audit(
        ctx.audit,
        &manifest_ref.agent.name,
        &schedule_id,
        &args_slug,
        &manifest_sha256,
        outcome.exit_code,
        outcome.duration_seconds,
        outcome.timed_out,
    );

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

// AuditEvent::AgentRun mirrors the durable audit schema, so keeping these
// fields explicit avoids a short-lived wrapper obscuring that contract.
#[allow(clippy::too_many_arguments)]
fn append_agent_run_audit(
    audit: Option<&AuditLog>,
    agent: &str,
    schedule: &str,
    slug: &str,
    manifest_sha256: &str,
    exit_code: i32,
    duration_seconds: i64,
    timed_out: bool,
) {
    let Some(log) = audit else { return };
    if let Err(error) = log.append(AuditEvent::AgentRun {
        agent: agent.to_string(),
        schedule: schedule.to_string(),
        slug: slug.to_string(),
        manifest_sha256: manifest_sha256.to_string(),
        exit_code,
        duration_seconds,
        timed_out,
    }) {
        warn!(
            agent,
            schedule,
            slug,
            exit_code,
            duration_seconds,
            timed_out,
            error = %error,
            "could not append agent_run audit event"
        );
    }
}

/// Run the agent with timeout, stdio capture, heartbeat lifecycle. Returns the
/// outcome — the caller is responsible for deciding what notifications to
/// emit.
///
/// When `supervisor` is `None`, a one-shot supervisor is created for this
/// call. Pass the daemon's singleton to make the agent visible in
/// `dotagent status`/`doctor` and share the kill-on-shutdown machinery.
pub async fn run(
    spec: RunSpec<'_>,
    state: &StateStore,
    supervisor: Option<&Supervisor>,
) -> Result<RunOutcome> {
    run_streaming(spec, StreamOptions::default(), state, supervisor).await
}

/// [`run`] with real-time stdout taps. See [`StreamOptions`].
pub async fn run_streaming(
    spec: RunSpec<'_>,
    stream: StreamOptions,
    state: &StateStore,
    supervisor: Option<&Supervisor>,
) -> Result<RunOutcome> {
    let name = spec.manifest.agent.name.clone();
    let slug = spec.slug();

    // Heartbeat start
    let start = Local::now();
    let heartbeat_path = state.heartbeat_path(&name, &slug);

    if !spec.dry_run {
        begin_heartbeat(state, &name, &slug, spec.args, &start)?;
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
    let stdout = handle.take_stdout().expect("piped stdout");
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
    let mut log_for_stdout = log_file.as_ref().and_then(|f| f.try_clone().ok());
    let log_for_stderr = log_file;

    // Drain stdio in background tasks so the OS pipe buffer never fills up.
    //
    // Stdout is read line by line so a tap (if any) sees each line as it
    // arrives instead of after exit. Retention is a ring buffer of the last
    // TAIL_LINES lines plus a counter: the log tee above already persists
    // everything to disk, so memory stays bounded no matter how chatty the
    // agent is. (This is strictly better than the `read_to_string` it
    // replaced, which held the entire stream in memory only to truncate it
    // later.) Timeout semantics are untouched: the deadline and kill-tree
    // live in the supervisor; this loop simply ends when the pipe closes.
    let tap = stream.on_stdout_line;
    let stdout_task = tokio::spawn(async move {
        let mut tail: VecDeque<String> = VecDeque::with_capacity(TAIL_LINES);
        let mut total: usize = 0;
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(e) => {
                    warn!(error = %e, "agent stdout read failed mid-run");
                    break;
                }
            };
            if let Some(f) = log_for_stdout.as_mut() {
                use std::io::Write;
                let _ = writeln!(f, "{line}");
            }
            if let Some(tap) = tap.as_ref() {
                tap(&line);
            }
            if tail.len() == TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
            total += 1;
        }
        (tail, total)
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

    let (stdout_lines, stdout_total): (VecDeque<String>, usize) =
        stdout_task.await.unwrap_or_default();
    let stderr_buf = stderr_task.await.unwrap_or_default();

    let finish = Local::now();
    let duration = (finish - start).num_seconds();
    let exit_code = status_opt.and_then(|s| s.code()).unwrap_or(if timed_out {
        TIMED_OUT_EXIT_CODE
    } else {
        -1
    });

    if !spec.dry_run {
        finish_heartbeat(state, &name, &slug, spec.args, &start, &finish, exit_code)?;
    }

    // Same shape the old `tail_lines` produced: last TAIL_LINES lines joined
    // by `\n` (a trailing newline in the stream does not survive), and how
    // many lines were dropped.
    let stdout_tail = stdout_lines.into_iter().collect::<Vec<_>>().join("\n");
    let stdout_truncated_lines = stdout_total.saturating_sub(TAIL_LINES);
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

/// Write the "this run started" heartbeat.
///
/// Shared with the persistent pool, which writes one per *request* rather
/// than one per process — same file, same shape, so `status`, health states
/// and retry accounting cannot tell the two execution modes apart.
pub(crate) fn begin_heartbeat(
    state: &StateStore,
    name: &str,
    slug: &str,
    args: &[String],
    start: &chrono::DateTime<Local>,
) -> Result<()> {
    let prev = state.read_heartbeat(name, slug)?;
    let hb = Heartbeat {
        name: name.to_string(),
        slug: slug.to_string(),
        args: args.to_vec(),
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
    Ok(())
}

/// Close out the heartbeat for a finished run.
pub(crate) fn finish_heartbeat(
    state: &StateStore,
    name: &str,
    slug: &str,
    args: &[String],
    start: &chrono::DateTime<Local>,
    finish: &chrono::DateTime<Local>,
    exit_code: i32,
) -> Result<()> {
    // The start heartbeat was written above, but it is a file on disk and
    // another process (`run-now`, `dotagent mcp`, an operator with `rm`)
    // can remove it while the agent runs. Reconstructing beats panicking:
    // the run already happened, and losing its record to an `expect` would
    // turn a cosmetic problem into a dead daemon.
    let mut hb = match state.read_heartbeat(name, slug) {
        Ok(Some(hb)) => hb,
        Ok(None) => {
            warn!(
                agent = %name,
                slug = %slug,
                "heartbeat vanished mid-run — rebuilding from this run"
            );
            Heartbeat {
                name: name.to_string(),
                slug: slug.to_string(),
                args: args.to_vec(),
                started_at: start.timestamp(),
                started_at_iso: start.format("%Y-%m-%dT%H:%M:%S%z").to_string(),
                finished_at: None,
                finished_at_iso: None,
                exit_code: None,
                duration_seconds: None,
                // Unknown, and guessing would be worse: claiming a success
                // that never happened makes the scheduler skip a window.
                last_success_at: None,
                last_success_at_iso: None,
            }
        }
        Err(e) => {
            warn!(agent = %name, slug = %slug, error = %e, "heartbeat unreadable mid-run");
            return Err(e.into());
        }
    };
    hb.finished_at = Some(finish.timestamp());
    hb.finished_at_iso = Some(finish.format("%Y-%m-%dT%H:%M:%S%z").to_string());
    hb.exit_code = Some(exit_code);
    hb.duration_seconds = Some((*finish - *start).num_seconds());
    if exit_code == 0 {
        hb.last_success_at = Some(finish.timestamp());
        hb.last_success_at_iso = Some(finish.format("%Y-%m-%dT%H:%M:%S%z").to_string());
    }
    state.write_heartbeat(&hb)?;
    Ok(())
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
    apply_env_for(cmd, spec, name, slug, start, tmpdir, heartbeat, None)
}

/// `apply_env`, plus the two things that only make sense for an instance that
/// outlives a single request.
///
/// `persist_key` present means: this process is persistent. The
/// `AGENT_TRIGGER_*` block is dropped, because it describes one message and
/// this process will see many — a stale payload frozen at spawn reads as
/// perfectly valid and is the worst possible failure mode. Trigger context
/// travels in the request frame instead.
#[allow(clippy::too_many_arguments)]
fn apply_env_for(
    cmd: &mut Command,
    spec: &RunSpec<'_>,
    name: &str,
    slug: &str,
    start: &chrono::DateTime<Local>,
    tmpdir: &Path,
    heartbeat: &Path,
    persist_key: Option<&str>,
) {
    let env_cfg = spec.manifest.env.as_ref();
    let inherit = env_cfg.map_or(true, |e| e.inherit);
    if !inherit {
        cmd.env_clear();
    }
    remove_inherited_trigger_env(cmd);
    // Skipped entirely when the manifest names any locale variable: `LC_CTYPE`
    // outranks `LANG`, so merely writing this first would not let a manifest
    // that says `LANG = "pt_BR.UTF-8"` win — it would silently lose the one
    // category that matters.
    if !names_locale(env_cfg) {
        if let Some(locale) = default_locale(
            inherit,
            std::env::var_os("LANG"),
            std::env::var_os("LC_CTYPE"),
            std::env::var_os("LC_ALL"),
        ) {
            cmd.env("LC_CTYPE", locale);
        }
    }
    if let Some(cfg) = env_cfg {
        for (k, v) in &cfg.extra {
            cmd.env(k, v);
        }
    }
    // Per-invocation env goes before the AGENT_* block on purpose: a trigger
    // payload must never be able to redefine AGENT_NAME or AGENT_HEARTBEAT_FILE.
    for (k, v) in spec.extra_env {
        if persist_key.is_some() && protocol::is_trigger_env(k) {
            continue;
        }
        cmd.env(k, v);
    }
    if let Some(key) = persist_key {
        cmd.env("AGENT_LIFECYCLE", "persistent");
        cmd.env("AGENT_PERSIST_KEY", key);
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

fn remove_inherited_trigger_env(cmd: &mut Command) {
    remove_trigger_env(cmd, std::env::vars_os());
}

fn remove_trigger_env<I>(cmd: &mut Command, vars: I)
where
    I: IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
{
    for (key, _) in vars {
        if key.to_str().is_some_and(protocol::is_trigger_env) {
            cmd.env_remove(key);
        }
    }
}

/// The locale to fall back to when the parent process names none.
///
/// macOS ships no `C.UTF-8`, and glibc only grew one in 2.35 — so neither
/// value works everywhere and the target decides. On a system without the
/// chosen locale `setlocale` fails and the process lands back in `C`, which is
/// exactly where it started: no regression, just no fix.
#[cfg(target_os = "macos")]
const FALLBACK_LOCALE: &str = "en_US.UTF-8";
#[cfg(not(target_os = "macos"))]
const FALLBACK_LOCALE: &str = "C.UTF-8";

/// Whether the manifest already names the locale itself.
///
/// Any of the three counts. `[env.extra]` is applied after this block, so a
/// manifest naming `LANG` would still be beaten by an injected `LC_CTYPE`;
/// standing down entirely is what makes "the manifest wins" true.
fn names_locale(env_cfg: Option<&dotagent_core::EnvConfig>) -> bool {
    env_cfg.is_some_and(|cfg| {
        cfg.extra
            .keys()
            .any(|k| matches!(k.as_str(), "LANG" | "LC_ALL" | "LC_CTYPE"))
    })
}

/// Pick an `LC_CTYPE` for the agent, or `None` when the parent already named
/// the character-type locale.
///
/// launchd and systemd start a daemon with no locale at all, and every agent
/// inherits that gap. A process in the resulting `C` locale has
/// `MB_CUR_MAX == 1`, so it reads each **byte** of an environment variable as
/// one character — Latin-1 — and writes it back out as UTF-8. The UTF-8 "é" a
/// Telegram message puts in `AGENT_TRIGGER_PAYLOAD` reaches the agent as "Ã©",
/// and the agent has no way to tell that apart from text that really said
/// "Ã©".
///
/// Observed with fish 3.7.1: an env var round-trips as `c3 a9 -> c3 83 c2 a9`
/// under `C` and unchanged under `en_US.UTF-8`. Command-substitution output is
/// *not* affected, which is why the same run logged a mangled prompt beside a
/// clean answer for two months without anyone being able to place the bug.
///
/// **`LC_CTYPE`, not `LANG`.** `MB_CUR_MAX` is the character-type category and
/// nothing else, while `LANG` is the fallback for *every* category. Naming
/// `LANG` would also move `LC_COLLATE` — on macOS from byte order to ICU
/// collation, which silently changes `sort`, `[[ a < b ]]` and `[a-z]` ranges
/// in `grep`/`tr` for every shell agent, and only on macOS, since `C.UTF-8`
/// keeps byte order. Fixing mojibake is no reason to reorder somebody's output.
///
/// All three parent variables are consulted because POSIX precedence is
/// `LC_ALL` > `LC_CTYPE` > `LANG`: an inherited `LC_CTYPE` (ssh forwards one
/// from macOS via `SendEnv`, often without a `LANG` beside it) is the operator
/// having chosen this exact category, and overriding it here would be the same
/// class of surprise as moving `LC_COLLATE`. This function only ever fills a
/// gap; it never overrules.
///
/// Kept pure so the decision is testable without mutating the process
/// environment, which every other test in this file would then race against.
fn default_locale(
    inherit: bool,
    parent_lang: Option<std::ffi::OsString>,
    parent_lc_ctype: Option<std::ffi::OsString>,
    parent_lc_all: Option<std::ffi::OsString>,
) -> Option<&'static str> {
    // `env_clear()` drops whatever the parent had, so an inherited locale only
    // counts when the agent is actually inheriting.
    let inherited =
        inherit && (parent_lang.is_some() || parent_lc_ctype.is_some() || parent_lc_all.is_some());
    if inherited {
        return None;
    }
    Some(FALLBACK_LOCALE)
}

fn tail_lines(s: &str, n: usize) -> (String, usize) {
    let lines: Vec<&str> = s.lines().collect();
    let total = lines.len();
    let start = total.saturating_sub(n);
    (lines[start..].join("\n"), start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(toml_src: &str) -> AgentManifest {
        toml::from_str(toml_src).expect("fixture manifest must parse")
    }

    fn minimal() -> AgentManifest {
        manifest(
            r#"
[agent]
name = "x"
[run]
command = "true"
"#,
        )
    }

    fn spec_with<'a>(
        m: &'a AgentManifest,
        args: &'a [String],
        slug_override: Option<&'a str>,
        extra_env: &'a [(String, String)],
    ) -> RunSpec<'a> {
        RunSpec {
            manifest: m,
            manifest_dir: Path::new("/tmp"),
            schedule_id: "daily",
            args,
            dry_run: false,
            manifest_sha256: None,
            slug_override,
            extra_env,
        }
    }

    /// Collect the env a `Command` would apply, as (key, value) strings.
    fn env_of(cmd: &Command) -> Vec<(String, String)> {
        cmd.as_std()
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_str()?.to_string(),
                    v.and_then(|v| v.to_str()).unwrap_or_default().to_string(),
                ))
            })
            .collect()
    }

    fn lookup(cmd: &Command, key: &str) -> Option<String> {
        env_of(cmd)
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    fn env_change(cmd: &Command, key: &str) -> Option<Option<String>> {
        cmd.as_std().get_envs().find_map(|(name, value)| {
            if name.to_str()? != key {
                return None;
            }
            Some(value.map(|value| value.to_string_lossy().into_owned()))
        })
    }

    // --- locale: the `C` locale mangles every non-ASCII byte we inject ---

    fn os(s: &str) -> Option<std::ffi::OsString> {
        Some(std::ffi::OsString::from(s))
    }

    #[test]
    fn locale_is_named_when_the_parent_named_none() {
        // launchd and systemd both start a daemon this way.
        assert_eq!(
            default_locale(true, None, None, None),
            Some(FALLBACK_LOCALE)
        );
    }

    #[test]
    fn the_locale_is_named_on_lc_ctype_not_lang() {
        // `MB_CUR_MAX` is the ctype category alone. `LANG` also carries
        // `LC_COLLATE`, and on macOS `en_US.UTF-8` swaps byte order for ICU
        // collation — every shell agent that sorts would start ordering
        // differently, on macOS only. That is not a fix, it is a second bug.
        let m = minimal();
        let args: Vec<String> = vec![];
        let spec = spec_with(&m, &args, None, &[]);
        let mut cmd = Command::new("true");
        apply_env(
            &mut cmd,
            &spec,
            "x",
            "default",
            &Local::now(),
            Path::new("/tmp"),
            Path::new("/tmp/hb.json"),
        );
        assert_eq!(
            lookup(&cmd, "LANG"),
            None,
            "naming LANG would move LC_COLLATE too"
        );
    }

    #[test]
    fn fallback_locale_is_utf8() {
        // The whole point is `MB_CUR_MAX > 1`. A locale without a UTF-8
        // charset would satisfy "is set" and still corrupt the payload.
        assert!(
            FALLBACK_LOCALE.to_ascii_uppercase().ends_with("UTF-8"),
            "fallback must name a UTF-8 charset, got {FALLBACK_LOCALE}"
        );
    }

    #[test]
    fn an_inherited_lang_is_left_alone() {
        assert_eq!(default_locale(true, os("pt_BR.UTF-8"), None, None), None);
    }

    #[test]
    fn an_inherited_lc_all_is_left_alone() {
        // LC_ALL outranks everything, so naming anything under it would be a
        // no-op that reads like a fix.
        assert_eq!(default_locale(true, None, None, os("pt_BR.UTF-8")), None);
    }

    #[test]
    fn an_inherited_lc_ctype_is_left_alone() {
        // Precedence is LC_ALL > LC_CTYPE > LANG. An `ssh` session from macOS
        // forwards `LC_CTYPE` through `SendEnv` with no `LANG` beside it, so
        // this is the one that arrives alone in practice — and it is the
        // operator having named this exact category.
        assert_eq!(default_locale(true, None, os("UTF-8"), None), None);
    }

    #[test]
    fn a_cleared_environment_gets_the_locale_back() {
        // `inherit = false` calls `env_clear()`, so whatever the parent had is
        // gone and the agent is back in `C` — the case that needs it most.
        assert_eq!(
            default_locale(
                false,
                os("pt_BR.UTF-8"),
                os("pt_BR.UTF-8"),
                os("pt_BR.UTF-8")
            ),
            Some(FALLBACK_LOCALE)
        );
    }

    #[test]
    fn the_manifest_can_override_the_locale() {
        let m = manifest(
            r#"
[agent]
name = "x"
[run]
command = "true"
[env.extra]
LANG = "pt_BR.UTF-8"
"#,
        );
        let args: Vec<String> = vec![];
        let spec = spec_with(&m, &args, None, &[]);
        let mut cmd = Command::new("true");
        apply_env(
            &mut cmd,
            &spec,
            "x",
            "default",
            &Local::now(),
            Path::new("/tmp"),
            Path::new("/tmp/hb.json"),
        );
        assert_eq!(lookup(&cmd, "LANG").as_deref(), Some("pt_BR.UTF-8"));
        // And nothing of ours outranks it. `LC_CTYPE` beats `LANG`, so
        // injecting one here would quietly ignore what the manifest asked for.
        assert_eq!(
            lookup(&cmd, "LC_CTYPE"),
            None,
            "a manifest that names a locale must not be overruled by LC_CTYPE"
        );
    }

    #[test]
    fn a_manifest_locale_wins_even_when_nothing_is_inherited() {
        // `inherit = false` is the case where the fallback definitely applies,
        // so this is the one that proves the manifest is not overruled rather
        // than merely happening to agree with the parent environment.
        let m = manifest(
            r#"
[agent]
name = "x"
[run]
command = "true"
[env]
inherit = false
[env.extra]
LANG = "pt_BR.UTF-8"
"#,
        );
        let args: Vec<String> = vec![];
        let spec = spec_with(&m, &args, None, &[]);
        let mut cmd = Command::new("true");
        apply_env(
            &mut cmd,
            &spec,
            "x",
            "default",
            &Local::now(),
            Path::new("/tmp"),
            Path::new("/tmp/hb.json"),
        );
        assert_eq!(lookup(&cmd, "LANG").as_deref(), Some("pt_BR.UTF-8"));
        assert_eq!(
            lookup(&cmd, "LC_CTYPE"),
            None,
            "LC_CTYPE outranks LANG — injecting one would discard the manifest's choice"
        );
    }

    #[test]
    fn a_trigger_payload_keeps_its_bytes_through_the_env() {
        // The value the agent reads must be the value the daemon serialized,
        // byte for byte. Anything that re-encodes it here is the bug this
        // whole locale dance exists to prevent.
        let m = minimal();
        let args: Vec<String> = vec![];
        let payload = r#"{"text":"quem é thiago avelino?"}"#.to_string();
        let extra = vec![("AGENT_TRIGGER_PAYLOAD".to_string(), payload.clone())];
        let spec = spec_with(&m, &args, None, &extra);
        let mut cmd = Command::new("true");
        apply_env(
            &mut cmd,
            &spec,
            "x",
            "default",
            &Local::now(),
            Path::new("/tmp"),
            Path::new("/tmp/hb.json"),
        );
        assert_eq!(lookup(&cmd, "AGENT_TRIGGER_PAYLOAD"), Some(payload));
        // Either the runner named a locale or the parent already had one —
        // what must never happen is the agent starting in `C` with this in
        // its environment. (`cargo test` inherits a locale, so the second arm
        // is the one that fires locally; under launchd it is the first.)
        assert!(
            lookup(&cmd, "LC_CTYPE").is_some()
                || std::env::var_os("LANG").is_some()
                || std::env::var_os("LC_CTYPE").is_some()
                || std::env::var_os("LC_ALL").is_some(),
            "a payload with non-ASCII needs a locale that can read it"
        );
    }

    #[test]
    fn slug_defaults_to_args_derivation() {
        let m = minimal();
        let args = vec!["--period".to_string(), "yesterday".to_string()];
        let spec = spec_with(&m, &args, None, &[]);
        assert_eq!(spec.slug(), slug_from_args(&args));
    }

    #[test]
    fn slug_override_wins_over_args() {
        let m = minimal();
        let args = vec!["--period".to_string(), "yesterday".to_string()];
        let spec = spec_with(&m, &args, Some("trigger-telegram"), &[]);
        assert_eq!(spec.slug(), "trigger-telegram");
    }

    #[test]
    fn slug_override_keeps_triggered_state_off_the_scheduled_slug() {
        // The whole point: an on-demand run must not write the heartbeat of
        // the cron window, or the scheduler stops retrying a missed run.
        let m = minimal();
        let args = vec!["--period".to_string(), "yesterday".to_string()];
        let scheduled = spec_with(&m, &args, None, &[]);
        let triggered = spec_with(&m, &args, Some("trigger-telegram"), &[]);
        assert_ne!(scheduled.slug(), triggered.slug());
    }

    #[test]
    fn extra_env_reaches_the_command() {
        let m = minimal();
        let extra = vec![
            ("AGENT_TRIGGER_PAYLOAD".to_string(), "payload".to_string()),
            ("AGENT_TRIGGER_SOURCE".to_string(), "telegram".to_string()),
            ("AGENT_SESSION_ID".to_string(), "session".to_string()),
        ];
        let spec = spec_with(&m, &[], None, &extra);
        let mut cmd = Command::new("true");
        apply_env(
            &mut cmd,
            &spec,
            "x",
            "slug",
            &Local::now(),
            Path::new("/tmp"),
            Path::new("/tmp/hb.json"),
        );
        assert_eq!(
            lookup(&cmd, "AGENT_TRIGGER_PAYLOAD").as_deref(),
            Some("payload")
        );
        assert_eq!(
            lookup(&cmd, "AGENT_TRIGGER_SOURCE").as_deref(),
            Some("telegram")
        );
        assert_eq!(lookup(&cmd, "AGENT_SESSION_ID").as_deref(), Some("session"));
    }

    #[test]
    fn inherited_trigger_env_is_removed_without_touching_other_agent_env() {
        let mut cmd = Command::new("true");
        let inherited = [
            ("AGENT_TRIGGER_PAYLOAD", "parent payload"),
            ("AGENT_TRIGGER_SOURCE", "parent source"),
            ("AGENT_SESSION_ID", "parent session"),
            ("AGENT_NAME", "parent name"),
        ];
        for &(key, value) in &inherited {
            cmd.env(key, value);
        }

        remove_trigger_env(
            &mut cmd,
            inherited
                .into_iter()
                .map(|(key, value)| (key.into(), value.into())),
        );

        assert_eq!(env_change(&cmd, "AGENT_TRIGGER_PAYLOAD"), Some(None));
        assert_eq!(env_change(&cmd, "AGENT_TRIGGER_SOURCE"), Some(None));
        assert_eq!(env_change(&cmd, "AGENT_SESSION_ID"), Some(None));
        assert_eq!(
            env_change(&cmd, "AGENT_NAME"),
            Some(Some("parent name".to_string()))
        );
    }

    #[test]
    fn audit_errors_convert_to_runner_errors() {
        let error = RunnerError::from(dotagent_state::AuditError::NoHome);
        assert_eq!(error.to_string(), "audit: no home directory");
    }

    #[test]
    fn extra_env_cannot_override_agent_name() {
        // A trigger payload is untrusted input. If it could redefine
        // AGENT_NAME, an agent's own identity would be attacker-controlled.
        let m = minimal();
        let extra = vec![("AGENT_NAME".to_string(), "evil".to_string())];
        let spec = spec_with(&m, &[], None, &extra);
        let mut cmd = Command::new("true");
        apply_env(
            &mut cmd,
            &spec,
            "real-name",
            "slug",
            &Local::now(),
            Path::new("/tmp"),
            Path::new("/tmp/hb.json"),
        );
        assert_eq!(lookup(&cmd, "AGENT_NAME").as_deref(), Some("real-name"));
    }

    #[test]
    fn extra_env_cannot_override_heartbeat_file() {
        // Redirecting the heartbeat would let a trigger corrupt scheduling
        // state for an unrelated agent.
        let m = minimal();
        let extra = vec![(
            "AGENT_HEARTBEAT_FILE".to_string(),
            "/tmp/evil.json".to_string(),
        )];
        let spec = spec_with(&m, &[], None, &extra);
        let mut cmd = Command::new("true");
        apply_env(
            &mut cmd,
            &spec,
            "x",
            "slug",
            &Local::now(),
            Path::new("/tmp"),
            Path::new("/tmp/real.json"),
        );
        assert_eq!(
            lookup(&cmd, "AGENT_HEARTBEAT_FILE").as_deref(),
            Some("/tmp/real.json")
        );
    }

    #[test]
    fn extra_env_cannot_override_tmpdir_or_slug() {
        let m = minimal();
        let extra = vec![
            ("AGENT_TMPDIR".to_string(), "/evil".to_string()),
            ("AGENT_SLUG".to_string(), "evil".to_string()),
            ("AGENT_DRY_RUN".to_string(), "true".to_string()),
        ];
        let spec = spec_with(&m, &[], None, &extra);
        let mut cmd = Command::new("true");
        apply_env(
            &mut cmd,
            &spec,
            "x",
            "real-slug",
            &Local::now(),
            Path::new("/tmp/real"),
            Path::new("/tmp/hb.json"),
        );
        assert_eq!(lookup(&cmd, "AGENT_TMPDIR").as_deref(), Some("/tmp/real"));
        assert_eq!(lookup(&cmd, "AGENT_SLUG").as_deref(), Some("real-slug"));
        assert_eq!(lookup(&cmd, "AGENT_DRY_RUN").as_deref(), Some("false"));
    }

    #[test]
    fn manifest_env_extra_still_applies() {
        let m = manifest(
            r#"
[agent]
name = "x"
[run]
command = "true"
[env.extra]
FOO = "bar"
"#,
        );
        let spec = spec_with(&m, &[], None, &[]);
        let mut cmd = Command::new("true");
        apply_env(
            &mut cmd,
            &spec,
            "x",
            "slug",
            &Local::now(),
            Path::new("/tmp"),
            Path::new("/tmp/hb.json"),
        );
        assert_eq!(lookup(&cmd, "FOO").as_deref(), Some("bar"));
    }

    #[test]
    fn extra_env_wins_over_manifest_env_extra() {
        // Per-invocation context is more specific than the manifest's
        // static block, so it is applied later.
        let m = manifest(
            r#"
[agent]
name = "x"
[run]
command = "true"
[env.extra]
FOO = "from-manifest"
"#,
        );
        let extra = vec![("FOO".to_string(), "from-trigger".to_string())];
        let spec = spec_with(&m, &[], None, &extra);
        let mut cmd = Command::new("true");
        apply_env(
            &mut cmd,
            &spec,
            "x",
            "slug",
            &Local::now(),
            Path::new("/tmp"),
            Path::new("/tmp/hb.json"),
        );
        assert_eq!(lookup(&cmd, "FOO").as_deref(), Some("from-trigger"));
    }

    #[test]
    fn dry_run_omits_the_heartbeat_path() {
        let m = minimal();
        let mut spec = spec_with(&m, &[], None, &[]);
        spec.dry_run = true;
        let mut cmd = Command::new("true");
        apply_env(
            &mut cmd,
            &spec,
            "x",
            "slug",
            &Local::now(),
            Path::new("/tmp"),
            Path::new("/tmp/hb.json"),
        );
        assert!(lookup(&cmd, "AGENT_HEARTBEAT_FILE").is_none());
        assert_eq!(lookup(&cmd, "AGENT_DRY_RUN").as_deref(), Some("true"));
    }

    #[test]
    fn tail_lines_keeps_the_last_n_and_reports_the_drop() {
        let body = (1..=10)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let (tail, dropped) = tail_lines(&body, 3);
        assert_eq!(tail, "8\n9\n10");
        assert_eq!(dropped, 7);
    }

    #[test]
    fn tail_lines_keeps_everything_when_under_the_limit() {
        let (tail, dropped) = tail_lines("a\nb", 10);
        assert_eq!(tail, "a\nb");
        assert_eq!(dropped, 0);
    }
}
