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
use dotagent_runner::persistent::PersistentPool;
use dotagent_runner::{run_with_hooks, OrchestratedOutcome, RunContext, RunSpec};
use dotagent_state::{AuditLog, StateStore};
use dotagent_supervisor::Supervisor;
use dotagent_unit_gen::GenContext;

pub mod audit;
pub mod completions;
pub mod daemon;
pub mod daily_summary;
pub mod list_agents;
pub mod mcp;
pub mod output;
pub mod status;
pub mod utility;

use crate::discovery;

/// Run one agent from a short-lived process — `dotagent run`, `run-now`, an
/// MCP tool call. Everything the daemon does not own.
///
/// A persistent agent still speaks the JSON-lines protocol here. Running it
/// one-shot instead would hand it a closed stdin, which every correct
/// implementation reads as "shut down" — so the agent would exit without
/// answering and the operator would be debugging a protocol they never left.
/// It gets a pool that lives exactly as long as this call: the startup cost is
/// paid and thrown away, which is precisely what the daemon exists to avoid,
/// and precisely what a one-off invocation should do.
pub(crate) async fn run_scoped(
    spec: RunSpec<'_>,
    state: &StateStore,
    supervisor: &Supervisor,
    plugins: Option<&PluginClient>,
    audit: Option<&AuditLog>,
) -> dotagent_runner::Result<OrchestratedOutcome> {
    let pool = spec
        .manifest
        .lifecycle
        .is_persistent()
        .then(|| PersistentPool::new(supervisor.clone()));
    let ctx = RunContext {
        state,
        plugins,
        audit,
        supervisor: Some(supervisor),
        persistent: pool.as_ref(),
    };
    let outcome = run_with_hooks(spec, &ctx).await;
    if let Some(pool) = &pool {
        // Before returning, not on drop: the instance has to be reaped while
        // there is still an async context to kill it in.
        pool.shutdown(audit).await;
    }
    outcome
}

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
        slug_override: None,
        extra_env: &[],
    };
    // No plugins and no audit: `dotagent run` is the ad-hoc foreground path,
    // and firing an agent's sinks from a debugging session would publish
    // whatever it printed.
    let supervisor = Supervisor::new();
    let outcome = match run_scoped(spec, &state, &supervisor, None, None)
        .await
        .context("runner failed")?
    {
        OrchestratedOutcome::Ran(outcome) => outcome,
        // Unreachable with `plugins: None` (preflight never runs), but the
        // type says otherwise and a panic here would be gratuitous.
        OrchestratedOutcome::PreflightFailed { plugin, suggest } => {
            eprintln!(
                "[dotagent] {name}/{schedule}: preflight {plugin} aborted the run{}",
                suggest.map(|s| format!(": {s}")).unwrap_or_default()
            );
            std::process::exit(1);
        }
    };

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
    // A pool that lives for this one tick. Same reasoning as `run_scoped`: a
    // persistent agent must speak its protocol here too, and nothing should
    // outlive the command that started it.
    let pool = PersistentPool::new(plugins.supervisor().clone());
    // Honors `[power]` exactly as the daemon does. A `tick` that dispatched
    // what the daemon would have held back would be a debugging tool that
    // lies about the thing it exists to reproduce.
    let cfg = dotagent_core::Config::load(dotagent_state::paths::config_file()).unwrap_or_default();
    let r = daemon::tick_once(
        &state,
        &audit,
        &plugins,
        &cache,
        Some(&pool),
        &cfg.power,
        now,
    )
    .await;
    pool.shutdown(Some(&audit)).await;
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
    warnings += report_memory_status();
    warnings += report_skills_status();
    warnings += report_commands_status();

    let found = discovery::discover();
    // After the scan, so the dispatcher check reuses it instead of triggering
    // a second walk of the filesystem — and cannot disagree with it.
    warnings += report_telegram_status(&found.agents);
    // Report unloadable manifests first: an agent that cannot parse never
    // runs, and its absence from the list below is otherwise silent.
    for bad in &found.invalid {
        println!("✗ {}: {}", bad.path.display(), bad.error);
        errors += 1;
    }
    let agents = found.agents;
    if agents.is_empty() {
        if errors == 0 {
            println!("no agents discovered");
        }
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
        // Lifecycle — only worth a line when it is not the default shape.
        if agent.manifest.lifecycle.is_persistent() {
            let lc = &agent.manifest.lifecycle;
            println!(
                "    lifecycle=persistent key={} max_instances={} idle={}s max_invocations={}",
                lc.key.as_deref().unwrap_or("(single instance)"),
                lc.max_instances,
                lc.idle_timeout_seconds,
                if lc.max_invocations == 0 {
                    "unlimited".to_string()
                } else {
                    lc.max_invocations.to_string()
                }
            );
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
    // Supervisor health — flag any live subprocess past 80% of its deadline.
    if let Some(snap) = crate::commands::status::read_supervisor_snapshot() {
        let mut hot = snap.iter().filter(|p| p.deadline_pct >= 80).peekable();
        if hot.peek().is_some() {
            println!();
            println!("supervisor: subprocess(es) approaching deadline");
            for p in hot {
                let icon = if p.deadline_pct >= 100 { "✗" } else { "⚠" };
                let kind = p.kind.to_string();
                println!(
                    "    {icon} {kind}.{} pid={} agent={} age={}s deadline={}s ({}%)",
                    p.label,
                    p.pid,
                    p.owner.agent,
                    p.age_seconds,
                    p.deadline_seconds,
                    p.deadline_pct
                );
                if p.deadline_pct >= 100 {
                    errors += 1;
                } else {
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
/// **Note**: `dotagent doctor` runs in its OWN process, separate from
/// the daemon, so the in-memory `dotagent_secrets::snapshot()` is always
/// empty here — there is no daemon store to inspect. That means doctor
/// has to call `SecretsStore::load` again, which re-shells out to
/// `op read` once per `op://` reference. For an interactive run this
/// is fine (single-digit refs in practice). If a future CI gate calls
/// `doctor` in a tight loop with many refs, cache inside this function
/// rather than reaching for `snapshot()`.
/// Report the skill catalog. Returns a warning count.
///
/// Skills come from several roots — including `~/.claude/skills`, which the
/// operator did not create for dotagent — so the count alone is not enough.
/// A skill that fails to parse is a warning, not an error: nothing stops
/// running, an assistant just answers without a procedure it should have had.
fn report_skills_status() -> usize {
    let config =
        dotagent_core::Config::load(dotagent_state::paths::config_file()).unwrap_or_default();
    if !config.skills.enabled {
        println!("skills: off ([skills] enabled = false)");
        return 0;
    }

    let found = crate::skills::discover();
    let claude = if config.skills.claude_skills {
        ", including ~/.claude/skills"
    } else {
        ""
    };
    println!("skills: {} found{claude}", found.skills.len());

    let mut warnings = 0;
    for bad in &found.invalid {
        println!("    ⚠ {}: {}", bad.path.display(), bad.error);
        warnings += 1;
    }
    // Collisions are silent in the catalog (first wins), so say them out loud
    // here — a skill you wrote and cannot call is otherwise a mystery.
    let mut taken: std::collections::HashMap<String, String> = Default::default();
    for skill in &found.skills {
        let tool = dotagent_mcp::skill_tool_name_for(&skill.manifest.name);
        if let Some(first) = taken.get(&tool) {
            println!(
                "    ⚠ {} maps to the same tool as {first} ({tool}) — only the first is callable",
                skill.manifest.name
            );
            warnings += 1;
        } else {
            taken.insert(tool, skill.manifest.name.clone());
        }
    }
    warnings
}

/// Report the command catalog. Returns a warning count.
///
/// Says more than the skill report because a command carries **two** derived
/// names — the MCP tool and the Telegram menu entry — and they collide under
/// different rules. A command shadowed on Telegram is invisible: it is in the
/// catalog, `command-get` resolves it, and the menu never offers it.
fn report_commands_status() -> usize {
    let config =
        dotagent_core::Config::load(dotagent_state::paths::config_file()).unwrap_or_default();
    if !config.commands.enabled {
        println!("commands: off ([commands] enabled = false)");
        return 0;
    }

    let found = crate::slash::discover();
    let claude = if config.commands.claude_commands {
        ", including ~/.claude/commands"
    } else {
        ""
    };
    println!("commands: {} found{claude}", found.commands.len());

    let mut warnings = 0;
    for bad in &found.invalid {
        println!("    ⚠ {}: {}", bad.path.display(), bad.error);
        warnings += 1;
    }
    for (telegram, cmds) in found.telegram_collisions() {
        // Naming the files, not just the commands: two names that differ only
        // by `-` versus `_` are near-identical on screen, and the useful
        // question is which file to rename.
        let where_from: Vec<String> = cmds
            .iter()
            .map(|c| format!("{} ({})", c.manifest.name, c.path.display()))
            .collect();
        println!(
            "    ⚠ {} all want /{telegram} — only the first is in the menu",
            where_from.join(", ")
        );
        warnings += 1;
    }
    if !found.commands.is_empty() && !config.telegram.is_enabled() {
        println!("    ℹ no Telegram ingress configured — no menu is published");
    }
    warnings
}

/// Report where long-term memory lives. Returns a warning count.
///
/// Says the path out loud because memory is a directory you are meant to open
/// and read — an outl workspace, not an opaque store.
fn report_memory_status() -> usize {
    let config =
        dotagent_core::Config::load(dotagent_state::paths::config_file()).unwrap_or_default();
    if !config.memory.enabled {
        println!("memory: off ([memory] enabled = false)");
        return 0;
    }

    match config.memory.workspace_override() {
        Some(path) => {
            let p = std::path::Path::new(path);
            if p.join(".outl").exists() {
                println!("memory: {path} (from config.toml)");
                0
            } else {
                println!("memory: {path} (from config.toml)");
                println!("    ⚠ no outl workspace there — run `outl init` in it, or clear the setting to use the default");
                1
            }
        }
        None => {
            let path = dotagent_state::paths::memory_workspace_dir();
            let exists = path.join(".outl").exists();
            println!(
                "memory: {} (default){}",
                path.display(),
                if exists {
                    ""
                } else {
                    " — created on first use"
                }
            );
            0
        }
    }
}

/// Report inbound Telegram status. Returns a warning count.
///
/// Silent when `[telegram]` is absent — the ingress is off by default and
/// saying so on every `doctor` run would be noise. It speaks up in the two
/// cases the operator needs to hear about: a half-configured section that
/// silently does nothing, and a dispatcher agent that does not resolve.
fn report_telegram_status(agents: &[discovery::DiscoveredAgent]) -> usize {
    let config =
        dotagent_core::Config::load(dotagent_state::paths::config_file()).unwrap_or_default();
    let tg = &config.telegram;
    if tg.bot_token.is_empty() && tg.allowed_user_ids.is_empty() {
        return 0;
    }

    let mut warnings = 0usize;
    if tg.is_enabled() {
        println!(
            "telegram ingress: on — {} allowed user(s), dispatcher '{}'",
            tg.allowed_user_ids.len(),
            tg.dispatcher_agent
        );
        // A dispatcher that does not resolve means every accepted message
        // fails after passing the allowlist — worth catching here rather
        // than in the chat.
        match agents
            .iter()
            .find(|a| a.manifest.agent.name == tg.dispatcher_agent)
        {
            None => {
                println!(
                    "    ⚠ dispatcher agent '{}' not found — every message will fail",
                    tg.dispatcher_agent
                );
                warnings += 1;
            }
            // A persistent dispatcher without a key is one process for every
            // conversation: whatever the process remembers from one sender is
            // there for the next one. Worth saying out loud, because nothing
            // about it looks wrong until two people use the bot.
            Some(a)
                if a.manifest.lifecycle.is_persistent() && a.manifest.lifecycle.key.is_none() =>
            {
                println!(
                    "    ⚠ dispatcher '{}' is persistent with no [lifecycle] key — every \
                     conversation shares one process, and whatever it holds. \
                     Set key = \"chat_id\".",
                    tg.dispatcher_agent
                );
                warnings += 1;
            }
            Some(_) => {}
        }
    } else if tg.allowed_user_ids.is_empty() {
        println!("telegram ingress: OFF — bot_token set but allowed_user_ids is empty");
        println!(
            "    ⚠ an empty allowlist means nobody, never everybody. Add your numeric user id."
        );
        warnings += 1;
    } else {
        println!("telegram ingress: OFF — allowed_user_ids set but bot_token is empty");
        warnings += 1;
    }
    warnings
}

fn report_secrets_status() -> (usize, usize) {
    let config =
        dotagent_core::Config::load(dotagent_state::paths::config_file()).unwrap_or_default();
    let path = daemon::resolve_secrets_path(&config);
    // Match the resolver's actual behavior: it ignores empty / non-absolute
    // overrides and falls back to default. Reporting "(from VAR)" when the
    // var is empty would tell the operator a lie.
    let cfg_absolute =
        config.secrets.is_set() && std::path::Path::new(&config.secrets.file).is_absolute();
    let env_absolute = std::env::var("DOTAGENT_SECRETS_FILE")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| std::path::PathBuf::from(v).is_absolute())
        .unwrap_or(false);
    let source_hint = if cfg_absolute {
        " (from config.toml [secrets].file)"
    } else if env_absolute {
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
