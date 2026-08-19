//! Per-conversation worker: runs one trigger at a time and shapes delivery.
//!
//! A worker owns exactly one [`ConversationKey`]. Messages routed to it are
//! processed strictly in arrival order (the ordering guarantee the daemon's
//! single-worker design exists to keep), while different conversations run
//! concurrently up to the gateway's cap.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::anyhow;
use dotagent_core::assistant::{self, AssistantEvent};
use dotagent_core::{AuditEvent, TriggerRequest, TRIGGER_SCHEDULE_ID};
use dotagent_notify::telegram_inbound::TYPING_REFRESH;
use dotagent_runner::{OrchestratedOutcome, StreamOptions};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, error, warn};

use super::sink::{ReplySink, SinkFuture};
use super::{ConversationKey, GatewayRunner, WorkerSlot};

/// How long the delta pump gets to drain buffered lines after the runner
/// returns. The tap normally dies with the runner's stream; a badly behaved
/// runner that keeps a clone of it alive must not wedge the worker.
const PUMP_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// The stdout tap is synchronous, so it must never wait for a slow sink. A
/// full queue drops the newest delta; the protocol shaper still sees every
/// line and the terminal reply is delivered through a separate path.
const DELTA_CHANNEL_DEPTH: usize = 64;

/// One admitted trigger waiting for its conversation's turn.
pub(super) struct Job {
    pub req: TriggerRequest,
    pub sink: Arc<dyn ReplySink>,
}

/// Accumulates the final reply from assistant-protocol frames seen on stdout.
///
/// When enabled, protocol detection says the run "spoke the protocol" the
/// moment **any** line parses as an [`AssistantEvent`]. The final reply is the
/// **last** `reply` frame; a run that parsed frames but never sent one falls
/// back to raw stdout. `session` frames are parsed (they count for detection)
/// and otherwise ignored — the gateway is a harness and does not persist
/// assistant sessions.
#[derive(Default)]
struct ReplyShaper {
    enabled: bool,
    saw_protocol: bool,
    last_reply: Option<String>,
}

impl ReplyShaper {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ..Self::default()
        }
    }

    fn feed(&mut self, line: &str) {
        if !self.enabled {
            return;
        }
        if let Some(event) = assistant::parse_line(line) {
            self.saw_protocol = true;
            if let AssistantEvent::Reply { text } = event {
                self.last_reply = Some(text);
            }
        }
    }
}

/// Shape the delivered reply from a run's outcome.
///
/// Preserves the daemon's former direct-trigger wording on non-protocol paths.
/// The one addition: when the run spoke the assistant protocol, the last
/// `reply` frame replaces raw stdout.
fn shape_final_reply(
    agent: &str,
    outcome: anyhow::Result<OrchestratedOutcome>,
    shaper: &ReplyShaper,
) -> String {
    match outcome {
        Ok(OrchestratedOutcome::Ran(run)) if run.exit_code == 0 => {
            if shaper.enabled {
                if let Some(reply) = &shaper.last_reply {
                    return reply.clone();
                }
            }
            if run.stdout_tail.trim().is_empty() {
                format!("{agent} finished with no output.")
            } else {
                run.stdout_tail
            }
        }
        Ok(OrchestratedOutcome::Ran(run)) => {
            warn!(
                exit_code = run.exit_code,
                timed_out = run.timed_out,
                "gateway: triggered run failed"
            );
            let what = if run.timed_out {
                "timed out".to_string()
            } else {
                format!("exited {}", run.exit_code)
            };
            format!("{agent} {what}.\n{}", run.stderr_tail)
        }
        Ok(OrchestratedOutcome::PreflightFailed { plugin, suggest }) => {
            let hint = suggest.map(|s| format!(": {s}")).unwrap_or_default();
            format!("{agent} blocked by preflight {plugin}{hint}")
        }
        Err(e) => {
            warn!(error = %e, "gateway: trigger could not run");
            format!("Could not run {agent}: {e}")
        }
    }
}

/// The conversation worker loop.
pub(super) struct ConversationWorker {
    key: ConversationKey,
    runner: Arc<dyn GatewayRunner>,
    audit: Option<dotagent_state::audit::AuditLog>,
    workers: Arc<Mutex<HashMap<ConversationKey, WorkerSlot>>>,
    /// Our own sender, so retirement only changes/removes *our* entry — never
    /// a replacement spawned after a race.
    own_tx: mpsc::Sender<Job>,
    idle_timeout: Duration,
    #[cfg(test)]
    retirement_control: Option<Arc<super::RetirementControl>>,
}

#[derive(Default)]
struct DeltaStats {
    dropped: AtomicUsize,
}

impl DeltaStats {
    fn record_drop(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }

    fn report(&self, req: &TriggerRequest) {
        let dropped = self.dropped();
        if dropped > 0 {
            warn!(
                source = %req.source,
                agent = %req.agent,
                session = req.session_id.as_deref().unwrap_or("-"),
                dropped,
                channel_capacity = DELTA_CHANNEL_DEPTH,
                "gateway: stdout deltas dropped because the sink was slower than the runner"
            );
        }
    }
}

type RunTask = tokio::task::JoinHandle<anyhow::Result<OrchestratedOutcome>>;

/// Owns every task created while processing one job.
///
/// Normal completion and forced shutdown both await these handles. `Drop` is
/// only the last line of defence for a worker panic or an unexpected abort:
/// it requests cancellation so a child cannot keep a persistent-pool lock.
struct ChildTasks {
    run: Option<RunTask>,
    pump: Option<tokio::task::JoinHandle<()>>,
    typing: Option<tokio::task::JoinHandle<()>>,
}

impl ChildTasks {
    async fn abort_and_join(&mut self) {
        if let Some(task) = self.run.as_mut() {
            abort_and_join_task(task, "gateway runner").await;
        }
        self.run = None;
        if let Some(task) = self.pump.as_mut() {
            abort_and_join_task(task, "gateway delta pump").await;
        }
        self.pump = None;
        if let Some(task) = self.typing.as_mut() {
            abort_and_join_task(task, "gateway typing loop").await;
        }
        self.typing = None;
    }
}

impl Drop for ChildTasks {
    fn drop(&mut self) {
        if let Some(task) = self.run.as_ref() {
            task.abort();
        }
        if let Some(task) = self.pump.as_ref() {
            task.abort();
        }
        if let Some(task) = self.typing.as_ref() {
            task.abort();
        }
    }
}

enum ProcessResult {
    Finished,
    ForcedShutdown,
}

impl ConversationWorker {
    // Each parameter is a distinct worker dependency; grouping them would make
    // ownership and test-only replacement of individual dependencies less clear.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        key: ConversationKey,
        runner: Arc<dyn GatewayRunner>,
        audit: Option<dotagent_state::audit::AuditLog>,
        workers: Arc<Mutex<HashMap<ConversationKey, WorkerSlot>>>,
        own_tx: mpsc::Sender<Job>,
        idle_timeout: Duration,
    ) -> Self {
        Self {
            key,
            runner,
            audit,
            workers,
            own_tx,
            idle_timeout,
            #[cfg(test)]
            retirement_control: None,
        }
    }

    #[cfg(test)]
    pub(super) fn with_retirement_control(
        mut self,
        retirement_control: Option<Arc<super::RetirementControl>>,
    ) -> Self {
        self.retirement_control = retirement_control;
        self
    }

    /// Drain the conversation queue until it goes idle, then retire.
    ///
    /// Returns the key on a normal exit so the supervisor can log it (a
    /// panicked worker never returns — its map entry is healed lazily on the
    /// conversation's next submit, via the `is_closed` check).
    pub(super) async fn run(
        self,
        mut rx: mpsc::Receiver<Job>,
        mut shutdown: watch::Receiver<bool>,
        mut force_shutdown: watch::Receiver<bool>,
    ) -> ConversationKey {
        let idle = self.idle_timeout;
        loop {
            let job = tokio::select! {
                biased;
                maybe = rx.recv() => match maybe {
                    Some(job) => job,
                    // The supervisor dropped every sender (shutdown).
                    None => break,
                },
                _ = shutdown.changed() => break,
                _ = force_shutdown.changed() => break,
                _ = tokio::time::sleep(idle) => {
                    #[cfg(test)]
                    if let Some(control) = &self.retirement_control {
                        control.before_close.notify_one();
                        control.release_close.notified().await;
                    }
                    // Close the receiver and mark the slot while holding the
                    // same mutex admission uses. This prevents a replacement
                    // from starting while this worker drains accepted jobs.
                    self.begin_retirement(&mut rx);
                    #[cfg(test)]
                    if let Some(control) = &self.retirement_control {
                        control.after_close.notify_one();
                        control.release_drain.notified().await;
                    }
                    while let Ok(job) = rx.try_recv() {
                        if matches!(
                            self.process(job, force_shutdown.clone()).await,
                            ProcessResult::ForcedShutdown
                        ) {
                            break;
                        }
                    }
                    self.finish_retirement();
                    break;
                }
            };
            if matches!(
                self.process(job, force_shutdown.clone()).await,
                ProcessResult::ForcedShutdown
            ) {
                break;
            }
        }
        // Do this explicitly: the worker owns a sender even after the
        // supervisor drops the map entry, so relying on field drop keeps an
        // idle worker alive until the supervisor's drain grace expires.
        drop(self.own_tx);
        self.key
    }

    /// Mark this slot closed before draining. Submits that arrive after this
    /// point are rejected instead of being accepted into a worker that is
    /// about to exit.
    fn begin_retirement(&self, rx: &mut mpsc::Receiver<Job>) {
        let mut map = self.workers.lock().expect("gateway workers map poisoned");
        if let Some(slot) = map.get_mut(&self.key) {
            if slot.tx.same_channel(&self.own_tx) {
                slot.retiring = true;
            }
        }
        rx.close();
    }

    /// Remove our entry only after every job accepted before the close marker
    /// has been processed.
    fn finish_retirement(&self) {
        let mut map = self.workers.lock().expect("gateway workers map poisoned");
        let still_mine = map
            .get(&self.key)
            .is_some_and(|slot| slot.retiring && slot.tx.same_channel(&self.own_tx));
        if still_mine {
            map.remove(&self.key);
        }
    }

    /// Run one trigger end to end: audit, typing, stream, shape, deliver.
    async fn process(&self, job: Job, mut force_shutdown: watch::Receiver<bool>) -> ProcessResult {
        let Job { req, sink } = job;
        let session = req.session_id.clone();

        if *force_shutdown.borrow() {
            return ProcessResult::ForcedShutdown;
        }

        if !self.record_triggered(&req) {
            // Do not expose the AuditLog error: it can contain a filesystem
            // path, and the trigger payload may contain secrets.
            let delivered = await_sink_or_force(
                sink.reply(
                    session.as_deref(),
                    "Could not run trigger: audit unavailable.",
                ),
                &mut force_shutdown,
            )
            .await;
            return if delivered {
                ProcessResult::Finished
            } else {
                ProcessResult::ForcedShutdown
            };
        }

        // One indicator before anything else, so even a run that fails in
        // milliseconds shows it was heard. The refresh loop keeps it alive
        // for longer runs; it stops before the reply, so it never outlives it.
        if !await_sink_or_force(sink.typing(session.as_deref()), &mut force_shutdown).await {
            return ProcessResult::ForcedShutdown;
        }
        let (typing_stop, typing_task) = spawn_typing_loop(sink.clone(), session.clone());

        // Raw stdout lines flow to the sink in arrival order while the shaper
        // accumulates the protocol reply. The tap is sync by contract, so it
        // only parses and enqueues; the pump does the async delivering.
        let (line_tx, line_rx) = mpsc::channel::<String>(DELTA_CHANNEL_DEPTH);
        let pump = tokio::spawn(delta_pump(line_rx, sink.clone(), session.clone()));
        let delta_stats = Arc::new(DeltaStats::default());
        let shaper = Arc::new(Mutex::new(ReplyShaper::new(
            self.runner.uses_assistant_protocol(&req),
        )));
        let tap_shaper = shaper.clone();
        let tap_stats = delta_stats.clone();
        let stream = StreamOptions {
            on_stdout_line: Some(Arc::new(move |line: &str| {
                tap_shaper.lock().expect("reply shaper poisoned").feed(line);
                enqueue_delta(&line_tx, &tap_stats, line);
            })),
        };

        // The runner executes on its own task so a panic inside it surfaces
        // as an error reply instead of unwinding this worker — one bad run
        // must not take the conversation's whole queue with it. All children
        // stay in this guard, so forced shutdown aborts and joins them.
        let mut children = ChildTasks {
            run: Some(tokio::spawn(self.runner.run_trigger(req.clone(), stream))),
            pump: Some(pump),
            typing: Some(typing_task),
        };
        let run_result = tokio::select! {
            biased;
            result = children.run.as_mut().expect("runner task missing") => Some(result),
            _ = wait_for_force(&mut force_shutdown) => None,
        };
        let Some(run_result) = run_result else {
            children.abort_and_join().await;
            delta_stats.report(&req);
            return ProcessResult::ForcedShutdown;
        };
        children.run = None;
        let outcome = match run_result {
            Ok(outcome) => outcome,
            Err(e) if e.is_panic() => Err(anyhow!("agent runner panicked: {e}")),
            Err(e) => Err(anyhow!("agent runner task failed: {e}")),
        };

        // The tap went down with the runner's stream; give the pump a moment
        // to deliver what it buffered, then cut it loose.
        let pump_finished = finish_delta_pump(
            children.pump.as_mut().expect("delta pump missing"),
            &mut force_shutdown,
        )
        .await;
        children.pump = None;
        if !pump_finished {
            children.abort_and_join().await;
            delta_stats.report(&req);
            return ProcessResult::ForcedShutdown;
        }

        let typing_finished = stop_typing_loop(
            typing_stop,
            children.typing.as_mut().expect("typing task missing"),
            &mut force_shutdown,
        )
        .await;
        children.typing = None;
        if !typing_finished {
            children.abort_and_join().await;
            delta_stats.report(&req);
            return ProcessResult::ForcedShutdown;
        }

        // Scope the lock so the guard is provably gone before the await.
        let reply = {
            let shaper = shaper.lock().expect("reply shaper poisoned");
            debug!(
                protocol = shaper.saw_protocol,
                "gateway: shaping final reply"
            );
            shape_final_reply(&req.agent, outcome, &shaper)
        };
        delta_stats.report(&req);
        if !await_sink_or_force(sink.reply(session.as_deref(), &reply), &mut force_shutdown).await {
            children.abort_and_join().await;
            return ProcessResult::ForcedShutdown;
        }
        ProcessResult::Finished
    }

    fn record_triggered(&self, req: &TriggerRequest) -> bool {
        let Some(audit) = &self.audit else {
            error!(
                source = %req.source,
                agent = %req.agent,
                "gateway: cannot append agent_triggered audit event; audit log unavailable"
            );
            return false;
        };

        if let Err(e) = audit.append(AuditEvent::AgentTriggered {
            source: req.source.to_string(),
            actor: req.actor.clone(),
            agent: req.agent.clone(),
            // The gateway never resolves schedules — the runner does.
            // Recorded as the requested id, or the trigger pseudo-id.
            schedule: req
                .schedule
                .clone()
                .unwrap_or_else(|| TRIGGER_SCHEDULE_ID.to_string()),
        }) {
            error!(
                error = %e,
                source = %req.source,
                agent = %req.agent,
                "gateway: failed to append agent_triggered audit event; trigger will not run"
            );
            return false;
        }
        true
    }
}

/// Refresh the typing indicator until the stop channel fires.
///
/// Uses Telegram's refresh cadence (`sendChatAction` expires after ~5s);
/// sinks whose indicator does not expire simply see repeated no-op calls.
fn spawn_typing_loop(
    sink: Arc<dyn ReplySink>,
    session: Option<String>,
) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(TYPING_REFRESH) => {
                    let _ = sink.typing(session.as_deref()).await;
                }
                // Also fires when the sender is dropped (worker aborted),
                // so an orphaned loop can never keep typing forever.
                _ = &mut stop_rx => break,
            }
        }
    });
    (stop_tx, task)
}

fn enqueue_delta(tx: &mpsc::Sender<String>, stats: &DeltaStats, line: &str) {
    if let Err(error) = tx.try_send(line.to_string()) {
        if matches!(error, mpsc::error::TrySendError::Full(_)) {
            stats.record_drop();
        }
    }
}

async fn wait_for_force(force_shutdown: &mut watch::Receiver<bool>) {
    if !*force_shutdown.borrow() {
        let _ = force_shutdown.changed().await;
    }
}

async fn await_sink_or_force<'a>(
    delivery: SinkFuture<'a>,
    force_shutdown: &mut watch::Receiver<bool>,
) -> bool {
    if *force_shutdown.borrow() {
        return false;
    }
    tokio::pin!(delivery);
    tokio::select! {
        biased;
        _ = &mut delivery => true,
        _ = wait_for_force(force_shutdown) => false,
    }
}

async fn abort_and_join_task<T>(task: &mut tokio::task::JoinHandle<T>, name: &'static str) {
    task.abort();
    if let Err(error) = task.await {
        if !error.is_cancelled() {
            warn!(task = name, error = %error, "gateway child task failed while stopping");
        }
    }
}

/// Deliver raw stdout lines to the sink in arrival order.
async fn delta_pump(
    mut rx: mpsc::Receiver<String>,
    sink: Arc<dyn ReplySink>,
    session: Option<String>,
) {
    while let Some(line) = rx.recv().await {
        sink.delta(session.as_deref(), &line).await;
    }
}

/// Give buffered deltas a bounded drain window, then cancel and join the pump
/// so a blocked sink cannot remain detached after the worker moves on.
async fn finish_delta_pump(
    pump: &mut tokio::task::JoinHandle<()>,
    force_shutdown: &mut watch::Receiver<bool>,
) -> bool {
    if *force_shutdown.borrow() {
        abort_and_join_task(pump, "gateway delta pump").await;
        return false;
    }
    tokio::select! {
        biased;
        result = &mut *pump => {
            match result {
                Ok(()) => {},
                Err(error) => warn!(error = %error, "gateway: delta pump task failed"),
            }
            true
        }
        _ = tokio::time::sleep(PUMP_DRAIN_GRACE) => {
            warn!("gateway: delta pump did not drain in time — aborting");
            abort_and_join_task(pump, "gateway delta pump").await;
            true
        }
        _ = wait_for_force(force_shutdown) => {
            abort_and_join_task(pump, "gateway delta pump").await;
            false
        }
    }
}

async fn stop_typing_loop(
    stop: oneshot::Sender<()>,
    task: &mut tokio::task::JoinHandle<()>,
    force_shutdown: &mut watch::Receiver<bool>,
) -> bool {
    let _ = stop.send(());
    if *force_shutdown.borrow() {
        abort_and_join_task(task, "gateway typing loop").await;
        return false;
    }
    tokio::select! {
        biased;
        result = &mut *task => {
            if let Err(error) = result {
                warn!(error = %error, "gateway: typing loop failed");
            }
            true
        }
        _ = wait_for_force(force_shutdown) => {
            abort_and_join_task(task, "gateway typing loop").await;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::sink::SinkFuture;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn shaper_from(lines: &[&str]) -> ReplyShaper {
        let mut s = ReplyShaper::new(true);
        for l in lines {
            s.feed(l);
        }
        s
    }

    #[test]
    fn last_reply_frame_wins() {
        let s = shaper_from(&[
            r#"{"type":"delta","text":"partial"}"#,
            r#"{"type":"reply","text":"first answer"}"#,
            r#"{"type":"reply","text":"final answer"}"#,
        ]);
        assert!(s.saw_protocol);
        assert_eq!(s.last_reply.as_deref(), Some("final answer"));
    }

    #[test]
    fn session_frames_count_as_protocol_but_shape_nothing() {
        let s = shaper_from(&[r#"{"type":"session","claude_session":"s-1","transcript_bytes":9}"#]);
        assert!(s.saw_protocol, "detection is any-parsed-line");
        assert!(s.last_reply.is_none());
    }

    #[test]
    fn plain_stdout_is_not_protocol() {
        let s = shaper_from(&["hello", "world", "{not json"]);
        assert!(!s.saw_protocol);
        assert!(s.last_reply.is_none());
    }

    #[test]
    fn disabled_shaper_never_parses_protocol_frames() {
        let mut shaper = ReplyShaper::new(false);
        shaper.feed(r#"{"type":"reply","text":"not a reply"}"#);

        assert!(!shaper.saw_protocol);
        assert!(shaper.last_reply.is_none());
        assert_eq!(
            shape_final_reply("plain", ok_outcome("raw stdout"), &shaper),
            "raw stdout"
        );
    }

    fn ok_outcome(stdout: &str) -> anyhow::Result<OrchestratedOutcome> {
        Ok(crate::gateway::testutil::ran_ok(stdout))
    }

    #[test]
    fn reply_uses_last_reply_frame_when_protocol_was_spoken() {
        let shaper = shaper_from(&[r#"{"type":"reply","text":"the answer"}"#]);
        assert_eq!(
            shape_final_reply("a", ok_outcome("raw stdout"), &shaper),
            "the answer"
        );
    }

    #[test]
    fn reply_falls_back_to_raw_stdout_without_protocol() {
        let shaper = shaper_from(&["hello", "world"]);
        assert_eq!(
            shape_final_reply("a", ok_outcome("hello\nworld"), &shaper),
            "hello\nworld"
        );
    }

    #[test]
    fn empty_stdout_reply_names_the_agent() {
        let shaper = shaper_from(&[]);
        assert_eq!(
            shape_final_reply("echoer", ok_outcome("  "), &shaper),
            "echoer finished with no output."
        );
    }

    #[test]
    fn failed_runs_report_exit_code_and_stderr() {
        use dotagent_runner::RunOutcome;
        let shaper = ReplyShaper::default();
        let failed = OrchestratedOutcome::Ran(RunOutcome {
            exit_code: 3,
            timed_out: false,
            duration_seconds: 1,
            stdout_tail: String::new(),
            stderr_tail: "boom".into(),
            stdout_truncated_lines: 0,
            stderr_truncated_lines: 0,
        });
        assert_eq!(
            shape_final_reply("a", Ok(failed), &shaper),
            "a exited 3.\nboom"
        );
    }

    #[test]
    fn preflight_failure_and_run_error_have_stable_wording() {
        let shaper = ReplyShaper::default();
        assert_eq!(
            shape_final_reply(
                "a",
                Ok(OrchestratedOutcome::PreflightFailed {
                    plugin: "preflight-warp".into(),
                    suggest: Some("run warp-cli connect".into()),
                }),
                &shaper
            ),
            "a blocked by preflight preflight-warp: run warp-cli connect"
        );
        assert_eq!(
            shape_final_reply("a", Err(anyhow!("unknown agent")), &shaper),
            "Could not run a: unknown agent"
        );
    }

    struct BlockingDeltaSink {
        entered: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }

    impl ReplySink for BlockingDeltaSink {
        fn started(&self, _session: Option<&str>, _agent: &str) {}

        fn reply<'a>(&'a self, _session: Option<&'a str>, _text: &'a str) -> SinkFuture<'a> {
            Box::pin(std::future::ready(()))
        }

        fn typing<'a>(&'a self, _session: Option<&'a str>) -> SinkFuture<'a> {
            Box::pin(std::future::ready(()))
        }

        fn delta<'a>(&'a self, _session: Option<&'a str>, _line: &'a str) -> SinkFuture<'a> {
            let entered = self.entered.clone();
            let dropped = self.dropped.clone();
            Box::pin(async move {
                struct DropMarker(Arc<AtomicBool>);

                impl Drop for DropMarker {
                    fn drop(&mut self) {
                        self.0.store(true, Ordering::SeqCst);
                    }
                }

                entered.store(true, Ordering::SeqCst);
                let _marker = DropMarker(dropped);
                std::future::pending::<()>().await;
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn delta_pump_timeout_aborts_and_joins_the_task() {
        let entered = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel(DELTA_CHANNEL_DEPTH);
        let mut pump = tokio::spawn(delta_pump(
            rx,
            Arc::new(BlockingDeltaSink {
                entered: entered.clone(),
                dropped: dropped.clone(),
            }),
            None,
        ));
        tx.try_send("blocked".into()).unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            while !entered.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("delta pump should enter the blocked sink");

        let (_force_tx, mut force_rx) = watch::channel(false);
        finish_delta_pump(&mut pump, &mut force_rx).await;
        assert!(
            dropped.load(Ordering::SeqCst),
            "aborting and joining the pump must drop the blocked sink future"
        );
    }

    #[test]
    fn delta_pressure_is_bounded_and_records_drops() {
        let (tx, _rx) = mpsc::channel(DELTA_CHANNEL_DEPTH);
        let stats = DeltaStats::default();

        for i in 0..(DELTA_CHANNEL_DEPTH * 2) {
            enqueue_delta(&tx, &stats, &format!("line-{i}"));
        }

        assert_eq!(tx.max_capacity(), DELTA_CHANNEL_DEPTH);
        assert_eq!(tx.capacity(), 0);
        assert_eq!(stats.dropped(), DELTA_CHANNEL_DEPTH);
    }

    #[tokio::test]
    async fn terminal_reply_is_delivered_after_delta_overflow() {
        let (tx, rx) = mpsc::channel(DELTA_CHANNEL_DEPTH);
        let stats = DeltaStats::default();
        for i in 0..(DELTA_CHANNEL_DEPTH * 2) {
            enqueue_delta(&tx, &stats, &format!("line-{i}"));
        }
        drop(tx);

        let sink = Arc::new(crate::gateway::testutil::RecordingSink::default());
        let mut pump = tokio::spawn(delta_pump(rx, sink.clone(), None));
        let (_force_tx, mut force_rx) = watch::channel(false);
        assert!(finish_delta_pump(&mut pump, &mut force_rx).await);

        sink.reply(None, "terminal").await;
        assert!(stats.dropped() > 0);
        assert_eq!(
            sink.events().last().map(String::as_str),
            Some("reply[-] terminal")
        );
    }
}
