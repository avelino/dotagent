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
use tracing::{info, warn};

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
    let outcome = match ctx.persistent {
        Some(pool) if manifest_ref.lifecycle.is_persistent() && !spec.dry_run => {
            pool.dispatch(&spec, ctx.state, ctx.audit).await?
        }
        _ => run(spec, ctx.state, ctx.supervisor).await?,
    };

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
        finish_heartbeat(state, &name, &slug, spec.args, &start, &finish, exit_code)?;
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
    // Before `[env.extra]`, so a manifest that names a locale still wins.
    if let Some(lang) = default_locale(
        inherit,
        std::env::var_os("LANG"),
        std::env::var_os("LC_ALL"),
    ) {
        cmd.env("LANG", lang);
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

/// Pick a `LANG` for the agent, or `None` when the parent already named one.
///
/// launchd and systemd start a daemon with no `LANG` and no `LC_ALL`, and
/// every agent inherits that gap. A process in the resulting `C` locale has
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
/// Kept pure so the decision is testable without mutating the process
/// environment, which every other test in this file would then race against.
fn default_locale(
    inherit: bool,
    parent_lang: Option<std::ffi::OsString>,
    parent_lc_all: Option<std::ffi::OsString>,
) -> Option<&'static str> {
    // `env_clear()` drops whatever the parent had, so an inherited locale only
    // counts when the agent is actually inheriting.
    let inherited = inherit && (parent_lang.is_some() || parent_lc_all.is_some());
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

    // --- locale: the `C` locale mangles every non-ASCII byte we inject ---

    fn os(s: &str) -> Option<std::ffi::OsString> {
        Some(std::ffi::OsString::from(s))
    }

    #[test]
    fn locale_is_named_when_the_parent_named_none() {
        // launchd and systemd both start a daemon this way.
        assert_eq!(default_locale(true, None, None), Some(FALLBACK_LOCALE));
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
        assert_eq!(default_locale(true, os("pt_BR.UTF-8"), None), None);
    }

    #[test]
    fn an_inherited_lc_all_is_left_alone() {
        // LC_ALL outranks LANG, so naming LANG under it would be a no-op that
        // reads like a fix.
        assert_eq!(default_locale(true, None, os("pt_BR.UTF-8")), None);
    }

    #[test]
    fn a_cleared_environment_gets_the_locale_back() {
        // `inherit = false` calls `env_clear()`, so whatever the parent had is
        // gone and the agent is back in `C` — the case that needs it most.
        assert_eq!(
            default_locale(false, os("pt_BR.UTF-8"), os("pt_BR.UTF-8")),
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
            lookup(&cmd, "LANG").is_some()
                || std::env::var_os("LANG").is_some()
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
        let extra = vec![("AGENT_TRIGGER_SOURCE".to_string(), "telegram".to_string())];
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
            lookup(&cmd, "AGENT_TRIGGER_SOURCE").as_deref(),
            Some("telegram")
        );
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
