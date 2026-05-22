//! Utility commands: `logs`, `inspect`, `reload`, `run-now`.
//!
//! Each is a thin wrapper around state already on disk. Useful for
//! day-to-day debugging without spinning up the daemon.

use anyhow::{anyhow, bail, Context, Result};
use chrono::Local;
use dotagent_plugin::PluginClient;
use dotagent_runner::{run_with_hooks, RunContext, RunSpec};
use dotagent_state::{slug_from_args, AuditLog, ManifestCache, StateStore};

use crate::commands::output::{render_outcome, Format};
use crate::discovery;

// ---------------------------------------------------------------------
// `dotagent logs <agent> [--schedule <id>] [--follow] [-n <count>]`
// ---------------------------------------------------------------------

/// Tail the daemon-captured stdout for one agent — or every agent at once.
///
/// With `agent = Some(name)`: reads from `$DOTAGENT_HOME/logs/agents/<name>/`
/// and picks up both `<name>.log` and any rolled `<name>.log.YYYY-MM-DD`
/// files so `--follow` keeps working across a rotation boundary.
///
/// With `agent = None`: walks `$DOTAGENT_HOME/logs/agents/*/` and tails
/// every agent's active + rolled logs together. `tail` prefixes each
/// chunk with `==> path <==` so you can tell who's logging.
pub async fn logs(
    agent: Option<String>,
    _schedule: Option<String>,
    lines: usize,
    follow: bool,
) -> Result<()> {
    let entries = match agent.as_deref() {
        Some(name) => collect_agent_logs(name)?,
        None => collect_all_agent_logs()?,
    };

    if entries.is_empty() {
        match agent.as_deref() {
            Some(name) => bail!(
                "no logs for {name} in {}",
                dotagent_state::paths::agent_logs_dir(name).display()
            ),
            None => bail!(
                "no agent logs found under {}",
                dotagent_state::paths::logs_dir().join("agents").display()
            ),
        }
    }

    // Use `tail` since reimplementing -F portably is out of scope.
    let mut cmd = std::process::Command::new("tail");
    cmd.arg("-n").arg(lines.to_string());
    if follow {
        cmd.arg("-F");
    }
    for p in &entries {
        cmd.arg(p);
    }
    let status = cmd.status().context("invoking tail")?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Collect `<agent>.log` + rolled `<agent>.log.YYYY-MM-DD` files (skip .gz —
/// `tail` can't follow compressed files). Sort so the active log comes
/// last → `tail -F` follows the most recent.
fn collect_agent_logs(agent: &str) -> Result<Vec<std::path::PathBuf>> {
    let log_dir = dotagent_state::paths::agent_logs_dir(agent);
    let prefix = format!("{agent}.log");
    let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(&log_dir)
        .map_err(|_| anyhow!("no logs found at {}", log_dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or_default();
            name.starts_with(&prefix) && !name.ends_with(".gz")
        })
        .collect();
    entries.sort();
    Ok(entries)
}

/// Walk `logs/agents/*/` and merge each agent's collected files into a
/// single list. Missing or unreadable subdirs are silently skipped — we
/// only fail the command if NO agent has any logs.
fn collect_all_agent_logs() -> Result<Vec<std::path::PathBuf>> {
    let root = dotagent_state::paths::logs_dir().join("agents");
    let read = match std::fs::read_dir(&root) {
        Ok(r) => r,
        Err(_) => return Ok(Vec::new()),
    };
    let mut all = Vec::new();
    for entry in read.filter_map(|e| e.ok()) {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Ok(files) = collect_agent_logs(&name) {
            all.extend(files);
        }
    }
    all.sort();
    Ok(all)
}

// ---------------------------------------------------------------------
// `dotagent inspect <agent>`
// ---------------------------------------------------------------------

/// Print heartbeat + window state + manifest hash for an agent.
pub async fn inspect(agent_name: String) -> Result<()> {
    let agent = discovery::find_by_name(&agent_name)?;
    let state = StateStore::from_home()?;
    let cache = ManifestCache::from_home()?.load().unwrap_or_default();

    println!("agent:        {}", agent.manifest.agent.name);
    println!("manifest_dir: {}", agent.dir.display());
    if let Some(known) = cache.entries.get(&agent_name) {
        println!(
            "manifest_sha: {} (first seen {})",
            known.sha256, known.first_seen_at_iso
        );
    }
    println!("monitor:      {}", agent.manifest.agent.monitor);
    println!("timeout:      {}s", agent.manifest.agent.timeout_seconds);
    println!();

    for sched in &agent.manifest.schedules {
        let slug = slug_from_args(sched.args());
        println!("─── schedule '{}' (slug={slug}) ───", sched.id());
        match state.read_heartbeat(&agent.manifest.agent.name, &slug) {
            Ok(Some(hb)) => {
                let v = serde_json::to_string_pretty(&hb)?;
                for line in v.lines() {
                    println!("  {line}");
                }
            }
            _ => println!("  (no heartbeat)"),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// `dotagent reload`
// ---------------------------------------------------------------------

/// Send SIGHUP to the running daemon so it re-reads manifests + plugins.
pub async fn reload() -> Result<()> {
    let path = crate::commands::daemon::pidfile_path().ok_or_else(|| anyhow!("no home dir"))?;
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {} (daemon not running?)", path.display()))?;
    let pid: i32 = raw.trim().parse()?;
    #[cfg(unix)]
    {
        // SAFETY: kill(2) with a parsed PID and SIGHUP (1) is standard.
        let rc = unsafe { libc::kill(pid, libc::SIGHUP) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("sending SIGHUP");
        }
        println!("sent SIGHUP to dotagent daemon (pid={pid})");
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        bail!("reload is unix-only");
    }
}

// ---------------------------------------------------------------------
// `dotagent run-now <agent> [--schedule <id>]`
// ---------------------------------------------------------------------

/// Force an agent to run NOW, regardless of schedule windows. Useful for
/// manual triggering after a failure. Uses the schedule's `args` if a
/// schedule id is provided; otherwise uses the first declared schedule's
/// args.
///
/// Visibility note: `run-now` runs in its OWN process and instantiates its
/// OWN supervisor — separate from the daemon's. If a daemon is also
/// running, `dotagent status` only reflects the daemon's supervisor; the
/// subprocess tree spawned by this call won't appear there. Tracked as a
/// follow-up to issue #36 (unify via per-PID snapshot files or a real
/// daemon-side IPC).
pub async fn run_now(agent_name: String, schedule: Option<String>, format: Format) -> Result<()> {
    let agent = discovery::find_by_name(&agent_name)?;
    let sched = match schedule {
        Some(id) => discovery::schedule_by_id(&agent.manifest, &id)?,
        None => agent
            .manifest
            .schedules
            .first()
            .ok_or_else(|| anyhow!("agent {agent_name} declares no schedules"))?,
    };
    let args = sched.args().to_vec();
    let state = StateStore::from_home()?;
    let audit = AuditLog::from_home()?;
    let plugins = PluginClient::from_environment();

    let manifest_path = agent.dir.join("agent.toml");
    let manifest_sha256 = dotagent_state::hash_manifest_file(&manifest_path).ok();
    let spec = RunSpec {
        manifest: &agent.manifest,
        manifest_dir: &agent.dir,
        schedule_id: sched.id(),
        args: &args,
        dry_run: false,
        manifest_sha256,
    };
    let ctx = RunContext {
        state: &state,
        plugins: Some(&plugins),
        audit: Some(&audit),
        supervisor: Some(plugins.supervisor()),
    };
    let started = Local::now();
    let outcome = run_with_hooks(spec, &ctx).await?;
    let duration = (Local::now() - started).num_seconds();
    render_outcome(&agent_name, sched.id(), &outcome, duration, format);
    Ok(())
}
