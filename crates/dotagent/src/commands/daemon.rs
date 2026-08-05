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
use dotagent_core::{
    audit::AuditEvent, AgentManifest, Heartbeat, Schedule, TriggerRequest, TriggerSource,
    WindowState, TRIGGER_SCHEDULE_ID,
};
use dotagent_plugin::PluginClient;
use dotagent_runner::persistent::PersistentPool;
use dotagent_runner::{run_with_hooks, OrchestratedOutcome, RunContext, RunSpec};
use dotagent_scheduler::{
    compute_next_event, expected_at, is_stale, should_retry, AgentSchedulePair, ResolvedPolicy,
};
use dotagent_state::{
    audit::AuditLog,
    manifest_cache::{hash_manifest_file, KnownManifest, ManifestCache},
    slug_from_args, StateStore,
};
use dotagent_supervisor::{Supervisor, DEFAULT_REAPER_TICK};
use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, info, warn};

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

/// Scaffold the memory workspace at boot when `[memory]` is enabled.
///
/// `dotagent mcp` would create it on the first tool call anyway, but lazily:
/// `doctor` would report memory as on, the tools would be listed, and the
/// directory would not exist until something happened to use it. Creating it
/// here means the state on disk matches what the config says.
///
/// Never fatal. A workspace that cannot be created is a reason for memory to
/// fail, not for scheduled agents to stop running.
fn ensure_memory_workspace(config: &dotagent_core::Config) {
    if !config.memory.enabled {
        return;
    }
    // A configured path came from a human; a typo must not silently scaffold
    // an empty workspace somewhere nobody will look.
    if config.memory.workspace_override().is_some() {
        return;
    }
    let path = dotagent_state::paths::memory_workspace_dir();
    match dotagent_memory::MemoryStore::open_or_init(&path) {
        Ok(_) => info!(path = %path.display(), "memory workspace ready"),
        Err(e) => warn!(path = %path.display(), error = %e, "could not prepare memory workspace"),
    }
}

/// The three handles every dispatch path needs.
///
/// Grouped because they always travel together and passing them individually
/// pushed `give_up` past clippy's argument limit. All three are path handles or
/// `Arc`s — cheap to hold, and sharing them is what keeps a run visible to
/// `dotagent status` and reapable on SIGTERM.
struct DaemonCtx<'a> {
    state: &'a StateStore,
    audit: &'a AuditLog,
    plugins: &'a PluginClient,
}

/// How many pending triggers to hold before the producer blocks. Deep enough
/// that a burst of chat messages survives one slow run, shallow enough that a
/// wedged daemon applies backpressure instead of growing without bound.
const TRIGGER_CHANNEL_DEPTH: usize = 64;

/// Run an agent because something asked, not because a window came due.
///
/// Deliberately **not** routed through [`dispatch_one`]: its first four guards
/// exist to answer "is this cron window due and retryable", and a trigger has
/// no window. Forcing one would corrupt retry accounting for the schedule.
///
/// Mirrors `utility::run_now`, but reuses the daemon's `PluginClient` (and so
/// its `Supervisor`), which keeps the run visible in `dotagent status` and
/// reapable on SIGTERM. Window state is untouched on purpose — a triggered run
/// has no attempts counter and can never mark a cron window as given up.
pub(crate) async fn run_triggered(
    req: &TriggerRequest,
    state: &StateStore,
    audit: &AuditLog,
    plugins: &PluginClient,
    pool: Option<&PersistentPool>,
) -> Result<OrchestratedOutcome> {
    let agent = discovery::find_by_name(&req.agent)
        .with_context(|| format!("trigger names unknown agent '{}'", req.agent))?;

    let (schedule_id, mut args) = match &req.schedule {
        Some(id) => {
            let sched = discovery::schedule_by_id(&agent.manifest, id)?;
            (sched.id().to_string(), sched.args().to_vec())
        }
        None => match agent.manifest.schedules.first() {
            Some(sched) => (sched.id().to_string(), sched.args().to_vec()),
            None => (TRIGGER_SCHEDULE_ID.to_string(), Vec::new()),
        },
    };
    args.extend(req.args.iter().cloned());

    let manifest_sha256 = hash_manifest_file(&agent.dir.join("agent.toml")).ok();
    let slug = req.slug();
    let extra_env = trigger_env(req);

    let _ = audit.append(AuditEvent::AgentTriggered {
        source: req.source.to_string(),
        actor: req.actor.clone(),
        agent: agent.manifest.agent.name.clone(),
        schedule: schedule_id.clone(),
    });

    info!(
        agent = %agent.manifest.agent.name,
        schedule = %schedule_id,
        source = %req.source,
        "dispatching triggered run"
    );

    let spec = RunSpec {
        manifest: &agent.manifest,
        manifest_dir: &agent.dir,
        schedule_id: &schedule_id,
        args: &args,
        dry_run: false,
        manifest_sha256,
        slug_override: Some(&slug),
        extra_env: &extra_env,
    };
    let ctx = RunContext {
        state,
        plugins: Some(plugins),
        audit: Some(audit),
        supervisor: Some(plugins.supervisor()),
        persistent: pool,
    };
    run_with_hooks(spec, &ctx).await.map_err(Into::into)
}

/// Run one trigger and send whatever it produced back to whoever asked.
///
/// Never propagates an error: a malformed or hostile trigger must not be able
/// to stop the daemon. Failures are logged and, when the source gave us a way
/// to answer, reported back to it.
async fn handle_trigger(
    req: TriggerRequest,
    state: &StateStore,
    audit: &AuditLog,
    plugins: &PluginClient,
    pool: &PersistentPool,
) {
    // Show "typing…" for as long as the run takes. A triggered run is seconds
    // to minutes of silence otherwise, which reads as the bot being broken.
    let typing = spawn_typing_indicator(&req);

    let reply = match run_triggered(&req, state, audit, plugins, Some(pool)).await {
        Ok(OrchestratedOutcome::Ran(outcome)) if outcome.exit_code == 0 => {
            if outcome.stdout_tail.trim().is_empty() {
                format!("{} finished with no output.", req.agent)
            } else {
                outcome.stdout_tail
            }
        }
        Ok(OrchestratedOutcome::Ran(outcome)) => {
            warn!(
                agent = %req.agent,
                exit_code = outcome.exit_code,
                timed_out = outcome.timed_out,
                "triggered run failed"
            );
            let what = if outcome.timed_out {
                "timed out".to_string()
            } else {
                format!("exited {}", outcome.exit_code)
            };
            format!("{} {what}.\n{}", req.agent, outcome.stderr_tail)
        }
        Ok(OrchestratedOutcome::PreflightFailed { plugin, suggest }) => {
            let hint = suggest.map(|s| format!(": {s}")).unwrap_or_default();
            format!("{} blocked by preflight {plugin}{hint}", req.agent)
        }
        Err(e) => {
            warn!(agent = %req.agent, error = %e, "trigger could not run");
            format!("Could not run {}: {e}", req.agent)
        }
    };

    // Stop before answering, so the indicator never outlives the reply.
    if let Some(t) = typing {
        t.abort();
    }
    deliver_reply(&req, &reply).await;
}

/// The configured bot token, or `None` when inbound Telegram is not set up.
///
/// Both the typing indicator and the reply need it, and each was re-reading
/// `config.toml` from disk — twice per message for the same string.
fn telegram_bot_token() -> Option<String> {
    let token = dotagent_core::Config::load(dotagent_state::paths::config_file())
        .unwrap_or_default()
        .telegram
        .bot_token;
    (!token.is_empty()).then_some(token)
}

/// Keep "typing…" alive in the chat until the returned handle is aborted.
///
/// `None` when the source cannot show one. Failures are logged at debug and
/// never surface: a missing indicator is cosmetic, and a run that dies because
/// a spinner failed would be a bad trade.
fn spawn_typing_indicator(req: &TriggerRequest) -> Option<tokio::task::JoinHandle<()>> {
    if req.source != TriggerSource::Telegram {
        return None;
    }
    let chat_id: i64 = req.reply_to.as_ref()?.parse().ok()?;
    let token = telegram_bot_token()?;

    Some(tokio::spawn(async move {
        loop {
            if let Err(e) = dotagent_notify::telegram_inbound::typing(&token, chat_id).await {
                debug!(error = %e, "typing indicator failed");
            }
            tokio::time::sleep(dotagent_notify::telegram_inbound::TYPING_REFRESH).await;
        }
    }))
}

/// Deliver a trigger's answer back to its origin. No-op when the source is
/// fire-and-forget (no `reply_to`).
async fn deliver_reply(req: &TriggerRequest, body: &str) {
    let Some(reply_to) = &req.reply_to else {
        return;
    };
    match req.source {
        TriggerSource::Telegram => {
            let Some(token) = telegram_bot_token() else {
                warn!("telegram reply skipped — no bot_token configured");
                return;
            };
            let Ok(chat_id) = reply_to.parse::<i64>() else {
                warn!(reply_to = %reply_to, "telegram reply_to is not a chat id");
                return;
            };
            // Quote the message being answered. Runs are asynchronous, so a
            // chat with several questions in flight would otherwise show
            // answers with no way to tell which is which. Absent (older
            // payload, or a source that has no such concept) just sends
            // unquoted rather than dropping the answer.
            let message_id = req
                .payload
                .as_ref()
                .and_then(|p| p.get("message_id"))
                .and_then(|v| v.as_i64());
            if let Err(e) =
                dotagent_notify::telegram_inbound::reply(&token, chat_id, message_id, body).await
            {
                // `NotifyError` from this path is already token-sanitized.
                warn!(error = %e, "could not deliver telegram reply");
            }
        }
        // Nothing to answer: the CLI already printed, MCP returned inline.
        TriggerSource::Cli | TriggerSource::Mcp => {}
    }
}

/// Start the ingress when the config asks for it, logging why when it does not.
///
/// Called at boot and again on every SIGHUP, so `[telegram]` changes take
/// effect on `dotagent reload` rather than waiting for a restart. That matters
/// most for `allowed_user_ids`: an operator revoking access expects it to be
/// revoked now.
fn start_telegram_ingress(
    cfg: &dotagent_core::TelegramIngressConfig,
    tx: &tokio::sync::mpsc::Sender<TriggerRequest>,
    audit: &AuditLog,
) -> Option<tokio::task::JoinHandle<()>> {
    if cfg.is_enabled() {
        return Some(spawn_telegram_ingress(
            cfg.clone(),
            tx.clone(),
            audit.clone(),
        ));
    }
    if !cfg.bot_token.is_empty() {
        warn!("telegram bot_token set but allowed_user_ids is empty — ingress stays off");
    }
    None
}

/// Long-poll Telegram and feed accepted messages into the trigger channel.
///
/// Runs as its own task rather than inside the main loop, which sleeps up to
/// [`MAX_SLEEP_MINUTES`] and would make the bot answer half an hour late.
/// Policy lives here (allowlist, rate limit, audit) while
/// `dotagent_notify::telegram_inbound` stays pure transport.
fn spawn_telegram_ingress(
    cfg: dotagent_core::TelegramIngressConfig,
    tx: tokio::sync::mpsc::Sender<TriggerRequest>,
    audit: AuditLog,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut poller = dotagent_notify::telegram_inbound::Poller::new(
            cfg.bot_token.clone(),
            cfg.poll_timeout_seconds,
            dotagent_state::paths::telegram_offset_file(),
        );
        let mut limiter =
            dotagent_notify::telegram_inbound::RateLimiter::new(cfg.rate_limit_per_minute);

        info!(
            allowed_users = cfg.allowed_user_ids.len(),
            dispatcher = %cfg.dispatcher_agent,
            "telegram ingress started"
        );
        publish_command_menu(&cfg).await;

        // Backs off on transport failure so a dropped network does not turn
        // into a tight retry loop against the Bot API.
        let mut backoff = Duration::from_secs(1);
        loop {
            let messages = match poller.poll().await {
                Ok(m) => {
                    backoff = Duration::from_secs(1);
                    m
                }
                Err(e) => {
                    warn!(error = %e, backoff_secs = backoff.as_secs(), "telegram poll failed");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                    continue;
                }
            };

            // Re-read per batch rather than caching: a command file added
            // while the daemon runs should work without a reload, the same way
            // a new manifest does.
            let catalog = crate::slash::discover();

            for msg in messages {
                let actor = msg.user_id.to_string();
                // Recorded before resolution so an unknown name is still
                // attributable — repeated misses from one sender is what
                // probing looks like. The name only; arguments are content.
                if let Some((name, _)) = dotagent_core::command::parse_invocation(&msg.text) {
                    let known = catalog.resolve(&name).is_some();
                    let _ = audit.append(AuditEvent::CommandDispatched {
                        command: name,
                        actor: Some(actor.clone()),
                        known,
                    });
                }

                match screen(&msg, &cfg, &mut limiter, &catalog) {
                    Screened::Reject(reason) => {
                        warn!(user_id = msg.user_id, reason, "telegram message refused");
                        let _ = audit.append(AuditEvent::TriggerRejected {
                            source: TriggerSource::Telegram.to_string(),
                            actor,
                            reason: reason.into(),
                        });
                    }
                    Screened::Answer(text) => {
                        // Costs no model call, which is the point of answering
                        // a catalog question from the catalog.
                        if let Err(e) = dotagent_notify::telegram_inbound::reply(
                            &cfg.bot_token,
                            msg.chat_id,
                            Some(msg.message_id),
                            &text,
                        )
                        .await
                        {
                            warn!(error = %e, "could not answer a catalog question");
                        }
                    }
                    Screened::Run(req) => {
                        let _ = audit.append(AuditEvent::TriggerReceived {
                            source: TriggerSource::Telegram.to_string(),
                            actor,
                            reply_to: msg.chat_id.to_string(),
                        });
                        if tx.send(*req).await.is_err() {
                            info!("trigger channel closed — stopping telegram ingress");
                            return;
                        }
                    }
                }
            }
        }
    })
}

/// Register the `/` menu with Telegram, once per allowlisted chat.
///
/// Called wherever the ingress starts — boot and every SIGHUP — so adding a
/// command file and running `dotagent reload` updates the menu. Best-effort by
/// design: a failed registration costs autocomplete, and every command stays
/// typable and dispatchable without it. Trading a working bot for a menu would
/// be the wrong way round.
///
/// An empty catalog still publishes, clearing the menu. A command you deleted
/// must stop being offered.
async fn publish_command_menu(cfg: &dotagent_core::TelegramIngressConfig) {
    if !dotagent_core::Config::load(dotagent_state::paths::config_file())
        .unwrap_or_default()
        .commands
        .enabled
    {
        return;
    }
    let found = crate::slash::discover();
    for bad in &found.invalid {
        warn!(path = %bad.path.display(), error = %bad.error, "command not registered");
    }
    let menu: Vec<dotagent_notify::telegram_inbound::BotCommand> = found
        .telegram_menu()
        .into_iter()
        // The name is what Telegram already renders, so only the tail goes in
        // the description.
        .map(|(tg, cmd)| dotagent_notify::telegram_inbound::BotCommand::new(tg, cmd.summary()))
        .collect();

    // Per chat, not BotCommandScopeDefault: the default scope would publish
    // every command name and description to anyone who finds the bot.
    for user_id in &cfg.allowed_user_ids {
        match dotagent_notify::telegram_inbound::set_my_commands(&cfg.bot_token, *user_id, &menu)
            .await
        {
            Ok(()) => debug!(
                chat = user_id,
                commands = menu.len(),
                "command menu published"
            ),
            Err(e) => warn!(error = %e, chat = user_id, "could not publish command menu"),
        }
    }
}

/// What screening decided about one inbound message.
enum Screened {
    /// Hand it to the dispatcher.
    Run(Box<TriggerRequest>),
    /// Answer from the daemon without running anything.
    ///
    /// Only ever about the **catalog** — which commands exist — never about
    /// what one means. That line is what keeps "dotagent itself interprets
    /// nothing" true: listing what is installed is the same class of knowledge
    /// as discovering manifests, while resolving a command to its body stays
    /// with the dispatcher, over `command-get`.
    Answer(String),
    Reject(&'static str),
}

#[cfg(test)]
impl Screened {
    fn rejected(self) -> &'static str {
        match self {
            Screened::Reject(r) => r,
            _ => panic!("expected a rejection"),
        }
    }
    fn is_rejected(&self) -> bool {
        matches!(self, Screened::Reject(_))
    }
    fn is_run(&self) -> bool {
        matches!(self, Screened::Run(_))
    }
    fn run(self) -> TriggerRequest {
        match self {
            Screened::Run(r) => *r,
            Screened::Answer(t) => panic!("expected a run, got an answer: {t}"),
            Screened::Reject(r) => panic!("expected a run, got a rejection: {r}"),
        }
    }
    fn answer(self) -> String {
        match self {
            Screened::Answer(t) => t,
            _ => panic!("expected a direct answer"),
        }
    }
}

/// Decide what to do with one inbound message.
///
/// The whole authorization decision lives here, deliberately away from IO: the
/// allowlist is the only thing standing between a bot token and local
/// execution, and it should be testable without a network. The command catalog
/// arrives as an argument for the same reason.
fn screen(
    msg: &dotagent_notify::telegram_inbound::InboundMessage,
    cfg: &dotagent_core::TelegramIngressConfig,
    limiter: &mut dotagent_notify::telegram_inbound::RateLimiter,
    catalog: &crate::slash::CommandDiscovery,
) -> Screened {
    if !cfg.allows(msg.user_id) {
        // Someone found the bot.
        return Screened::Reject("user id not in allowed_user_ids");
    }
    if !limiter.check(msg.user_id) {
        return Screened::Reject("rate limit exceeded");
    }

    // Lexical only: `/name args` is Telegram wire syntax, the same class of
    // thing as reading `update_id`. Nothing here resolves a name to a body.
    let invocation = dotagent_core::command::parse_invocation(&msg.text);
    let command = match &invocation {
        Some((name, args)) => match catalog.resolve(name) {
            Some(found) => Some((found.manifest.name.clone(), args.clone())),
            // A built-in only when nobody installed their own: someone who
            // writes help.md meant to replace this.
            None if name == "help" => return Screened::Answer(help_text(catalog)),
            None => {
                // Falling through to the dispatcher would mean a model
                // improvising an answer to something meant to be exact.
                return Screened::Answer(unknown_command_text(name, catalog));
            }
        },
        None => None,
    };

    Screened::Run(Box::new(TriggerRequest {
        source: TriggerSource::Telegram,
        agent: cfg.dispatcher_agent.clone(),
        schedule: None,
        args: Vec::new(),
        // The body reaches the agent through AGENT_TRIGGER_PAYLOAD, never argv.
        // `message_id` rides along so the reply can quote what it answers.
        payload: Some(serde_json::json!({
            "text": msg.text,
            "chat_id": msg.chat_id,
            "user_id": msg.user_id,
            "message_id": msg.message_id,
            // What the sender was replying to, when they used Telegram's
            // reply. "sim" means nothing without it.
            "reply_to_text": msg.reply_to_text,
            // When that message was a notification dotagent sent, this is the
            // run it came from. Resolved from the id rather than the text:
            // the wording differs per event (one carries `agent/schedule`,
            // another does not) and two agents can fail the same way, so
            // parsing prose would guess wrong eventually.
            "reply_to_run": msg
                .reply_to_message_id
                .and_then(|id| dotagent_state::SentMessageStore::from_home().resolve(id))
                .map(|s| serde_json::json!({
                    "agent": s.agent,
                    "schedule": s.schedule,
                    "event": s.event,
                })),
            // Present only when the sender invoked one. The dispatcher passes
            // both fields straight to `command-get`; it does not have to parse
            // the text or guess whether a leading slash meant anything.
            "command": command.as_ref().map(|(name, args)| serde_json::json!({
                "name": name,
                "args": args,
            })),
        })),
        actor: Some(msg.user_id.to_string()),
        reply_to: Some(msg.chat_id.to_string()),
    }))
}

/// The built-in `/help`: what is installed, nothing about what any of it means.
fn help_text(catalog: &crate::slash::CommandDiscovery) -> String {
    let menu = catalog.telegram_menu();
    if menu.is_empty() {
        return "No commands installed. Just say what you need in plain words.".into();
    }
    let mut out = String::from("Commands:\n");
    for (tg, cmd) in menu {
        out.push_str(&cmd.menu_line(&tg));
        out.push('\n');
    }
    out.push_str("\nOr just say what you need in plain words.");
    out
}

fn unknown_command_text(name: &str, catalog: &crate::slash::CommandDiscovery) -> String {
    match catalog.installed().as_str() {
        "" => format!("No command named /{name}, and none are installed. Try plain words."),
        installed => format!("No command named /{name}. Installed: {installed}. Or /help."),
    }
}

/// Trigger context handed to the agent process.
///
/// `AGENT_TRIGGER_PAYLOAD` carries the source-specific body as JSON. It rides
/// in the environment rather than a file because every current producer is
/// bounded (a Telegram message caps at 4096 characters, far under `ARG_MAX`).
/// A source with unbounded payloads should switch to a file in `AGENT_TMPDIR`
/// rather than grow this variable.
fn trigger_env(req: &TriggerRequest) -> Vec<(String, String)> {
    let mut env = vec![("AGENT_TRIGGER_SOURCE".into(), req.source.to_string())];
    if let Some(actor) = &req.actor {
        env.push(("AGENT_TRIGGER_ACTOR".into(), actor.clone()));
    }
    if let Some(reply_to) = &req.reply_to {
        env.push(("AGENT_TRIGGER_REPLY_TO".into(), reply_to.clone()));
    }
    if let Some(payload) = &req.payload {
        // A payload that won't serialize is a bug in the producer, not a
        // reason to drop the run — the agent sees an absent variable.
        if let Ok(json) = serde_json::to_string(payload) {
            env.push(("AGENT_TRIGGER_PAYLOAD".into(), json));
        }
    }
    env
}

pub async fn run() -> Result<()> {
    let state = StateStore::from_home().context("opening state store")?;
    let audit = AuditLog::from_home().context("opening audit log")?;
    // Singleton supervisor: every plugin invocation (preflight / on_success /
    // on_failure / notify-via-plugin) AND every agent spawn goes through it,
    // so `dotagent status`/`doctor` can see the live subprocess tree and
    // `shutdown` can reap everything on SIGTERM.
    let supervisor = Supervisor::new();
    let _reaper = supervisor.start_reaper(DEFAULT_REAPER_TICK);
    // Periodic snapshot dump so `dotagent status` and `dotagent doctor`
    // (separate processes) can see what the daemon is supervising.
    let _snapshot_writer = supervisor.start_snapshot_writer(
        dotagent_state::paths::supervisor_snapshot_file(),
        Duration::from_secs(2),
    );
    let plugins = PluginClient::from_environment().with_supervisor(supervisor.clone());
    // Live processes for `[lifecycle] mode = "persistent"` agents. Shares the
    // singleton supervisor, so an instance is as visible and as reapable as a
    // one-shot run — which is the whole reason this lives in the orchestrator
    // rather than beside it.
    let pool = PersistentPool::new(supervisor.clone());
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

    // Secrets load happens after the config is in hand because the config
    // can override the secrets path. Failures here never abort the daemon —
    // the file is optional, and a refused (insecure-mode) file should
    // still let the daemon run so the operator can see the doctor warning
    // and fix permissions without losing scheduled runs.
    load_secrets_at_startup(&app_config, &audit);
    ensure_memory_workspace(&app_config);

    // Inbound triggers. Consumed by an arm of the `select!` below rather than
    // a spawned task, so a triggered run is serialized against the tick loop.
    // Concurrency here would race two known hazards that stay dormant while
    // execution is serial: the heartbeat read-modify-write in
    // `dotagent-runner` and the lockfile unlink in `dotagent-state`.
    let (trigger_tx, mut trigger_rx) =
        tokio::sync::mpsc::channel::<TriggerRequest>(TRIGGER_CHANNEL_DEPTH);

    // Inbound chat. Off unless `[telegram]` names both a token and at least
    // one allowed user id — an empty allowlist is misconfiguration, not
    // permission to run anything for anyone.
    let mut telegram = start_telegram_ingress(&app_config.telegram, &trigger_tx, &audit);

    let mut last_summary_date: Option<chrono::NaiveDate> = None;
    let mut last_retention_date: Option<chrono::NaiveDate> = None;
    let exit_reason = loop {
        let cycle_start = Local::now();
        let TickResult { next_event, .. } =
            tick_once(&state, &audit, &plugins, &cache, Some(&pool), cycle_start).await;

        // Collect persistent instances the reaper took while nobody was
        // looking. The reaper kills a process; it does not know the pool
        // exists, and an idle conversation might not send another message for
        // days — so without this the recycle would go unrecorded until then.
        pool.sweep(Some(&audit)).await;

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
                // Re-read secrets so operators can rotate without a full
                // daemon restart (the issue explicitly calls out SIGHUP /
                // restart as the supported refresh mechanism).
                let reloaded = dotagent_core::Config::load(dotagent_state::paths::config_file())
                    .unwrap_or_default();
                load_secrets_at_startup(&reloaded, &audit);
                // Restart the ingress against the new config. Without this a
                // reload would leave the old allowlist live: removing a user
                // id and reloading would look like it took effect while the
                // daemon kept accepting their messages.
                if let Some(handle) = telegram.take() {
                    handle.abort();
                }
                telegram = start_telegram_ingress(&reloaded.telegram, &trigger_tx, &audit);
                // Same reasoning as restarting the ingress: a persistent
                // instance was spawned from the manifest as it read then, and
                // leaving it up would make the reload look applied while the
                // behavior stayed put.
                pool.reload(Some(&audit)).await;
                continue;
            }
            Some(req) = trigger_rx.recv() => {
                handle_trigger(req, &state, &audit, &plugins, &pool).await;
                continue;
            }
            _ = sigterm.recv() => break "SIGTERM",
            _ = sigint.recv()  => break "SIGINT",
        }
    };

    info!(reason = exit_reason, "daemon stopping");
    // Graceful supervisor shutdown — SIGTERM every live subprocess, wait
    // grace, SIGKILL stragglers. Without this, daemon exit would orphan
    // long-running plugin invocations.
    // Retire persistent instances first: `supervisor.shutdown` would reap
    // them either way, but as anonymous subprocesses. Going through the pool
    // writes down which conversation each one was holding.
    pool.shutdown(Some(&audit)).await;
    let pre_shutdown_live = supervisor.snapshot().len();
    if pre_shutdown_live > 0 {
        info!(
            live_subprocesses = pre_shutdown_live,
            "supervisor: reaping live subprocesses before exit"
        );
    }
    supervisor.shutdown(Duration::from_secs(5)).await;
    // Abort the snapshot writer + reaper BEFORE removing the file — without
    // this, the writer's next tick could recreate the file we just deleted,
    // leaving a stale snapshot for the next CLI read.
    drop(_snapshot_writer);
    drop(_reaper);
    let _ = std::fs::remove_file(dotagent_state::paths::supervisor_snapshot_file());
    audit.append(AuditEvent::DaemonStopped {
        reason: exit_reason.into(),
    })?;
    Ok(())
}

/// Load the daemon-level secrets file (default `~/.config/dotagent/secrets.env`,
/// overridable via `[secrets] file = "..."` in `config.toml` or the
/// `DOTAGENT_SECRETS_FILE` env var).
///
/// Audit outcome:
/// - file missing → no event (audit log is already noisy enough).
/// - load ok → `SecretsLoaded { path, key_count, unresolved_references }`
///   (never the values).
/// - refuse (insecure mode / parse / IO error) → `SecretsRefused`. On a
///   SIGHUP reload the previously-installed store is **dropped** before
///   we give up — better to fall through to `std::env` (and fail loud
///   on missing `${VAR}`) than to keep serving a token the operator
///   thought they had rotated. The audit reason records that the
///   previous store was dropped so the chain stays self-explanatory.
///
/// Daemon does NOT abort on refusal — operators should keep seeing scheduled
/// runs and notice the warning via `dotagent doctor`.
fn load_secrets_at_startup(config: &dotagent_core::Config, audit: &AuditLog) {
    let path = resolve_secrets_path(config);
    match dotagent_secrets::SecretsStore::load(&path) {
        Ok(Some(store)) => {
            let key_count = store.len();
            let unresolved_references = store.unresolved_references();
            if unresolved_references > 0 {
                warn!(
                    path = %path.display(),
                    unresolved_references,
                    "some secret references failed to resolve — those keys are unset"
                );
            }
            info!(
                path = %path.display(),
                key_count,
                unresolved_references,
                "loaded secrets file"
            );
            dotagent_secrets::install(store);
            let _ = audit.append(AuditEvent::SecretsLoaded {
                path: path.display().to_string(),
                key_count,
                unresolved_references,
            });
        }
        Ok(None) => {
            // Missing file is the default state on startup — no audit
            // noise. But on a reload, dropping the previously-loaded
            // store IS noteworthy (operator may have deleted the file
            // by mistake).
            if dotagent_secrets::reset_if_present() {
                warn!(path = %path.display(), "secrets file disappeared; previous store dropped");
                let _ = audit.append(AuditEvent::SecretsRefused {
                    path: path.display().to_string(),
                    reason: "file no longer exists; previous store dropped".into(),
                });
            }
        }
        Err(e) => {
            // `e.to_string()` is value-free by construction (see
            // `SecretsError`). Don't change that contract.
            let raw_reason = e.to_string();
            let dropped = dotagent_secrets::reset_if_present();
            let reason = if dropped {
                format!("{raw_reason}; previous store dropped")
            } else {
                raw_reason
            };
            warn!(
                path = %path.display(),
                dropped_previous = dropped,
                %reason,
                "refusing to load secrets file"
            );
            let _ = audit.append(AuditEvent::SecretsRefused {
                path: path.display().to_string(),
                reason,
            });
        }
    }
}

/// `[secrets] file` in `config.toml` wins over the `DOTAGENT_SECRETS_FILE`
/// env var, which wins over the default. Empty string in config falls
/// through to the env-based resolver. Non-absolute config values are
/// ignored with a warning — under launchd / systemd the daemon's
/// working directory is not predictable, so relative paths would
/// resolve to surprising places.
pub(crate) fn resolve_secrets_path(config: &dotagent_core::Config) -> std::path::PathBuf {
    if config.secrets.is_set() {
        let candidate = std::path::PathBuf::from(&config.secrets.file);
        if candidate.is_absolute() {
            return candidate;
        }
        warn!(
            value = %config.secrets.file,
            "ignoring [secrets].file: must be absolute, falling back to default resolver"
        );
    }
    dotagent_state::paths::secrets_file()
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
    pool: Option<&PersistentPool>,
    now: DateTime<Local>,
) -> TickResult {
    let found = discovery::discover();
    // A broken manifest used to abort the whole scan, leaving the daemon with
    // an empty agent list and nothing but a log line to show for it. Now the
    // healthy agents keep running and each failure is auditable.
    for bad in &found.invalid {
        warn!(path = %bad.path.display(), error = %bad.error, "manifest failed to load");
        let _ = audit.append(AuditEvent::ManifestInvalid {
            path: bad.path.display().to_string(),
            error: bad.error.clone(),
        });
    }
    let agents = found.agents;

    if let Err(e) = check_cache(&agents, cache, audit) {
        warn!(error = ?e, "manifest cache check failed");
    }

    let _ = audit.append(AuditEvent::TickStarted {
        agents_scanned: agents.len() as u32,
    });

    let runs_dispatched = dispatch_due_runs(&agents, state, audit, plugins, pool, now).await;
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
    pool: Option<&PersistentPool>,
    now: DateTime<Local>,
) -> u32 {
    let mut dispatched = 0u32;
    for agent in agents {
        if !agent.manifest.agent.monitor {
            continue;
        }
        for sched in &agent.manifest.schedules {
            if dispatch_one(agent, sched, state, audit, plugins, pool, now).await {
                dispatched += 1;
            }
        }
    }
    dispatched
}

/// Returns `true` if a run was dispatched (regardless of outcome).
#[allow(clippy::too_many_arguments)]
async fn dispatch_one(
    agent: &DiscoveredAgent,
    sched: &Schedule,
    state: &StateStore,
    audit: &AuditLog,
    plugins: &PluginClient,
    pool: Option<&PersistentPool>,
    now: DateTime<Local>,
) -> bool {
    let dctx = DaemonCtx {
        state,
        audit,
        plugins,
    };
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
        give_up(agent, sched, &mut window, &dctx, &slug, expected).await;
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
        slug_override: None,
        extra_env: &[],
    };
    let ctx = RunContext {
        state,
        plugins: Some(plugins),
        audit: Some(audit),
        supervisor: Some(plugins.supervisor()),
        persistent: pool,
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
                give_up(agent, sched, &mut window, &dctx, &slug, expected).await;
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
    ctx: &DaemonCtx<'_>,
    slug: &str,
    expected: DateTime<Local>,
) {
    let (state, audit, plugins) = (ctx.state, ctx.audit, ctx.plugins);
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

    // Use the caller's store, not `StateStore::from_home()`. Re-deriving the
    // root here silently ignored an injected one, so a daemon (or a test)
    // running against a non-default `DOTAGENT_HOME` wrote the give-up marker
    // somewhere the rest of the run never looks.
    if let Err(e) = state.write_window(window, slug, expected) {
        warn!(error = %e, "writing given_up window state failed");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn req(source: TriggerSource) -> TriggerRequest {
        TriggerRequest {
            source,
            agent: "a".into(),
            schedule: None,
            args: vec![],
            payload: None,
            actor: None,
            reply_to: None,
        }
    }

    fn get<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn trigger_env_always_carries_the_source() {
        let env = trigger_env(&req(TriggerSource::Telegram));
        assert_eq!(get(&env, "AGENT_TRIGGER_SOURCE"), Some("telegram"));
    }

    #[test]
    fn trigger_env_omits_absent_optional_fields() {
        // An agent should see the variable missing rather than empty, so
        // `${VAR:-default}` in a shell script behaves predictably.
        let env = trigger_env(&req(TriggerSource::Cli));
        assert!(get(&env, "AGENT_TRIGGER_ACTOR").is_none());
        assert!(get(&env, "AGENT_TRIGGER_REPLY_TO").is_none());
        assert!(get(&env, "AGENT_TRIGGER_PAYLOAD").is_none());
    }

    #[test]
    fn trigger_env_carries_actor_and_reply_to() {
        let mut r = req(TriggerSource::Telegram);
        r.actor = Some("123".into());
        r.reply_to = Some("456".into());
        let env = trigger_env(&r);
        assert_eq!(get(&env, "AGENT_TRIGGER_ACTOR"), Some("123"));
        assert_eq!(get(&env, "AGENT_TRIGGER_REPLY_TO"), Some("456"));
    }

    #[test]
    fn trigger_env_serializes_payload_as_json() {
        let mut r = req(TriggerSource::Telegram);
        r.payload = Some(serde_json::json!({"text": "hi", "chat_id": 7}));
        let env = trigger_env(&r);
        let raw = get(&env, "AGENT_TRIGGER_PAYLOAD").expect("payload must be present");
        let parsed: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed["text"], "hi");
        assert_eq!(parsed["chat_id"], 7);
    }

    #[test]
    fn trigger_env_keeps_message_text_out_of_argv() {
        // The body must only ever reach the agent through the environment.
        // Shell metacharacters in a chat message are then just bytes.
        let mut r = req(TriggerSource::Telegram);
        r.payload = Some(serde_json::json!({"text": "; rm -rf / #"}));
        let env = trigger_env(&r);
        assert!(r.args.is_empty(), "args must stay empty");
        let raw = get(&env, "AGENT_TRIGGER_PAYLOAD").unwrap();
        assert!(raw.contains("rm -rf"), "payload carries it verbatim: {raw}");
    }

    #[test]
    fn trigger_env_never_emits_agent_reserved_keys() {
        // trigger_env only produces AGENT_TRIGGER_* names. The runner also
        // defends by ordering, but producing a reserved key here would be
        // a bug worth catching at the source.
        let mut r = req(TriggerSource::Telegram);
        r.actor = Some("1".into());
        r.reply_to = Some("2".into());
        r.payload = Some(serde_json::json!({}));
        for (k, _) in trigger_env(&r) {
            assert!(
                k.starts_with("AGENT_TRIGGER_"),
                "unexpected key produced: {k}"
            );
        }
    }

    fn inbound(user_id: i64) -> dotagent_notify::telegram_inbound::InboundMessage {
        dotagent_notify::telegram_inbound::InboundMessage {
            update_id: 1,
            user_id,
            chat_id: 999,
            message_id: 55,
            text: "how's disk?".into(),
            reply_to_text: None,
            reply_to_message_id: None,
        }
    }

    fn cfg(allowed: Vec<i64>) -> dotagent_core::TelegramIngressConfig {
        dotagent_core::TelegramIngressConfig {
            bot_token: "t".into(),
            allowed_user_ids: allowed,
            ..Default::default()
        }
    }

    /// No commands installed — the state every allowlist test wants, since a
    /// catalog would only change what a message starting with `/` does.
    fn empty_catalog() -> crate::slash::CommandDiscovery {
        crate::slash::CommandDiscovery::default()
    }

    fn catalog_with(files: &[(&str, &str)]) -> (tempfile::TempDir, crate::slash::CommandDiscovery) {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            std::fs::write(dir.path().join(name), body).unwrap();
        }
        let found = crate::slash::discover_in(&[dir.path().to_path_buf()]);
        (dir, found)
    }

    fn limiter() -> dotagent_notify::telegram_inbound::RateLimiter {
        dotagent_notify::telegram_inbound::RateLimiter::new(10)
    }

    // --- screen(): the allowlist is the only thing between a leaked bot
    // token and local execution. Deny cases dominate on purpose. ---

    #[test]
    fn screen_refuses_an_unlisted_user() {
        let err = screen(
            &inbound(7),
            &cfg(vec![1, 2]),
            &mut limiter(),
            &empty_catalog(),
        )
        .rejected();
        assert_eq!(err, "user id not in allowed_user_ids");
    }

    #[test]
    fn screen_refuses_everyone_when_the_allowlist_is_empty() {
        // Empty must mean nobody. Reading it as "no restriction" would turn a
        // forgotten config line into an open execution endpoint.
        assert!(screen(&inbound(7), &cfg(vec![]), &mut limiter(), &empty_catalog()).is_rejected());
        assert!(screen(&inbound(0), &cfg(vec![]), &mut limiter(), &empty_catalog()).is_rejected());
    }

    #[test]
    fn screen_refuses_a_negated_id() {
        assert!(screen(
            &inbound(-7),
            &cfg(vec![7]),
            &mut limiter(),
            &empty_catalog()
        )
        .is_rejected());
    }

    #[test]
    fn screen_refuses_past_the_rate_limit() {
        let c = cfg(vec![7]);
        let mut rl = dotagent_notify::telegram_inbound::RateLimiter::new(2);
        assert!(screen(&inbound(7), &c, &mut rl, &empty_catalog()).is_run());
        assert!(screen(&inbound(7), &c, &mut rl, &empty_catalog()).is_run());
        let err = screen(&inbound(7), &c, &mut rl, &empty_catalog()).rejected();
        assert_eq!(err, "rate limit exceeded");
    }

    #[test]
    fn screen_checks_the_allowlist_before_spending_rate_budget() {
        // Otherwise an unlisted flooder could exhaust a listed user's quota.
        let c = cfg(vec![7]);
        let mut rl = dotagent_notify::telegram_inbound::RateLimiter::new(1);
        assert!(screen(&inbound(99), &c, &mut rl, &empty_catalog()).is_rejected());
        assert!(
            screen(&inbound(7), &c, &mut rl, &empty_catalog()).is_run(),
            "the listed user's budget must be untouched"
        );
    }

    #[test]
    fn screen_accepts_a_listed_user_and_targets_the_dispatcher() {
        let req = screen(&inbound(7), &cfg(vec![7]), &mut limiter(), &empty_catalog()).run();
        assert_eq!(req.agent, "telegram-assistant");
        assert_eq!(req.source, TriggerSource::Telegram);
        assert_eq!(req.actor.as_deref(), Some("7"));
        assert_eq!(req.reply_to.as_deref(), Some("999"));
    }

    #[test]
    fn screen_never_puts_the_message_body_in_argv() {
        let req = screen(&inbound(7), &cfg(vec![7]), &mut limiter(), &empty_catalog()).run();
        assert!(req.args.is_empty(), "body must travel in the payload only");
        assert_eq!(req.payload.unwrap()["text"], "how's disk?");
    }

    #[test]
    fn screen_ignores_the_message_when_choosing_the_agent() {
        // The message selects nothing: the dispatcher is operator config.
        let mut msg = inbound(7);
        msg.text = "run disk-alert; rm -rf /".into();
        let req = screen(&msg, &cfg(vec![7]), &mut limiter(), &empty_catalog()).run();
        assert_eq!(req.agent, "telegram-assistant");
    }

    // --- commands. The daemon parses `/name`, publishes the catalog and
    // answers about it — and resolves nothing beyond that. ---

    const CMD: &str = "---\ndescription: Writes a commit message.\n---\nThe prompt.\n";

    fn said(text: &str, catalog: &crate::slash::CommandDiscovery) -> Screened {
        let mut msg = inbound(7);
        msg.text = text.into();
        screen(&msg, &cfg(vec![7]), &mut limiter(), catalog)
    }

    #[test]
    fn plain_prose_carries_no_command() {
        let payload = said("how's disk?", &empty_catalog()).run().payload.unwrap();
        assert_eq!(payload["text"], "how's disk?");
        assert!(
            payload["command"].is_null(),
            "prose must not look like an invocation"
        );
    }

    #[test]
    fn an_invocation_reaches_the_dispatcher_as_a_resolved_name_and_raw_args() {
        // The sender types the Telegram spelling; the payload carries the
        // catalog name, so the dispatcher passes it to command-get unchanged.
        let (_dir, catalog) = catalog_with(&[("commit-message.md", CMD)]);
        let payload = said("/commit_message --staged src/", &catalog)
            .run()
            .payload
            .unwrap();
        assert_eq!(payload["command"]["name"], "commit-message");
        assert_eq!(payload["command"]["args"], "--staged src/");
        // The original text survives alongside it — a dispatcher that would
        // rather read the raw message still can.
        assert_eq!(payload["text"], "/commit_message --staged src/");
    }

    #[test]
    fn an_invocation_without_arguments_carries_an_empty_string() {
        let (_dir, catalog) = catalog_with(&[("commit-message.md", CMD)]);
        let payload = said("/commit_message", &catalog).run().payload.unwrap();
        assert_eq!(payload["command"]["args"], "");
    }

    #[test]
    fn an_unknown_command_is_answered_rather_than_improvised() {
        // The whole point of exactness: `/typo` must not reach a model that
        // would answer something plausible.
        let (_dir, catalog) = catalog_with(&[("commit-message.md", CMD)]);
        let answer = said("/typo", &catalog).answer();
        assert!(answer.contains("/typo"), "{answer}");
        assert!(answer.contains("/commit_message"), "{answer}");
    }

    #[test]
    fn help_lists_the_catalog_without_running_anything() {
        let (_dir, catalog) = catalog_with(&[("commit-message.md", CMD)]);
        let answer = said("/help", &catalog).answer();
        assert!(answer.contains("/commit_message"), "{answer}");
        assert!(answer.contains("Writes a commit message."), "{answer}");
    }

    #[test]
    fn help_is_still_useful_with_an_empty_catalog() {
        let answer = said("/help", &empty_catalog()).answer();
        assert!(answer.contains("No commands installed"), "{answer}");
    }

    #[test]
    fn an_installed_help_command_beats_the_builtin() {
        // Writing help.md is an explicit choice to replace the default.
        let (_dir, catalog) = catalog_with(&[("help.md", CMD)]);
        let payload = said("/help", &catalog).run().payload.unwrap();
        assert_eq!(payload["command"]["name"], "help");
    }

    #[test]
    fn the_allowlist_is_checked_before_any_command_is_answered() {
        // Otherwise an unlisted sender could enumerate the catalog with /help
        // — a menu that leaks is exactly what the per-chat scope avoids.
        let (_dir, catalog) = catalog_with(&[("commit-message.md", CMD)]);
        let mut msg = inbound(99);
        msg.text = "/help".into();
        assert!(screen(&msg, &cfg(vec![7]), &mut limiter(), &catalog).is_rejected());
    }

    #[test]
    fn an_unknown_command_still_spends_rate_budget() {
        // Answering is cheap but not free, and a sender looping `/typo` should
        // hit the same wall as one looping prose.
        let (_dir, catalog) = catalog_with(&[("commit-message.md", CMD)]);
        let c = cfg(vec![7]);
        let mut rl = dotagent_notify::telegram_inbound::RateLimiter::new(1);
        let mut msg = inbound(7);
        msg.text = "/typo".into();
        assert!(matches!(
            screen(&msg, &c, &mut rl, &catalog),
            Screened::Answer(_)
        ));
        assert_eq!(
            screen(&msg, &c, &mut rl, &catalog).rejected(),
            "rate limit exceeded"
        );
    }

    #[test]
    fn slug_is_namespaced_per_source() {
        assert_eq!(req(TriggerSource::Telegram).slug(), "trigger-telegram");
        assert_eq!(req(TriggerSource::Mcp).slug(), "trigger-mcp");
    }
}
