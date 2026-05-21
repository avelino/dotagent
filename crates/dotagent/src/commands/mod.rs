//! Command implementations.
//!
//! Each command is a thin glue layer over the supporting crates:
//! - `run`            → dotagent-runner + manifest discovery
//! - `tick`           → dotagent-scheduler + dotagent-state + dotagent-runner + dotagent-plugin
//! - `status`         → dotagent-scheduler + dotagent-state (read-only)
//! - `daily-summary`  → status + dotagent-plugin (notify)
//! - `install`/`uninstall` → dotagent-unit-gen
//! - `doctor`         → manifest validation + plugin discovery

use anyhow::{anyhow, bail, Context, Result};
use dotagent_plugin::PluginClient;
use dotagent_runner::{run as runner_run, RunSpec};
use dotagent_state::StateStore;
use dotagent_unit_gen::GenContext;

pub mod completions;
pub mod daemon;
pub mod daily_summary;
pub mod list_agents;
pub mod output;
pub mod status;
pub mod utility;

use crate::discovery;

/// Execute one schedule of one agent.
pub async fn run(name: String, schedule: String, dry_run: bool) -> Result<()> {
    let agent = discovery::find_by_name(&name)?;
    let sched = discovery::schedule_by_id(&agent.manifest, &schedule)?;
    let args = sched.args().to_vec();
    let state = StateStore::from_home().context("opening state store")?;

    let spec = RunSpec {
        manifest: &agent.manifest,
        manifest_dir: &agent.dir,
        schedule_id: &schedule,
        args: &args,
        dry_run,
        manifest_sha256: None,
    };
    let outcome = runner_run(spec, &state).await.context("runner failed")?;

    if !outcome.stdout_tail.is_empty() {
        println!("{}", outcome.stdout_tail);
    }
    if outcome.timed_out {
        eprintln!(
            "[dotagent] {name}/{schedule}: timeout (exit {})",
            outcome.exit_code
        );
    }
    std::process::exit(outcome.exit_code);
}

/// One-shot tick: discover, retry, notify, and exit. Same logic as a single
/// daemon loop iteration but without sleeping.
pub async fn tick(dry_run: bool, _verbose: bool) -> Result<()> {
    let state = StateStore::from_home().context("opening state store")?;
    let now = chrono::Local::now();

    if dry_run {
        let r = daemon::tick_dry_run(&state, now).await;
        println!(
            "(dry-run) scanned {} agent(s); would dispatch {}; next event: {}",
            r.agents_scanned,
            r.runs_dispatched,
            r.next_event
                .map(|t| t.format("%Y-%m-%dT%H:%M:%S%z").to_string())
                .unwrap_or_else(|| "—".into())
        );
        return Ok(());
    }

    let audit = dotagent_state::AuditLog::from_home().context("opening audit log")?;
    let plugins = PluginClient::from_environment();
    let cache = dotagent_state::ManifestCache::from_home().context("opening manifest cache")?;
    let r = daemon::tick_once(&state, &audit, &plugins, &cache, now).await;
    println!(
        "scanned {} agent(s); dispatched {}; next event: {}",
        r.agents_scanned,
        r.runs_dispatched,
        r.next_event
            .map(|t| t.format("%Y-%m-%dT%H:%M:%S%z").to_string())
            .unwrap_or_else(|| "—".into())
    );
    Ok(())
}

pub async fn daemon_cmd() -> Result<()> {
    daemon::run().await
}

pub async fn status() -> Result<()> {
    status::run().await
}

pub async fn daily_summary(dry_run: bool) -> Result<()> {
    daily_summary::run(dry_run).await
}

pub async fn bootstrap() -> Result<()> {
    Err(anyhow!("bootstrap — not yet implemented"))
}

/// Install the dotagent daemon unit (one per system, not per agent).
///
/// Arguments are accepted for CLI compatibility but logged as no-op hints —
/// scheduling is now centralized in the daemon itself.
pub async fn install(all: bool, name: Option<String>) -> Result<()> {
    if all || name.is_some() {
        eprintln!(
            "[install] note: dotagent now uses ONE daemon unit (run.avelino.dotagent). \
            --all and per-agent install are no-ops; the daemon manages every discovered \
            manifest internally."
        );
    }
    let ctx = gen_context()?;
    let unit =
        dotagent_unit_gen::generate_daemon_unit(&ctx).context("generating daemon unit file")?;
    println!("wrote {}", unit.path.display());
    println!();
    println!("Next steps:");
    #[cfg(target_os = "macos")]
    println!(
        "  launchctl bootstrap \"gui/$(id -u)\" {}",
        unit.path.display()
    );
    #[cfg(target_os = "linux")]
    println!(
        "  systemctl --user daemon-reload && systemctl --user enable --now {}",
        dotagent_unit_gen::DAEMON_LABEL
    );
    Ok(())
}

/// Remove the dotagent daemon unit.
pub async fn uninstall(all: bool, name: Option<String>) -> Result<()> {
    if all || name.is_some() {
        eprintln!(
            "[uninstall] note: dotagent now uses ONE daemon unit (run.avelino.dotagent). \
            --all and per-agent uninstall are no-ops."
        );
    }
    match dotagent_unit_gen::uninstall_daemon_unit().context("removing daemon unit")? {
        Some(path) => println!("removed {}", path.display()),
        None => println!("nothing to remove (daemon unit not found)"),
    }
    Ok(())
}

/// Validate every discovered manifest + check that referenced plugins resolve
/// + warn about missing `[security]` declarations + detect manifest drift.
pub async fn doctor() -> Result<()> {
    // Report secrets file status first — it's the most common source of
    // confusion when notifiers can't resolve `${VAR}`. Counted toward
    // warnings (insecure mode is a problem the operator should fix), never
    // toward errors (daemon still runs without secrets).
    let (mut errors, mut warnings) = report_secrets_status();

    let agents = discovery::discover_all()?;
    if agents.is_empty() {
        println!("no agents discovered");
        return Ok(());
    }
    let client = PluginClient::from_environment();
    let cache = dotagent_state::ManifestCache::from_home()
        .context("opening manifest cache")?
        .load()
        .unwrap_or_default();

    for agent in &agents {
        let name = &agent.manifest.agent.name;
        match agent.manifest.validate() {
            Ok(()) => println!("✓ {name}: manifest ok"),
            Err(e) => {
                println!("✗ {name}: {e}");
                errors += 1;
                continue;
            }
        }
        // Plugin resolution (preflight + legacy on_success/on_failure)
        for plugin_ref in agent
            .manifest
            .preflight
            .iter()
            .chain(agent.manifest.on_success.iter())
            .chain(agent.manifest.on_failure.iter())
        {
            match client.resolve(&plugin_ref.plugin) {
                Ok(path) => println!("    plugin {} → {}", plugin_ref.plugin, path.display()),
                Err(e) => {
                    println!("    ✗ plugin {} not found: {e}", plugin_ref.plugin);
                    errors += 1;
                }
            }
        }
        // Built-in notifiers — print driver. Plugin escape-hatch entries
        // still need PATH resolution.
        for entry in &agent.manifest.notifiers {
            let driver = entry.driver_name();
            if let Some(p) = entry.as_plugin() {
                match client.resolve(&p.name) {
                    Ok(path) => {
                        println!(
                            "    notifier driver=plugin name={} → {}",
                            p.name,
                            path.display()
                        )
                    }
                    Err(e) => {
                        println!("    ✗ notifier plugin {} not found: {e}", p.name);
                        errors += 1;
                    }
                }
            } else {
                println!("    notifier driver={driver} (built-in)");
            }
        }
        // [security] declaration
        if !agent.manifest.security.is_explicit() {
            println!(
                "    ⚠ {name}: no [security] section — blast radius is unbounded. \
                See docs/security/threat-model.md."
            );
            warnings += 1;
        }
        // Manifest drift vs. cache
        let manifest_path = agent.dir.join("agent.toml");
        if let Ok(sha) = dotagent_state::hash_manifest_file(&manifest_path) {
            if let Some(entry) = cache.entries.get(name) {
                if entry.sha256 != sha {
                    println!(
                        "    ⚠ {name}: manifest drift since last daemon run \
                        (cached {} → now {})",
                        &entry.sha256[..12.min(entry.sha256.len())],
                        &sha[..12.min(sha.len())]
                    );
                    warnings += 1;
                }
            }
        }
    }
    println!();
    println!(
        "summary: {} agent(s), {} error(s), {} warning(s)",
        agents.len(),
        errors,
        warnings
    );
    if errors > 0 {
        bail!("{errors} issue(s) found");
    }
    Ok(())
}

pub async fn plugin_list() -> Result<()> {
    let client = PluginClient::from_environment();
    let agents = discovery::discover_all()?;
    let mut names: std::collections::BTreeSet<String> = Default::default();
    for agent in &agents {
        for pr in agent
            .manifest
            .preflight
            .iter()
            .chain(agent.manifest.on_success.iter())
            .chain(agent.manifest.on_failure.iter())
        {
            names.insert(pr.plugin.clone());
        }
        // Notifier escape-hatch (driver = "plugin") still resolves a binary.
        for entry in &agent.manifest.notifiers {
            if let Some(p) = entry.as_plugin() {
                names.insert(p.name.clone());
            }
        }
    }
    for name in names {
        match client.resolve(&name) {
            Ok(path) => match client.info(&name).await {
                Ok(info) => println!(
                    "{name}\t{}\t{}\t{}",
                    info.version.unwrap_or_default(),
                    info.kinds
                        .iter()
                        .map(|k| serde_json::to_string(k).unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join(","),
                    path.display(),
                ),
                Err(e) => println!("{name}\t(info failed: {e})\t{}", path.display()),
            },
            Err(e) => println!("{name}\t(not found: {e})"),
        }
    }
    Ok(())
}

pub async fn plugin_invoke(name: String, _payload: String) -> Result<()> {
    Err(anyhow!("plugin invoke {name} — not yet implemented"))
}

/// Print a one-block status for the secrets file. Returns `(errors,
/// warnings)` so the caller can fold the counts into its summary.
///
/// Never prints keys or values — only path, presence, permission state,
/// and key count.
///
/// **Note**: this calls `SecretsStore::load` directly, which means any
/// `op://...` references in the file are resolved *again* via `op read`
/// (one fork per reference). For an interactive `doctor` invocation
/// this is fine — the count is single-digit in practice. If a future
/// CI gate calls `doctor` in a tight loop, swap to
/// `dotagent_secrets::snapshot()` to read the daemon's in-memory copy
/// without re-shelling out.
fn report_secrets_status() -> (usize, usize) {
    let config =
        dotagent_core::Config::load(dotagent_state::paths::config_file()).unwrap_or_default();
    let path = daemon::resolve_secrets_path(&config);
    let source_hint = if config.secrets.is_set() {
        " (from config.toml [secrets].file)"
    } else if std::env::var("DOTAGENT_SECRETS_FILE").is_ok() {
        " (from DOTAGENT_SECRETS_FILE)"
    } else {
        " (default)"
    };
    println!("secrets file: {}{source_hint}", path.display());

    let mut errors = 0usize;
    let mut warnings = 0usize;
    match dotagent_secrets::SecretsStore::load(&path) {
        Ok(Some(store)) => {
            #[cfg(unix)]
            let mode_str = std::fs::metadata(&path)
                .ok()
                .map(|m| {
                    use std::os::unix::fs::PermissionsExt;
                    format!("{:o}", m.permissions().mode() & 0o777)
                })
                .unwrap_or_else(|| "?".into());
            #[cfg(not(unix))]
            let mode_str = "n/a".to_string();
            println!("    ✓ loaded — {} key(s), mode {mode_str}", store.len());
            if store.unresolved_references() > 0 {
                // Counted as warnings — daemon still ran but some keys
                // are unset and any notifier needing them will fail.
                println!(
                    "    ⚠ {} secret reference(s) failed to resolve (e.g. `op://...`). \
                    Affected keys are unset; check daemon logs for which reference failed.",
                    store.unresolved_references()
                );
                warnings += 1;
            }
        }
        Ok(None) => {
            println!("    (not present — secrets are optional)");
        }
        Err(dotagent_secrets::SecretsError::InsecureMode { mode, .. }) => {
            println!(
                "    ⚠ insecure permissions (mode {mode:o}) — daemon will refuse to load. \
                Fix with: chmod 600 {}",
                path.display()
            );
            warnings += 1;
        }
        Err(e) => {
            println!("    ✗ {e}");
            errors += 1;
        }
    }
    println!();
    (errors, warnings)
}

fn gen_context() -> Result<GenContext> {
    let dotagent_binary = std::env::current_exe().context("locating dotagent binary")?;
    // launchd / systemd `StandardOutPath` lands here. The daemon itself
    // ALSO writes structured JSON logs into the same directory via
    // `dotagent-telemetry`, so leave this scoped to `logs/daemon/`.
    let log_dir = dotagent_state::paths::daemon_logs_dir();
    std::fs::create_dir_all(&log_dir).ok();
    Ok(GenContext {
        dotagent_binary,
        log_dir,
    })
}
