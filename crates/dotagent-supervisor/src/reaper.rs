//! Background sweeper that enforces deadlines even when no one is awaiting
//! the `SupervisedHandle`.
//!
//! The per-handle `wait_with_output` already enforces the deadline for the
//! happy path. The reaper exists as defense-in-depth: panics, detached
//! tasks, `mem::forget`-style misuse, or simply a caller that drops the
//! handle without awaiting it would otherwise leave a zombie supervised
//! entry hanging forever.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::{signal, Inner, SupervisorEvent};

/// Handle returned by `Supervisor::start_reaper`. Aborts the loop on drop or
/// when `abort()` is called.
#[derive(Debug)]
pub struct ReaperHandle {
    handle: JoinHandle<()>,
}

impl ReaperHandle {
    pub fn abort(&self) {
        self.handle.abort();
    }

    /// Wrap any existing `JoinHandle<()>` so the supervisor's snapshot writer
    /// (a sibling background task) gets the same drop-aborts-it semantics.
    pub fn wrap(handle: JoinHandle<()>) -> Self {
        Self { handle }
    }
}

impl Drop for ReaperHandle {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

pub(crate) fn start(inner: Arc<Inner>) -> ReaperHandle {
    let handle = tokio::spawn(async move {
        loop {
            // Subscribed before the registry is read: a spawn that lands
            // between the read and the park below must not be missed, or the
            // reaper sleeps through a deadline nearer than the one it sized
            // its sleep for.
            let mut wake = inner.wake.subscribe();

            match next_deadline(&inner) {
                // Nothing the reaper could ever act on. Park — this is where
                // an idle daemon spends its day, and it costs no wake-ups.
                None => {
                    let _ = wake.changed().await;
                }
                // Already past due: sweep without going through the timer.
                Some(d) if d.is_zero() => {}
                Some(d) => {
                    tokio::select! {
                        _ = tokio::time::sleep(d) => {}
                        // A nearer deadline may have just been registered;
                        // recompute rather than finish this sleep.
                        _ = wake.changed() => continue,
                    }
                }
            }
            sweep_once(&inner).await;
        }
    });
    ReaperHandle::wrap(handle)
}

/// Time until the nearest deadline the reaper is allowed to enforce.
///
/// `None` means there is nothing to wait for: either the registry is empty or
/// every entry is one the reaper will skip anyway. Returning `Some(ZERO)` for
/// those would spin — `sweep_once` ignores them, so the loop would find the
/// same entry past due on every pass.
fn next_deadline(inner: &Inner) -> Option<Duration> {
    let now = Instant::now();
    let reg = inner.registry.lock().expect("registry lock poisoned");
    reg.values()
        .filter(|e| !e.killed_by_reaper && e.pgid.is_some())
        .map(|e| {
            e.deadline
                .saturating_sub(now.saturating_duration_since(e.started_instant))
        })
        .min()
}

async fn sweep_once(inner: &Inner) {
    let now = Instant::now();
    // First pass: identify victims AND atomically mark them under a single
    // lock acquisition. Marking inside the same critical section as the
    // check guarantees that a racing `SupervisedHandle::wait_*` sees the
    // claim and bails — no double-emit.
    let (victims, grace) = {
        let mut reg = inner.registry.lock().expect("registry lock poisoned");
        let mut found: Vec<(u64, i32, Duration)> = Vec::new();
        for (id, entry) in reg.iter_mut() {
            if entry.killed_by_reaper {
                continue;
            }
            let age = now.saturating_duration_since(entry.started_instant);
            if age >= entry.deadline {
                if let Some(pgid) = entry.pgid {
                    entry.killed_by_reaper = true;
                    entry
                        .reaper_owned
                        .store(true, std::sync::atomic::Ordering::Release);
                    found.push((*id, pgid, age));
                }
            }
        }
        (found, inner.grace)
    };

    if victims.is_empty() {
        return;
    }

    for (id, pgid, age) in &victims {
        warn!(
            proc_id = id,
            pgid,
            age_seconds = age.as_secs(),
            "reaper: deadline exceeded, sending SIGTERM"
        );
        let _ = signal::killpg(*pgid, signal::SIGTERM);
    }

    tokio::time::sleep(grace).await;

    // Final pass: SIGKILL stragglers + remove from registry + emit events.
    let mut reg = inner.registry.lock().expect("registry lock poisoned");
    for (id, pgid, age) in victims {
        let _ = signal::killpg(pgid, signal::SIGKILL);
        if let Some(entry) = reg.remove(&id) {
            debug!(proc_id = id, pgid, "reaper: removed entry");
            inner.emit(SupervisorEvent::KilledTimeout {
                id,
                owner: entry.info_template.owner.clone(),
                kind: entry.info_template.kind,
                elapsed: age,
                deadline: entry.deadline,
            });
        }
    }
}
