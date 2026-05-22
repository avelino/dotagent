//! Subprocess supervisor for dotagent.
//!
//! Centralizes ownership of every child process the orchestrator creates:
//!
//! - **Bounded deadlines.** Every spawn carries a `Duration` the supervisor
//!   enforces with `SIGTERM → grace → SIGKILL`.
//! - **Kill-tree.** Children are placed in their own process group via
//!   `setpgid(0, 0)`. Termination signals are sent with `killpg(2)` so
//!   grandchildren (e.g. `mcp` invoked by a sink plugin) die with the parent.
//! - **Live registry.** `Supervisor::snapshot` returns what is running right
//!   now — feeds `dotagent status` and `dotagent doctor`.
//! - **Reaper task.** A periodic sweeper kills entries whose deadline elapsed
//!   even if the per-handle timeout was bypassed (panic, detached task,
//!   forgotten handle).
//!
//! The public contract is intentionally small — see `Supervisor`,
//! `SpawnSpec`, and `SupervisedHandle`.

#![deny(missing_debug_implementations)]

pub mod reaper;
mod signal;

use std::collections::HashMap;
use std::process::Output;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::{Child, Command};
use tokio::time::timeout;
use tracing::{debug, warn};

pub use crate::reaper::ReaperHandle;

/// Default grace window between `SIGTERM` and `SIGKILL`.
pub const DEFAULT_KILL_GRACE: Duration = Duration::from_secs(5);

/// Default reaper tick interval. Five seconds is a balance between catching
/// stuck processes quickly and not waking the runtime needlessly.
pub const DEFAULT_REAPER_TICK: Duration = Duration::from_secs(5);

/// Stable identifier handed out by the supervisor to refer to a live process
/// without exposing the OS pid (which can be reused).
pub type ProcId = u64;

/// What role this subprocess plays. Drives defaults and surfaces in `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessKind {
    /// The agent process itself (fish / python / binary declared in `[run]`).
    Agent,
    /// `dotagent-plugin-<name> info`.
    PluginInfo,
    /// `dotagent-plugin-<name> validate`.
    PluginValidate,
    /// `dotagent-plugin-<name> invoke` for a `[[preflight]]` hook.
    Preflight,
    /// `dotagent-plugin-<name> invoke` for an `[[on_success]]` hook.
    Sink,
    /// `dotagent-plugin-<name> invoke` for an `[[on_failure]]` or notifier.
    Notify,
}

/// Who is responsible for this process. All fields are best-effort labels —
/// the supervisor never inspects them, but they show up in audit + status.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProcessOwner {
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_event: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
}

/// Caller intent for one spawn. Combines `kind` + `owner` + `deadline`.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub kind: ProcessKind,
    pub owner: ProcessOwner,
    pub deadline: Duration,
    /// Short human label for logs / status (e.g. `"sink-roam.invoke"`).
    pub label: String,
}

/// Snapshot of a live entry. Cloned out of the registry — never holds a lock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub id: ProcId,
    pub pid: u32,
    pub kind: ProcessKind,
    pub owner: ProcessOwner,
    pub label: String,
    pub started_at: DateTime<Local>,
    pub deadline_seconds: u64,
    /// Elapsed since spawn, in seconds, at the moment the snapshot was taken.
    pub age_seconds: u64,
    /// `age / deadline` as a 0..=100 integer (clamped). Useful for warnings.
    pub deadline_pct: u8,
}

/// Audit-ish event the supervisor publishes. The orchestrator wires this into
/// `dotagent_state::AuditLog`; tests use it to assert behavior.
#[derive(Debug, Clone)]
pub enum SupervisorEvent {
    Started(ProcessInfo),
    Finished {
        id: ProcId,
        owner: ProcessOwner,
        kind: ProcessKind,
        exit_code: Option<i32>,
        elapsed: Duration,
    },
    KilledTimeout {
        id: ProcId,
        owner: ProcessOwner,
        kind: ProcessKind,
        elapsed: Duration,
        deadline: Duration,
    },
}

/// Callback type for `Supervisor::with_event_handler`.
pub type EventHandler = Arc<dyn Fn(SupervisorEvent) + Send + Sync + 'static>;

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("spawn failed: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("io while supervising: {0}")]
    Io(#[from] std::io::Error),
    #[error("process {label} (pid {pid:?}) killed after {elapsed:?} (deadline {deadline:?})")]
    TimedOut {
        label: String,
        pid: Option<u32>,
        elapsed: Duration,
        deadline: Duration,
    },
}

pub type Result<T> = std::result::Result<T, SupervisorError>;

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor")
            .field("grace", &self.inner.grace)
            .finish_non_exhaustive()
    }
}

pub(crate) struct Inner {
    /// Live entries. Sync mutex (not tokio's): operations are short and
    /// `SupervisedHandle::Drop` needs to remove its entry synchronously to
    /// prevent the reaper from later `killpg`ing a pgid the OS may have
    /// reused for an unrelated process.
    registry: Mutex<HashMap<ProcId, Entry>>,
    next_id: AtomicU64,
    grace: Duration,
    /// Optional audit callback. RwLock instead of `Arc::get_mut`-once so
    /// callers can install it after clones exist (e.g. daemon clones the
    /// Supervisor across the plugin client before deciding to wire audit).
    on_event: RwLock<Option<EventHandler>>,
}

/// Internal registry record. Holds the data the reaper needs to enforce the
/// deadline without touching the child handle.
pub(crate) struct Entry {
    pub(crate) info_template: ProcessInfo,
    pub(crate) started_instant: Instant,
    pub(crate) pgid: Option<i32>,
    pub(crate) deadline: Duration,
    /// `false` while the per-handle timeout owns the lifecycle. Set to `true`
    /// by the reaper when it sends `SIGTERM` so the handle reports
    /// `TimedOut` if it ever wakes up.
    pub(crate) killed_by_reaper: bool,
}

impl Supervisor {
    /// Build a fresh supervisor with the default grace window.
    pub fn new() -> Self {
        Self::with_grace(DEFAULT_KILL_GRACE)
    }

    pub fn with_grace(grace: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                registry: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                grace,
                on_event: RwLock::new(None),
            }),
        }
    }

    /// Attach an audit callback. The handler is invoked synchronously inside
    /// the supervisor's task; keep it cheap (e.g. push to a channel). Safe to
    /// call at any time — even after the supervisor was cloned — because the
    /// handler is held behind a lock, not behind `Arc::get_mut`.
    #[must_use]
    pub fn with_event_handler(self, handler: EventHandler) -> Self {
        if let Ok(mut slot) = self.inner.on_event.write() {
            *slot = Some(handler);
        }
        self
    }

    /// Spawn `cmd` under supervision. On Unix the child is placed in its own
    /// process group so kill-tree semantics work for grandchildren.
    pub async fn spawn_supervised(
        &self,
        mut cmd: Command,
        spec: SpawnSpec,
    ) -> Result<SupervisedHandle> {
        // Pipe stdio is not forced here — callers configure stdout/stderr/stdin
        // before passing the Command in. We only enforce process-group + the
        // env contract.
        #[cfg(unix)]
        cmd.process_group(0);

        let child = cmd.spawn().map_err(SupervisorError::Spawn)?;
        let pid = child.id();
        // On Unix, with `process_group(0)`, the child becomes its own group
        // leader (pgid == pid). On other platforms we leave the field empty
        // and `killpg` becomes a no-op.
        let pgid = if cfg!(unix) {
            pid.map(|p| p as i32)
        } else {
            None
        };

        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let started_at = Local::now();
        let started_instant = Instant::now();
        let info_template = ProcessInfo {
            id,
            pid: pid.unwrap_or(0),
            kind: spec.kind,
            owner: spec.owner.clone(),
            label: spec.label.clone(),
            started_at,
            deadline_seconds: spec.deadline.as_secs(),
            age_seconds: 0,
            deadline_pct: 0,
        };

        {
            let mut reg = self.inner.registry.lock().expect("registry lock poisoned");
            reg.insert(
                id,
                Entry {
                    info_template: info_template.clone(),
                    started_instant,
                    pgid,
                    deadline: spec.deadline,
                    killed_by_reaper: false,
                },
            );
        }

        self.inner.emit(SupervisorEvent::Started(info_template));

        debug!(
            proc_id = id,
            pid,
            label = %spec.label,
            deadline_seconds = spec.deadline.as_secs(),
            "supervised spawn"
        );

        Ok(SupervisedHandle {
            id,
            child: Some(child),
            pgid,
            deadline: spec.deadline,
            grace: self.inner.grace,
            label: spec.label,
            kind: spec.kind,
            owner: spec.owner,
            supervisor: self.inner.clone(),
            started_instant,
        })
    }

    /// Cloned snapshot of every live entry — never holds the registry lock
    /// across an `.await` point.
    pub fn snapshot(&self) -> Vec<ProcessInfo> {
        let now = Instant::now();
        let reg = self.inner.registry.lock().expect("registry lock poisoned");
        reg.values()
            .map(|e| project_info(&e.info_template, e.started_instant, e.deadline, now))
            .collect()
    }

    /// Send `SIGTERM` to every live entry, wait the grace window, then
    /// `SIGKILL`. Used by the daemon on `SIGTERM`/`SIGINT`. Returns once
    /// every entry has been signalled — does not wait for the children to
    /// actually exit beyond the grace window.
    pub async fn shutdown(&self, grace: Duration) {
        let pgids: Vec<i32> = {
            let reg = self.inner.registry.lock().expect("registry lock poisoned");
            reg.values().filter_map(|e| e.pgid).collect()
        };
        if pgids.is_empty() {
            return;
        }
        for pgid in &pgids {
            let _ = signal::killpg(*pgid, signal::SIGTERM);
        }
        tokio::time::sleep(grace).await;
        let pgids_after: Vec<i32> = {
            let reg = self.inner.registry.lock().expect("registry lock poisoned");
            reg.values().filter_map(|e| e.pgid).collect()
        };
        for pgid in &pgids_after {
            let _ = signal::killpg(*pgid, signal::SIGKILL);
        }
    }

    /// Start the deadline sweeper in a background tokio task. Returns a
    /// `ReaperHandle` whose `abort()` stops the loop. Safe to call once per
    /// process; calling twice gives you two reapers (harmless but wasteful).
    pub fn start_reaper(&self, tick: Duration) -> ReaperHandle {
        reaper::start(self.inner.clone(), tick)
    }

    /// Start a background task that rewrites `path` with the current snapshot
    /// every `tick`. Lets out-of-process consumers (`dotagent status`,
    /// `doctor`) see what the daemon is supervising without needing IPC.
    /// Returns a `ReaperHandle` so callers can abort on shutdown.
    ///
    /// Writes are atomic: payload goes to `<path>.tmp` first, then `rename`
    /// swaps it in. Readers therefore never observe a half-written file.
    pub fn start_snapshot_writer(&self, path: std::path::PathBuf, tick: Duration) -> ReaperHandle {
        let sup = self.clone();
        let tmp_path = {
            let mut t = path.clone();
            let mut name = t.file_name().map(|n| n.to_os_string()).unwrap_or_default();
            name.push(".tmp");
            t.set_file_name(name);
            t
        };
        if let Some(parent) = path.parent() {
            // One-shot create at startup so the first tick isn't racing the
            // daemon's other state-dir initializers.
            let _ = std::fs::create_dir_all(parent);
        }
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let snap = sup.snapshot();
                let payload = match serde_json::to_vec_pretty(&snap) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if tokio::fs::write(&tmp_path, &payload).await.is_err() {
                    continue;
                }
                let _ = tokio::fs::rename(&tmp_path, &path).await;
            }
        });
        ReaperHandle::wrap(handle)
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Inner {
    pub(crate) fn emit(&self, evt: SupervisorEvent) {
        if let Ok(slot) = self.on_event.read() {
            if let Some(h) = slot.as_ref() {
                (h)(evt);
            }
        }
    }
}

fn project_info(
    base: &ProcessInfo,
    started: Instant,
    deadline: Duration,
    now: Instant,
) -> ProcessInfo {
    let age = now.saturating_duration_since(started);
    let age_seconds = age.as_secs();
    let pct = if deadline.is_zero() {
        0
    } else {
        let raw = (age.as_millis() * 100) / deadline.as_millis().max(1);
        raw.min(100) as u8
    };
    ProcessInfo {
        age_seconds,
        deadline_pct: pct,
        ..base.clone()
    }
}

// ---------------------------------------------------------------------------
// SupervisedHandle
// ---------------------------------------------------------------------------

/// Handle returned by `spawn_supervised`. Dropping without calling
/// `wait_with_output` leaves the child to the reaper.
pub struct SupervisedHandle {
    id: ProcId,
    child: Option<Child>,
    pgid: Option<i32>,
    deadline: Duration,
    grace: Duration,
    label: String,
    kind: ProcessKind,
    owner: ProcessOwner,
    supervisor: Arc<Inner>,
    started_instant: Instant,
}

impl std::fmt::Debug for SupervisedHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupervisedHandle")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

impl SupervisedHandle {
    pub fn id(&self) -> ProcId {
        self.id
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(|c| c.id())
    }

    /// Take ownership of the child's stdin pipe. Used by callers that need to
    /// stream a payload (e.g. the plugin protocol's JSON stdio contract)
    /// before waiting on output.
    pub fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.child.as_mut().and_then(|c| c.stdin.take())
    }

    /// Take ownership of the child's stdout pipe. Callers that want to drain
    /// in parallel (tee to a log file, line-by-line metrics) take stdout/stderr
    /// here and then call `wait_status` to await the exit code.
    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.child.as_mut().and_then(|c| c.stdout.take())
    }

    pub fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.child.as_mut().and_then(|c| c.stderr.take())
    }

    /// Wait for exit, enforcing the deadline. Unlike `wait_with_output`, on
    /// timeout this method still returns `Ok((status, true))` after killing
    /// the process group — callers that drain stdio themselves rely on the
    /// status code to fill their own outcome record. The `bool` is
    /// `timed_out`.
    pub async fn wait_status(mut self) -> Result<(std::process::ExitStatus, bool)> {
        let mut child = self.child.take().expect("child consumed only by wait");
        let deadline = self.deadline;
        let pid = child.id();

        match timeout(deadline, child.wait()).await {
            Ok(Ok(status)) => {
                let elapsed = self.started_instant.elapsed();
                self.finish(status.code(), elapsed);
                Ok((status, false))
            }
            Ok(Err(io_err)) => {
                let elapsed = self.started_instant.elapsed();
                self.finish(None, elapsed);
                Err(SupervisorError::Io(io_err))
            }
            Err(_) => {
                // Race guard: if the reaper already won the deadline, it's
                // already TERM/KILLed the group and emitted the audit event.
                // Just collect the status and return without double-emit.
                let reaper_owns = self.claim_handle_kill();
                if reaper_owns {
                    let status = child.wait().await?;
                    return Ok((status, true));
                }
                warn!(
                    proc_id = self.id,
                    label = %self.label,
                    pid,
                    deadline_seconds = deadline.as_secs(),
                    "subprocess deadline exceeded — SIGTERM → grace → SIGKILL"
                );
                self.kill_tree().await;
                let status = child.wait().await?;
                let elapsed = self.started_instant.elapsed();
                self.supervisor.emit(SupervisorEvent::KilledTimeout {
                    id: self.id,
                    owner: self.owner.clone(),
                    kind: self.kind,
                    elapsed,
                    deadline,
                });
                self.deregister();
                Ok((status, true))
            }
        }
    }

    /// Wait for the child to exit with stdio captured, enforcing the deadline.
    /// On timeout: `SIGTERM` → `grace` → `SIGKILL` to the process group, then
    /// `TimedOut` is returned.
    pub async fn wait_with_output(mut self) -> Result<Output> {
        let child = self.child.take().expect("child consumed only by wait");
        let deadline = self.deadline;
        let label = self.label.clone();
        let pid = child.id();

        let res = timeout(deadline, child.wait_with_output()).await;
        let elapsed = self.started_instant.elapsed();

        match res {
            Ok(Ok(output)) => {
                self.finish(output.status.code(), elapsed);
                Ok(output)
            }
            Ok(Err(io_err)) => {
                self.finish(None, elapsed);
                Err(SupervisorError::Io(io_err))
            }
            Err(_elapsed_err) => {
                // Race guard: if the reaper already owns the kill (it
                // claimed the entry first), don't double-TERM/KILL or
                // double-emit. The reaper has the audit event.
                if self.claim_handle_kill() {
                    return Err(SupervisorError::TimedOut {
                        label,
                        pid,
                        elapsed,
                        deadline,
                    });
                }
                warn!(
                    proc_id = self.id,
                    label = %label,
                    pid,
                    deadline_seconds = deadline.as_secs(),
                    "subprocess deadline exceeded — SIGTERM → grace → SIGKILL"
                );
                self.kill_tree().await;
                let elapsed = self.started_instant.elapsed();
                self.supervisor.emit(SupervisorEvent::KilledTimeout {
                    id: self.id,
                    owner: self.owner.clone(),
                    kind: self.kind,
                    elapsed,
                    deadline,
                });
                self.deregister();
                Err(SupervisorError::TimedOut {
                    label,
                    pid,
                    elapsed,
                    deadline,
                })
            }
        }
    }

    fn finish(&self, exit_code: Option<i32>, elapsed: Duration) {
        self.supervisor.emit(SupervisorEvent::Finished {
            id: self.id,
            owner: self.owner.clone(),
            kind: self.kind,
            exit_code,
            elapsed,
        });
        self.deregister();
    }

    fn deregister(&self) {
        let mut reg = self
            .supervisor
            .registry
            .lock()
            .expect("registry lock poisoned");
        reg.remove(&self.id);
    }

    /// Returns `true` when the reaper has already claimed this entry (set
    /// `killed_by_reaper`). In that case the handle MUST NOT kill/emit again
    /// — the reaper holds the lifecycle. Returns `false` (without mutating)
    /// when the handle still owns the kill.
    fn claim_handle_kill(&self) -> bool {
        let reg = self
            .supervisor
            .registry
            .lock()
            .expect("registry lock poisoned");
        reg.get(&self.id)
            .map(|e| e.killed_by_reaper)
            .unwrap_or(true) // entry already removed by reaper → it owned
    }

    async fn kill_tree(&self) {
        if let Some(pgid) = self.pgid {
            let _ = signal::killpg(pgid, signal::SIGTERM);
            tokio::time::sleep(self.grace).await;
            let _ = signal::killpg(pgid, signal::SIGKILL);
        } else if let Some(child_id) = self.child.as_ref().and_then(|c| c.id()) {
            // Fallback for non-Unix or when pgid couldn't be captured.
            #[cfg(unix)]
            {
                let _ = signal::killpg(child_id as i32, signal::SIGTERM);
                tokio::time::sleep(self.grace).await;
                let _ = signal::killpg(child_id as i32, signal::SIGKILL);
            }
            #[cfg(not(unix))]
            {
                let _ = child_id; // suppress unused warning on Windows
            }
        }
    }
}

impl Drop for SupervisedHandle {
    fn drop(&mut self) {
        // Caller dropped without awaiting `wait_with_output` / `wait_status`.
        // Without this hook the entry would linger in the registry forever
        // and the reaper would later `killpg` the stored pgid — which the OS
        // may have reused for an unrelated process by then. Removing the
        // entry now (synchronous mutex, no .await) closes that hole. We
        // still don't kill the child here: tokio drops it via the Child
        // we held, and its background reaper waitpids it; the user opted
        // out of supervised kill by dropping without await.
        if self.child.is_some() {
            self.deregister();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_info_clamps_pct_at_100() {
        let now = Instant::now();
        let started = now - Duration::from_secs(10);
        let base = ProcessInfo {
            id: 1,
            pid: 100,
            kind: ProcessKind::Sink,
            owner: ProcessOwner::default(),
            label: "x".into(),
            started_at: Local::now(),
            deadline_seconds: 5,
            age_seconds: 0,
            deadline_pct: 0,
        };
        let p = project_info(&base, started, Duration::from_secs(5), now);
        assert_eq!(p.deadline_pct, 100);
        assert!(p.age_seconds >= 9);
    }

    #[test]
    fn project_info_zero_deadline_does_not_panic() {
        let now = Instant::now();
        let base = ProcessInfo {
            id: 1,
            pid: 100,
            kind: ProcessKind::Sink,
            owner: ProcessOwner::default(),
            label: "x".into(),
            started_at: Local::now(),
            deadline_seconds: 0,
            age_seconds: 0,
            deadline_pct: 0,
        };
        let p = project_info(&base, now, Duration::ZERO, now);
        assert_eq!(p.deadline_pct, 0);
    }
}
