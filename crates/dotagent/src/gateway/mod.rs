//! The trigger gateway: admission policy, per-conversation workers,
//! delivery shaping.
//!
//! dotagent is a harness. The gateway routes triggers to agent runs, applies
//! admission policy (rate limit, session cap, session-id validation) and
//! delivers output through [`ReplySink`]s — "any client becomes just one
//! more transport". It deliberately holds **no** conversation state beyond
//! queueing: a `session_id` is an opaque string, never a claude session,
//! never persisted.
//!
//! ## Topology
//!
//! [`TriggerGateway::start`] spawns one **supervisor** task. The supervisor
//! owns a map of `(source, session)` → worker channel — this is the
//! per-conversation worker design the daemon's single-worker comment
//! (daemon.rs, "the channel becomes a map keyed by `(source, reply_to)`")
//! planned for: messages from one conversation are answered strictly in
//! order (one FIFO per key), different conversations run concurrently up to
//! [`GatewayConfig::max_concurrent_sessions`].
//!
//! ## Supervision contract
//!
//! The supervisor's `JoinHandle` (via [`GatewayHandle::task`]) must be
//! selected on by the embedder exactly like the daemon selects on its
//! trigger worker: when it finishes — clean return **or** panic — no trigger
//! can be answered anymore and the embedding process must stop rather than
//! tick on deaf. [`GatewayHandle::shutdown`] is the graceful path: stop
//! admitting, let in-flight workers drain for [`SHUTDOWN_DRAIN_GRACE`],
//! abort stragglers, return.
//!
//! A **worker** dying is contained: a panicked worker is logged at ERROR and
//! its conversation heals on the next submit (the supervisor lazily drops
//! map entries whose channel is closed). Only the supervisor's death is
//! fatal.

mod sink;
#[cfg(test)]
pub(crate) mod testutil;
mod worker;

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dotagent_core::slug::is_valid_session_id;
use dotagent_core::{AuditEvent, TriggerRequest, TriggerSource};
use dotagent_runner::{OrchestratedOutcome, StreamOptions};
use dotagent_state::audit::AuditLog;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

use worker::{ConversationWorker, Job};

// Re-exports are the module's public surface; the daemon wiring that uses
// them lands with the integration slice.
#[allow(unused_imports)]
pub use sink::{ReplySink, SinkFuture, TelegramSink};

/// Pending submits before the producer blocks — the same depth the daemon's
/// trigger channel uses, for the same reason: survive a burst, apply
/// backpressure instead of growing without bound.
const SUBMIT_CHANNEL_DEPTH: usize = 64;

/// Per-conversation queue depth. Deep enough that a fast exchange behind one
/// slow run stays queued; a conversation flooding past this is rejected with
/// [`SubmitRejected::QueueFull`] rather than blocking every other
/// conversation.
const WORKER_QUEUE_DEPTH: usize = 64;

/// How long shutdown lets in-flight workers finish before aborting them.
const SHUTDOWN_DRAIN_GRACE: Duration = Duration::from_secs(5);

/// Forced worker cancellation is normally handled immediately by the
/// worker's child-task group. Keep a final supervisor bound for a broken sink
/// or worker that cannot observe the force signal.
const FORCE_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// The future returned by [`GatewayRunner::run_trigger`].
pub type RunFuture = Pin<Box<dyn Future<Output = anyhow::Result<OrchestratedOutcome>> + Send>>;

/// The gateway's executor: run one trigger, report the outcome.
///
/// Injected so the gateway is testable without a filesystem or subprocess —
/// the daemon injects an adapter over `run_triggered`, tests inject a fake.
///
/// Contract:
/// - the returned future runs on its own task, so it must be `'static` and
///   self-contained (clone what it needs from `req` / `stream`);
/// - the stream options must be dropped when the run ends (the delivery pump
///   drains on the tap's drop; a leaked clone is abandoned after a grace);
/// - audit: the gateway emits `AgentTriggered` before calling the runner —
///   an adapter must not emit it again.
pub trait GatewayRunner: Send + Sync {
    fn run_trigger(&self, req: TriggerRequest, stream: StreamOptions) -> RunFuture;

    /// Whether this request's manifest explicitly opts into assistant-v1
    /// reply shaping. Plain agents may emit JSON as ordinary stdout.
    fn uses_assistant_protocol(&self, _req: &TriggerRequest) -> bool {
        false
    }
}

/// Gateway admission policy.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// How many live conversations may run at once. Submits for **new**
    /// conversations beyond this are rejected with
    /// [`SubmitRejected::CapExceeded`]; existing open conversations enqueue,
    /// while a retiring conversation rejects until its slot is removed.
    pub max_concurrent_sessions: usize,
    /// Sliding-window limit for `local` triggers (per actor). Telegram is
    /// *not* limited here — the ingress applies its own `RateLimiter` before
    /// the trigger ever reaches `submit`, and the gateway trusts it. Cli and
    /// MCP sources are operator-driven and unlimited.
    pub local_rate_per_minute: u32,
    /// A worker with no messages for this long retires and frees its session
    /// slot. Bounded so old conversations cannot hold the cap forever.
    pub worker_idle_timeout: Duration,
    #[cfg(test)]
    retirement_control: Option<Arc<RetirementControl>>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            max_concurrent_sessions: 4,
            local_rate_per_minute: 30,
            worker_idle_timeout: Duration::from_secs(30),
            #[cfg(test)]
            retirement_control: None,
        }
    }
}

/// One conversation: the (source, session) pair a worker serializes.
///
/// The session half is `session_id` when present, else `reply_to`, else
/// `"default"` — the same semantics the daemon's redesign note assigns to
/// the map key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationKey {
    pub source: TriggerSource,
    pub session: String,
}

// `TriggerSource` (a core type, not ours to touch in this slice) does not
// implement `Hash`; its Display form ("telegram" | "cli" | "mcp" | "local")
// is injective, so it stands in for the discriminant.
impl std::hash::Hash for ConversationKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.source.to_string().hash(state);
        self.session.hash(state);
    }
}

impl ConversationKey {
    fn of(req: &TriggerRequest) -> Self {
        let session = req
            .session_id
            .clone()
            .or_else(|| req.reply_to.clone())
            .unwrap_or_else(|| "default".to_string());
        Self {
            source: req.source,
            session,
        }
    }
}

/// A trigger admitted into a conversation's queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitAccepted {
    /// The conversation the trigger was routed to.
    pub conversation: ConversationKey,
    /// Live sessions after this submit (including it).
    pub active_sessions: usize,
    /// Jobs already waiting in this conversation's queue ahead of this one.
    pub queued_behind: usize,
}

/// Why a trigger did not enter the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitRejected {
    /// `local` source exceeded [`GatewayConfig::local_rate_per_minute`].
    RateLimited { per_minute: u32 },
    /// A new conversation arrived while [`GatewayConfig::
    /// max_concurrent_sessions`] workers are live.
    CapExceeded { active: usize, max: usize },
    /// `session_id` failed [`is_valid_session_id`] — the harness passes ids
    /// through verbatim, so a dirty one is refused rather than sanitized.
    InvalidSessionId,
    /// The conversation's queue is full — a flood, by definition.
    QueueFull { depth: usize },
    /// The supervisor is gone or shutting down; nobody would answer.
    Unavailable,
}

impl SubmitRejected {
    /// Stable wording for the audit log and warnings.
    pub fn reason(&self) -> &'static str {
        match self {
            SubmitRejected::RateLimited { .. } => "rate limit exceeded",
            SubmitRejected::CapExceeded { .. } => "session cap reached",
            SubmitRejected::InvalidSessionId => "invalid session id",
            SubmitRejected::QueueFull { .. } => "conversation queue full",
            SubmitRejected::Unavailable => "gateway unavailable",
        }
    }
}

enum Command {
    Submit {
        req: TriggerRequest,
        sink: Arc<dyn ReplySink>,
        ack: oneshot::Sender<Result<SubmitAccepted, SubmitRejected>>,
    },
}

struct WorkerSlot {
    tx: mpsc::Sender<Job>,
    retiring: bool,
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct RetirementControl {
    pub(crate) before_close: Arc<tokio::sync::Notify>,
    pub(crate) release_close: Arc<tokio::sync::Notify>,
    pub(crate) after_close: Arc<tokio::sync::Notify>,
    pub(crate) release_drain: Arc<tokio::sync::Notify>,
}

#[cfg(test)]
impl Default for RetirementControl {
    fn default() -> Self {
        Self {
            before_close: Arc::new(tokio::sync::Notify::new()),
            release_close: Arc::new(tokio::sync::Notify::new()),
            after_close: Arc::new(tokio::sync::Notify::new()),
            release_drain: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

/// The client half of the gateway: submit triggers, nothing else.
pub struct TriggerGateway {
    cmd_tx: mpsc::Sender<Command>,
}

impl TriggerGateway {
    /// Spawn the supervisor and return the submit handle plus the
    /// supervision handle. Call from a tokio runtime.
    pub fn start(
        config: GatewayConfig,
        runner: Arc<dyn GatewayRunner>,
        audit: Option<AuditLog>,
    ) -> (Arc<Self>, GatewayHandle) {
        #[cfg(test)]
        let retirement_control = config.retirement_control.clone();
        let (cmd_tx, cmd_rx) = mpsc::channel(SUBMIT_CHANNEL_DEPTH);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (force_shutdown_tx, force_shutdown_rx) = watch::channel(false);
        let supervisor = Supervisor {
            local_limiter: dotagent_notify::telegram_inbound::RateLimiter::new(
                config.local_rate_per_minute,
            ),
            config,
            runner,
            audit,
            workers: Arc::new(Mutex::new(HashMap::new())),
            join_set: JoinSet::new(),
            shutdown_tx: shutdown_tx.clone(),
            force_shutdown_tx,
            force_shutdown_rx,
            #[cfg(test)]
            retirement_control,
        };
        let join = tokio::spawn(supervisor.run(shutdown_rx, cmd_rx));
        (
            Arc::new(Self { cmd_tx }),
            GatewayHandle { join, shutdown_tx },
        )
    }

    /// Submit a trigger for execution. Awaits the admission decision only —
    /// the run itself happens on the conversation's worker.
    ///
    /// The sink travels with the request: whoever submits decides where the
    /// answer goes, which is what makes any client just another transport.
    pub async fn submit(
        &self,
        req: TriggerRequest,
        sink: Arc<dyn ReplySink>,
    ) -> Result<SubmitAccepted, SubmitRejected> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Submit {
                req,
                sink,
                ack: ack_tx,
            })
            .await
            .map_err(|_| SubmitRejected::Unavailable)?;
        ack_rx.await.map_err(|_| SubmitRejected::Unavailable)?
    }
}

/// Supervision handle for the embedder (the daemon, in production).
pub struct GatewayHandle {
    join: tokio::task::JoinHandle<()>,
    shutdown_tx: watch::Sender<bool>,
}

impl GatewayHandle {
    /// The supervisor task. Select on it exactly like the daemon selects on
    /// its trigger worker: this handle finishing — returning or panicking —
    /// means no trigger can be answered anymore and the process must stop.
    pub fn task(&mut self) -> &mut tokio::task::JoinHandle<()> {
        &mut self.join
    }

    /// Graceful stop: no new admits, in-flight workers get
    /// [`SHUTDOWN_DRAIN_GRACE`] to finish, stragglers are aborted.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        // `task()` is also selected by the daemon's main loop. If it already
        // observed a supervisor failure, its JoinHandle output was consumed;
        // polling it a second time would be invalid.
        if self.join.is_finished() {
            return;
        }
        match self.join.await {
            Ok(()) => {}
            Err(e) if e.is_panic() => {
                warn!(error = %e, "gateway supervisor panicked during shutdown")
            }
            Err(e) => warn!(error = %e, "gateway supervisor ended unexpectedly"),
        }
    }
}

/// Owns the conversation map and the admission policy.
struct Supervisor {
    config: GatewayConfig,
    runner: Arc<dyn GatewayRunner>,
    audit: Option<AuditLog>,
    workers: Arc<Mutex<HashMap<ConversationKey, WorkerSlot>>>,
    join_set: JoinSet<ConversationKey>,
    shutdown_tx: watch::Sender<bool>,
    force_shutdown_tx: watch::Sender<bool>,
    force_shutdown_rx: watch::Receiver<bool>,
    local_limiter: dotagent_notify::telegram_inbound::RateLimiter,
    #[cfg(test)]
    retirement_control: Option<Arc<RetirementControl>>,
}

impl Supervisor {
    async fn run(mut self, mut shutdown: watch::Receiver<bool>, mut rx: mpsc::Receiver<Command>) {
        info!(
            max_sessions = self.config.max_concurrent_sessions,
            "gateway supervisor started"
        );
        loop {
            tokio::select! {
                maybe = rx.recv() => match maybe {
                    Some(Command::Submit { req, sink, ack }) => {
                        self.handle_submit(req, sink, ack, &shutdown)
                    },
                    // Every TriggerGateway handle is gone: no submit can
                    // ever arrive again.
                    None => break,
                },
                _ = shutdown.changed() => break,
                Some(joined) = self.join_set.join_next() => match joined {
                    Ok(key) => debug!(session = %key.session, "gateway: conversation worker retired"),
                    Err(e) if e.is_panic() => error!(
                        error = %e,
                        "gateway: conversation worker panicked — gateway continues; the conversation recovers on its next submit"
                    ),
                    Err(e) => warn!(error = %e, "gateway: conversation worker ended unexpectedly"),
                },
            }
        }
        // The command channel can close independently of GatewayHandle. Make
        // the worker signal explicit in that case too, before dropping slots.
        let _ = self.shutdown_tx.send(true);
        // Stop admitting, then drain. Clearing the map drops every worker
        // channel, so each worker finishes its current queue and exits; runs
        // still in flight get the grace, then they are aborted (the runs'
        // subprocesses belong to the daemon's supervisor, not to us).
        self.workers
            .lock()
            .expect("gateway workers map poisoned")
            .clear();
        let drained = async { while self.join_set.join_next().await.is_some() {} };
        if tokio::time::timeout(SHUTDOWN_DRAIN_GRACE, drained)
            .await
            .is_err()
        {
            // Graceful shutdown deliberately leaves the current run alone.
            // Once the grace expires, tell workers to abort and join every
            // child task they own before the supervisor considers them gone.
            let _ = self.force_shutdown_tx.send(true);
            let forced = async { while self.join_set.join_next().await.is_some() {} };
            if tokio::time::timeout(FORCE_SHUTDOWN_GRACE, forced)
                .await
                .is_err()
            {
                self.join_set.abort_all();
                while self.join_set.join_next().await.is_some() {}
                warn!("gateway: aborted workers that outlived forced shutdown");
            }
        }
        info!("gateway supervisor stopped");
    }

    fn handle_submit(
        &mut self,
        req: TriggerRequest,
        sink: Arc<dyn ReplySink>,
        ack: oneshot::Sender<Result<SubmitAccepted, SubmitRejected>>,
        shutdown: &watch::Receiver<bool>,
    ) {
        if *shutdown.borrow() {
            self.reject(&req, ack, SubmitRejected::Unavailable);
            return;
        }

        // 1. Session ids are passed through verbatim, so a dirty one is
        //    refused here — the same predicate the slug layer uses.
        if let Some(sid) = req.session_id.as_deref() {
            if !is_valid_session_id(sid) {
                self.reject(&req, ack, SubmitRejected::InvalidSessionId);
                return;
            }
        }

        // 2. Local sources are rate limited at the gateway; Telegram already
        //    went through the ingress limiter, CLI/MCP are operator-driven.
        if req.source == TriggerSource::Local && !self.local_limiter.check(limiter_key(&req)) {
            self.reject(
                &req,
                ack,
                SubmitRejected::RateLimited {
                    per_minute: self.config.local_rate_per_minute,
                },
            );
            return;
        }

        // 3. Record admission before anything can enqueue or spawn. A trigger
        // that cannot be accounted for must not execute.
        if !self.record_received(&req) {
            self.reject(&req, ack, SubmitRejected::Unavailable);
            return;
        }

        // 4. Route to the conversation's worker.
        let key = ConversationKey::of(&req);
        let mut map = self.workers.lock().expect("gateway workers map poisoned");
        if map.get(&key).is_some_and(|slot| slot.retiring) {
            drop(map);
            self.reject(&req, ack, SubmitRejected::Unavailable);
            return;
        }

        // Heal entries left behind by workers that died without retiring
        // (a panic): a closed channel will never be drained. A retiring slot is
        // handled above and must remain in the map until its drain finishes.
        if map.get(&key).is_some_and(|slot| slot.tx.is_closed()) {
            map.remove(&key);
        }
        let existing = map.get(&key).map(|slot| slot.tx.clone());
        let (tx, queued_behind) = match existing {
            Some(tx) => {
                let queued = tx.max_capacity() - tx.capacity();
                (tx, queued)
            }
            None => {
                if map.len() >= self.config.max_concurrent_sessions {
                    let rejection = SubmitRejected::CapExceeded {
                        active: map.len(),
                        max: self.config.max_concurrent_sessions,
                    };
                    drop(map);
                    self.reject(&req, ack, rejection);
                    return;
                }
                let (tx, rx) = mpsc::channel(WORKER_QUEUE_DEPTH);
                let worker = ConversationWorker::new(
                    key.clone(),
                    Arc::clone(&self.runner),
                    self.audit.clone(),
                    Arc::clone(&self.workers),
                    tx.clone(),
                    self.config.worker_idle_timeout,
                );
                #[cfg(test)]
                let worker = worker.with_retirement_control(self.retirement_control.clone());
                map.insert(
                    key.clone(),
                    WorkerSlot {
                        tx: tx.clone(),
                        retiring: false,
                    },
                );
                self.join_set.spawn(worker.run(
                    rx,
                    shutdown.clone(),
                    self.force_shutdown_rx.clone(),
                ));
                (tx, 0)
            }
        };
        let active_sessions = map.len();
        if tx.capacity() == 0 {
            drop(map);
            self.reject(
                &req,
                ack,
                SubmitRejected::QueueFull {
                    depth: WORKER_QUEUE_DEPTH,
                },
            );
            return;
        }

        // This callback is synchronous and must happen before the job becomes
        // visible to a worker. Local clients use it to order run.started ahead
        // of the accepted response and all run output.
        sink.started(req.session_id.as_deref(), &req.agent);
        match tx.try_send(Job {
            req: req.clone(),
            sink,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                drop(map);
                self.reject(
                    &req,
                    ack,
                    SubmitRejected::QueueFull {
                        depth: WORKER_QUEUE_DEPTH,
                    },
                );
                return;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // The worker died between the map lookup and the send.
                map.remove(&key);
                drop(map);
                self.reject(&req, ack, SubmitRejected::Unavailable);
                return;
            }
        }
        drop(map);
        let _ = ack.send(Ok(SubmitAccepted {
            conversation: key,
            active_sessions,
            queued_behind,
        }));
    }

    fn reject(
        &self,
        req: &TriggerRequest,
        ack: oneshot::Sender<Result<SubmitAccepted, SubmitRejected>>,
        rejection: SubmitRejected,
    ) {
        if let Some(audit) = &self.audit {
            if let Err(e) = audit.append(AuditEvent::TriggerRejected {
                source: req.source.to_string(),
                actor: req.actor.clone().unwrap_or_default(),
                reason: rejection.reason().to_string(),
            }) {
                error!(
                    error = %e,
                    source = %req.source,
                    reason = rejection.reason(),
                    "gateway: failed to append trigger_rejected audit event"
                );
            }
        } else {
            error!(
                source = %req.source,
                reason = rejection.reason(),
                "gateway: cannot append trigger_rejected audit event; audit log unavailable"
            );
        }
        warn!(
            source = %req.source,
            reason = rejection.reason(),
            "gateway: trigger rejected"
        );
        let _ = ack.send(Err(rejection));
    }

    fn record_received(&self, req: &TriggerRequest) -> bool {
        let Some(audit) = &self.audit else {
            error!(
                source = %req.source,
                "gateway: trigger rejected because the audit log is unavailable"
            );
            return false;
        };

        if let Err(e) = audit.append(AuditEvent::TriggerReceived {
            source: req.source.to_string(),
            actor: req.actor.clone().unwrap_or_default(),
            reply_to: req.reply_to.clone().unwrap_or_default(),
        }) {
            error!(
                error = %e,
                source = %req.source,
                "gateway: failed to append trigger_received audit event; trigger rejected"
            );
            return false;
        }
        true
    }
}

/// Rate-limit key for local triggers: the attested actor when present, else
/// the session. Hashed because the reused `RateLimiter` keys on `i64` (its
/// Telegram user ids); the hash is stable for the process's lifetime, which
/// is exactly the limiter's window of validity.
fn limiter_key(req: &TriggerRequest) -> i64 {
    let raw = req
        .actor
        .as_deref()
        .or(req.session_id.as_deref())
        .or(req.reply_to.as_deref())
        .unwrap_or("default");
    let mut hasher = DefaultHasher::new();
    raw.hash(&mut hasher);
    hasher.finish() as i64
}

#[cfg(test)]
mod tests {
    use super::testutil::{local_req, ran_ok, FakeRunner, RecordingSink, Trace};
    use super::*;
    use dotagent_core::TriggerSource;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct DropMarker(Arc<AtomicBool>);

    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct PersistentLockRunner {
        lock: Arc<tokio::sync::Mutex<()>>,
        started: Arc<tokio::sync::Notify>,
        dropped: Arc<AtomicBool>,
    }

    impl GatewayRunner for PersistentLockRunner {
        fn run_trigger(&self, _req: TriggerRequest, _stream: StreamOptions) -> RunFuture {
            let lock = self.lock.clone();
            let started = self.started.clone();
            let dropped = self.dropped.clone();
            Box::pin(async move {
                let _guard = lock.lock().await;
                started.notify_one();
                let _marker = DropMarker(dropped);
                std::future::pending::<anyhow::Result<OrchestratedOutcome>>().await
            })
        }
    }

    fn test_audit() -> AuditLog {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        drop(file);
        AuditLog::with_path(path)
    }

    fn fast_gateway() -> (Arc<TriggerGateway>, GatewayHandle) {
        TriggerGateway::start(
            GatewayConfig::default(),
            Arc::new(FakeRunner::default()),
            Some(test_audit()),
        )
    }

    async fn wait_until(what: &str, pred: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !pred() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("condition never met: {what}"));
    }

    #[tokio::test]
    async fn invalid_session_id_is_rejected() {
        let (gw, handle) = fast_gateway();
        let mut req = local_req("a");
        for bad in ["../x", "a/b", "", "café"] {
            req.session_id = Some(bad.into());
            let rejection = gw
                .submit(req.clone(), Arc::new(RecordingSink::default()))
                .await
                .expect_err("must reject a dirty session id");
            assert_eq!(rejection, SubmitRejected::InvalidSessionId, "{bad:?}");
        }
        // And a clean one passes.
        req.session_id = Some("chat-9_a".into());
        assert!(gw
            .submit(req, Arc::new(RecordingSink::default()))
            .await
            .is_ok());
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn local_rate_limit_rejects_past_the_cap() {
        let config = GatewayConfig {
            local_rate_per_minute: 1,
            ..Default::default()
        };
        let (gw, handle) =
            TriggerGateway::start(config, Arc::new(FakeRunner::default()), Some(test_audit()));
        let sink = Arc::new(RecordingSink::default());

        assert!(gw.submit(local_req("a"), sink.clone()).await.is_ok());
        let rejection = gw
            .submit(local_req("a"), sink.clone())
            .await
            .expect_err("second local hit in the window must be denied");
        assert_eq!(rejection, SubmitRejected::RateLimited { per_minute: 1 });
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn telegram_trusts_the_ingress_limiter() {
        // local rate of 1 must not throttle Telegram — the ingress already
        // applied its own RateLimiter before submit.
        let config = GatewayConfig {
            local_rate_per_minute: 1,
            ..Default::default()
        };
        let (gw, handle) =
            TriggerGateway::start(config, Arc::new(FakeRunner::default()), Some(test_audit()));
        let sink = Arc::new(RecordingSink::default());

        for _ in 0..3 {
            let mut req = local_req("a");
            req.source = TriggerSource::Telegram;
            req.reply_to = Some("99".into());
            assert!(
                gw.submit(req, sink.clone()).await.is_ok(),
                "telegram must bypass the local limiter"
            );
        }
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn cap_rejects_new_conversations_when_full() {
        let (release_tx, release_rx) = tokio::sync::watch::channel(false);
        let runner = FakeRunner {
            hold: Some(release_rx),
            ..Default::default()
        };
        let config = GatewayConfig {
            max_concurrent_sessions: 1,
            ..Default::default()
        };
        let (gw, handle) = TriggerGateway::start(config, Arc::new(runner), Some(test_audit()));
        let sink = Arc::new(RecordingSink::default());

        let mut a = local_req("a");
        a.reply_to = Some("chat-a".into());
        let accepted = gw
            .submit(a, sink.clone())
            .await
            .expect("first conversation is admitted");
        assert_eq!(accepted.active_sessions, 1);

        let mut b = local_req("a");
        b.reply_to = Some("chat-b".into());
        let rejection = gw
            .submit(b, sink.clone())
            .await
            .expect_err("a second live conversation must not fit");
        assert_eq!(rejection, SubmitRejected::CapExceeded { active: 1, max: 1 });

        release_tx.send(true).unwrap();
        wait_until("the held run replies", || sink.replies() == 1).await;
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn same_conversation_is_serialized() {
        let trace = Arc::new(Trace::default());
        let runner = FakeRunner {
            delay: Duration::from_millis(40),
            trace: Some(trace.clone()),
            ..Default::default()
        };
        let (gw, handle) = TriggerGateway::start(
            GatewayConfig::default(),
            Arc::new(runner),
            Some(test_audit()),
        );
        let sink = Arc::new(RecordingSink::default());

        for label in ["first", "second"] {
            let mut req = local_req("a");
            req.reply_to = Some("chat-1".into());
            req.args = vec![label.into()];
            gw.submit(req, sink.clone()).await.expect("admitted");
        }
        wait_until("both runs answered", || sink.replies() == 2).await;
        handle.shutdown().await;

        assert_eq!(trace.max_active(), 1, "one conversation, one run at a time");
        assert_eq!(
            trace.events(),
            vec![
                "start first".to_string(),
                "end first".to_string(),
                "start second".to_string(),
                "end second".to_string(),
            ],
            "answers preserve arrival order"
        );
    }

    #[tokio::test]
    async fn distinct_conversations_run_concurrently() {
        // Barrier of 2: each run blocks until both are in flight. Serialized
        // conversations would deadlock against it and hit the timeout.
        let trace = Arc::new(Trace::default());
        let runner = FakeRunner {
            barrier: Some(Arc::new(tokio::sync::Barrier::new(2))),
            trace: Some(trace.clone()),
            ..Default::default()
        };
        let (gw, handle) = TriggerGateway::start(
            GatewayConfig::default(),
            Arc::new(runner),
            Some(test_audit()),
        );
        let sink = Arc::new(RecordingSink::default());

        for chat in ["chat-a", "chat-b"] {
            let mut req = local_req("a");
            req.reply_to = Some(chat.into());
            gw.submit(req, sink.clone()).await.expect("admitted");
        }
        wait_until("both runs answered", || sink.replies() == 2).await;
        handle.shutdown().await;

        assert_eq!(trace.max_active(), 2, "distinct keys must overlap");
    }

    #[tokio::test]
    async fn reply_shaping_uses_the_last_reply_frame() {
        let runner = FakeRunner {
            lines: vec![
                r#"{"type":"delta","text":"partial"}"#.into(),
                r#"{"type":"reply","text":"first answer"}"#.into(),
                r#"{"type":"reply","text":"final answer"}"#.into(),
                r#"{"type":"session","claude_session":"s-1","transcript_bytes":10}"#.into(),
            ],
            outcome: Ok(ran_ok("raw stdout")),
            assistant_protocol: true,
            ..Default::default()
        };
        let (gw, handle) = TriggerGateway::start(
            GatewayConfig::default(),
            Arc::new(runner),
            Some(test_audit()),
        );
        let sink = Arc::new(RecordingSink::default());

        let mut req = local_req("a");
        req.session_id = Some("s1".into());
        gw.submit(req, sink.clone()).await.expect("admitted");
        wait_until("the run replies", || sink.replies() == 1).await;
        handle.shutdown().await;

        let events = sink.events();
        // Admission is visible before the worker can emit anything.
        assert_eq!(events.first().map(String::as_str), Some("started[s1] a"));
        assert_eq!(events.get(1).map(String::as_str), Some("typing[s1]"));
        // Raw lines stream in arrival order — protocol frames included; the
        // client interprets them.
        let deltas: Vec<&String> = events
            .iter()
            .filter(|e| e.starts_with("delta[s1]"))
            .collect();
        assert_eq!(deltas.len(), 4, "every raw line is forwarded");
        assert!(deltas[0].ends_with(r#"{"type":"delta","text":"partial"}"#));
        // The final reply is the last reply frame, not raw stdout.
        assert_eq!(
            events.last().map(String::as_str),
            Some("reply[s1] final answer")
        );
    }

    #[tokio::test]
    async fn plain_stdout_becomes_the_reply() {
        let runner = FakeRunner {
            lines: vec!["hello".into(), "world".into()],
            outcome: Ok(ran_ok("hello\nworld")),
            ..Default::default()
        };
        let (gw, handle) = TriggerGateway::start(
            GatewayConfig::default(),
            Arc::new(runner),
            Some(test_audit()),
        );
        let sink = Arc::new(RecordingSink::default());

        gw.submit(local_req("echoer"), sink.clone())
            .await
            .expect("admitted");
        wait_until("the run replies", || sink.replies() == 1).await;
        handle.shutdown().await;

        assert_eq!(
            sink.events().last().map(String::as_str),
            Some("reply[-] hello\nworld")
        );
    }

    #[tokio::test]
    async fn plain_json_stdout_stays_raw_without_protocol() {
        let line = r#"{"type":"reply","text":"looks like a protocol frame"}"#;
        let runner = FakeRunner {
            lines: vec![line.into()],
            outcome: Ok(ran_ok(line)),
            ..Default::default()
        };
        let (gw, handle) = TriggerGateway::start(
            GatewayConfig::default(),
            Arc::new(runner),
            Some(test_audit()),
        );
        let sink = Arc::new(RecordingSink::default());

        gw.submit(local_req("plain-agent"), sink.clone())
            .await
            .expect("admitted");
        wait_until("the run replies", || sink.replies() == 1).await;
        handle.shutdown().await;

        let events = sink.events();
        assert!(events.iter().any(|event| {
            event == "delta[-] {\"type\":\"reply\",\"text\":\"looks like a protocol frame\"}"
        }));
        assert_eq!(
            events.last().map(String::as_str),
            Some("reply[-] {\"type\":\"reply\",\"text\":\"looks like a protocol frame\"}")
        );
    }

    #[tokio::test]
    async fn empty_stdout_names_the_agent() {
        let runner = FakeRunner {
            outcome: Ok(ran_ok("   ")),
            ..Default::default()
        };
        let (gw, handle) = TriggerGateway::start(
            GatewayConfig::default(),
            Arc::new(runner),
            Some(test_audit()),
        );
        let sink = Arc::new(RecordingSink::default());

        gw.submit(local_req("echoer"), sink.clone())
            .await
            .expect("admitted");
        wait_until("the run replies", || sink.replies() == 1).await;
        handle.shutdown().await;

        assert_eq!(
            sink.events().last().map(String::as_str),
            Some("reply[-] echoer finished with no output.")
        );
    }

    #[tokio::test]
    async fn idle_worker_retires_and_frees_the_cap() {
        let config = GatewayConfig {
            max_concurrent_sessions: 1,
            worker_idle_timeout: Duration::from_millis(80),
            ..Default::default()
        };
        let (gw, handle) =
            TriggerGateway::start(config, Arc::new(FakeRunner::default()), Some(test_audit()));
        let sink = Arc::new(RecordingSink::default());

        let mut a = local_req("a");
        a.reply_to = Some("chat-a".into());
        gw.submit(a, sink.clone()).await.expect("admitted");
        wait_until("the run replies", || sink.replies() == 1).await;

        // Long enough for the idle worker to retire. Without reaping, the
        // second conversation would be CapExceeded forever.
        tokio::time::sleep(Duration::from_millis(250)).await;
        let mut b = local_req("a");
        b.reply_to = Some("chat-b".into());
        gw.submit(b, sink.clone())
            .await
            .expect("the retired worker's slot must be free");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn idle_submit_race_keeps_fifo_and_single_runner() {
        let retirement = Arc::new(RetirementControl::default());
        let trace = Arc::new(Trace::default());
        let runner = FakeRunner {
            trace: Some(trace.clone()),
            ..Default::default()
        };
        let config = GatewayConfig {
            worker_idle_timeout: Duration::from_millis(30),
            retirement_control: Some(retirement.clone()),
            ..Default::default()
        };
        let (gw, handle) = TriggerGateway::start(config, Arc::new(runner), Some(test_audit()));
        let sink = Arc::new(RecordingSink::default());

        let mut first = local_req("worker");
        first.reply_to = Some("chat-1".into());
        first.args = vec!["first".into()];
        gw.submit(first, sink.clone())
            .await
            .expect("first admitted");
        wait_until("first run", || sink.replies() == 1).await;

        // The worker has selected its idle branch but has not closed its slot.
        retirement.before_close.notified().await;
        let mut second = local_req("worker");
        second.reply_to = Some("chat-1".into());
        second.args = vec!["second".into()];
        gw.submit(second, sink.clone())
            .await
            .expect("the racing job is accepted by the old worker");

        retirement.release_close.notify_one();
        retirement.after_close.notified().await;

        let mut third = local_req("worker");
        third.reply_to = Some("chat-1".into());
        third.args = vec!["third".into()];
        let rejection = gw
            .submit(third, sink.clone())
            .await
            .expect_err("a retiring worker must not get a replacement");
        assert_eq!(rejection, SubmitRejected::Unavailable);

        retirement.release_drain.notify_one();
        wait_until("the drained job", || sink.replies() == 2).await;
        handle.shutdown().await;

        assert_eq!(
            trace.max_active(),
            1,
            "the old worker must remain exclusive"
        );
        assert_eq!(
            trace.events(),
            vec![
                "start first".to_string(),
                "end first".to_string(),
                "start second".to_string(),
                "end second".to_string(),
            ],
            "accepted jobs preserve FIFO while the worker retires"
        );
    }

    #[tokio::test]
    async fn audit_records_received_rejected_and_triggered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let audit = AuditLog::with_path(path.clone());
        let (gw, handle) = TriggerGateway::start(
            GatewayConfig::default(),
            Arc::new(FakeRunner::default()),
            Some(audit),
        );
        let sink = Arc::new(RecordingSink::default());

        let mut received = local_req("a");
        received.payload = Some(serde_json::json!({
            "text": "/private/path/with-secret"
        }));
        gw.submit(received, sink.clone()).await.expect("admitted");
        let mut dirty = local_req("a");
        dirty.session_id = Some("../x".into());
        let _ = gw.submit(dirty, sink).await;
        // Audit events serialize snake_case (`event_type` tag).
        wait_until("the run is audited", || {
            std::fs::read_to_string(&path)
                .map(|raw| raw.contains("agent_triggered"))
                .unwrap_or(false)
        })
        .await;
        handle.shutdown().await;

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("trigger_received"), "{raw}");
        assert!(raw.contains("trigger_rejected"), "{raw}");
        assert!(raw.contains("agent_triggered"), "{raw}");
        assert!(!raw.contains("/private/path/with-secret"), "{raw}");
    }

    #[tokio::test]
    async fn received_audit_failure_rejects_before_worker_creation() {
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLog::with_path(dir.path().to_path_buf());
        let trace = Arc::new(Trace::default());
        let runner = FakeRunner {
            trace: Some(trace.clone()),
            ..Default::default()
        };
        let (gw, handle) =
            TriggerGateway::start(GatewayConfig::default(), Arc::new(runner), Some(audit));
        let sink = Arc::new(RecordingSink::default());

        let rejection = gw
            .submit(local_req("never-runs"), sink)
            .await
            .expect_err("audit failure must fail closed");
        assert_eq!(rejection, SubmitRejected::Unavailable);
        assert!(trace.events().is_empty(), "the runner must not be called");

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn triggered_audit_failure_replies_safely_and_worker_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let backup = dir.path().join("audit.log.backup");
        let audit = AuditLog::with_path(path.clone());
        let (release_tx, release_rx) = tokio::sync::watch::channel(false);
        let trace = Arc::new(Trace::default());
        let runner = FakeRunner {
            hold: Some(release_rx),
            trace: Some(trace.clone()),
            ..Default::default()
        };
        let (gw, handle) =
            TriggerGateway::start(GatewayConfig::default(), Arc::new(runner), Some(audit));
        let sink = Arc::new(RecordingSink::default());

        let mut first = local_req("worker");
        first.args = vec!["first".into()];
        gw.submit(first, sink.clone())
            .await
            .expect("first admitted");
        wait_until("first trigger audit", || {
            std::fs::read_to_string(&path)
                .map(|raw| raw.matches("agent_triggered").count() == 1)
                .unwrap_or(false)
        })
        .await;

        let mut second = local_req("worker");
        second.args = vec!["second".into()];
        gw.submit(second, sink.clone())
            .await
            .expect("second admitted behind the held run");

        std::fs::rename(&path, &backup).unwrap();
        std::fs::create_dir(&path).unwrap();
        release_tx.send(true).unwrap();

        wait_until("the failed audit reply", || sink.replies() == 2).await;
        let events = sink.events();
        assert!(
            events
                .iter()
                .any(|event| event == "reply[-] Could not run trigger: audit unavailable."),
            "audit failure must produce a safe sink reply: {events:?}"
        );
        assert!(
            events
                .iter()
                .all(|event| !event.contains(path.to_string_lossy().as_ref())),
            "the audit path must not reach the sink: {events:?}"
        );
        assert_eq!(
            trace.events(),
            vec!["start first".to_string(), "end first".to_string()],
            "the failed job must not invoke the runner"
        );

        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&backup, &path).unwrap();
        let mut third = local_req("worker");
        third.args = vec!["third".into()];
        gw.submit(third, sink.clone())
            .await
            .expect("the worker must continue after an audit failure");
        wait_until("the worker's next run", || sink.replies() == 3).await;
        handle.shutdown().await;

        assert_eq!(
            trace.events(),
            vec![
                "start first".to_string(),
                "end first".to_string(),
                "start third".to_string(),
                "end third".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn submit_after_shutdown_reports_unavailable() {
        let (gw, handle) = fast_gateway();
        handle.shutdown().await;
        let rejection = gw
            .submit(local_req("a"), Arc::new(RecordingSink::default()))
            .await
            .expect_err("a stopped gateway cannot admit");
        assert_eq!(rejection, SubmitRejected::Unavailable);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_aborts_and_joins_a_persistent_request_task() {
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let started = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let runner = PersistentLockRunner {
            lock: lock.clone(),
            started: started.clone(),
            dropped: dropped.clone(),
        };
        let (gw, handle) = TriggerGateway::start(
            GatewayConfig::default(),
            Arc::new(runner),
            Some(test_audit()),
        );

        gw.submit(local_req("persistent"), Arc::new(RecordingSink::default()))
            .await
            .expect("persistent request admitted");
        started.notified().await;

        let shutdown_task = tokio::spawn(handle.shutdown());
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(SHUTDOWN_DRAIN_GRACE + FORCE_SHUTDOWN_GRACE + Duration::from_secs(1))
            .await;
        tokio::time::timeout(Duration::from_secs(1), shutdown_task)
            .await
            .expect("gateway shutdown must stay bounded")
            .expect("shutdown task must not panic");

        assert!(
            dropped.load(Ordering::SeqCst),
            "the runner child task must be joined and dropped"
        );
        assert!(
            lock.try_lock().is_ok(),
            "shutdown must release the persistent instance lock"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_stops_idle_workers_without_waiting_for_idle_timeout() {
        let config = GatewayConfig {
            worker_idle_timeout: Duration::from_secs(60 * 60),
            ..Default::default()
        };
        let (gw, handle) =
            TriggerGateway::start(config, Arc::new(FakeRunner::default()), Some(test_audit()));
        let sink = Arc::new(RecordingSink::default());

        gw.submit(local_req("idle"), sink.clone())
            .await
            .expect("admitted");
        wait_until("the worker becomes idle", || sink.replies() == 1).await;

        tokio::time::timeout(Duration::from_secs(1), handle.shutdown())
            .await
            .expect("shutdown must not wait for an idle worker timeout");
    }

    #[tokio::test]
    async fn shutdown_drains_pending_runs() {
        let trace = Arc::new(Trace::default());
        let runner = FakeRunner {
            delay: Duration::from_millis(30),
            trace: Some(trace.clone()),
            ..Default::default()
        };
        let (gw, handle) = TriggerGateway::start(
            GatewayConfig::default(),
            Arc::new(runner),
            Some(test_audit()),
        );
        let sink = Arc::new(RecordingSink::default());

        for label in ["one", "two"] {
            let mut req = local_req("a");
            req.reply_to = Some("chat-1".into());
            req.args = vec![label.into()];
            gw.submit(req, sink.clone()).await.expect("admitted");
        }
        // Graceful shutdown waits for the queue to drain (well under grace).
        handle.shutdown().await;
        assert_eq!(sink.replies(), 2, "queued work finished before exit");
        assert_eq!(trace.max_active(), 1);
    }
}
