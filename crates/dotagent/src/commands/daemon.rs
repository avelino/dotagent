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
    compute_next_event, expected_at, health_state, is_stale, should_retry, window_key,
    AgentSchedulePair, HealthState, ResolvedPolicy,
};
use dotagent_state::{
    audit::AuditLog,
    manifest_cache::{hash_manifest_file, KnownManifest, ManifestCache},
    notify_dedup::{alert_key, AlertEpisode, NotifyDedupStore},
    slug_from_args, StateStore,
};
use dotagent_supervisor::{Supervisor, SNAPSHOT_TICK};
use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, info, warn};

use crate::discovery::{self, DiscoveredAgent};
use crate::power::PowerGate;

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

/// Drain the trigger channel from a task of its own, so an inbound message is
/// answered on its own clock rather than the scheduler's.
///
/// The bug this exists for: [`run`]'s `select!` is only reached *after* a tick
/// returns, and a tick awaits every scheduled run inline. One 20-minute agent
/// therefore held every queued chat message for 20 minutes — the sender saw
/// silence, resent, and then got both answers at once when the run ended.
///
/// Deliberately **one** worker, not one task per message:
///
/// - **Ordering.** Messages from one conversation must be answered in the
///   order they were sent. A task per message would hand that ordering to the
///   scheduler and to whichever task happened to reach the persistent pool's
///   per-key mutex first — non-deterministic, and wrong exactly when a
///   conversation is going fast.
/// - **Concurrency ceiling.** A burst of N messages stays N *queued* requests
///   rather than N concurrent agent processes. The ingress rate limit bounds
///   arrivals; this bounds execution, which is the expensive half.
///
/// The price is that it serializes conversations that never needed it: with two
/// chats live and a 20-minute run, the second chat waits out the first. The
/// alternative is a worker per conversation — `HashMap<key, Sender>`, each FIFO,
/// same per-conversation ordering, ceiling equal to the number of live chats.
/// Not taken yet, for three reasons:
///
/// - The blocking that was actually observed was a **scheduled** run holding a
///   chat, and that one is fixed where it belonged: the persistent pool keys
///   scheduled and triggered runs apart, so they no longer queue behind one
///   mutex. Chat-against-chat blocking needs two people talking at once.
/// - "Conversation" would have to mean the same thing here and in
///   `[lifecycle] key`. If it does not, two workers land on one instance mutex
///   and ordering becomes whichever task got there first — the guarantee this
///   design exists to keep, lost in exchange for the concurrency.
/// - N workers is N concurrent agent processes for a `oneshot` agent, and N
///   handles to watch instead of one. The ceiling is not `allowed_user_ids`:
///   that list is who may speak, not how many may run at once.
///
/// Worth revisiting the moment a second concurrent conversation is real. The
/// change is contained — the channel becomes a map keyed by
/// `(source, reply_to)` — and by then `[lifecycle] key` says what a
/// conversation is.
///
/// What it does introduce is exactly one new concurrent pair — a triggered run
/// beside a scheduled one — and that pair is safe on every shared resource
/// they touch:
///
/// - **Heartbeat.** A triggered run's slug is namespaced by source
///   (`trigger-telegram`), so it never shares a file with the scheduled
///   history of the same agent. `dotagent-state` takes a per-file `flock`
///   regardless.
/// - **Window state.** Triggered runs never write it (see [`run_triggered`]),
///   so retry accounting stays single-writer.
/// - **Audit log.** Every append takes an exclusive `flock` on the log.
/// - **Persistent pool.** Slots are `(agent, key)`-keyed mutexes, so a
///   scheduled and a triggered request for the same instance queue instead of
///   interleaving.
///
/// The handler is a parameter so tests can drive this loop's real shape
/// without a filesystem, a manifest, or a subprocess.
///
/// The returned handle is **not** optional bookkeeping: [`wait_for_event`]
/// selects on it, so this task ending — returning or panicking — stops the
/// daemon instead of leaving a process that ticks forever and answers nobody.
fn spawn_trigger_worker<H, F>(
    mut rx: tokio::sync::mpsc::Receiver<TriggerRequest>,
    handler: H,
) -> tokio::task::JoinHandle<()>
where
    H: Fn(TriggerRequest) -> F + Send + 'static,
    F: std::future::Future<Output = ()> + Send,
{
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            handler(req).await;
        }
        // Only reachable once every sender is gone, which for a live daemon
        // means the loop itself dropped them — worth a line at a level someone
        // reads, because from here on no trigger will ever be answered.
        warn!("trigger worker stopped — channel closed");
    })
}

/// What ended a sleep cycle.
#[derive(Debug, PartialEq, Eq)]
enum Wake {
    /// The sleep elapsed. Run the next tick.
    Tick,
    /// SIGHUP: re-read config and restart what it configures.
    Reload,
    /// Stop the daemon, with the reason the audit log records.
    Stop(&'static str),
}

/// The three signals the daemon reacts to, registered once.
struct Signals {
    hangup: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}

impl Signals {
    fn register() -> Result<Self> {
        Ok(Self {
            hangup: signal(SignalKind::hangup()).context("registering SIGHUP")?,
            terminate: signal(SignalKind::terminate()).context("registering SIGTERM")?,
            interrupt: signal(SignalKind::interrupt()).context("registering SIGINT")?,
        })
    }
}

/// Sleep until something asks the loop to do its next thing.
///
/// The trigger worker is one of those things. It used to be spawned and then
/// only ever `abort()`ed, which meant its death was invisible: the task ends,
/// its receiver drops, the channel closes, and the ingress logs one line and
/// returns. The bot goes permanently deaf while the loop keeps ticking and
/// `dotagent status` keeps saying the daemon is fine.
///
/// Before the worker existed, a trigger ran inline in this `select!`, so a
/// panic unwound the daemon and launchd restarted it — loud, and self-healing.
/// Selecting on the handle restores exactly that: the daemon exits through its
/// normal shutdown path and comes back with a worker that works.
///
/// Split out of [`run`] so it is testable — the property is "a dead worker
/// stops the loop", which cannot be asserted on a `select!` buried in a
/// function that needs a state store, a supervisor and a signal disposition.
async fn wait_for_event(
    sleep_for: Duration,
    signals: &mut Signals,
    trigger_worker: &mut tokio::task::JoinHandle<()>,
) -> Wake {
    tokio::select! {
        _ = tokio::time::sleep(sleep_for) => Wake::Tick,
        _ = signals.hangup.recv() => Wake::Reload,
        _ = signals.terminate.recv() => Wake::Stop("SIGTERM"),
        _ = signals.interrupt.recv() => Wake::Stop("SIGINT"),
        outcome = &mut *trigger_worker => {
            match outcome {
                Ok(()) => warn!("trigger worker returned — no trigger can be answered anymore"),
                Err(e) if e.is_panic() => {
                    warn!(error = %e, "trigger worker panicked — no trigger can be answered anymore")
                }
                Err(e) => warn!(error = %e, "trigger worker ended unexpectedly"),
            }
            Wake::Stop("trigger worker died")
        }
    }
}

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

/// Retire every persistent instance for a reload, off the loop's clock.
///
/// `PersistentPool::reload` drains the pool by taking each slot's mutex, and a
/// slot held by an in-flight request stays held for that request's whole
/// deadline — up to `agent.timeout_seconds`. Awaiting that inline is a scheduler
/// that stops ticking, dispatching and summarizing for as long as the busiest
/// agent takes; a 1200-second run turned `dotagent reload` into a 20-minute
/// freeze. The shutdown path avoids this by aborting the trigger worker first;
/// a reload has no equivalent, because the worker keeps running.
///
/// Detached rather than time-boxed: a `timeout` here would drop the drain
/// mid-way, and the instances it had already pulled out of the map would be
/// terminated by their `Drop` with nothing written down about why. Letting it
/// finish on its own task keeps every recycle audited. Overlapping reloads are
/// safe — `drain` empties the map under its lock, so a second one finds only
/// what the first had not claimed yet.
fn spawn_pool_reload(
    pool: &std::sync::Arc<PersistentPool>,
    audit: &AuditLog,
) -> tokio::task::JoinHandle<()> {
    let (pool, audit) = (pool.clone(), audit.clone());
    tokio::spawn(async move {
        pool.reload(Some(&audit)).await;
        debug!("persistent instances retired for reload");
    })
}

pub async fn run() -> Result<()> {
    let state = StateStore::from_home().context("opening state store")?;
    let audit = AuditLog::from_home().context("opening audit log")?;
    // Claim the pidfile first. `reap_boot_orphans` decides what it may signal
    // by asking whether another daemon is alive, and that answer is read from
    // the pidfile — so publishing ours after the reap leaves a window (the
    // whole scan, including its kill grace) in which a second daemon starting
    // up sees nobody home and reaps the same snapshot we are reaping.
    write_pidfile()?;
    let _pid_guard = PidGuard;
    // Before anything of ours exists: collect what a previous daemon left
    // behind. Must run before `start_snapshot_writer`, whose first tick would
    // otherwise overwrite the only record of those processes.
    reap_boot_orphans(&audit).await;
    // Singleton supervisor: every plugin invocation (preflight / on_success /
    // on_failure / notify-via-plugin) AND every agent spawn goes through it,
    // so `dotagent status`/`doctor` can see the live subprocess tree and
    // `shutdown` can reap everything on SIGTERM.
    let supervisor = Supervisor::new();
    let _reaper = supervisor.start_reaper();
    // Snapshot dump so `dotagent status` and `dotagent doctor` (separate
    // processes) can see what the daemon is supervising. Both background tasks
    // park while nothing is supervised, so an idle daemon holds no timers.
    let _snapshot_writer = supervisor.start_snapshot_writer(
        dotagent_state::paths::supervisor_snapshot_file(),
        SNAPSHOT_TICK,
    );
    let plugins = PluginClient::from_environment().with_supervisor(supervisor.clone());
    // Live processes for `[lifecycle] mode = "persistent"` agents. Shares the
    // singleton supervisor, so an instance is as visible and as reapable as a
    // one-shot run — which is the whole reason this lives in the orchestrator
    // rather than beside it.
    //
    // `Arc` because the trigger worker runs in its own task (see
    // `spawn_trigger_worker`) and needs an owned handle; the pool is
    // internally synchronized, so sharing it is what keeps a chat message and
    // a scheduled run from spawning two instances for the same key.
    let pool = std::sync::Arc::new(PersistentPool::new(supervisor.clone()));
    let cache = ManifestCache::from_home().context("opening manifest cache")?;

    audit
        .append(AuditEvent::DaemonStarted {
            version: env!("CARGO_PKG_VERSION").into(),
            pid: std::process::id(),
        })
        .with_context(|| format!("appending daemon_started to {}", audit.path().display()))?;

    // Verify the existing chain at startup; emit `AuditChainBroken` (which
    // itself becomes a chained entry) if tampered.
    //
    // The `Err` arm is not the same as a clean chain: it means the log could
    // not be read at all, and refusing to say so would turn "I could not
    // check" into "I checked and it is fine" — the exact substitution this
    // release exists to stop making.
    match audit.verify_chain() {
        Ok(Some(brk)) => {
            warn!(position = brk.position, "audit chain broken");
            let _ = audit.append(AuditEvent::AuditChainBroken {
                position: brk.position,
                expected_prev_hash: brk.expected,
                actual_prev_hash: brk.actual,
            });
        }
        Ok(None) => {}
        Err(e) => warn!(
            error = %e,
            path = %audit.path().display(),
            "could not verify the audit chain"
        ),
    }

    let mut signals = Signals::register()?;

    info!("daemon started");
    let mut app_config =
        dotagent_core::Config::load(dotagent_state::paths::config_file()).unwrap_or_default();

    // Secrets load happens after the config is in hand because the config
    // can override the secrets path. Failures here never abort the daemon —
    // the file is optional, and a refused (insecure-mode) file should
    // still let the daemon run so the operator can see the doctor warning
    // and fix permissions without losing scheduled runs.
    load_secrets_at_startup(&app_config, &audit);
    ensure_memory_workspace(&app_config);

    // Inbound triggers. Drained by a worker task rather than by an arm of the
    // `select!` below: that arm is only polled between ticks, so a long
    // scheduled run used to hold every queued message for its whole duration.
    // See `spawn_trigger_worker` for why there is exactly one worker and why
    // running it beside the tick is safe.
    let (trigger_tx, trigger_rx) =
        tokio::sync::mpsc::channel::<TriggerRequest>(TRIGGER_CHANNEL_DEPTH);
    let mut trigger_worker = {
        let state = state.clone();
        let audit = audit.clone();
        let plugins = plugins.clone();
        let pool = pool.clone();
        spawn_trigger_worker(trigger_rx, move |req| {
            let state = state.clone();
            let audit = audit.clone();
            let plugins = plugins.clone();
            let pool = pool.clone();
            async move { handle_trigger(req, &state, &audit, &plugins, &pool).await }
        })
    };

    // Inbound chat. Off unless `[telegram]` names both a token and at least
    // one allowed user id — an empty allowlist is misconfiguration, not
    // permission to run anything for anyone.
    let mut telegram = start_telegram_ingress(&app_config.telegram, &trigger_tx, &audit);

    let mut last_summary_date: Option<chrono::NaiveDate> = None;
    let mut last_retention_date: Option<chrono::NaiveDate> = None;
    let exit_reason = loop {
        let cycle_start = Local::now();
        let TickResult { next_event, .. } = tick_once(
            &state,
            &audit,
            &plugins,
            &cache,
            Some(&pool),
            &app_config.power,
            cycle_start,
        )
        .await;

        // Collect persistent instances the reaper took while nobody was
        // looking. The reaper kills a process; it does not know the pool
        // exists, and an idle conversation might not send another message for
        // days — so without this the recycle would go unrecorded until then.
        pool.sweep(Some(&audit)).await;

        // Daily summary at `[daily_summary].time` (default 22:45 local).
        // Fires once per window; the `last_summary_date` guard avoids a
        // double-fire when the daemon re-enters it (e.g. `dotagent reload`).
        // The date recorded is the window's, not today's — they differ when
        // `grace_minutes` crosses midnight.
        if let Some(date) =
            should_run_daily_summary(cycle_start, last_summary_date, &app_config.daily_summary)
        {
            if let Err(e) =
                crate::commands::daily_summary::run_with(&app_config.daily_summary, false).await
            {
                warn!(error = %e, "daily-summary delivery failed");
            }
            // Recorded even when delivery failed: a broken notifier would
            // otherwise re-fire on every tick for the whole grace window.
            last_summary_date = Some(date);
        }

        // Retention sweep: runs once per day at 03:00 (chosen so it
        // happens during natural quiet hours and never fights with the
        // 22:45 summary). Covers logs and per-window state alike — windows
        // outlive their usefulness the same way logs do, and left alone they
        // accumulate one file per (agent, slug, window) forever.
        if should_run_retention(cycle_start, last_retention_date) {
            let stats = dotagent_telemetry::retention::sweep_all_with(
                &app_config.logging,
                &app_config.state,
            );
            info!(
                compressed = stats.compressed,
                deleted = stats.deleted,
                scanned = stats.scanned,
                windows_scanned = stats.windows_scanned,
                windows_deleted = stats.windows_deleted,
                "retention sweep completed"
            );
            last_retention_date = Some(cycle_start.date_naive());
        }

        let sleep_target = compute_sleep_target(
            cycle_start,
            next_event,
            next_summary_at(cycle_start, &app_config.daily_summary),
            next_retention_at(cycle_start),
        );
        let sleep_for = (sleep_target - Local::now())
            .to_std()
            .unwrap_or(Duration::from_secs(60));
        info!(
            "sleeping until {} ({}s)",
            sleep_target.format("%Y-%m-%dT%H:%M:%S%z"),
            sleep_for.as_secs()
        );

        match wait_for_event(sleep_for, &mut signals, &mut trigger_worker).await {
            Wake::Tick => continue,
            Wake::Stop(reason) => break reason,
            Wake::Reload => {
                info!("SIGHUP — reloading on next tick");
                let _ = audit.append(AuditEvent::ConfigReloaded {
                    reason: "SIGHUP".into(),
                });
                // Re-read secrets so operators can rotate without a full
                // daemon restart (the issue explicitly calls out SIGHUP /
                // restart as the supported refresh mechanism).
                let reloaded = dotagent_core::Config::load(dotagent_state::paths::config_file())
                    .unwrap_or_default();
                load_secrets_at_startup(&reloaded, &audit);
                // Config can turn memory on, or move the workspace. Left out,
                // this was the last thing still pinned to whatever boot read.
                ensure_memory_workspace(&reloaded);
                // Restart the ingress against the new config. Without this a
                // reload would leave the old allowlist live: removing a user
                // id and reloading would look like it took effect while the
                // daemon kept accepting their messages.
                if let Some(handle) = telegram.take() {
                    handle.abort();
                }
                telegram = start_telegram_ingress(&reloaded.telegram, &trigger_tx, &audit);
                // Same reasoning once more, for everything the loop itself
                // reads: retention thresholds and the daily-summary time and
                // destination were pinned at boot, so editing config.toml and
                // reloading looked applied while the daemon kept the old
                // values until a full restart.
                app_config = reloaded;
                // Same reasoning as restarting the ingress: a persistent
                // instance was spawned from the manifest as it read then, and
                // leaving it up would make the reload look applied while the
                // behavior stayed put.
                spawn_pool_reload(&pool, &audit);
                continue;
            }
        }
    };

    info!(reason = exit_reason, "daemon stopping");
    // Stop accepting new work first. A trigger already in flight loses its
    // reply, which is the honest outcome — the run itself is a supervised
    // subprocess and gets reaped below like everything else.
    trigger_worker.abort();
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

/// The pid of another daemon that looks alive right now, if there is one.
///
/// `None` covers all the safe readings: no pidfile, an unparseable one, our
/// own pid, or a pid nothing answers to. Only a *live, different* daemon is
/// reason to keep our hands off shared state.
fn live_peer_daemon() -> Option<u32> {
    let path = pidfile_path()?;
    let pid: u32 = std::fs::read_to_string(&path).ok()?.trim().parse().ok()?;
    if pid == std::process::id() {
        return None;
    }
    dotagent_supervisor::orphan::process_exists(pid).then_some(pid)
}

/// Collect agent and plugin processes a previous daemon left running.
///
/// A daemon that exits through its shutdown path reaps its children and
/// deletes the snapshot. A daemon that is `SIGKILL`ed, panics, or is replaced
/// by `launchctl kickstart -k` does neither: its children survive, and the
/// deadline nobody is holding anymore never fires. Observed in production as a
/// persistent agent alive for 26 minutes against a 600-second deadline.
///
/// The snapshot's mere existence at boot is the signal that the previous exit
/// was not clean. Everything after that is proof of identity — pids are
/// recycled, and a stale record is not a licence to signal whatever now holds
/// the number. `dotagent_supervisor::orphan` refuses on any doubt.
async fn reap_boot_orphans(audit: &AuditLog) {
    let snapshot = dotagent_state::paths::supervisor_snapshot_file();
    if !snapshot.exists() {
        return;
    }
    // Two daemons at once is a misconfiguration, not a reason to kill the
    // other one's children out from under it.
    if let Some(pid) = live_peer_daemon() {
        warn!(
            peer_pid = pid,
            "another daemon looks alive — skipping boot orphan reap"
        );
        return;
    }

    let report = dotagent_supervisor::orphan::OrphanReaper::new()
        .reap_snapshot_file(&snapshot)
        .await;

    if let Some(err) = &report.snapshot_error {
        // Nothing was signalled, and the file is the only evidence of what was
        // running — but leaving it in place would not preserve it: the snapshot
        // writer starts two seconds later and overwrites it. Move it aside so
        // it survives long enough to be read.
        let kept = std::path::PathBuf::from(format!("{}.corrupt", snapshot.display()));
        let preserved = std::fs::rename(&snapshot, &kept).is_ok();
        warn!(
            path = %snapshot.display(),
            error = %err,
            kept = %kept.display(),
            preserved,
            "supervisor snapshot is unreadable — no orphan was reaped"
        );
        return;
    }

    for reaped in &report.reaped {
        let _ = audit.append(AuditEvent::OrphanReaped {
            agent: reaped.info.owner.agent.clone(),
            kind: reaped.info.kind.to_string(),
            label: reaped.info.label.clone(),
            pid: reaped.info.pid,
            age_seconds: reaped.info.age_seconds,
            deadline_seconds: reaped.info.deadline_seconds,
        });
        warn!(
            agent = %reaped.info.owner.agent,
            kind = %reaped.info.kind,
            label = %reaped.info.label,
            pid = reaped.info.pid,
            pgid = reaped.pgid,
            age_seconds = reaped.info.age_seconds,
            deadline_seconds = reaped.info.deadline_seconds,
            "reaped an orphan left by a previous daemon"
        );
    }
    for (info, reason) in &report.skipped {
        debug!(
            agent = %info.owner.agent,
            pid = info.pid,
            reason = %reason,
            "left a recorded process alone"
        );
    }
    if !report.reaped.is_empty() {
        info!(
            reaped = report.reaped.len(),
            skipped = report.skipped.len(),
            "boot orphan reap completed"
        );
    }
    // The record has been acted on; the snapshot writer starts fresh.
    let _ = std::fs::remove_file(&snapshot);
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
    power_config: &dotagent_core::PowerConfig,
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

    // Ticks are telemetry, not an audit trail. "Woke up and looked at 17
    // agents" is not a security event, and writing it made the hash-chained
    // log 64% scheduler heartbeat — every append re-reads the whole file to
    // find the tail hash, so the noise also made every real event more
    // expensive to record. `tracing` already captures this, and already
    // rotates. See `docs/guides/observability.md`.
    debug!(agents_scanned = agents.len(), "tick started");

    // Resolved here, not in the run loop, because it needs the manifests: a
    // gate built from `[power]` alone cannot see a schedule's own
    // `on_battery` and would silently ignore every per-schedule override.
    let power = PowerGate::detect(
        power_config,
        agents.iter().flat_map(|a| a.manifest.schedules.iter()),
    );

    let runs_dispatched = dispatch_due_runs(&agents, state, audit, plugins, pool, power, now).await;

    // Alerting on conditions the run loop cannot see: an agent that stopped
    // being scheduled never reaches `dispatch_one`, so it never fires anything.
    // A power-deferred schedule is intentionally not eligible; do not let
    // the timing-only health sweep turn that policy into a stale alert.
    let power_defers = agents
        .iter()
        .flat_map(|a| a.manifest.schedules.iter())
        .any(|sched| power.defers(sched));
    if !power_defers {
        sweep_health_notifications(&agents, state, audit, plugins, now).await;
    }

    let next_event = compute_next_event_from_agents(&agents, state, now);

    debug!(
        agents_scanned = agents.len(),
        runs_dispatched,
        next_event = next_event
            .map(|t| t.format("%Y-%m-%dT%H:%M:%S%z").to_string())
            .unwrap_or_else(|| "none".into()),
        "tick completed"
    );

    TickResult {
        agents_scanned: agents.len() as u32,
        runs_dispatched,
        next_event,
    }
}

/// Dry-run variant: reports what `tick_once` *would* do without dispatching
/// or writing to the audit log.
pub async fn tick_dry_run(
    state: &StateStore,
    power_config: &dotagent_core::config::PowerConfig,
    now: DateTime<Local>,
) -> TickResult {
    let agents = discovery::discover_all().unwrap_or_default();
    let power = PowerGate::detect(
        power_config,
        agents.iter().flat_map(|a| a.manifest.schedules.iter()),
    );
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
            if power.defers(sched) {
                continue;
            }
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

/// When the retention sweep's daily window opens.
const RETENTION_HOUR: u32 = 3;
/// How long it stays open. A tick that starts inside this runs the sweep.
const RETENTION_WINDOW_MINUTES: u32 = 30;

/// Log retention runs once per day at 03:00 ± 30min.
fn should_run_retention(now: DateTime<Local>, last_date: Option<chrono::NaiveDate>) -> bool {
    use chrono::Timelike;
    let in_window = now.hour() == RETENTION_HOUR && now.minute() < RETENTION_WINDOW_MINUTES;
    if !in_window {
        return false;
    }
    match last_date {
        Some(d) => d != now.date_naive(),
        None => true,
    }
}

/// When the daemon has to be awake next to run the retention sweep.
///
/// Same reasoning as [`next_summary_at`], and the same bug: the loop only ever
/// landed inside `[03:00, 03:30)` because `MAX_SLEEP_MINUTES` happens to equal
/// the window's width — a coincidence no code states. A tick that overruns its
/// own sleep budget (a 1200-second run is on record) leaves the next sleep at
/// the `unwrap_or(60s)` fallback and puts the following `cycle_start` past
/// 03:30. `last_retention_date` only advances on a sweep that ran, so the day
/// is skipped with nothing logged.
fn next_retention_at(now: DateTime<Local>) -> Option<DateTime<Local>> {
    let time = chrono::NaiveTime::from_hms_opt(RETENTION_HOUR, 0, 0)?;
    let today = now.date_naive();
    [Some(today), today.succ_opt()]
        .into_iter()
        .flatten()
        .filter_map(|date| local_at(now, date, time))
        .find(|t| *t > now)
}

/// Resolve a local wall-clock time on `date` against `reference`'s timezone.
///
/// `None` when that wall-clock time does not exist on that date — the hour a
/// DST jump skips. The summary for that one day is then not delivered, which
/// only matters if `time` is inside the skipped hour (never for the 22:45
/// default; DST transitions happen around 02:00–03:00).
fn local_at(
    reference: DateTime<Local>,
    date: chrono::NaiveDate,
    time: chrono::NaiveTime,
) -> Option<DateTime<Local>> {
    reference
        .timezone()
        .from_local_datetime(&date.and_time(time))
        .earliest()
}

/// The summary date `now` falls into, or `None` when it is outside every
/// delivery window.
///
/// Returns the date the window *opened* on, not `now`'s date. With a `time`
/// late enough that `grace_minutes` crosses midnight the two differ, and
/// keying the once-per-day guard on `now` would let a 00:10 delivery mark
/// tonight's 23:50 window as already done.
fn summary_window_date(
    now: DateTime<Local>,
    time: chrono::NaiveTime,
    grace_minutes: u32,
) -> Option<chrono::NaiveDate> {
    let grace = chrono::Duration::minutes(i64::from(grace_minutes));
    let today = now.date_naive();
    let candidates = [Some(today), today.pred_opt()];
    for date in candidates.into_iter().flatten() {
        let Some(start) = local_at(now, date, time) else {
            continue;
        };
        if now >= start && now < start + grace {
            return Some(date);
        }
    }
    None
}

/// `Some(date)` when the summary is due and has not been delivered for that
/// window yet — the date is what the caller records as delivered.
fn should_run_daily_summary(
    now: DateTime<Local>,
    last_summary_date: Option<chrono::NaiveDate>,
    cfg: &dotagent_core::DailySummaryConfig,
) -> Option<chrono::NaiveDate> {
    if !cfg.enabled {
        return None;
    }
    let date = summary_window_date(now, cfg.time_or_default(), cfg.effective_grace_minutes())?;
    (last_summary_date != Some(date)).then_some(date)
}

/// When the daemon has to be awake next to deliver the summary.
///
/// Without this the loop only ever landed in the window because
/// `MAX_SLEEP_MINUTES` happened to equal the hardcoded 30-minute window —
/// two unrelated constants with no code connecting them. That coincidence
/// breaks the moment a tick overruns its own sleep budget (the next sleep is
/// then `unwrap_or(60s)`, pushing the following cycle past the window), and it
/// never held at all for a `time`/`grace_minutes` a user picks.
fn next_summary_at(
    now: DateTime<Local>,
    cfg: &dotagent_core::DailySummaryConfig,
) -> Option<DateTime<Local>> {
    if !cfg.enabled {
        return None;
    }
    let time = cfg.time_or_default();
    let today = now.date_naive();
    let candidates = [Some(today), today.succ_opt()];
    candidates
        .into_iter()
        .flatten()
        .filter_map(|date| local_at(now, date, time))
        .find(|t| *t > now)
}

/// Returns the earliest pending wake-up, capped at `now + MAX_SLEEP`.
///
/// The daily summary and the retention sweep are wake-up reasons in their own
/// right: they are the scheduled things the daemon does that no agent schedule
/// accounts for. Anything time-triggered the loop grows from here belongs in
/// this list too — a window the loop only reaches because the safety cap
/// happens to be as wide as the window is a window that gets skipped the first
/// time a tick runs long.
fn compute_sleep_target(
    now: DateTime<Local>,
    next_event: Option<DateTime<Local>>,
    next_summary: Option<DateTime<Local>>,
    next_retention: Option<DateTime<Local>>,
) -> DateTime<Local> {
    let safety_cap = now + chrono::Duration::minutes(MAX_SLEEP_MINUTES);
    [next_event, next_summary, next_retention]
        .into_iter()
        .flatten()
        .filter(|t| *t > now && *t < safety_cap)
        .min()
        .unwrap_or(safety_cap)
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
    power: PowerGate,
    now: DateTime<Local>,
) -> u32 {
    let mut dispatched = 0u32;
    for agent in agents {
        if !agent.manifest.agent.monitor {
            continue;
        }
        for sched in &agent.manifest.schedules {
            if dispatch_one(agent, sched, state, audit, plugins, pool, power, now).await {
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
    power: PowerGate,
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

    // 2. Power gate. Deliberately before the window is read or written: a
    //    deferred run must leave no trace, so that plugging the charger back
    //    in finds the window exactly as it was and dispatches it. Recording an
    //    attempt here would burn a retry for something that never ran.
    //
    //    Note this sits *after* the staleness check: a machine left on battery
    //    past `stale_after_minutes` drops the window rather than running a
    //    stale job hours late. That is the same call staleness always makes.
    if power.defers(sched) {
        debug!(
            agent = %agent.manifest.agent.name,
            schedule = sched.id(),
            battery_percent = power.battery_percent(),
            "run deferred: on battery"
        );
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

    // 3. Backoff gate. If we've already attempted ≥1 and the wait hasn't
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

    // 4. max_retries gate. If we've already burned them, mark given_up and
    //    fire on_failure(given_up).
    if window.attempts >= policy.max_retries {
        give_up(agent, sched, &mut window, &dctx, &slug, expected).await;
        return false;
    }

    // 5. Dispatch.
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

    // 6. Update window state from outcome.
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

    // Through the same ladder the tick sweep uses. An interval window that
    // gives up, rolls forward and gives up again is one ongoing failure, not a
    // fresh one every tick — announcing each would be 96 messages a day from a
    // 15-minute schedule. The episode is dropped the moment the schedule
    // succeeds, so the next real failure is still loud immediately.
    let name = agent.manifest.agent.name.clone();
    let now = Local::now();
    if let Some(episode) = claim_alert(&name, sched.id(), "given_up", now.timestamp()) {
        let message = alert_message(
            "given_up",
            &name,
            sched.id(),
            None,
            Some(window),
            &episode,
            now,
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
    }

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

/// What the sweep decided about one `(agent, schedule)` pair.
enum AlertVerdict {
    /// The current window succeeded. Forget any episode — the next failure
    /// should be loud from its first second, not inherit a day-long silence.
    Healthy,
    /// Nothing to say: no window in scope, or retries are still in flight and
    /// the run loop owns the outcome. Whatever we remembered stays remembered.
    Quiet,
    /// Say this, if the ladder allows it.
    Alert(&'static str),
}

/// Which alert, if any, this pair is currently owed.
///
/// Reads [`HealthState`] — the same verdict `dotagent status` prints — rather
/// than re-deriving staleness here. That is the whole point of the change: the
/// daemon already knew `inbox-triage/every-90min` was stale, it just had no
/// way to say so unless a human ran `status`. It also keeps the interval
/// rolling-window subtlety in one place: dispatch judges against the *current*
/// tick, health against the *first missed* one.
///
/// Pure so the policy is testable without a filesystem or a clock.
fn alert_verdict(
    health: HealthState,
    expected: Option<DateTime<Local>>,
    last_success: Option<DateTime<Local>>,
    window: Option<&WindowState>,
) -> AlertVerdict {
    match health {
        // `health_state` also calls a never-run agent stale. A freshly
        // installed one whose first window has not come yet is not an outage,
        // and alerting on the tick after install is the fastest way to teach
        // someone to mute dotagent.
        HealthState::Stale if expected.is_some() || last_success.is_some() => {
            AlertVerdict::Alert("stale")
        }
        HealthState::Stale => AlertVerdict::Quiet,

        // `give_up` fires this the instant the last retry burns. Repeating it
        // is the addition: an agent broken for a week should keep asking.
        HealthState::Failing if window.is_some_and(|w| w.given_up) => {
            AlertVerdict::Alert("given_up")
        }
        // Retries still in flight — the run loop owns the outcome and will
        // fire `given_up` itself if it runs out.
        HealthState::Failing => AlertVerdict::Quiet,

        // Cron reports no window between midnight and the daily hour, which
        // `health_state` calls Ok. Reading that as recovery would forget the
        // episode every night and make the ladder start over loud every
        // morning; only an actual success ends an episode.
        HealthState::Ok | HealthState::Degraded if expected.is_none() => AlertVerdict::Quiet,
        HealthState::Ok | HealthState::Degraded => AlertVerdict::Healthy,
    }
}

/// Does anything on this manifest want to hear about `event`?
///
/// Covers both delivery paths: built-in `[[notifiers]]` and the legacy
/// `[[on_failure]]` plugin hooks. Empty `events` means "all" on both.
fn anyone_listening(manifest: &AgentManifest, event: &str) -> bool {
    manifest.notifiers.iter().any(|n| n.matches_event(event))
        || manifest
            .on_failure
            .iter()
            .any(|h| h.events.is_empty() || h.events.iter().any(|e| e == event))
}

/// Which event name to actually deliver an alert under, or `None` when nobody
/// asked to hear about it.
///
/// `stale` falls back to the `given_up` channel when nothing lists it. Someone
/// who wrote `events = ["given_up"]` asked to be told their agent is broken,
/// and stale is the same news only worse — it stopped even trying. Making them
/// edit every manifest to learn about the *more* severe case would recreate the
/// exact failure this alert exists for: a config that looks complete while the
/// worst outage stays silent.
fn notify_channel(manifest: &AgentManifest, event: &'static str) -> Option<&'static str> {
    if anyone_listening(manifest, event) {
        return Some(event);
    }
    if event == "stale" && anyone_listening(manifest, "given_up") {
        return Some("given_up");
    }
    None
}

/// `55d 4h` / `4h 12m` / `9m`. Rough on purpose — the reader wants an order of
/// magnitude, not a stopwatch.
fn human_duration(seconds: i64) -> String {
    let s = seconds.max(0);
    let (d, h, m) = (s / 86_400, (s % 86_400) / 3_600, (s % 3_600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

/// Keep the tail of what the agent printed. The end is where the error is, and
/// a notification that scrolls past a phone screen gets dismissed unread.
fn stderr_tail(window: Option<&WindowState>) -> String {
    const MAX: usize = 400;
    let raw = window
        .and_then(|w| w.last_attempt_stderr.as_deref())
        .unwrap_or_default()
        .trim();
    if raw.is_empty() {
        return String::new();
    }
    if raw.chars().count() <= MAX {
        return format!("\n{raw}");
    }
    let tail: String = raw.chars().skip(raw.chars().count() - MAX).collect();
    format!("\n…{tail}")
}

/// The alert text: what, which schedule, how long, and the last thing the agent
/// said. Enough to decide whether to care without opening a terminal.
fn alert_message(
    event: &str,
    agent: &str,
    schedule: &str,
    last_success: Option<DateTime<Local>>,
    window: Option<&WindowState>,
    episode: &AlertEpisode,
    now: DateTime<Local>,
) -> String {
    let repeat = if episode.count > 1 {
        format!(" (notice #{})", episode.count)
    } else {
        String::new()
    };
    let body = match event {
        "stale" => {
            let age = match last_success {
                Some(ls) => format!(
                    "no successful run in {}",
                    human_duration((now - ls).num_seconds())
                ),
                None => "it has never run successfully".to_string(),
            };
            format!("⏰ {agent}/{schedule} is not being scheduled — {age}{repeat}.")
        }
        // The first notice is the moment it happened, so it reads in the past
        // tense and says nothing about elapsed time. Repeats are reminders
        // about something still true, and the age is the point of sending one.
        _ if episode.count <= 1 => {
            let attempts = window.map(|w| w.attempts).unwrap_or(0);
            let exit = window.and_then(|w| w.last_attempt_exit_code).unwrap_or(-1);
            format!("🚨 {agent}/{schedule} gave up after {attempts} attempts (exit {exit})")
        }
        _ => {
            let attempts = window.map(|w| w.attempts).unwrap_or(0);
            let exit = window.and_then(|w| w.last_attempt_exit_code).unwrap_or(-1);
            let since = window
                .and_then(|w| w.given_up_at)
                .map(|t| human_duration(now.timestamp() - t))
                .unwrap_or_else(|| "?".into());
            format!(
                "🚨 {agent}/{schedule} still given up after {attempts} attempts \
                 (exit {exit}), {since} ago{repeat}."
            )
        }
    };
    format!("{body}{}", stderr_tail(window))
}

/// Alert on conditions the run loop cannot report, because they are the absence
/// of a run rather than a failed one.
///
/// Runs once per tick, after dispatch, so a give-up that just happened is
/// already on record and does not get announced twice.
async fn sweep_health_notifications(
    agents: &[DiscoveredAgent],
    state: &StateStore,
    audit: &AuditLog,
    plugins: &PluginClient,
    now: DateTime<Local>,
) {
    let store = NotifyDedupStore::from_home();
    let mut table = store.load();
    let mut changed = false;

    for agent in agents {
        if !agent.manifest.agent.monitor {
            continue;
        }
        let name = &agent.manifest.agent.name;
        for sched in &agent.manifest.schedules {
            let policy = ResolvedPolicy::resolve(&agent.manifest, sched);
            let slug = slug_from_args(sched.args());
            let hb = state.read_heartbeat(name, &slug).ok().flatten();
            let last_success = hb
                .as_ref()
                .and_then(|h| h.last_success_at)
                .and_then(|s| Local.timestamp_opt(s, 0).single());
            let expected = expected_at(sched, now, last_success);
            // Same key `status` and the daily summary read. For interval it is
            // not derivable from the schedule, because a success re-phases the
            // tick sequence — deriving it here would leave the alert path blind
            // to exactly the recoveries the dashboard now sees.
            let dispatched = super::status::last_dispatched_window(state, name, &slug, now);
            let window = window_key(sched, hb.as_ref(), dispatched, now)
                .and_then(|key| state.read_window(name, &slug, key).ok().flatten());
            let (health, _) = health_state(sched, &policy, hb.as_ref(), window.as_ref(), now);

            let event = match alert_verdict(health, expected, last_success, window.as_ref()) {
                AlertVerdict::Alert(e) => e,
                AlertVerdict::Healthy => {
                    changed |= table.clear_pair(name, sched.id());
                    continue;
                }
                AlertVerdict::Quiet => continue,
            };

            // Nobody configured to hear it: say nothing, and remember nothing.
            // Recording an episode no one will read would grow the dedup file
            // for every agent that never wired up a notifier.
            let Some(channel) = notify_channel(&agent.manifest, event) else {
                continue;
            };

            // Keyed on the condition, not the channel it goes out on, so
            // editing a manifest's `events` never resets a running ladder.
            let key = alert_key(name, sched.id(), event);
            if !table.should_notify(&key, now.timestamp()) {
                continue;
            }
            let episode = table.record(&key, now.timestamp());
            changed = true;

            warn!(
                agent = %name,
                schedule = %sched.id(),
                event,
                channel,
                notice = episode.count,
                "health alert"
            );
            let message = alert_message(
                event,
                name,
                sched.id(),
                last_success,
                window.as_ref(),
                &episode,
                now,
            );
            fire_on_failure_event(
                &agent.manifest,
                sched.id(),
                channel,
                &message,
                plugins,
                audit,
            )
            .await;
        }
    }

    if changed {
        if let Err(e) = store.save(&table) {
            warn!(error = %e, "writing alert dedup state failed");
        }
    }
}

/// Ask the ladder whether this alert may speak right now, recording it when the
/// answer is yes. `None` means stay quiet.
///
/// Called by [`give_up`], which alerts the instant the last retry burns. The
/// tick sweep applies the same policy against the same table, but inline: it
/// walks every (agent, schedule) pair in one pass and loads and saves the store
/// once for the batch rather than once per alert. Same ladder either way — which
/// is what stops one failure being announced twice inside a second, and what
/// makes a still-broken agent keep asking on one cadence regardless of which
/// path noticed.
fn claim_alert(agent: &str, schedule: &str, event: &str, now: i64) -> Option<AlertEpisode> {
    let store = NotifyDedupStore::from_home();
    let mut table = store.load();
    let key = alert_key(agent, schedule, event);
    if !table.should_notify(&key, now) {
        return None;
    }
    let episode = table.record(&key, now);
    if let Err(e) = store.save(&table) {
        warn!(error = %e, "writing alert dedup state failed");
    }
    Some(episode)
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
mod trigger_worker_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::time::{advance, Duration as TokioDuration, Instant};

    // Why an integration-style test on paused time rather than a pure
    // "what should run now" function:
    //
    // The bug was never a wrong decision — every decision the daemon made was
    // correct. It was *when* the decision got a chance to be made: the
    // `select!` arm holding `trigger_rx` is not polled while a tick awaits a
    // scheduled run inline. A pure function cannot express "was not polled".
    // The property under test is temporal, so the test has to be too.
    //
    // `tokio::time::pause()` makes it deterministic and instant: sleeps
    // auto-advance, so "the tick takes 20 minutes" costs microseconds, and
    // the assertions are on the *virtual* clock, not on wall-clock luck.

    fn req(agent: &str) -> TriggerRequest {
        TriggerRequest {
            agent: agent.into(),
            schedule: None,
            args: Vec::new(),
            source: TriggerSource::Telegram,
            actor: None,
            reply_to: Some("42".into()),
            payload: None,
        }
    }

    /// The regression: a 20-minute scheduled run must not delay a message that
    /// arrived 21 seconds into it. Reproduces the production timeline
    /// (dispatch → trigger 21s later → answered 19m45s late).
    #[tokio::test(start_paused = true)]
    async fn a_long_scheduled_run_does_not_delay_a_trigger() {
        let seen: Arc<Mutex<Vec<TokioDuration>>> = Arc::new(Mutex::new(Vec::new()));
        let t0 = Instant::now();

        let (tx, rx) = tokio::sync::mpsc::channel::<TriggerRequest>(TRIGGER_CHANNEL_DEPTH);
        let sink = seen.clone();
        let worker = spawn_trigger_worker(rx, move |_req| {
            let sink = sink.clone();
            async move {
                sink.lock().unwrap().push(Instant::now() - t0);
            }
        });

        // The main loop, busy the way it was: one scheduled run awaited inline
        // for 1200 seconds. Nothing in here touches the trigger channel.
        let tick = tokio::spawn(async move {
            tokio::time::sleep(TokioDuration::from_secs(1200)).await;
        });

        advance(TokioDuration::from_secs(21)).await;
        tx.send(req("telegram-assistant")).await.unwrap();
        // Yield enough for the worker task to be scheduled and run.
        tokio::task::yield_now().await;
        tokio::time::sleep(TokioDuration::from_millis(1)).await;

        let observed = seen.lock().unwrap().clone();
        assert_eq!(observed.len(), 1, "the trigger was never picked up");
        assert!(
            observed[0] < TokioDuration::from_secs(60),
            "trigger waited {:?} — it must not wait for the scheduled run",
            observed[0]
        );

        tick.await.unwrap();
        worker.abort();
    }

    /// A conversation's messages must come out in the order they went in, even
    /// when each one takes real time. This is the property that rules out
    /// "just `tokio::spawn` each trigger".
    #[tokio::test(start_paused = true)]
    async fn messages_are_handled_one_at_a_time_in_order() {
        let order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let inflight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_inflight = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let (tx, rx) = tokio::sync::mpsc::channel::<TriggerRequest>(TRIGGER_CHANNEL_DEPTH);
        let (o, f, m) = (order.clone(), inflight.clone(), max_inflight.clone());
        let worker = spawn_trigger_worker(rx, move |req: TriggerRequest| {
            let (o, f, m) = (o.clone(), f.clone(), m.clone());
            async move {
                use std::sync::atomic::Ordering::SeqCst;
                let now = f.fetch_add(1, SeqCst) + 1;
                m.fetch_max(now, SeqCst);
                // A persistent agent answering a chat message takes seconds.
                tokio::time::sleep(TokioDuration::from_secs(30)).await;
                o.lock().unwrap().push(req.agent.clone());
                f.fetch_sub(1, SeqCst);
            }
        });

        for name in ["first", "second", "third"] {
            tx.send(req(name)).await.unwrap();
        }
        // No explicit `advance`: with the clock paused, an idle runtime jumps
        // to the next pending timer on its own, so the worker's three 30s
        // sleeps resolve in sequence inside this one.
        tokio::time::sleep(TokioDuration::from_secs(200)).await;

        assert_eq!(*order.lock().unwrap(), vec!["first", "second", "third"]);
        assert_eq!(
            max_inflight.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a burst must not become concurrent agent processes"
        );
        worker.abort();
    }

    /// Closing the channel ends the worker instead of leaking a task that
    /// spins on a dead receiver.
    #[tokio::test(start_paused = true)]
    async fn worker_stops_when_the_channel_closes() {
        let (tx, rx) = tokio::sync::mpsc::channel::<TriggerRequest>(4);
        let worker = spawn_trigger_worker(rx, |_req| async {});
        drop(tx);
        tokio::time::timeout(TokioDuration::from_secs(1), worker)
            .await
            .expect("worker should finish")
            .expect("worker should not panic");
    }

    // --- the worker has to be watched by somebody ---
    //
    // Moving trigger handling off the `select!` moved it out of anyone's sight:
    // the handle was spawned and only ever `abort()`ed. A panicking handler
    // therefore killed the worker, closed the channel, and the ingress logged
    // one line and returned — a bot that never answers again, on a daemon that
    // keeps ticking and reports itself healthy. Before the worker existed the
    // same panic unwound `run()` and launchd restarted the process.

    /// A worker that panics must stop the daemon, not silence it.
    #[tokio::test(start_paused = true)]
    async fn a_panicking_trigger_worker_stops_the_daemon() {
        let (tx, rx) = tokio::sync::mpsc::channel::<TriggerRequest>(TRIGGER_CHANNEL_DEPTH);
        let mut worker = spawn_trigger_worker(rx, |_req| async { panic!("handler blew up") });
        tx.send(req("telegram-assistant")).await.unwrap();

        let mut signals = Signals::register().expect("signals must register");
        // A full safety-cap sleep: nothing but the dead worker can end this.
        let wake = wait_for_event(
            Duration::from_secs(MAX_SLEEP_MINUTES as u64 * 60),
            &mut signals,
            &mut worker,
        )
        .await;

        assert_eq!(wake, Wake::Stop("trigger worker died"));
    }

    /// Same for a worker that ends without panicking — the channel is closed
    /// either way, and a daemon that cannot hear is a daemon that should exit.
    #[tokio::test(start_paused = true)]
    async fn a_worker_that_returns_also_stops_the_daemon() {
        let (tx, rx) = tokio::sync::mpsc::channel::<TriggerRequest>(4);
        let mut worker = spawn_trigger_worker(rx, |_req| async {});
        drop(tx);

        let mut signals = Signals::register().expect("signals must register");
        let wake = wait_for_event(
            Duration::from_secs(MAX_SLEEP_MINUTES as u64 * 60),
            &mut signals,
            &mut worker,
        )
        .await;

        assert_eq!(wake, Wake::Stop("trigger worker died"));
    }

    /// And a live worker must not be mistaken for a dead one: an ordinary sleep
    /// still ends in a tick.
    #[tokio::test(start_paused = true)]
    async fn a_live_worker_lets_the_loop_tick() {
        let (tx, rx) = tokio::sync::mpsc::channel::<TriggerRequest>(4);
        let mut worker = spawn_trigger_worker(rx, |_req| async {});

        let mut signals = Signals::register().expect("signals must register");
        let wake = wait_for_event(Duration::from_secs(600), &mut signals, &mut worker).await;

        assert_eq!(wake, Wake::Tick);
        drop(tx);
        worker.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- daily summary scheduling ---
    //
    // Two bugs live here. The window used to be hardcoded to `[22:45, 23:15)`
    // while the module doc claimed a configurable `daily_summary_time`, and
    // nothing ever scheduled a wake-up for it: the loop reached the window
    // only because `MAX_SLEEP_MINUTES` (30) happened to equal the window's
    // width, which no code stated and a long tick already broke.

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    fn summary_cfg(time: &str, grace: u32) -> dotagent_core::DailySummaryConfig {
        dotagent_core::DailySummaryConfig {
            time: time.into(),
            grace_minutes: grace,
            ..Default::default()
        }
    }

    #[test]
    fn summary_fires_inside_the_configured_window() {
        let cfg = summary_cfg("07:30", 30);
        let day = chrono::NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        for (h, m) in [(7, 30), (7, 45), (7, 59)] {
            assert_eq!(
                should_run_daily_summary(dt(2026, 8, 6, h, m), None, &cfg),
                Some(day),
                "{h}:{m} is inside [07:30, 08:00)"
            );
        }
    }

    #[test]
    fn summary_does_not_fire_outside_the_window() {
        let cfg = summary_cfg("07:30", 30);
        for (h, m) in [(7, 29), (8, 0), (22, 45), (0, 0)] {
            assert_eq!(
                should_run_daily_summary(dt(2026, 8, 6, h, m), None, &cfg),
                None,
                "{h}:{m} is outside [07:30, 08:00)"
            );
        }
    }

    #[test]
    fn the_default_window_is_still_2245() {
        // The comment promised 22:45 long before anything read it from config.
        // Nobody who never opens config.toml should notice this change.
        let cfg = dotagent_core::DailySummaryConfig::default();
        assert!(should_run_daily_summary(dt(2026, 8, 6, 22, 45), None, &cfg).is_some());
        assert!(should_run_daily_summary(dt(2026, 8, 6, 23, 14), None, &cfg).is_some());
        assert!(should_run_daily_summary(dt(2026, 8, 6, 23, 15), None, &cfg).is_none());
        assert!(should_run_daily_summary(dt(2026, 8, 6, 22, 44), None, &cfg).is_none());
    }

    #[test]
    fn summary_fires_once_per_window() {
        let cfg = summary_cfg("22:45", 30);
        let day = chrono::NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        assert_eq!(
            should_run_daily_summary(dt(2026, 8, 6, 22, 50), Some(day), &cfg),
            None
        );
        // Next day's window is a different window.
        assert!(should_run_daily_summary(dt(2026, 8, 7, 22, 50), Some(day), &cfg).is_some());
    }

    #[test]
    fn a_window_crossing_midnight_keys_on_the_date_it_opened() {
        // 23:50 + 30min grace runs to 00:20. Keying the once-a-day guard on
        // `now` would record Aug 7 for a delivery that belongs to Aug 6's
        // window — and then skip Aug 7's own 23:50 delivery entirely.
        let cfg = summary_cfg("23:50", 30);
        let aug6 = chrono::NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let aug7 = chrono::NaiveDate::from_ymd_opt(2026, 8, 7).unwrap();

        assert_eq!(
            should_run_daily_summary(dt(2026, 8, 7, 0, 10), None, &cfg),
            Some(aug6),
            "00:10 belongs to the window that opened yesterday"
        );
        assert_eq!(
            should_run_daily_summary(dt(2026, 8, 7, 0, 10), Some(aug6), &cfg),
            None,
            "already delivered for that window"
        );
        assert_eq!(
            should_run_daily_summary(dt(2026, 8, 7, 23, 50), Some(aug6), &cfg),
            Some(aug7),
            "tonight's window must not be swallowed by yesterday's guard"
        );
    }

    #[test]
    fn a_disabled_summary_never_fires_and_never_wakes_the_daemon() {
        let cfg = dotagent_core::DailySummaryConfig {
            enabled: false,
            ..Default::default()
        };
        assert_eq!(
            should_run_daily_summary(dt(2026, 8, 6, 22, 45), None, &cfg),
            None
        );
        assert_eq!(next_summary_at(dt(2026, 8, 6, 10, 0), &cfg), None);
    }

    #[test]
    fn a_zero_grace_window_is_not_an_empty_one() {
        // `grace_minutes = 0` would make the half-open window empty and stop
        // delivery with no error anywhere.
        let cfg = summary_cfg("22:45", 0);
        assert!(should_run_daily_summary(dt(2026, 8, 6, 22, 45), None, &cfg).is_some());
    }

    #[test]
    fn an_unparseable_time_falls_back_instead_of_silencing_delivery() {
        let cfg = summary_cfg("quarter to eleven", 30);
        assert!(should_run_daily_summary(dt(2026, 8, 6, 22, 45), None, &cfg).is_some());
    }

    #[test]
    fn the_daemon_schedules_a_wake_up_for_the_summary() {
        // The regression that mattered: with no agent event in sight the loop
        // used to sleep the full safety cap and reach the window only because
        // the cap happened to equal the window width.
        let cfg = summary_cfg("22:45", 30);
        let now = dt(2026, 8, 6, 22, 30);
        assert_eq!(
            compute_sleep_target(now, None, next_summary_at(now, &cfg), None),
            dt(2026, 8, 6, 22, 45)
        );
    }

    #[test]
    fn a_narrow_window_no_longer_depends_on_the_safety_cap() {
        // A one-minute grace is unreachable by a 30-minute sleep cap alone.
        let cfg = summary_cfg("22:45", 1);
        let now = dt(2026, 8, 6, 22, 20);
        let target = compute_sleep_target(now, None, next_summary_at(now, &cfg), None);
        assert_eq!(target, dt(2026, 8, 6, 22, 45));
        assert!(should_run_daily_summary(target, None, &cfg).is_some());
    }

    #[test]
    fn the_earlier_of_agent_event_and_summary_wins() {
        let cfg = summary_cfg("22:45", 30);
        let now = dt(2026, 8, 6, 22, 30);
        let agent_event = dt(2026, 8, 6, 22, 35);
        assert_eq!(
            compute_sleep_target(now, Some(agent_event), next_summary_at(now, &cfg), None),
            agent_event
        );
        let later_event = dt(2026, 8, 6, 22, 50);
        assert_eq!(
            compute_sleep_target(now, Some(later_event), next_summary_at(now, &cfg), None),
            dt(2026, 8, 6, 22, 45)
        );
    }

    #[test]
    fn a_summary_past_the_safety_cap_does_not_extend_the_sleep() {
        // The cap also bounds how stale the loaded manifests may get; a
        // wake-up 8 hours out must not override it.
        let cfg = summary_cfg("22:45", 30);
        let now = dt(2026, 8, 6, 14, 0);
        assert_eq!(
            compute_sleep_target(now, None, next_summary_at(now, &cfg), None),
            now + chrono::Duration::minutes(MAX_SLEEP_MINUTES)
        );
    }

    #[test]
    fn the_next_wake_up_rolls_to_tomorrow_once_today_passed() {
        let cfg = summary_cfg("22:45", 30);
        assert_eq!(
            next_summary_at(dt(2026, 8, 6, 22, 45), &cfg),
            Some(dt(2026, 8, 7, 22, 45)),
            "exactly at the time, the next one is tomorrow's"
        );
        assert_eq!(
            next_summary_at(dt(2026, 8, 6, 23, 59), &cfg),
            Some(dt(2026, 8, 7, 22, 45))
        );
    }

    // --- retention scheduling ---
    //
    // The sweep had exactly the bug `next_summary_at` was written to fix: a
    // 30-minute window reached only because `MAX_SLEEP_MINUTES` is also 30.

    #[test]
    fn the_daemon_schedules_a_wake_up_for_the_retention_sweep() {
        let now = dt(2026, 8, 6, 2, 50);
        let target = compute_sleep_target(now, None, None, next_retention_at(now));
        assert_eq!(target, dt(2026, 8, 6, 3, 0));
        assert!(
            should_run_retention(target, None),
            "waking at 03:00 has to land inside the sweep's own window"
        );
    }

    #[test]
    fn the_next_retention_wake_up_rolls_to_tomorrow_once_today_passed() {
        assert_eq!(
            next_retention_at(dt(2026, 8, 6, 3, 0)),
            Some(dt(2026, 8, 7, 3, 0)),
            "exactly at the hour, the next one is tomorrow's"
        );
        assert_eq!(
            next_retention_at(dt(2026, 8, 6, 3, 29)),
            Some(dt(2026, 8, 7, 3, 0))
        );
        assert_eq!(
            next_retention_at(dt(2026, 8, 6, 23, 59)),
            Some(dt(2026, 8, 7, 3, 0))
        );
    }

    #[test]
    fn retention_never_extends_a_sleep_past_the_safety_cap() {
        // 03:00 is 9 hours out from here; the cap on manifest staleness wins.
        let now = dt(2026, 8, 5, 18, 0);
        assert_eq!(
            compute_sleep_target(now, None, None, next_retention_at(now)),
            now + chrono::Duration::minutes(MAX_SLEEP_MINUTES)
        );
    }

    #[test]
    fn the_earliest_of_the_three_wake_ups_wins() {
        let cfg = summary_cfg("03:10", 30);
        let now = dt(2026, 8, 6, 2, 50);
        let agent_event = dt(2026, 8, 6, 2, 55);
        // Agent event, then retention (03:00), then summary (03:10).
        assert_eq!(
            compute_sleep_target(
                now,
                Some(agent_event),
                next_summary_at(now, &cfg),
                next_retention_at(now)
            ),
            agent_event
        );
        assert_eq!(
            compute_sleep_target(
                now,
                None,
                next_summary_at(now, &cfg),
                next_retention_at(now)
            ),
            dt(2026, 8, 6, 3, 0),
            "retention is earlier than the summary here"
        );
    }

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

    // --- health alerts. `stale` is the failure with no output channel: the
    // agent stops being scheduled, so nothing runs, so nothing fails, so
    // nothing fires. Two real agents sat dead for 55 and 5 days in silence. ---

    fn at(hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 5, 19, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn given_up_window(attempts: u32, stderr: &str) -> WindowState {
        WindowState {
            agent: "a".into(),
            schedule_id: "daily".into(),
            expected_at: 0,
            attempts,
            last_attempt_at: None,
            last_attempt_exit_code: Some(1),
            last_attempt_stderr: Some(stderr.into()),
            given_up: true,
            given_up_at: Some(at(2, 0).timestamp()),
        }
    }

    /// 2026-05-19 is a Tuesday (weekday 2).
    const CRON_8AM: &str =
        "id = \"daily\"\ntype = \"cron\"\nweekdays = [0,1,2,3,4,5,6]\nhours = [8]";
    const EVERY_15MIN: &str = "id = \"q\"\ntype = \"interval\"\ninterval_minutes = 15";

    fn heartbeat_at(ls: DateTime<Local>) -> Heartbeat {
        Heartbeat {
            name: "a".into(),
            slug: "default".into(),
            args: vec![],
            started_at: ls.timestamp(),
            started_at_iso: String::new(),
            finished_at: Some(ls.timestamp()),
            finished_at_iso: None,
            exit_code: Some(0),
            duration_seconds: Some(1),
            last_success_at: Some(ls.timestamp()),
            last_success_at_iso: None,
        }
    }

    /// Drives the real [`health_state`] rather than feeding a [`HealthState`]
    /// in, because the interesting bugs live in the seam: interval windows roll
    /// forward for dispatch but are judged for staleness from the first missed
    /// one, and getting that wrong is what made a 55-day-dead agent look busy.
    fn verdict(
        schedule_toml: &str,
        stale_after: u32,
        last_success: Option<DateTime<Local>>,
        window: Option<&WindowState>,
        now: DateTime<Local>,
    ) -> AlertVerdict {
        let m: AgentManifest = toml::from_str(&format!(
            "[agent]\nname = \"a\"\n[run]\ncommand = \"true\"\n\
             [defaults]\nstale_after_minutes = {stale_after}\n\
             [[schedules]]\n{schedule_toml}\n"
        ))
        .expect("test manifest must parse");
        let sched = &m.schedules[0];
        let policy = ResolvedPolicy::resolve(&m, sched);
        let hb = last_success.map(heartbeat_at);
        let expected = expected_at(sched, now, last_success);
        let (health, _) = health_state(sched, &policy, hb.as_ref(), window, now);
        alert_verdict(health, expected, last_success, window)
    }

    #[test]
    fn a_cron_window_aged_past_stale_is_alerted() {
        // Exactly the case `dispatch_one` refuses to touch: too old to retry,
        // so nothing runs, so nothing else would ever say a word.
        let v = verdict(CRON_8AM, 60, Some(at(7, 0)), None, at(12, 0));
        assert!(matches!(v, AlertVerdict::Alert("stale")));
    }

    #[test]
    fn an_interval_agent_dead_for_weeks_is_alerted() {
        // The production case. Interval windows roll forward every tick so
        // dispatch never sees a stale one; judging from the first missed window
        // is what makes weeks of silence visible.
        let long_ago = at(8, 0) - chrono::Duration::days(55);
        let v = verdict(EVERY_15MIN, 60, Some(long_ago), None, at(12, 0));
        assert!(matches!(v, AlertVerdict::Alert("stale")));
    }

    #[test]
    fn an_interval_agent_that_just_missed_a_tick_is_not_stale_yet() {
        let v = verdict(EVERY_15MIN, 60, Some(at(11, 40)), None, at(12, 0));
        assert!(!matches!(v, AlertVerdict::Alert("stale")));
    }

    #[test]
    fn an_agent_that_never_ran_still_alerts_once_its_window_ages_out() {
        let v = verdict(CRON_8AM, 60, None, None, at(12, 0));
        assert!(matches!(v, AlertVerdict::Alert("stale")));
    }

    #[test]
    fn a_freshly_installed_agent_is_not_an_outage() {
        // Cron window is tonight, nothing has run, nothing is wrong. Alerting
        // on the tick after install is how you teach someone to mute dotagent.
        let m = "id = \"nightly\"\ntype = \"cron\"\nweekdays = [0,1,2,3,4,5,6]\nhours = [22]";
        assert!(matches!(
            verdict(m, 60, None, None, at(10, 0)),
            AlertVerdict::Quiet
        ));
        // Same for an interval agent with no anchor: the OS scheduler
        // bootstraps the first run, dotagent never forces it.
        assert!(matches!(
            verdict(EVERY_15MIN, 60, None, None, at(10, 0)),
            AlertVerdict::Quiet
        ));
    }

    #[test]
    fn no_window_in_scope_does_not_clear_a_chronic_failure() {
        // Cron reports no window between midnight and the daily hour. Reading
        // that as recovery would forget the episode every night and make the
        // ladder start over loud every morning.
        assert!(matches!(
            verdict(
                CRON_8AM,
                60,
                Some(at(8, 0) - chrono::Duration::days(3)),
                None,
                at(2, 0)
            ),
            AlertVerdict::Quiet
        ));
    }

    #[test]
    fn a_window_that_succeeded_is_healthy() {
        let v = verdict(CRON_8AM, 60, Some(at(8, 5)), None, at(12, 0));
        assert!(matches!(v, AlertVerdict::Healthy));
    }

    #[test]
    fn retries_in_flight_are_the_run_loops_business() {
        // Inside `stale_after_minutes`, not given up — the daemon is working on
        // it and will fire `given_up` itself if it runs out.
        let mut w = given_up_window(1, "boom");
        w.given_up = false;
        let v = verdict(CRON_8AM, 60, Some(at(7, 0)), Some(&w), at(8, 30));
        assert!(matches!(v, AlertVerdict::Quiet));
    }

    #[test]
    fn a_window_that_gave_up_keeps_asking() {
        let w = given_up_window(3, "boom");
        let v = verdict(CRON_8AM, 60, Some(at(7, 0)), Some(&w), at(8, 30));
        assert!(matches!(v, AlertVerdict::Alert("given_up")));
    }

    #[test]
    fn stale_wins_over_given_up() {
        // Both are true once a given-up window ages out. "Not being scheduled
        // at all" is the more urgent thing to say.
        let w = given_up_window(3, "boom");
        let v = verdict(CRON_8AM, 60, Some(at(7, 0)), Some(&w), at(12, 0));
        assert!(matches!(v, AlertVerdict::Alert("stale")));
    }

    #[test]
    fn the_stale_message_says_agent_schedule_and_how_long() {
        let episode = AlertEpisode {
            first_notified_at: 0,
            last_notified_at: 0,
            count: 1,
        };
        let msg = alert_message(
            "stale",
            "inbox-triage",
            "every-90min",
            Some(at(12, 0) - chrono::Duration::days(55)),
            None,
            &episode,
            at(12, 0),
        );
        assert!(msg.contains("inbox-triage/every-90min"), "{msg}");
        assert!(msg.contains("55d"), "{msg}");
        assert!(!msg.contains("notice #"), "first notice is not numbered");
    }

    #[test]
    fn a_repeat_notice_is_numbered_so_it_reads_as_a_reminder() {
        let episode = AlertEpisode {
            first_notified_at: 0,
            last_notified_at: 0,
            count: 4,
        };
        let msg = alert_message("stale", "a", "daily", None, None, &episode, at(12, 0));
        assert!(msg.contains("notice #4"), "{msg}");
        assert!(msg.contains("never run successfully"), "{msg}");
    }

    #[test]
    fn the_first_give_up_notice_reads_as_an_event_not_a_reminder() {
        let episode = AlertEpisode {
            first_notified_at: 0,
            last_notified_at: 0,
            count: 1,
        };
        let w = given_up_window(3, "boom");
        let msg = alert_message("given_up", "a", "daily", None, Some(&w), &episode, at(2, 0));
        assert!(msg.contains("gave up after 3 attempts"), "{msg}");
        assert!(!msg.contains("still"), "nothing has elapsed yet: {msg}");
        assert!(!msg.contains("0m ago"), "{msg}");
    }

    #[test]
    fn the_given_up_message_carries_the_last_stderr() {
        let episode = AlertEpisode {
            first_notified_at: 0,
            last_notified_at: 0,
            count: 2,
        };
        let w = given_up_window(3, "ImportError: no module named gmail");
        let msg = alert_message(
            "given_up",
            "a",
            "daily",
            None,
            Some(&w),
            &episode,
            at(12, 0),
        );
        assert!(msg.contains("3 attempts"), "{msg}");
        assert!(msg.contains("ImportError"), "{msg}");
        assert!(msg.contains("10h"), "10h since give-up: {msg}");
    }

    #[test]
    fn no_message_ever_renders_a_rust_struct() {
        // CLAUDE.md forbids `{:?}` in anything user-facing, and these go to a
        // phone. A `Some(` in the output means a Debug leak.
        let episode = AlertEpisode {
            first_notified_at: 0,
            last_notified_at: 0,
            count: 1,
        };
        for event in ["stale", "given_up"] {
            let msg = alert_message(
                event,
                "a",
                "daily",
                None,
                Some(&given_up_window(1, "x")),
                &episode,
                at(12, 0),
            );
            assert!(!msg.contains("Some("), "{msg}");
            assert!(!msg.contains("WindowState"), "{msg}");
        }
    }

    #[test]
    fn a_huge_stderr_is_truncated_from_the_front() {
        let w = given_up_window(1, &format!("{}THE-ACTUAL-ERROR", "noise ".repeat(500)));
        let tail = stderr_tail(Some(&w));
        assert!(
            tail.contains("THE-ACTUAL-ERROR"),
            "the end is the useful end"
        );
        assert!(tail.starts_with("\n…"));
        assert!(tail.chars().count() < 450, "len {}", tail.chars().count());
    }

    #[test]
    fn an_empty_stderr_adds_nothing() {
        assert_eq!(stderr_tail(None), "");
        assert_eq!(stderr_tail(Some(&given_up_window(1, "   \n "))), "");
    }

    // --- which channel a health alert goes out on ---

    fn manifest_with(notifiers: &str) -> AgentManifest {
        let raw = format!("[agent]\nname = \"a\"\n[run]\ncommand = \"bash\"\n{notifiers}");
        toml::from_str(&raw).expect("test manifest must parse")
    }

    #[test]
    fn an_agent_with_no_notifiers_is_not_alerted() {
        assert_eq!(notify_channel(&manifest_with(""), "stale"), None);
        assert_eq!(notify_channel(&manifest_with(""), "given_up"), None);
    }

    #[test]
    fn a_notifier_that_lists_stale_gets_it_on_its_own_channel() {
        let m = manifest_with(
            "[[notifiers]]\ndriver = \"desktop\"\nevents = [\"given_up\", \"stale\"]\n",
        );
        assert_eq!(notify_channel(&m, "stale"), Some("stale"));
    }

    #[test]
    fn stale_reaches_a_given_up_only_notifier() {
        // Every agent in the wild was written as `events = ["given_up"]`.
        // Requiring a manifest edit to hear about the *worse* failure would
        // reproduce the outage this alert exists to end.
        let m = manifest_with("[[notifiers]]\ndriver = \"desktop\"\nevents = [\"given_up\"]\n");
        assert_eq!(notify_channel(&m, "stale"), Some("given_up"));
    }

    #[test]
    fn a_notifier_with_no_event_filter_hears_everything_directly() {
        let m = manifest_with("[[notifiers]]\ndriver = \"desktop\"\n");
        assert_eq!(notify_channel(&m, "stale"), Some("stale"));
    }

    #[test]
    fn a_notifier_listening_for_something_else_entirely_is_left_alone() {
        let m = manifest_with("[[notifiers]]\ndriver = \"desktop\"\nevents = [\"recovered\"]\n");
        assert_eq!(notify_channel(&m, "stale"), None);
    }

    #[test]
    fn legacy_on_failure_hooks_count_as_listeners() {
        let m = manifest_with("[[on_failure]]\nplugin = \"sink-file\"\nevents = [\"given_up\"]\n");
        assert_eq!(notify_channel(&m, "stale"), Some("given_up"));
        assert_eq!(notify_channel(&m, "given_up"), Some("given_up"));
    }

    #[test]
    fn durations_read_at_a_glance() {
        assert_eq!(human_duration(0), "0m");
        assert_eq!(human_duration(9 * 60), "9m");
        assert_eq!(human_duration(4 * 3600 + 12 * 60), "4h 12m");
        assert_eq!(human_duration(55 * 86_400 + 4 * 3600), "55d 4h");
        // A clock that moved backwards must not print a negative age.
        assert_eq!(human_duration(-500), "0m");
    }
}
