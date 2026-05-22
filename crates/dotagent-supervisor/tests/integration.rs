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

/// Defense-in-depth: the per-handle timeout is bypassed when the handle is
/// dropped without awaiting `wait_with_output`. The reaper must still kill
/// the child within deadline + grace + tick.
#[tokio::test(flavor = "current_thread")]
async fn reaper_kills_handle_dropped_without_await() {
    let sup = Supervisor::with_grace(Duration::from_millis(150));
    let _reaper = sup.start_reaper(Duration::from_millis(100));

    let handle = sup
        .spawn_supervised(sh("sleep 30"), spec("dropped", 300))
        .await
        .expect("spawn");
    let pid = handle.pid().expect("pid") as i32;
    drop(handle);

    // deadline(300) + grace(150) + tick(100) + slack
    tokio::time::sleep(Duration::from_millis(900)).await;

    assert!(
        sup.snapshot().is_empty(),
        "reaper should have removed the entry"
    );

    // SAFETY: kill(pid, 0) only probes existence; no mutation.
    let alive_rc = unsafe { libc::kill(pid, 0) };
    assert_ne!(
        alive_rc, 0,
        "pid {pid} should be gone after reaper sweep, but kill(0) succeeded"
    );
}

/// Shutdown reaps everything live. Used by the daemon on SIGTERM.
#[tokio::test(flavor = "current_thread")]
async fn shutdown_signals_every_live_entry() {
    let sup = Supervisor::with_grace(Duration::from_millis(150));
    let h1 = sup
        .spawn_supervised(sh("sleep 30"), spec("a", 60_000))
        .await
        .expect("spawn a");
    let h2 = sup
        .spawn_supervised(sh("sleep 30"), spec("b", 60_000))
        .await
        .expect("spawn b");
    let p1 = h1.pid().unwrap() as i32;
    let p2 = h2.pid().unwrap() as i32;
    // Drop (don't forget!) so tokio's internal child-reaping task can
    // waitpid() once the process is dead. mem::forget would leave the
    // processes zombified — visible to `kill(pid, 0)` as alive.
    drop(h1);
    drop(h2);

    sup.shutdown(Duration::from_millis(150)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    for p in [p1, p2] {
        assert!(
            is_dead_or_zombie(p),
            "pid {p} should be dead or zombie after shutdown (stat={:?})",
            proc_stat(p)
        );
    }
}

/// Returns `true` when `pid` is no longer running. A zombie (`Z`) counts as
/// dead — the process has exited and is only waiting for waitpid().
fn is_dead_or_zombie(pid: i32) -> bool {
    let stat = proc_stat(pid);
    stat.is_empty() || stat.starts_with('Z')
}

fn proc_stat(pid: i32) -> String {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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
    tokio::time::sleep(Duration::from_millis(200)).await;

    let snap = sup.snapshot();
    assert_eq!(snap.len(), 1);
    assert!(snap[0].deadline_pct > 0);
    assert!(snap[0].deadline_pct < 100);

    drop(handle); // release; child will be reaped only if we run a reaper —
                  // in this test we don't, so we kill it manually to clean up.
    let _ = std::process::Command::new("pkill")
        .args(["-P", &std::process::id().to_string()])
        .output();
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
