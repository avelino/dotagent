//! Integration tests for `dotagent-supervisor`.
//!
//! Spawn real subprocesses (Unix-only — supervisor relies on POSIX
//! process groups). Each test stays under ~2s to keep `cargo test` fast.

#![cfg(unix)]

use std::time::Duration;

use dotagent_supervisor::{
    ProcessKind, ProcessOwner, SpawnSpec, Supervisor, SupervisorError, SupervisorEvent,
};
use tokio::process::Command;

fn spec(label: &str, deadline_ms: u64) -> SpawnSpec {
    SpawnSpec {
        kind: ProcessKind::Sink,
        owner: ProcessOwner {
            agent: "test".into(),
            schedule: Some("default".into()),
            hook_event: None,
            plugin: Some(label.into()),
        },
        deadline: Duration::from_millis(deadline_ms),
        label: label.into(),
    }
}

fn sh(script: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd
}

#[tokio::test(flavor = "current_thread")]
async fn happy_path_returns_output_and_clears_registry() {
    let sup = Supervisor::with_grace(Duration::from_millis(200));
    let handle = sup
        .spawn_supervised(sh("echo hello"), spec("echo", 5_000))
        .await
        .expect("spawn");

    // While alive the registry sees it.
    assert_eq!(sup.snapshot().len(), 1);

    let out = handle.wait_with_output().await.expect("wait");
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
    assert!(
        sup.snapshot().is_empty(),
        "registry should drain after wait"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn timeout_kills_and_reports_timedout() {
    let sup = Supervisor::with_grace(Duration::from_millis(150));
    let handle = sup
        .spawn_supervised(sh("sleep 30"), spec("sleep", 200))
        .await
        .expect("spawn");

    let started = std::time::Instant::now();
    let res = handle.wait_with_output().await;
    let elapsed = started.elapsed();

    match res {
        Err(SupervisorError::TimedOut { .. }) => {}
        other => panic!("expected TimedOut, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(2),
        "should kill within deadline+grace+slack, got {elapsed:?}"
    );
    assert!(sup.snapshot().is_empty(), "registry should drain");
}

/// The smoking-gun test for issue #36: a sink plugin that spawns `mcp` (a
/// grandchild) used to leave it orphaned forever. With process groups, killing
/// the immediate child reaps the whole group.
#[tokio::test(flavor = "current_thread")]
async fn timeout_reaps_grandchildren_via_process_group() {
    let sup = Supervisor::with_grace(Duration::from_millis(150));
    // `sleep 30 & wait` makes sh fork a background sleep and then block in
    // `wait`. Without killpg the sleep would outlive `sh`.
    let handle = sup
        .spawn_supervised(sh("sleep 30 & wait"), spec("sh+sleep", 200))
        .await
        .expect("spawn");
    let pgid = handle.pid().expect("child pid") as i32;

    let _ = handle.wait_with_output().await;

    // Give the OS a beat to update its bookkeeping.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let ps = std::process::Command::new("ps")
        .args(["-o", "pid=", "-g", &pgid.to_string()])
        .output()
        .expect("ps");
    let stdout = String::from_utf8_lossy(&ps.stdout);
    assert!(
        stdout.trim().is_empty(),
        "process group {pgid} should be empty after kill_tree, ps stdout=\n{stdout}"
    );
}

/// Defense-in-depth: when a handle is leaked via `mem::forget`, Drop never
/// runs, so the synchronous deregister never fires. The reaper must still
/// enforce the deadline. (Plain `drop` deregisters and is exercised by
/// `drop_without_await_clears_registry`.)
#[tokio::test(flavor = "current_thread")]
async fn reaper_kills_forgotten_handle_via_deadline() {
    let sup = Supervisor::with_grace(Duration::from_millis(150));
    let _reaper = sup.start_reaper(Duration::from_millis(100));

    let handle = sup
        .spawn_supervised(sh("sleep 30 & wait"), spec("forgotten", 300))
        .await
        .expect("spawn");
    let pgid = handle.pid().expect("pid") as i32;
    // Bypass Drop so the supervisor is forced to lean on the reaper.
    std::mem::forget(handle);

    // deadline(300) + grace(150) + tick(100) + generous slack
    tokio::time::sleep(Duration::from_millis(1_200)).await;

    assert!(
        sup.snapshot().is_empty(),
        "reaper should have removed the entry"
    );
    // After killpg, every member of the group must be gone or zombie —
    // never still running (S/R). Using `ps -g` keeps the check stable
    // against macOS pid reuse.
    let ps = std::process::Command::new("ps")
        .args(["-o", "stat=", "-g", &pgid.to_string()])
        .output()
        .expect("ps");
    let stats = String::from_utf8_lossy(&ps.stdout);
    for stat in stats.lines().map(str::trim).filter(|s| !s.is_empty()) {
        assert!(
            stat.starts_with('Z'),
            "process group {pgid} still has a non-zombie entry stat={stat:?}"
        );
    }
}

/// Shutdown reaps everything live. Used by the daemon on SIGTERM.
///
/// The handles are kept (not dropped) — `SupervisedHandle::Drop` now
/// deregisters synchronously, which is correct semantics but would defeat
/// `shutdown` if the test dropped them first. Concrete daemon code keeps
/// its handles in `tokio::spawn`ed tasks; we mirror that here.
///
/// Validation is done at the **process-group** level. `ps -p <pid>` against
/// individual pids was flaky on macOS CI because freed pids get reused by
/// system processes (we'd see `stat="S<"` from an unrelated kernel helper).
#[tokio::test(flavor = "current_thread")]
async fn shutdown_signals_every_live_entry() {
    let sup = Supervisor::with_grace(Duration::from_millis(100));
    // `sleep & wait` keeps the shell alive as pgroup leader with one child.
    let h1 = sup
        .spawn_supervised(sh("sleep 30 & wait"), spec("a", 60_000))
        .await
        .expect("spawn a");
    let h2 = sup
        .spawn_supervised(sh("sleep 30 & wait"), spec("b", 60_000))
        .await
        .expect("spawn b");
    let pg1 = h1.pid().expect("pid a") as i32;
    let pg2 = h2.pid().expect("pid b") as i32;

    // Trigger shutdown in parallel with the waits. The handles stay alive
    // (their wait_status owns them) so the registry remains populated when
    // shutdown reads it.
    let sup_for_shutdown = sup.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        sup_for_shutdown.shutdown(Duration::from_millis(100)).await;
    });

    let (st1, _) = h1.wait_status().await.expect("wait a");
    let (st2, _) = h2.wait_status().await.expect("wait b");
    assert!(
        !st1.success(),
        "child a should have been killed by shutdown"
    );
    assert!(
        !st2.success(),
        "child b should have been killed by shutdown"
    );

    // Give the OS a beat to clear the process group bookkeeping.
    tokio::time::sleep(Duration::from_millis(300)).await;
    for pgid in [pg1, pg2] {
        let ps = std::process::Command::new("ps")
            .args(["-o", "pid=", "-g", &pgid.to_string()])
            .output()
            .expect("ps");
        let stdout = String::from_utf8_lossy(&ps.stdout);
        assert!(
            stdout.trim().is_empty(),
            "process group {pgid} should be empty after shutdown, ps stdout=\n{stdout}"
        );
    }
}

/// Snapshot exposes `age_seconds` and `deadline_pct` so `dotagent status`
/// can render a usable progress indicator.
#[tokio::test(flavor = "current_thread")]
async fn snapshot_reports_age_and_deadline_pct() {
    let sup = Supervisor::with_grace(Duration::from_millis(150));
    let handle = sup
        .spawn_supervised(sh("sleep 5"), spec("snap", 1_000))
        .await
        .expect("spawn");
    let pid = handle.pid().expect("pid") as i32;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let snap = sup.snapshot();
    assert_eq!(snap.len(), 1);
    assert!(snap[0].deadline_pct > 0);
    assert!(snap[0].deadline_pct < 100);

    drop(handle); // release; we kill the specific pgroup we spawned —
                  // never `pkill -P $$` because cargo test runs in parallel.
                  // SAFETY: kill with a captured pid is a stable syscall.
    let _ = unsafe { libc::killpg(pid, libc::SIGTERM) };
}

/// Issue #36 corrigir-1: race between handle's per-deadline timeout and the
/// reaper's sweep must NOT emit `KilledTimeout` twice for the same id.
#[tokio::test(flavor = "current_thread")]
async fn race_handle_vs_reaper_emits_killed_once() {
    use std::sync::{Arc, Mutex};

    let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let counter_clone = counter.clone();
    // Reaper tick (50ms) << deadline (200ms) makes the reaper the likely
    // winner; the handle's timeout fires immediately after and must bail.
    let sup = Supervisor::with_grace(Duration::from_millis(100)).with_event_handler(Arc::new(
        move |evt: SupervisorEvent| {
            if let SupervisorEvent::KilledTimeout { .. } = evt {
                *counter_clone.lock().unwrap() += 1;
            }
        },
    ));
    let _reaper = sup.start_reaper(Duration::from_millis(50));
    let handle = sup
        .spawn_supervised(sh("sleep 30"), spec("race", 200))
        .await
        .expect("spawn");

    let _ = handle.wait_with_output().await; // will TimedOut

    // Wait long enough for reaper + handle to both have finished.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let n = *counter.lock().unwrap();
    assert_eq!(n, 1, "expected exactly one KilledTimeout, got {n}");
}

/// Issue #36 corrigir-2: dropping a handle without awaiting must remove the
/// entry from the registry synchronously, so the reaper does NOT later
/// `killpg` a pgid the OS may have reused.
#[tokio::test(flavor = "current_thread")]
async fn drop_without_await_clears_registry() {
    let sup = Supervisor::with_grace(Duration::from_millis(100));
    let handle = sup
        .spawn_supervised(sh("sleep 30"), spec("dropped-sync", 60_000))
        .await
        .expect("spawn");
    let pid = handle.pid().expect("pid") as i32;
    drop(handle);

    // No sleep: snapshot must be empty immediately because Drop deregisters
    // synchronously.
    assert!(
        sup.snapshot().is_empty(),
        "Drop should deregister entry immediately, registry still: {:?}",
        sup.snapshot()
    );

    // Clean up the still-running child so we don't leak processes between
    // tests.
    // SAFETY: kill with a known pid is a stable syscall.
    let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
}

/// Audit hooks fire for the success path.
#[tokio::test(flavor = "current_thread")]
async fn event_handler_observes_started_and_finished() {
    use std::sync::{Arc, Mutex};

    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let log_clone = log.clone();
    let sup = Supervisor::with_grace(Duration::from_millis(150)).with_event_handler(Arc::new(
        move |evt: SupervisorEvent| {
            let tag = match evt {
                SupervisorEvent::Started(_) => "started",
                SupervisorEvent::Finished { .. } => "finished",
                SupervisorEvent::KilledTimeout { .. } => "killed",
            };
            log_clone.lock().unwrap().push(tag.to_string());
        },
    ));

    let handle = sup
        .spawn_supervised(sh("true"), spec("audit", 5_000))
        .await
        .expect("spawn");
    let _ = handle.wait_with_output().await.expect("wait");

    let events = log.lock().unwrap().clone();
    assert_eq!(events, vec!["started", "finished"]);
}

// ---------------------------------------------------------------------------
// Persistent-agent primitives: a process the caller holds across many
// requests instead of awaiting once.
// ---------------------------------------------------------------------------

fn persistent_spec(label: &str, deadline_ms: u64) -> SpawnSpec {
    SpawnSpec {
        kind: ProcessKind::PersistentAgent,
        owner: ProcessOwner {
            agent: "test".into(),
            schedule: Some("trigger".into()),
            hook_event: None,
            plugin: None,
        },
        deadline: Duration::from_millis(deadline_ms),
        label: label.into(),
    }
}

/// The whole point of `retime`: the reaper's clock can be re-pointed, so an
/// idle window and a request deadline take turns on one timer.
#[tokio::test(flavor = "current_thread")]
async fn retime_postpones_the_reaper() {
    let sup = Supervisor::with_grace(Duration::from_millis(100));
    let _reaper = sup.start_reaper(Duration::from_millis(50));

    let mut handle = sup
        .spawn_supervised(sh("sleep 30 & wait"), persistent_spec("warm", 300))
        .await
        .expect("spawn");
    let id = handle.id();

    // Keep pushing the deadline out past what the original 300ms would allow.
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(sup.retime(id, Duration::from_millis(300)), "still alive");
    }

    assert_eq!(sup.snapshot().len(), 1, "retime should have kept it alive");
    assert!(
        handle.try_wait().expect("try_wait").is_none(),
        "process must still be running"
    );

    sup.terminate(id).await;
}

/// A shortened deadline is honoured on the next sweep — that is how an
/// instance gets recycled for sitting idle.
#[tokio::test(flavor = "current_thread")]
async fn retime_to_a_shorter_deadline_lets_the_reaper_win() {
    let sup = Supervisor::with_grace(Duration::from_millis(100));
    let _reaper = sup.start_reaper(Duration::from_millis(50));

    let mut handle = sup
        .spawn_supervised(sh("sleep 30 & wait"), persistent_spec("idle", 30_000))
        .await
        .expect("spawn");
    let id = handle.id();

    assert!(sup.retime(id, Duration::from_millis(100)));
    tokio::time::sleep(Duration::from_millis(800)).await;

    assert!(sup.snapshot().is_empty(), "reaper should have collected it");
    assert!(
        !sup.retime(id, Duration::from_secs(60)),
        "retime on a collected entry must report the process is gone"
    );
    assert!(
        handle.try_wait().expect("try_wait").is_some(),
        "try_wait should see the exit"
    );
}

/// `terminate` is the third way something dies: not a deadline, not a global
/// shutdown, but the caller deciding this one process should go.
#[tokio::test(flavor = "current_thread")]
async fn terminate_kills_the_tree_and_deregisters() {
    let sup = Supervisor::with_grace(Duration::from_millis(150));
    let mut handle = sup
        .spawn_supervised(sh("sleep 30 & wait"), persistent_spec("evicted", 30_000))
        .await
        .expect("spawn");
    let id = handle.id();
    let pgid = handle.pid().expect("pid") as i32;

    sup.terminate(id).await;

    assert!(sup.snapshot().is_empty(), "entry must be gone");
    assert!(
        handle.try_wait().expect("try_wait").is_some(),
        "child must have exited"
    );

    // Nothing from the group may still be running — the grandchild `sleep`
    // included.
    let out = std::process::Command::new("ps")
        .args(["-g", &pgid.to_string(), "-o", "stat="])
        .output()
        .expect("ps");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout
            .lines()
            .all(|l| l.trim().starts_with('Z') || l.trim().is_empty()),
        "process group {pgid} should be dead, ps stdout=\n{stdout}"
    );
}

/// Terminating twice, or terminating something the reaper already took, must
/// not panic or emit a second event.
#[tokio::test(flavor = "current_thread")]
async fn terminate_is_idempotent_and_emits_once() {
    use std::sync::{Arc, Mutex};

    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let log_clone = log.clone();
    let sup = Supervisor::with_grace(Duration::from_millis(50)).with_event_handler(Arc::new(
        move |evt: SupervisorEvent| {
            if let SupervisorEvent::Finished { .. } = evt {
                log_clone.lock().unwrap().push("finished".to_string());
            }
        },
    ));

    let handle = sup
        .spawn_supervised(sh("sleep 30 & wait"), persistent_spec("twice", 30_000))
        .await
        .expect("spawn");
    let id = handle.id();

    sup.terminate(id).await;
    sup.terminate(id).await;
    sup.terminate(999_999).await; // unknown id

    assert_eq!(log.lock().unwrap().len(), 1, "exactly one Finished");
}

/// A handle held across requests reports liveness without blocking.
#[tokio::test(flavor = "current_thread")]
async fn try_wait_reports_liveness_without_blocking() {
    let sup = Supervisor::with_grace(Duration::from_millis(100));
    let mut handle = sup
        .spawn_supervised(sh("sleep 0.2"), persistent_spec("short", 30_000))
        .await
        .expect("spawn");

    assert!(
        handle.try_wait().expect("try_wait").is_none(),
        "still alive"
    );
    tokio::time::sleep(Duration::from_millis(600)).await;
    let status = handle.try_wait().expect("try_wait").expect("exited");
    assert!(status.success());
}
