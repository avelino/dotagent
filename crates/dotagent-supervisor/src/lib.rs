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
//! - **Reaper task.** A sweeper kills entries whose deadline elapsed even if
//!   the per-handle timeout was bypassed (panic, detached task, forgotten
//!   handle). It is deadline-driven, not periodic: it sleeps until the nearest
//!   deadline and parks outright while nothing is supervised, so an idle
//!   daemon holds no timers. The snapshot writer parks on the same signal.
//!
//! The public contract is intentionally small — see `Supervisor`,
//! `SpawnSpec`, and `SupervisedHandle`.

#![deny(missing_debug_implementations)]

pub mod orphan;
pub mod reaper;
mod signal;

use std::collections::HashMap;
use std::process::Output;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::time::timeout;
use tracing::{debug, warn};

pub use crate::reaper::ReaperHandle;

/// Default grace window between `SIGTERM` and `SIGKILL`.
pub const DEFAULT_KILL_GRACE: Duration = Duration::from_secs(5);

/// How often the snapshot file is refreshed *while something is supervised*.
///
/// There is no matching constant for the reaper: it is deadline-driven rather
/// than periodic. See [`Supervisor::start_reaper`].
pub const SNAPSHOT_TICK: Duration = Duration::from_secs(2);

/// Stable identifier handed out by the supervisor to refer to a live process
/// without exposing the OS pid (which can be reused).
pub type ProcId = u64;

/// What role this subprocess plays. Drives defaults and surfaces in `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessKind {
    /// The agent process itself (fish / python / binary declared in `[run]`).
    Agent,
    /// An agent declaring `[lifecycle] mode = "persistent"` — spawned once and
    /// handed requests until it is recycled. Its `deadline` is not the length
    /// of a run: it is whichever clock currently applies (the idle window
    /// while nobody is talking to it, the request deadline while somebody is).
    PersistentAgent,
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
    /// A `scripts/` executable packaged inside a skill, run on request from an
    /// MCP client. Not declared by any manifest, which is exactly why it needs
    /// its own label in `status` and the audit log.
    Skill,
}

impl std::fmt::Display for ProcessKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ProcessKind::Agent => "agent",
            ProcessKind::PersistentAgent => "persistent",
            ProcessKind::PluginInfo => "plugin_info",
            ProcessKind::PluginValidate => "plugin_validate",
            ProcessKind::Preflight => "preflight",
            ProcessKind::Sink => "sink",
            ProcessKind::Notify => "notify",
            ProcessKind::Skill => "skill",
        };
        f.write_str(s)
    }
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
    /// Process-group id, when the platform gave us one (always `Some(pid)` on
    /// Unix — every supervised spawn is its own group leader).
    ///
    /// Persisted, together with [`ProcessInfo::command`], so a *later* daemon
    /// can prove a pid it reads off disk is still the process that was
    /// recorded before it signals anything. See [`crate::orphan`]. Both carry
    /// `#[serde(default)]` because a snapshot written by an older build has
    /// neither, and a record that cannot be identity-checked must deserialize
    /// into one that is refused rather than fail the whole file.
    #[serde(default)]
    pub pgid: Option<i32>,
    /// The command line as spawned, tokens separated by single spaces.
    #[serde(default)]
    pub command: String,
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
    /// Bumped on every registration. The reaper and the snapshot writer park
    /// on it while the registry is empty, which is most of a laptop's day —
    /// polling instead would cost tens of thousands of timer wake-ups daily
    /// to observe that nothing changed.
    ///
    /// `watch` rather than `Notify` for two reasons: it stores the version, so
    /// a spawn racing a waiter that is about to park cannot be lost; and it
    /// wakes *every* waiter, whereas `Notify::notify_one` would wake only one
    /// of the two tasks that wait here.
    ///
    /// Waiters call `wake.subscribe()` **before** reading the registry:
    /// checking first and subscribing second can miss a spawn that lands in
    /// between, and then sleep past its deadline.
    wake: watch::Sender<u64>,
}

impl Inner {
    /// Wake everything parked on [`Inner::wake`].
    fn wake_all(&self) {
        self.wake.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Claim a live process for a lifecycle owner that will kill it.
    ///
    /// The claim and the flag update happen under one lock acquisition. A
    /// waiter that loses this race must only collect the child's status; the
    /// claimant owns the signal and the lifecycle event.
    fn claim(&self, id: ProcId) -> Option<KillClaim> {
        let mut reg = self.registry.lock().expect("registry lock poisoned");
        let entry = reg.get_mut(&id)?;
        if entry.killed_by_reaper {
            return None;
        }
        entry.killed_by_reaper = true;
        Some(KillClaim {
            id,
            pgid: entry.pgid,
            owner: entry.info_template.owner.clone(),
            kind: entry.info_template.kind,
        })
    }

    /// Finish a cancellation claim synchronously.
    ///
    /// Cancellation cannot await the normal TERM grace period: the owning
    /// task is already being dropped. `killpg(SIGKILL)` is a non-blocking
    /// syscall and preserves kill-tree semantics without leaving a cleanup
    /// task detached behind it.
    fn force_claimed(&self, claim: KillClaim) {
        if let Some(pgid) = claim.pgid {
            let _ = signal::killpg(pgid, signal::SIGKILL);
        }
        let elapsed = {
            let mut reg = self.registry.lock().expect("registry lock poisoned");
            reg.remove(&claim.id)
                .map(|entry| entry.started_instant.elapsed())
        };
        let Some(elapsed) = elapsed else {
            return;
        };
        self.wake_all();
        self.emit(SupervisorEvent::Finished {
            id: claim.id,
            owner: claim.owner,
            kind: claim.kind,
            exit_code: None,
            elapsed,
        });
    }
}

/// Internal registry record. Holds the data the reaper needs to enforce the
/// deadline without touching the child handle.
pub(crate) struct Entry {
    pub(crate) info_template: ProcessInfo,
    pub(crate) started_instant: Instant,
    pub(crate) pgid: Option<i32>,
    pub(crate) deadline: Duration,
    /// `false` while the waiting handle owns the lifecycle. Set to `true`
    /// when the reaper, `terminate`, or cancellation cleanup claims the kill,
    /// so the waiting handle does not kill or audit the process again. The
    /// legacy field name is retained because the reaper uses it as its claim
    /// guard; `reaper_owned` distinguishes who won that claim.
    pub(crate) killed_by_reaper: bool,
    /// Shared with the handle so callers can still observe reaper ownership
    /// after the reaper removes this entry from the registry.
    pub(crate) reaper_owned: Arc<AtomicBool>,
}

#[derive(Debug)]
struct KillClaim {
    id: ProcId,
    pgid: Option<i32>,
    owner: ProcessOwner,
    kind: ProcessKind,
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
                wake: watch::channel(0).0,
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

        // Snapshotted before the spawn consumes it: the next daemon needs to
        // know what this pid was running to tell it apart from a process that
        // merely inherited the number. See `orphan::classify`.
        let command = render_command(&cmd);

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
            pgid,
            command,
        };
        let reaper_owned = Arc::new(AtomicBool::new(false));

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
                    reaper_owned: reaper_owned.clone(),
                },
            );
        }
        // After the lock is released, so a woken reaper does not immediately
        // block on it. Must happen on every registration: the reaper's next
        // wake-up is computed from the nearest deadline in the registry, and
        // this entry may well be nearer than whatever it is sleeping on.
        self.inner.wake_all();

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
            reaper_owned,
        })
    }

    /// Restart the deadline clock for a live entry, with a new deadline.
    ///
    /// A one-shot subprocess has one deadline for its whole life. A persistent
    /// one has two, alternating: the request deadline while it is answering,
    /// and the idle window while it is not. Rather than growing a second timer
    /// beside the reaper, the pool re-points the reaper's existing clock at
    /// whichever one currently applies — so an idle timeout is enforced by the
    /// same sweep, with the same kill-tree, as every other deadline.
    ///
    /// Returns `false` when the entry is gone or the reaper has already
    /// claimed it. Callers must read that as "this process is dead" and
    /// respawn rather than write to it.
    pub fn retime(&self, id: ProcId, deadline: Duration) -> bool {
        let retimed = {
            let mut reg = self.inner.registry.lock().expect("registry lock poisoned");
            match reg.get_mut(&id) {
                Some(entry) if !entry.killed_by_reaper => {
                    entry.started_instant = Instant::now();
                    entry.deadline = deadline;
                    entry.info_template.deadline_seconds = deadline.as_secs();
                    true
                }
                _ => false,
            }
        };
        // The reaper sleeps until the nearest deadline it knew about when it
        // went to sleep. Re-pointing a clock under it — which is exactly what
        // the persistent pool does on every request — can move that deadline
        // *earlier*, so it has to recompute. Signalled after the lock drops so
        // the woken task is not immediately blocked on it.
        if retimed {
            self.inner.wake_all();
        }
        retimed
    }

    /// Kill one live entry: `SIGTERM` to its process group, grace, `SIGKILL`.
    ///
    /// The reaper kills on deadline and `shutdown` kills everything; this is
    /// the third case — the caller decided, for its own reasons (an idle
    /// instance evicted to make room, an invocation cap reached), that this
    /// specific process should go. Emits `Finished { exit_code: None }` and
    /// deregisters, so `status` stops showing it immediately.
    ///
    /// No-op when the id is unknown or already claimed by the reaper.
    pub async fn terminate(&self, id: ProcId) {
        let Some(claim) = self.inner.claim(id) else {
            return;
        };
        if let Some(pgid) = claim.pgid {
            let _ = signal::killpg(pgid, signal::SIGTERM);
            tokio::time::sleep(self.inner.grace).await;
            let _ = signal::killpg(pgid, signal::SIGKILL);
        }
        let elapsed = {
            let mut reg = self.inner.registry.lock().expect("registry lock poisoned");
            reg.remove(&id).map(|e| e.started_instant.elapsed())
        };
        let Some(elapsed) = elapsed else {
            return;
        };
        // Same reason as `SupervisedHandle::deregister`: the reaper is asleep
        // on a deadline this entry may have owned.
        self.inner.wake_all();
        self.inner.emit(SupervisorEvent::Finished {
            id,
            owner: claim.owner,
            kind: claim.kind,
            exit_code: None,
            elapsed,
        });
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
    ///
    /// The sweeper is deadline-driven, not periodic: it sleeps until the
    /// nearest deadline in the registry and parks entirely while the registry
    /// is empty. It therefore takes no tick argument — there is no interval to
    /// tune, and a sweep lands *on* the deadline rather than up to one tick
    /// late.
    pub fn start_reaper(&self) -> ReaperHandle {
        reaper::start(self.inner.clone())
    }

    /// Start a background task that keeps `path` in sync with the current
    /// snapshot. Lets out-of-process consumers (`dotagent status`, `doctor`)
    /// see what the daemon is supervising without needing IPC. Returns a
    /// `ReaperHandle` so callers can abort on shutdown.
    ///
    /// Writes are atomic: payload goes to `<path>.tmp` first, then `rename`
    /// swaps it in. Readers therefore never observe a half-written file.
    ///
    /// `tick` bounds how stale the file can get *while something is running*
    /// (`age_seconds` advances on its own, so a live snapshot is never twice
    /// the same). Once the registry drains, the task writes the empty snapshot
    /// once and then parks on [`Inner::wake`] rather than continuing to tick:
    /// the file is already correct, and rewriting identical bytes every couple
    /// of seconds is a wake-up and a dirtied page for no reader's benefit.
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
            let mut written: Option<Vec<u8>> = None;
            loop {
                // Subscribed before the snapshot is taken so a spawn racing
                // the park below is recorded rather than missed.
                let mut wake = sup.inner.wake.subscribe();

                let snap = sup.snapshot();
                let Ok(payload) = serde_json::to_vec_pretty(&snap) else {
                    tokio::time::sleep(tick).await;
                    continue;
                };

                let mut landed = written.as_deref() == Some(payload.as_slice());
                if !landed
                    && tokio::fs::write(&tmp_path, &payload).await.is_ok()
                    && tokio::fs::rename(&tmp_path, &path).await.is_ok()
                {
                    written = Some(payload);
                    landed = true;
                }

                // `landed` must describe *this* payload, not merely that some
                // earlier write succeeded: if the registry drains and writing
                // the empty snapshot then fails (ENOSPC, permissions), parking
                // here would strand a file still claiming a live process until
                // the next spawn. Ticking is how a failed write gets retried.
                if snap.is_empty() && landed {
                    let _ = wake.changed().await;
                } else {
                    tokio::time::sleep(tick).await;
                }
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

/// Program + args, single-space separated, lossy on non-UTF8.
///
/// Not a shell-quoted command line — it is never executed, only compared
/// against what `ps` reports for a pid a later daemon is about to signal.
fn render_command(cmd: &Command) -> String {
    let std = cmd.as_std();
    let mut parts = vec![std.get_program().to_string_lossy().into_owned()];
    parts.extend(std.get_args().map(|a| a.to_string_lossy().into_owned()));
    parts.join(" ")
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

/// Handle returned by `spawn_supervised`. Dropping without awaiting
/// `wait_with_output` / `wait_status` deregisters the entry synchronously
/// — the supervisor stops tracking it and the reaper will NOT enforce the
/// deadline anymore. The underlying child is dropped with the handle and
/// tokio reaps the process in the background; the caller has effectively
/// opted out of supervised kill semantics. This trade-off exists to avoid
/// `killpg`ing a pgid the OS may have reused for an unrelated process.
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
    reaper_owned: Arc<AtomicBool>,
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

/// Synchronous fallback for a wait future that is dropped while its child is
/// still being awaited.
///
/// The guard lives inside `wait_status`/`wait_with_output`, after the child has
/// been moved into the wait future. Its `Drop` runs before the child can be
/// released by the canceled future, so the PGID is still owned by the child
/// we spawned. It never sleeps or spawns a detached cleanup task.
struct WaitCleanup {
    supervisor: Arc<Inner>,
    id: ProcId,
    claim: Option<KillClaim>,
    armed: bool,
}

impl WaitCleanup {
    fn new(handle: &SupervisedHandle) -> Self {
        Self {
            supervisor: handle.supervisor.clone(),
            id: handle.id,
            claim: None,
            armed: true,
        }
    }

    /// Claim the kill for this waiter. `false` means another owner already
    /// won the race and will perform the signal and audit.
    fn claim(&mut self) -> bool {
        if self.claim.is_some() {
            return true;
        }
        let Some(claim) = self.supervisor.claim(self.id) else {
            self.armed = false;
            return false;
        };
        self.claim = Some(claim);
        true
    }

    /// Mark the lifecycle complete. The process has already exited or the
    /// caller has removed the claimed entry, so `Drop` must stay inert.
    fn complete(&mut self) {
        self.armed = false;
        self.claim = None;
    }
}

impl Drop for WaitCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let claim = self.claim.take().or_else(|| self.supervisor.claim(self.id));
        if let Some(claim) = claim {
            self.supervisor.force_claimed(claim);
        }
    }
}

impl SupervisedHandle {
    pub fn id(&self) -> ProcId {
        self.id
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(|c| c.id())
    }

    /// Has the child exited already? Never blocks.
    ///
    /// A caller that holds a handle across many requests — rather than
    /// awaiting it once — needs to know whether the process on the other end
    /// of the pipe is still there before writing to it. Without this, the
    /// first sign of a dead persistent agent is an `EPIPE` halfway through a
    /// request.
    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        match self.child.as_mut() {
            Some(child) => child.try_wait(),
            // The child was already consumed by a `wait_*` call.
            None => Ok(None),
        }
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

    /// Whether the reaper claimed this process for a deadline kill.
    ///
    /// The flag remains available after the reaper removes the process from
    /// the registry, which lets persistent request readers classify EOF as a
    /// timeout instead of ambiguous delivery.
    pub fn reaper_owns_deadline(&self) -> bool {
        self.reaper_owned.load(Ordering::Acquire)
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
        let mut cleanup = WaitCleanup::new(&self);

        match timeout(deadline, child.wait()).await {
            Ok(Ok(status)) => {
                cleanup.complete();
                let elapsed = self.started_instant.elapsed();
                self.finish(status.code(), elapsed);
                Ok((status, false))
            }
            Ok(Err(io_err)) => {
                cleanup.complete();
                let elapsed = self.started_instant.elapsed();
                self.finish(None, elapsed);
                Err(SupervisorError::Io(io_err))
            }
            Err(_) => {
                // Race guard: if the reaper or another explicit terminator
                // already claimed the lifecycle, just collect the status.
                if !cleanup.claim() {
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
                let elapsed = self.started_instant.elapsed();
                self.deregister();
                cleanup.complete();
                self.supervisor.emit(SupervisorEvent::KilledTimeout {
                    id: self.id,
                    owner: self.owner.clone(),
                    kind: self.kind,
                    elapsed,
                    deadline,
                });
                let status = child.wait().await?;
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
        let mut cleanup = WaitCleanup::new(&self);

        let res = timeout(deadline, child.wait_with_output()).await;
        let elapsed = self.started_instant.elapsed();

        match res {
            Ok(Ok(output)) => {
                cleanup.complete();
                self.finish(output.status.code(), elapsed);
                Ok(output)
            }
            Ok(Err(io_err)) => {
                cleanup.complete();
                self.finish(None, elapsed);
                Err(SupervisorError::Io(io_err))
            }
            Err(_elapsed_err) => {
                // Race guard: if the reaper or another explicit terminator
                // already claimed the kill, don't double-TERM/KILL or
                // double-emit. That owner has the lifecycle event.
                if !cleanup.claim() {
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
                self.deregister();
                cleanup.complete();
                self.supervisor.emit(SupervisorEvent::KilledTimeout {
                    id: self.id,
                    owner: self.owner.clone(),
                    kind: self.kind,
                    elapsed,
                    deadline,
                });
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
        let removed = {
            let mut reg = self
                .supervisor
                .registry
                .lock()
                .expect("registry lock poisoned");
            match reg.get(&self.id) {
                Some(entry) if !entry.killed_by_reaper => {
                    reg.remove(&self.id);
                    true
                }
                _ => false,
            }
        };
        if !removed {
            return;
        }
        self.supervisor.wake_all();
        self.supervisor.emit(SupervisorEvent::Finished {
            id: self.id,
            owner: self.owner.clone(),
            kind: self.kind,
            exit_code,
            elapsed,
        });
    }

    fn deregister(&self) {
        {
            let mut reg = self
                .supervisor
                .registry
                .lock()
                .expect("registry lock poisoned");
            reg.remove(&self.id);
        }
        // Removing an entry can only bring the nearest deadline *closer* or
        // empty the registry, and the reaper is asleep on the old one either
        // way. Without this, a process that finishes in a second leaves a
        // ten-minute timer armed behind it, and "an idle daemon holds no
        // timers" stays false until that timer fires on nothing.
        self.supervisor.wake_all();
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
        // may have reused for an unrelated process by then. Remove an
        // unclaimed entry now (synchronous mutex, no .await) to close that
        // hole. A claimed entry stays for its owner to finish the kill and
        // emit its one lifecycle event. We still don't kill the child here:
        // tokio drops it via the Child we held, and its background reaper
        // waitpids it; the user opted out of supervised kill without an owner.
        let claimed = self
            .supervisor
            .registry
            .lock()
            .ok()
            .and_then(|reg| reg.get(&self.id).map(|entry| entry.killed_by_reaper))
            .unwrap_or(true);
        if self.child.is_some() && !claimed {
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
            pgid: Some(100),
            command: "sleep 1".into(),
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
            pgid: Some(100),
            command: "sleep 1".into(),
        };
        let p = project_info(&base, now, Duration::ZERO, now);
        assert_eq!(p.deadline_pct, 0);
    }
}
