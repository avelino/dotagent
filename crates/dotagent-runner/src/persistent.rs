//! The pool behind `[lifecycle] mode = "persistent"`.
//!
//! An agent that holds state — a conversation, a warm cache, an open
//! connection — pays for all of it again every time dotagent spawns it fresh.
//! This keeps the process alive between runs and hands it the next request
//! over [`crate::protocol`].
//!
//! What it deliberately does *not* do is invent a second supervisor. Every
//! instance is spawned through [`Supervisor::spawn_supervised`] like any other
//! subprocess, so it shows up in `dotagent status`, the reaper enforces its
//! deadline with a real kill-tree, and daemon shutdown reaps it. The idle
//! timeout is not a timer of its own: it is the reaper's clock, re-pointed
//! with [`Supervisor::retime`] after every answer.
//!
//! ## Concurrency
//!
//! One mutex per instance, so requests for one key serialize. This does **not**
//! make different keys run in parallel today — the daemon reads triggers from
//! the same `select!` that drives its tick loop, and that serialization is
//! what keeps the heartbeat's read-modify-write safe. Instances of the same
//! agent share one heartbeat file (the slug is per source, not per key), so
//! the per-instance mutex is defense in depth, not a throughput promise.
//!
//! See `docs/concepts/lifecycle.md`.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use chrono::Local;
use dotagent_core::audit::AuditEvent;
use dotagent_state::{AuditLog, StateStore};
use dotagent_supervisor::{ProcId, ProcessKind, ProcessOwner, SpawnSpec, Supervisor};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::protocol::{HelloFrame, InboundFrame, RequestFrame, TriggerFrame};
use crate::{
    tail_lines, RequestLost, RunOutcome, RunSpec, RunnerError, TAIL_LINES, TIMED_OUT_EXIT_CODE,
};

/// Slice used when the manifest declares no `[lifecycle] key`.
pub const DEFAULT_INSTANCE_KEY: &str = "default";

/// Scope of a run the scheduler dispatched, as opposed to one something
/// triggered. See [`resolve_key`].
pub const SCHEDULED_SCOPE: &str = "scheduled";

/// Longest a resolved key may be before it is hashed. Keys land in process
/// labels and audit entries; a 4 KB chat field should not.
const MAX_KEY_LEN: usize = 64;

/// How many stderr lines one instance keeps for the next `stderr_tail`.
const STDERR_RING_LINES: usize = 500;

/// No process exit status exists when a request was written but its response
/// was lost. Keep the heartbeat's failure shape explicit instead of leaving it
/// open or pretending the request succeeded.
pub const REQUEST_LOST_EXIT_CODE: i32 = -1;

/// Why an instance went away. The string lands in the audit log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecycleReason {
    Idle,
    MaxInvocations,
    Crashed,
    Timeout,
    Evicted,
    Shutdown,
    ConfigChanged,
}

impl RecycleReason {
    fn as_str(self) -> &'static str {
        match self {
            RecycleReason::Idle => "idle",
            RecycleReason::MaxInvocations => "max_invocations",
            RecycleReason::Crashed => "crashed",
            RecycleReason::Timeout => "timeout",
            RecycleReason::Evicted => "evicted",
            RecycleReason::Shutdown => "shutdown",
            RecycleReason::ConfigChanged => "config_changed",
        }
    }
}

/// Rolling window of the instance's stderr.
///
/// A one-shot run has a natural boundary for "what did this print" — the
/// process exiting. A persistent one has none, so each request records where
/// the stream was when it started and takes the delta.
#[derive(Default)]
struct StderrRing {
    lines: VecDeque<String>,
    /// Total lines ever seen. Monotonic, so a request can subtract.
    total: usize,
}

impl StderrRing {
    fn push(&mut self, line: String) {
        self.total += 1;
        self.lines.push_back(line);
        while self.lines.len() > STDERR_RING_LINES {
            self.lines.pop_front();
        }
    }

    /// Lines seen since `mark`, capped at what the ring still holds.
    fn since(&self, mark: usize) -> String {
        let produced = self.total.saturating_sub(mark);
        let take = produced.min(self.lines.len());
        let start = self.lines.len() - take;
        self.lines
            .iter()
            .skip(start)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One live agent process.
struct Instance {
    proc_id: ProcId,
    pid: u32,
    handle: dotagent_supervisor::SupervisedHandle,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    stderr: Arc<StdMutex<StderrRing>>,
    stderr_task: tokio::task::JoinHandle<()>,
    /// `AGENT_TMPDIR`. Owned by the instance, not by a request: a one-shot run
    /// drops it on exit, and doing that here would delete the directory out
    /// from under a process that is still using it.
    _tmpdir: tempfile::TempDir,
    invocations: u32,
    seq: u64,
}

impl Drop for Instance {
    fn drop(&mut self) {
        // The stderr reader would otherwise outlive the process it drains.
        self.stderr_task.abort();
    }
}

/// Ownership state for one pool slot.
///
/// The state is deliberately separate from the process handle. `Starting` is
/// a reservation made before the subprocess exists, and `Retiring` keeps the
/// key owned while the supervisor's grace period is in progress. A stale
/// request that still holds an `Arc` can therefore never mistake either state
/// for permission to spawn a replacement.
enum SlotState {
    Starting,
    Live(Box<Instance>),
    Retiring,
    Stopped,
}

struct SlotCell {
    state: Mutex<SlotState>,
}

type Slot = Arc<SlotCell>;

impl SlotCell {
    fn starting() -> Slot {
        Arc::new(Self {
            state: Mutex::new(SlotState::Starting),
        })
    }

    #[cfg(test)]
    fn stopped() -> Slot {
        Arc::new(Self {
            state: Mutex::new(SlotState::Stopped),
        })
    }
}

struct SlotEntry {
    slot: Slot,
    last_used: Instant,
}

struct RetireOptions<'a> {
    reason: RecycleReason,
    audit: Option<&'a AuditLog>,
    already_dead: bool,
}

/// Live persistent agents, keyed by `(agent, instance key)`.
pub struct PersistentPool {
    supervisor: Supervisor,
    slots: Mutex<HashMap<(String, String), SlotEntry>>,
    /// Serializes map allocation and eviction. Retirement must finish before
    /// a replacement slot is inserted, otherwise `max_instances` is only a
    /// best effort during the supervisor grace period.
    allocation: Mutex<()>,
}

impl std::fmt::Debug for PersistentPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersistentPool").finish_non_exhaustive()
    }
}

/// How a request against a live instance ended.
enum Exchange {
    Answered(RunOutcome),
    /// The process was gone before the request bytes were written. Retryable
    /// exactly once, with a fresh instance.
    DeadBeforeRequest(String),
    /// The request was written, but no answer was received. The agent may have
    /// performed a side effect, so retrying would be an unsafe duplicate.
    RequestLost(RequestLost),
    /// The request outlived its deadline. Not retryable: the agent is
    /// presumably still working on it, and asking again would double the work.
    TimedOut,
}

impl PersistentPool {
    pub fn new(supervisor: Supervisor) -> Self {
        Self {
            supervisor,
            slots: Mutex::new(HashMap::new()),
            allocation: Mutex::new(()),
        }
    }

    /// Number of pool-owned keys, including spawn and retirement reservations.
    pub async fn live_count(&self) -> usize {
        let slots = self.slots.lock().await;
        slots
            .values()
            .filter(|entry| match entry.slot.state.try_lock() {
                Ok(guard) => !matches!(*guard, SlotState::Stopped),
                // A locked cell is owned by a spawn, request, or retirement.
                Err(_) => true,
            })
            .count()
    }

    /// Run one request against the agent's persistent instance, spawning it if
    /// there is not one already.
    ///
    /// Writes the same heartbeat a one-shot run writes, per request, so
    /// nothing downstream (`status`, health, retry) can tell the modes apart.
    pub async fn dispatch(
        &self,
        spec: &RunSpec<'_>,
        state: &StateStore,
        audit: Option<&AuditLog>,
    ) -> crate::Result<RunOutcome> {
        let name = spec.manifest.agent.name.clone();
        let slug = spec.slug();
        let key = resolve_key(spec);
        let start = Local::now();

        if !spec.dry_run {
            crate::begin_heartbeat(state, &name, &slug, spec.args, &start)?;
        }

        let result = self
            .exchange_with_retry(spec, state, &name, &key, audit)
            .await;

        let outcome = match result {
            Ok(outcome) => outcome,
            Err(e @ RunnerError::RequestLost(_)) => {
                // The request crossed the process boundary, so the heartbeat
                // must not remain in the "running" state. The daemon handles
                // the typed error as a terminal, counted failure and must not
                // send the same request again.
                if !spec.dry_run {
                    let finish = Local::now();
                    if let Err(finish_error) = crate::finish_heartbeat(
                        state,
                        &name,
                        &slug,
                        spec.args,
                        &start,
                        &finish,
                        REQUEST_LOST_EXIT_CODE,
                    ) {
                        warn!(
                            agent = %name,
                            slug = %slug,
                            error = %finish_error,
                            "could not close heartbeat after persistent request loss"
                        );
                    }
                }
                return Err(e);
            }
            Err(e) => {
                // A spawn that never happened has no exit code to report. Mirror
                // `run()`: propagate, leaving the started heartbeat open, which
                // is what crash detection reads.
                return Err(e);
            }
        };

        if !spec.dry_run {
            let finish = Local::now();
            crate::finish_heartbeat(
                state,
                &name,
                &slug,
                spec.args,
                &start,
                &finish,
                outcome.exit_code,
            )?;
        }
        Ok(outcome)
    }

    async fn exchange_with_retry(
        &self,
        spec: &RunSpec<'_>,
        state: &StateStore,
        name: &str,
        key: &str,
        audit: Option<&AuditLog>,
    ) -> crate::Result<RunOutcome> {
        let mut last_error = String::new();
        for attempt in 0..2 {
            let slot = self.slot_for(spec, name, key, audit).await;
            let mut guard = slot.state.lock().await;

            if matches!(*guard, SlotState::Stopped | SlotState::Retiring) {
                // A request can retain an Arc to a slot after eviction or
                // shutdown removed it from the map. Never spawn into that
                // detached cell; reacquire the current map entry instead.
                drop(guard);
                continue;
            }

            if matches!(*guard, SlotState::Starting) {
                match self.spawn_instance(spec, state, name, key).await {
                    Ok(inst) => {
                        let _ = audit.map(|log| {
                            log.append(AuditEvent::PersistentAgentStarted {
                                agent: name.to_string(),
                                key: key.to_string(),
                                pid: inst.pid,
                            })
                        });
                        info!(agent = %name, key = %key, pid = inst.pid, "persistent agent up");
                        *guard = SlotState::Live(Box::new(inst));
                    }
                    Err(e) => {
                        // A process that cannot start is not a race worth
                        // retrying — the next attempt would fail identically.
                        *guard = SlotState::Stopped;
                        drop(guard);
                        self.forget_slot(name, key, &slot).await;
                        return Err(e);
                    }
                }
            }

            let inst = match &mut *guard {
                SlotState::Live(inst) => inst.as_mut(),
                SlotState::Starting | SlotState::Retiring | SlotState::Stopped => {
                    drop(guard);
                    continue;
                }
            };
            match self.exchange(inst, spec).await {
                Exchange::Answered(outcome) => {
                    let recycle = spec.manifest.lifecycle.max_invocations > 0
                        && inst.invocations >= spec.manifest.lifecycle.max_invocations;
                    let idle = Duration::from_secs(spec.manifest.lifecycle.idle_timeout_seconds);
                    let proc_id = inst.proc_id;
                    if recycle {
                        let inst = match std::mem::replace(&mut *guard, SlotState::Retiring) {
                            SlotState::Live(inst) => inst,
                            _ => unreachable!("the request owns a live slot"),
                        };
                        self.retire_held(
                            name,
                            key,
                            &slot,
                            guard,
                            inst,
                            RetireOptions {
                                reason: RecycleReason::MaxInvocations,
                                audit,
                                already_dead: false,
                            },
                        )
                        .await;
                    } else {
                        // Hand the reaper the idle clock. A false return means
                        // it already collected the process; the next request
                        // will spawn a new one, which is exactly right.
                        if !self.supervisor.retime(proc_id, idle) {
                            debug!(agent = %name, key = %key, "instance collected before going idle");
                            let inst = match std::mem::replace(&mut *guard, SlotState::Retiring) {
                                SlotState::Live(inst) => inst,
                                _ => unreachable!("the request owns a live slot"),
                            };
                            self.retire_held(
                                name,
                                key,
                                &slot,
                                guard,
                                inst,
                                RetireOptions {
                                    reason: RecycleReason::Idle,
                                    audit,
                                    already_dead: true,
                                },
                            )
                            .await;
                        } else {
                            drop(guard);
                            self.touch(name, key).await;
                        }
                    }
                    return Ok(outcome);
                }
                Exchange::TimedOut => {
                    // Recycle rather than reuse: the instance is still working
                    // on a question nobody is waiting for anymore, and its next
                    // line would answer the wrong one.
                    let inst = match std::mem::replace(&mut *guard, SlotState::Retiring) {
                        SlotState::Live(inst) => inst,
                        _ => unreachable!("the request owns a live slot"),
                    };
                    self.retire_held(
                        name,
                        key,
                        &slot,
                        guard,
                        inst,
                        RetireOptions {
                            reason: RecycleReason::Timeout,
                            audit,
                            already_dead: false,
                        },
                    )
                    .await;
                    warn!(agent = %name, key = %key, "persistent request timed out — recycled");
                    return Ok(RunOutcome {
                        exit_code: TIMED_OUT_EXIT_CODE,
                        timed_out: true,
                        duration_seconds: spec.manifest.agent.timeout_seconds as i64,
                        stdout_tail: String::new(),
                        stderr_tail: format!(
                            "persistent agent did not answer within {}s — instance recycled",
                            spec.manifest.agent.timeout_seconds
                        ),
                        stdout_truncated_lines: 0,
                        stderr_truncated_lines: 0,
                    });
                }
                Exchange::DeadBeforeRequest(why) => {
                    let inst = match std::mem::replace(&mut *guard, SlotState::Retiring) {
                        SlotState::Live(inst) => inst,
                        _ => unreachable!("the request owns a live slot"),
                    };
                    self.retire_held(
                        name,
                        key,
                        &slot,
                        guard,
                        inst,
                        RetireOptions {
                            reason: RecycleReason::Crashed,
                            audit,
                            already_dead: true,
                        },
                    )
                    .await;
                    last_error = why;
                    if attempt == 0 {
                        debug!(
                            agent = %name, key = %key, error = %last_error,
                            "persistent instance was gone — retrying once with a fresh one"
                        );
                        continue;
                    }
                }
                Exchange::RequestLost(lost) => {
                    let inst = match std::mem::replace(&mut *guard, SlotState::Retiring) {
                        SlotState::Live(inst) => inst,
                        _ => unreachable!("the request owns a live slot"),
                    };
                    self.retire_held(
                        name,
                        key,
                        &slot,
                        guard,
                        inst,
                        RetireOptions {
                            reason: RecycleReason::Crashed,
                            audit,
                            already_dead: true,
                        },
                    )
                    .await;
                    return Err(RunnerError::RequestLost(lost));
                }
            }
        }
        Err(RunnerError::Spawn(format!(
            "persistent agent {name} died twice in a row: {last_error}"
        )))
    }

    /// One request against a live instance.
    async fn exchange(&self, inst: &mut Instance, spec: &RunSpec<'_>) -> Exchange {
        if let Ok(Some(status)) = inst.handle.try_wait() {
            return Exchange::DeadBeforeRequest(format!("process already exited ({status})"));
        }

        let deadline = Duration::from_secs(spec.manifest.agent.timeout_seconds);
        if !self.supervisor.retime(inst.proc_id, deadline) {
            return Exchange::DeadBeforeRequest(
                "supervisor had already collected the process".into(),
            );
        }

        inst.seq += 1;
        let id = inst.seq.to_string();
        let frame = RequestFrame {
            v: crate::protocol::PROTOCOL_VERSION,
            kind: "request",
            id: id.clone(),
            agent: spec.manifest.agent.name.clone(),
            schedule: spec.schedule_id.to_string(),
            args: spec.args.to_vec(),
            deadline_seconds: spec.manifest.agent.timeout_seconds,
            trigger: TriggerFrame::from_env(spec.extra_env),
        };

        let stderr_mark = inst
            .stderr
            .lock()
            .map(|ring| ring.total)
            .unwrap_or_default();
        let started = Instant::now();

        if let Err(e) = write_frame(&mut inst.stdin, &frame).await {
            return classify_write_failure(
                e,
                inst.handle.reaper_owns_deadline(),
                started.elapsed(),
            );
        }

        let answer = tokio::time::timeout(deadline, read_answer(&mut inst.stdout, &id)).await;
        let elapsed = started.elapsed().as_secs() as i64;
        let stderr_delta = inst
            .stderr
            .lock()
            .map(|ring| ring.since(stderr_mark))
            .unwrap_or_default();

        match answer {
            Err(_) => Exchange::TimedOut,
            Ok(Err(_)) if inst.handle.reaper_owns_deadline() => Exchange::TimedOut,
            Ok(Err(e)) => Exchange::RequestLost(RequestLost::new(e, started.elapsed())),
            Ok(Ok(_)) if inst.handle.reaper_owns_deadline() => Exchange::TimedOut,
            Ok(Ok(frame)) => {
                inst.invocations += 1;
                let exit_code = frame.resolved_exit_code();
                // A failure with only an `error` still has something to say —
                // and stdout is where the reply comes from, so that is where
                // it has to land or the chat gets silence.
                let stdout = frame.output.or(frame.error).unwrap_or_default();
                let (stdout_tail, stdout_truncated_lines) = tail_lines(&stdout, TAIL_LINES);
                let (stderr_tail, stderr_truncated_lines) = tail_lines(&stderr_delta, TAIL_LINES);
                Exchange::Answered(RunOutcome {
                    exit_code,
                    timed_out: false,
                    duration_seconds: elapsed,
                    stdout_tail,
                    stderr_tail,
                    stdout_truncated_lines,
                    stderr_truncated_lines,
                })
            }
        }
    }

    /// Get (or create) the slot for a key, evicting the least recently used
    /// instance first when the agent is at its ceiling.
    async fn slot_for(
        &self,
        spec: &RunSpec<'_>,
        name: &str,
        key: &str,
        audit: Option<&AuditLog>,
    ) -> Slot {
        let map_key = (name.to_string(), key.to_string());

        // A replacement is not inserted until the victim is fully retired.
        // This keeps the old key owned during the supervisor grace period and
        // makes `max_instances` a hard ceiling, not a best effort.
        let _allocation = self.allocation.lock().await;
        loop {
            let victim = {
                let mut slots = self.slots.lock().await;

                if let Some(entry) = slots.get_mut(&map_key) {
                    let stopped = entry
                        .slot
                        .state
                        .try_lock()
                        .map(|state| matches!(*state, SlotState::Stopped))
                        .unwrap_or(false);
                    if stopped {
                        slots.remove(&map_key);
                    } else {
                        entry.last_used = Instant::now();
                        return entry.slot.clone();
                    }
                }

                // New key. Make room before adding, counting reservations and
                // retiring slots as occupied. The victim stays in the map
                // until `retire_slot` finishes.
                // Direct PersistentPool callers may bypass manifest loading,
                // which rejects zero. Retain one slot rather than retrying an
                // impossible allocation forever.
                let max = (spec.manifest.lifecycle.max_instances as usize).max(1);
                let mine: Vec<_> = slots.keys().filter(|(a, _)| a == name).cloned().collect();
                if mine.len() < max {
                    let slot = SlotCell::starting();
                    slots.insert(
                        map_key.clone(),
                        SlotEntry {
                            slot: slot.clone(),
                            last_used: Instant::now(),
                        },
                    );
                    return slot;
                }

                mine.into_iter()
                    .min_by_key(|k| slots.get(k).map(|e| e.last_used))
                    .and_then(|k| slots.get(&k).map(|e| (k, e.slot.clone())))
            };

            if let Some((victim_key, victim_slot)) = victim {
                self.retire_slot(
                    &victim_key.0,
                    &victim_key.1,
                    &victim_slot,
                    RecycleReason::Evicted,
                    audit,
                    false,
                )
                .await;
            }
        }
    }

    async fn spawn_instance(
        &self,
        spec: &RunSpec<'_>,
        state: &StateStore,
        name: &str,
        key: &str,
    ) -> crate::Result<Instance> {
        let slug = spec.slug();
        let start = Local::now();
        let tmpdir = tempfile::tempdir()?;
        let heartbeat_path = state.heartbeat_path(name, &slug);

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
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        crate::apply_env_for(
            &mut cmd,
            spec,
            name,
            &slug,
            &start,
            tmpdir.path(),
            &heartbeat_path,
            Some(key),
        );

        let startup = Duration::from_secs(spec.manifest.lifecycle.startup_timeout_seconds);
        let spawn_spec = SpawnSpec {
            kind: ProcessKind::PersistentAgent,
            owner: ProcessOwner {
                agent: name.to_string(),
                schedule: Some(spec.schedule_id.to_string()),
                hook_event: None,
                plugin: None,
            },
            // Until the handshake lands, the startup window is the deadline —
            // an agent that never says `ready` is collected instead of sitting
            // there forever.
            deadline: startup,
            label: format!("{name}.persistent[{key}]"),
        };

        let mut handle = self
            .supervisor
            .spawn_supervised(cmd, spawn_spec)
            .await
            .map_err(|e| RunnerError::Spawn(e.to_string()))?;
        let proc_id = handle.id();
        let pid = handle.pid().unwrap_or(0);
        let mut stdin = handle.take_stdin().expect("piped stdin");
        let stdout = handle.take_stdout().expect("piped stdout");
        let stderr = handle.take_stderr().expect("piped stderr");

        let ring = Arc::new(StdMutex::new(StderrRing::default()));
        let stderr_task = spawn_stderr_reader(stderr, ring.clone(), name.to_string());
        let mut lines = BufReader::new(stdout).lines();

        // Handshake. Failure here terminates rather than leaks: the process is
        // already registered with the supervisor and nobody else holds it.
        let hello = HelloFrame::new(name, key, spec.schedule_id);
        let handshake = async {
            write_frame(&mut stdin, &hello)
                .await
                .map_err(|e| e.to_string())?;
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => match InboundFrame::parse(&line) {
                        Some(f) if f.is_ready() && f.ok => return Ok(()),
                        Some(f) if f.is_ready() => {
                            return Err(f
                                .error
                                .unwrap_or_else(|| "agent refused the handshake".into()))
                        }
                        _ => {
                            debug!(agent = %name, line = %line, "ignoring pre-handshake output");
                        }
                    },
                    Ok(None) => return Err("agent exited before the handshake".to_string()),
                    Err(e) => return Err(format!("reading handshake: {e}")),
                }
            }
        };

        match tokio::time::timeout(startup, handshake).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                stderr_task.abort();
                self.supervisor.terminate(proc_id).await;
                return Err(RunnerError::Spawn(format!(
                    "persistent agent {name} failed to start: {e}"
                )));
            }
            Err(_) => {
                stderr_task.abort();
                self.supervisor.terminate(proc_id).await;
                return Err(RunnerError::Spawn(format!(
                    "persistent agent {name} did not answer the handshake within {}s",
                    startup.as_secs()
                )));
            }
        }

        Ok(Instance {
            proc_id,
            pid,
            handle,
            stdin,
            stdout: lines,
            stderr: ring,
            stderr_task,
            _tmpdir: tmpdir,
            invocations: 0,
            seq: 0,
        })
    }

    /// Kill an instance and record why.
    ///
    /// Order matters: `terminate` runs **while the instance is still held**.
    /// Dropping it first releases the `Child`, tokio reaps the process in the
    /// background, and the pid becomes available for reuse — at which point
    /// the `killpg` we were about to send could land on somebody else's
    /// process group. Holding the handle across the kill keeps the pid
    /// reserved. This is the same hazard `SupervisedHandle::Drop` exists to
    /// avoid.
    async fn retire(
        &self,
        inst: Instance,
        name: &str,
        key: &str,
        reason: RecycleReason,
        audit: Option<&AuditLog>,
    ) {
        let invocations = inst.invocations;
        self.supervisor.terminate(inst.proc_id).await;
        drop(inst);
        self.record_recycle(name, key, reason, invocations, audit);
    }

    /// Retire an instance while retaining its slot mutex.
    ///
    /// The guard is intentionally held through the supervisor grace period.
    /// A request that obtained the slot just before retirement then waits for
    /// the old process to finish and sees `Stopped`; it cannot spawn into an
    /// Arc that is no longer present in the pool map.
    async fn retire_held(
        &self,
        name: &str,
        key: &str,
        slot: &Slot,
        mut guard: tokio::sync::MutexGuard<'_, SlotState>,
        inst: Box<Instance>,
        options: RetireOptions<'_>,
    ) {
        if options.already_dead {
            self.retire_dead(*inst, name, key, options.reason, options.audit)
                .await;
        } else {
            self.retire(*inst, name, key, options.reason, options.audit)
                .await;
        }
        *guard = SlotState::Stopped;
        drop(guard);
        self.forget_slot(name, key, slot).await;
    }

    /// Claim and retire whatever currently owns a slot.
    async fn retire_slot(
        &self,
        name: &str,
        key: &str,
        slot: &Slot,
        reason: RecycleReason,
        audit: Option<&AuditLog>,
        already_dead: bool,
    ) {
        let mut guard = slot.state.lock().await;
        match std::mem::replace(&mut *guard, SlotState::Retiring) {
            SlotState::Live(inst) => {
                self.retire_held(
                    name,
                    key,
                    slot,
                    guard,
                    inst,
                    RetireOptions {
                        reason,
                        audit,
                        already_dead,
                    },
                )
                .await;
            }
            SlotState::Starting | SlotState::Retiring | SlotState::Stopped => {
                // A reservation can be evicted or drained before its owner
                // acquires the mutex. Marking it stopped makes that owner
                // retry the map lookup instead of spawning an untracked child.
                *guard = SlotState::Stopped;
                drop(guard);
                self.forget_slot(name, key, slot).await;
            }
        }
    }

    /// Same, for an instance that is already dead. `terminate` is best effort:
    /// the reaper may have collected it, in which case it is a no-op.
    async fn retire_dead(
        &self,
        inst: Instance,
        name: &str,
        key: &str,
        reason: RecycleReason,
        audit: Option<&AuditLog>,
    ) {
        let invocations = inst.invocations;
        self.supervisor.terminate(inst.proc_id).await;
        drop(inst);
        self.record_recycle(name, key, reason, invocations, audit);
    }

    fn record_recycle(
        &self,
        name: &str,
        key: &str,
        reason: RecycleReason,
        invocations: u32,
        audit: Option<&AuditLog>,
    ) {
        if let Some(log) = audit {
            let _ = log.append(AuditEvent::PersistentAgentRecycled {
                agent: name.to_string(),
                key: key.to_string(),
                reason: reason.as_str().to_string(),
                invocations,
            });
        }
        info!(
            agent = %name, key = %key, reason = reason.as_str(), invocations,
            "persistent agent recycled"
        );
    }

    async fn touch(&self, name: &str, key: &str) {
        let mut slots = self.slots.lock().await;
        if let Some(entry) = slots.get_mut(&(name.to_string(), key.to_string())) {
            entry.last_used = Instant::now();
        }
    }

    async fn forget_slot(&self, name: &str, key: &str, slot: &Slot) {
        let mut slots = self.slots.lock().await;
        let map_key = (name.to_string(), key.to_string());
        let is_current = slots
            .get(&map_key)
            .map(|entry| Arc::ptr_eq(&entry.slot, slot))
            .unwrap_or(false);
        if is_current {
            slots.remove(&map_key);
        }
    }

    /// Collect instances that died while nobody was looking.
    ///
    /// The reaper enforces the idle window, but it kills a process — it does
    /// not know the pool exists. Without this sweep, an instance recycled for
    /// sitting idle would only be noticed (and audited) when the next message
    /// arrived, which for an idle conversation could be never.
    pub async fn sweep(&self, audit: Option<&AuditLog>) {
        // Decide and remove under one hold of the map — see [`reap_dead`].
        // Everything with a cost (terminating, auditing) happens after the
        // lock is back: `record_recycle` writes to a flocked file, and
        // dropping an `Instance` deletes its temp dir.
        let reaped = {
            let mut slots = self.slots.lock().await;
            reap_dead(&mut slots)
        };
        for (key, instance) in reaped {
            let Some(inst) = instance else { continue };
            let invocations = inst.invocations;
            drop(inst);
            self.record_recycle(&key.0, &key.1, RecycleReason::Idle, invocations, audit);
        }
    }

    /// Drop every instance of one agent — used when its manifest changed and
    /// the live process no longer matches what is on disk.
    pub async fn forget_agent(&self, name: &str, audit: Option<&AuditLog>) {
        let _allocation = self.allocation.lock().await;
        let mine: Vec<((String, String), Slot)> = {
            let slots = self.slots.lock().await;
            slots
                .iter()
                .filter(|((agent, _), _)| agent == name)
                .map(|(key, entry)| (key.clone(), entry.slot.clone()))
                .collect()
        };
        for (key, slot) in mine {
            self.retire_slot(
                &key.0,
                &key.1,
                &slot,
                RecycleReason::ConfigChanged,
                audit,
                false,
            )
            .await;
        }
    }

    /// Terminate every instance. Called on daemon shutdown so the audit log
    /// says why they went away — `Supervisor::shutdown` would reap them either
    /// way, but silently.
    pub async fn shutdown(&self, audit: Option<&AuditLog>) {
        self.drain(RecycleReason::Shutdown, audit).await
    }

    /// Terminate every instance because the operator asked for a reload.
    ///
    /// A live instance was spawned from the manifest as it read at the time:
    /// its `[run]` command, its `[env]`, its timeouts. Keeping it through a
    /// reload would make the reload look applied while the behavior stayed
    /// exactly where it was — the same reason the Telegram ingress is
    /// restarted rather than left alone. Costs one respawn per conversation,
    /// on an event the operator triggered deliberately.
    pub async fn reload(&self, audit: Option<&AuditLog>) {
        self.drain(RecycleReason::ConfigChanged, audit).await
    }

    async fn drain(&self, reason: RecycleReason, audit: Option<&AuditLog>) {
        let _allocation = self.allocation.lock().await;
        let all: Vec<((String, String), Slot)> = {
            let slots = self.slots.lock().await;
            slots
                .iter()
                .map(|(key, entry)| (key.clone(), entry.slot.clone()))
                .collect()
        };
        for (key, slot) in all {
            self.retire_slot(&key.0, &key.1, &slot, reason, audit, false)
                .await;
        }
    }
}

/// Remove every slot whose process is gone, returning what was removed.
///
/// Takes the map itself rather than `&self` on purpose. Sweeping used to
/// snapshot the map, release it, decide, and then take it again to delete —
/// and a dispatch landing in that gap could create a slot, put a live instance
/// in it, and have the sweep delete the entry from under it. The instance
/// stayed alive in an `Arc` nobody could reach: `reload` would not drain it,
/// `shutdown` would not audit it, `live_count` would not see it, and only
/// `Supervisor::shutdown` would ever kill it, blindly. Holding the map across
/// the whole decision makes that gap unrepresentable — hence no `.await` in
/// here, and none allowed.
///
/// Starting and retiring slots are reservations owned by another lifecycle
/// transition and are left alone. A locked live slot has a request in flight
/// and is left alone as well.
fn reap_dead(
    slots: &mut HashMap<(String, String), SlotEntry>,
) -> Vec<((String, String), Option<Instance>)> {
    let mut reaped = Vec::new();
    slots.retain(|key, entry| {
        let Ok(mut guard) = entry.slot.state.try_lock() else {
            return true;
        };
        match &mut *guard {
            SlotState::Starting | SlotState::Retiring => true,
            SlotState::Stopped => {
                reaped.push((key.clone(), None));
                false
            }
            SlotState::Live(inst) => {
                let exited = inst.handle.try_wait().ok().flatten().is_some();
                if !exited {
                    return true;
                }
                let state = std::mem::replace(&mut *guard, SlotState::Stopped);
                let instance = match state {
                    SlotState::Live(inst) => Some(*inst),
                    SlotState::Starting | SlotState::Retiring | SlotState::Stopped => None,
                };
                reaped.push((key.clone(), instance));
                false
            }
        }
    });
    reaped
}

/// Which instance answers this request.
///
/// Two parts, `<scope>:<slice>`.
///
/// The **slice** is a *selector over the payload the daemon already attested*,
/// never something the sender can point anywhere: `[lifecycle] key` names a
/// field, and whatever is in that field is sanitized before it reaches a
/// process label.
///
/// The **scope** is where the request came from, and it exists because the
/// slice alone collapses to `default` for every agent that does not declare a
/// `key` — and a scheduled run never carries a payload, so it collapses there
/// too. A persistent agent with both a `[[schedules]]` entry and a chat would
/// then put both through one instance and one mutex, and a 1200-second
/// scheduled run would hold the chat message for its whole duration: exactly
/// the head-of-line blocking that moving triggers off the tick was meant to
/// end. Separating them costs one extra process (the ceiling is
/// `max_instances`, default 8) and buys a chat that answers while a scheduled
/// run is in flight.
fn resolve_key(spec: &RunSpec<'_>) -> String {
    format!("{}:{}", instance_scope(spec), instance_slice(spec))
}

/// `scheduled` for a run the loop dispatched, `trigger-<source>` for one
/// something asked for. Sanitized: the source is dotagent's own enum today,
/// and this string ends up in a process label.
fn instance_scope(spec: &RunSpec<'_>) -> String {
    match spec
        .extra_env
        .iter()
        .find(|(k, _)| k == "AGENT_TRIGGER_SOURCE")
    {
        Some((_, source)) => format!("trigger-{}", sanitize_key(source)),
        None => SCHEDULED_SCOPE.to_string(),
    }
}

fn instance_slice(spec: &RunSpec<'_>) -> String {
    let Some(field) = spec.manifest.lifecycle.key.as_deref() else {
        return DEFAULT_INSTANCE_KEY.to_string();
    };
    let raw = spec
        .extra_env
        .iter()
        .find(|(k, _)| k == "AGENT_TRIGGER_PAYLOAD")
        .and_then(|(_, v)| serde_json::from_str::<serde_json::Value>(v).ok())
        .and_then(|payload| match payload.get(field) {
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(serde_json::Value::Number(n)) => Some(n.to_string()),
            _ => None,
        });
    match raw {
        Some(value) => sanitize_key(&value),
        None => DEFAULT_INSTANCE_KEY.to_string(),
    }
}

/// Reduce an arbitrary payload value to something safe to put in a process
/// label, a log line and an audit entry.
fn sanitize_key(raw: &str) -> String {
    let clean: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if clean.is_empty() || clean.len() != raw.len() || clean.len() > MAX_KEY_LEN {
        // Anything that is not already a plain identifier becomes a stable
        // digest of itself. Two different values never collapse to one key,
        // and no chat text ever reaches a label.
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        raw.hash(&mut hasher);
        format!("k{:016x}", hasher.finish())
    } else {
        clean
    }
}

#[derive(Debug)]
struct FrameWriteError {
    wrote_bytes: bool,
    message: String,
}

fn classify_write_failure(
    error: FrameWriteError,
    reaper_owned: bool,
    elapsed: Duration,
) -> Exchange {
    if reaper_owned {
        return Exchange::TimedOut;
    }
    if error.wrote_bytes() {
        return Exchange::RequestLost(RequestLost::new(
            format!("write failed after request bytes were written: {error}"),
            elapsed,
        ));
    }
    Exchange::DeadBeforeRequest(format!(
        "write failed before request bytes were written: {error}"
    ))
}

impl FrameWriteError {
    fn new(wrote_bytes: bool, message: impl Into<String>) -> Self {
        Self {
            wrote_bytes,
            message: message.into(),
        }
    }

    fn wrote_bytes(&self) -> bool {
        self.wrote_bytes
    }
}

impl fmt::Display for FrameWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Write one complete protocol frame and retain whether the pipe accepted any
/// bytes. A failure after a partial write is ambiguous delivery; a failure
/// before the first byte is safe to retry with a fresh instance.
async fn write_frame<T, W>(writer: &mut W, frame: &T) -> std::result::Result<(), FrameWriteError>
where
    T: serde::Serialize,
    W: AsyncWrite + Unpin,
{
    let mut line = serde_json::to_vec(frame)
        .map_err(|e| FrameWriteError::new(false, format!("serializing frame: {e}")))?;
    line.push(b'\n');
    let mut written = 0;
    while written < line.len() {
        match writer.write(&line[written..]).await {
            Ok(0) => {
                return Err(FrameWriteError::new(
                    written > 0,
                    "write returned zero bytes",
                ));
            }
            Ok(count) => written += count,
            Err(e) => return Err(FrameWriteError::new(written > 0, e.to_string())),
        }
    }
    writer
        .flush()
        .await
        .map_err(|e| FrameWriteError::new(true, e.to_string()))
}

/// Read until the frame answering `id` shows up.
///
/// Everything else is dropped: a log line that went to the wrong stream, a
/// banner, or the late answer to a request that already timed out. The last
/// one is why this exists — without it, one slow reply would shift every
/// subsequent answer by one, and the chat would silently start replying to the
/// previous question.
async fn read_answer(
    lines: &mut Lines<BufReader<ChildStdout>>,
    id: &str,
) -> std::result::Result<InboundFrame, String> {
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => match InboundFrame::parse(&line) {
                Some(frame) if frame.answers(id) => return Ok(frame),
                Some(_) => debug!(line = %line, "dropping frame that answers nothing in flight"),
                None => debug!(line = %line, "dropping non-protocol line on stdout"),
            },
            Ok(None) => return Err("agent closed stdout".to_string()),
            Err(e) => return Err(format!("reading stdout: {e}")),
        }
    }
}

/// Drain stderr into the ring and the agent's log file, forever.
fn spawn_stderr_reader(
    stderr: tokio::process::ChildStderr,
    ring: Arc<StdMutex<StderrRing>>,
    agent: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Only stderr is teed to the log. stdout is the protocol channel, and
        // a log full of JSON frames helps nobody.
        let log_file = {
            let dir = dotagent_state::paths::agent_logs_dir(&agent);
            std::fs::create_dir_all(&dir).ok().and_then(|()| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join(format!("{agent}.log")))
                    .ok()
            })
        };
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(mut f) = log_file.as_ref().and_then(|f| f.try_clone().ok()) {
                use std::io::Write;
                let _ = writeln!(f, "{line}");
            }
            if let Ok(mut ring) = ring.lock() {
                ring.push(line);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_identifier_is_kept_verbatim() {
        assert_eq!(sanitize_key("12345"), "12345");
        assert_eq!(sanitize_key("chat-9_a"), "chat-9_a");
    }

    #[test]
    fn anything_else_becomes_a_stable_digest() {
        let a = sanitize_key("../../etc/passwd");
        assert!(a.starts_with('k'), "{a}");
        assert_eq!(a, sanitize_key("../../etc/passwd"), "must be stable");
        assert_ne!(a, sanitize_key("../../etc/shadow"), "must not collide");
        // No path separator, no whitespace, nothing a label would have to quote.
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn an_overlong_key_is_hashed_rather_than_truncated() {
        // Truncating would let two different chats share one process.
        let long = "a".repeat(MAX_KEY_LEN + 1);
        let longer = "a".repeat(MAX_KEY_LEN + 2);
        assert_ne!(sanitize_key(&long), sanitize_key(&longer));
        assert!(sanitize_key(&long).len() <= MAX_KEY_LEN);
    }

    #[test]
    fn an_empty_key_never_yields_an_empty_label() {
        assert!(!sanitize_key("").is_empty());
    }

    #[test]
    fn the_stderr_ring_reports_only_what_this_request_produced() {
        let mut ring = StderrRing::default();
        ring.push("before".into());
        let mark = ring.total;
        ring.push("during one".into());
        ring.push("during two".into());
        assert_eq!(ring.since(mark), "during one\nduring two");
        assert_eq!(ring.since(ring.total), "");
    }

    #[test]
    fn the_stderr_ring_survives_more_lines_than_it_holds() {
        let mut ring = StderrRing::default();
        let mark = ring.total;
        for i in 0..(STDERR_RING_LINES + 10) {
            ring.push(format!("line {i}"));
        }
        let seen = ring.since(mark);
        assert_eq!(seen.lines().count(), STDERR_RING_LINES);
        assert!(seen.ends_with(&format!("line {}", STDERR_RING_LINES + 9)));
    }

    #[test]
    fn recycle_reasons_render_as_the_documented_strings() {
        assert_eq!(RecycleReason::MaxInvocations.as_str(), "max_invocations");
        assert_eq!(RecycleReason::ConfigChanged.as_str(), "config_changed");
    }

    struct FailingWriter {
        accepted: usize,
        fail_after: usize,
    }

    impl AsyncWrite for FailingWriter {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if self.accepted >= self.fail_after {
                return std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed",
                )));
            }
            let count = (self.fail_after - self.accepted).min(buf.len());
            self.accepted += count;
            std::task::Poll::Ready(Ok(count))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_frame_write_failure_before_any_byte_is_safe_to_retry() {
        let mut writer = FailingWriter {
            accepted: 0,
            fail_after: 0,
        };
        let error = write_frame(&mut writer, &serde_json::json!({"kind": "request"}))
            .await
            .expect_err("the writer is closed");

        assert!(!error.wrote_bytes());
    }

    #[tokio::test]
    async fn a_frame_write_failure_after_a_byte_is_ambiguous() {
        let mut writer = FailingWriter {
            accepted: 0,
            fail_after: 1,
        };
        let error = write_frame(&mut writer, &serde_json::json!({"kind": "request"}))
            .await
            .expect_err("the writer closes after one byte");

        assert!(error.wrote_bytes());
    }

    #[tokio::test]
    async fn a_partial_frame_write_becomes_request_lost() {
        let mut writer = FailingWriter {
            accepted: 0,
            fail_after: 1,
        };
        let error = write_frame(&mut writer, &serde_json::json!({"kind": "request"}))
            .await
            .expect_err("the writer closes after one byte");

        assert!(matches!(
            classify_write_failure(error, false, Duration::from_secs(1)),
            Exchange::RequestLost(_)
        ));
    }

    // --- which instance answers what ---

    fn manifest(toml_src: &str) -> dotagent_core::AgentManifest {
        toml::from_str(toml_src).expect("fixture manifest must parse")
    }

    fn persistent_manifest(key: Option<&str>) -> dotagent_core::AgentManifest {
        let key_line = key.map(|k| format!("key = \"{k}\"")).unwrap_or_default();
        manifest(&format!(
            r#"
[agent]
name = "dispatcher"
[run]
command = "true"
[lifecycle]
mode = "persistent"
{key_line}
"#
        ))
    }

    fn spec<'a>(
        m: &'a dotagent_core::AgentManifest,
        extra_env: &'a [(String, String)],
    ) -> RunSpec<'a> {
        RunSpec {
            manifest: m,
            manifest_dir: std::path::Path::new("/tmp"),
            schedule_id: "daily",
            args: &[],
            dry_run: false,
            manifest_sha256: None,
            slug_override: None,
            extra_env,
        }
    }

    fn telegram_env(payload: &str) -> Vec<(String, String)> {
        vec![
            ("AGENT_TRIGGER_SOURCE".into(), "telegram".into()),
            ("AGENT_TRIGGER_PAYLOAD".into(), payload.into()),
        ]
    }

    #[test]
    fn a_scheduled_run_and_a_chat_never_share_one_instance() {
        // The regression: without `[lifecycle] key`, both resolved to
        // "default" — one process, one mutex, and a 1200-second scheduled run
        // holding every chat message until it finished.
        let m = persistent_manifest(None);
        let chat = telegram_env(r#"{"chat_id":12345}"#);
        assert_ne!(resolve_key(&spec(&m, &chat)), resolve_key(&spec(&m, &[])));
    }

    #[test]
    fn a_scheduled_run_and_a_chat_stay_apart_even_with_a_key_declared() {
        // A declared `key` names a payload field, and a scheduled run carries
        // no payload — so it lands on the default slice, which is exactly
        // where a chat with no such field lands too.
        let m = persistent_manifest(Some("chat_id"));
        let keyless_chat = telegram_env(r#"{"text":"oi"}"#);
        assert_ne!(
            resolve_key(&spec(&m, &keyless_chat)),
            resolve_key(&spec(&m, &[]))
        );
    }

    #[test]
    fn one_conversation_keeps_one_instance() {
        let m = persistent_manifest(Some("chat_id"));
        let first = telegram_env(r#"{"chat_id":12345}"#);
        let again = telegram_env(r#"{"chat_id":12345,"text":"and another"}"#);
        let other = telegram_env(r#"{"chat_id":99999}"#);

        assert_eq!(
            resolve_key(&spec(&m, &first)),
            resolve_key(&spec(&m, &again))
        );
        assert_ne!(
            resolve_key(&spec(&m, &first)),
            resolve_key(&spec(&m, &other))
        );
        assert!(resolve_key(&spec(&m, &first)).contains("12345"));
    }

    #[test]
    fn a_scheduled_key_says_so() {
        let m = persistent_manifest(None);
        assert_eq!(
            resolve_key(&spec(&m, &[])),
            format!("{SCHEDULED_SCOPE}:{DEFAULT_INSTANCE_KEY}")
        );
    }

    #[test]
    fn a_hostile_source_cannot_shape_the_key() {
        // The source is dotagent's own enum today. It still lands in a process
        // label, so it goes through the same sanitizer as the payload slice.
        let m = persistent_manifest(None);
        let env = vec![("AGENT_TRIGGER_SOURCE".to_string(), "../../etc".to_string())];
        let key = resolve_key(&spec(&m, &env));
        assert!(!key.contains('/'), "{key}");
        assert!(!key.contains('.'), "{key}");
    }

    // --- sweeping ---

    fn starting_slot() -> SlotEntry {
        SlotEntry {
            slot: SlotCell::starting(),
            last_used: Instant::now(),
        }
    }

    fn stopped_slot() -> SlotEntry {
        SlotEntry {
            slot: SlotCell::stopped(),
            last_used: Instant::now(),
        }
    }

    #[tokio::test]
    async fn a_sweep_removes_exactly_what_it_reports() {
        let mut slots: HashMap<(String, String), SlotEntry> = HashMap::new();
        slots.insert(("a".into(), "gone".into()), stopped_slot());
        slots.insert(("a".into(), "also-gone".into()), stopped_slot());

        let reaped = reap_dead(&mut slots);

        assert_eq!(reaped.len(), 2);
        assert!(
            slots.is_empty(),
            "every reported key must be gone from the map"
        );
        assert!(reaped.iter().all(|(_, inst)| inst.is_none()));
    }

    #[tokio::test]
    async fn a_sweep_leaves_a_spawn_reservation_alone() {
        let pool = PersistentPool::new(Supervisor::with_grace(Duration::from_millis(50)));
        pool.slots
            .lock()
            .await
            .insert(("a".into(), "starting".into()), starting_slot());

        pool.sweep(None).await;

        assert!(
            pool.slots
                .lock()
                .await
                .contains_key(&("a".to_string(), "starting".to_string())),
            "a reservation is not a dead process"
        );
    }

    #[tokio::test]
    async fn a_sweep_leaves_a_locked_reservation_alone() {
        let mut slots: HashMap<(String, String), SlotEntry> = HashMap::new();
        let busy_slot = SlotCell::starting();
        let held = busy_slot.state.lock().await;
        slots.insert(
            ("a".into(), "busy".into()),
            SlotEntry {
                slot: busy_slot.clone(),
                last_used: Instant::now(),
            },
        );
        slots.insert(("a".into(), "gone".into()), stopped_slot());

        let reaped = reap_dead(&mut slots);

        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].0, ("a".to_string(), "gone".to_string()));
        assert!(
            slots.contains_key(&("a".to_string(), "busy".to_string())),
            "a locked reservation is owned by a dispatcher — removing it would strand the slot"
        );
        drop(held);
    }

    #[tokio::test]
    async fn forgetting_an_old_slot_does_not_remove_its_replacement() {
        let pool = PersistentPool::new(Supervisor::with_grace(Duration::from_millis(50)));
        let old_slot = SlotCell::starting();
        let new_slot = SlotCell::starting();
        let map_key = ("agent".to_string(), "key".to_string());

        pool.slots.lock().await.insert(
            map_key.clone(),
            SlotEntry {
                slot: new_slot.clone(),
                last_used: Instant::now(),
            },
        );

        pool.forget_slot("agent", "key", &old_slot).await;
        {
            let slots = pool.slots.lock().await;
            assert!(
                slots
                    .get(&map_key)
                    .is_some_and(|entry| Arc::ptr_eq(&entry.slot, &new_slot)),
                "a late request from the old slot must not remove its replacement"
            );
        }

        pool.forget_slot("agent", "key", &new_slot).await;
        assert!(pool.slots.lock().await.is_empty());
    }
}
