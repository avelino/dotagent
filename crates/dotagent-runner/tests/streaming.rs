//! Integration tests for real-time stdout streaming (`StreamOptions`).
//!
//! Same philosophy as `persistent.rs`: a real subprocess through the real
//! runner — a mocked pipe would only prove the mock works. `DOTAGENT_HOME`
//! is pointed at a shared tempdir so the per-agent log tee never touches
//! the user's real home.

#![cfg(unix)]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dotagent_core::AgentManifest;
use dotagent_runner::{run_streaming, RunSpec, StreamOptions};
use dotagent_state::StateStore;
use dotagent_supervisor::{Supervisor, SupervisorEvent};

/// Redirect `DOTAGENT_HOME` once per test binary. Tests in one binary share
/// a process, so per-fixture `set_var` calls would race each other; a single
/// leaked tempdir keeps every log tee offline with no cleanup to coordinate.
static LOG_HOME: std::sync::OnceLock<()> = std::sync::OnceLock::new();

fn redirect_log_home() {
    LOG_HOME.get_or_init(|| {
        let dir = tempfile::tempdir().expect("log home tempdir");
        std::env::set_var("DOTAGENT_HOME", dir.keep());
    });
}

struct Fixture {
    dir: tempfile::TempDir,
    manifest: AgentManifest,
    state_root: tempfile::TempDir,
}

impl Fixture {
    fn new(script: &str) -> Self {
        Self::with_script_args(script, &[])
    }

    fn with_script_args(script: &str, script_args: &[&str]) -> Self {
        redirect_log_home();
        let dir = tempfile::tempdir().expect("tempdir");
        let script_path = dir.path().join("agent.sh");
        std::fs::write(&script_path, script).expect("write script");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let mut args = vec!["\"./agent.sh\"".to_string()];
        args.extend(script_args.iter().map(|arg| format!("\"{arg}\"")));
        let toml = format!(
            r#"
[agent]
name = "fixture"
monitor = false
timeout_seconds = 30

[run]
command = "bash"
args = [{}]
"#,
            args.join(", ")
        );
        let manifest: AgentManifest = toml::from_str(&toml).expect("fixture manifest parses");
        manifest.validate().expect("fixture manifest validates");
        Self {
            dir,
            manifest,
            state_root: tempfile::tempdir().expect("state tempdir"),
        }
    }

    fn state(&self) -> StateStore {
        StateStore::with_root(self.state_root.path().to_path_buf())
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }
}

fn spec<'a>(f: &'a Fixture, args: &'a [String]) -> RunSpec<'a> {
    RunSpec {
        manifest: &f.manifest,
        manifest_dir: f.dir(),
        schedule_id: "manual",
        args,
        dry_run: false,
        manifest_sha256: None,
        slug_override: None,
        extra_env: &[],
    }
}

/// Live (non-zombie) processes whose command line contains `marker`.
///
/// `ps -g <pgid>` does not answer "who is in this process group" on both
/// platforms. procps treats a numeric `-g` as a session id, so a child that
/// called `setpgid` without `setsid` is invisible and a leaked group looks
/// empty. BSD `ps` selects the process group with `-g` only in unix2003
/// mode; in legacy mode the flag is ignored and the number is a pid. That
/// pid is also the pgid of every supervised spawn, and macOS recycles it
/// quickly enough that the next group leader looks like a survivor.
///
/// The marker is on the agent's argv and on the grandchild's argv0, so a
/// recycled pid running something else does not match, and a grandchild
/// that left the group still does.
fn live_marked(marker: &str) -> Vec<String> {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,pgid=,stat=,command="])
        .output()
        .expect("ps must be available on Unix");
    let mut live = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if line.is_empty() || !line.contains(marker) {
            continue;
        }
        let mut cols = line.split_whitespace();
        let Some(pid) = cols.next() else { continue };
        let Some(ppid) = cols.next() else { continue };
        let Some(pgid) = cols.next() else { continue };
        let Some(stat) = cols.next() else { continue };
        if stat.starts_with('Z') {
            continue;
        }
        live.push(format!(
            "pid={pid} ppid={ppid} pgid={pgid} stat={stat} line={line}"
        ));
    }
    live
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn without_a_tap_the_tail_is_the_whole_output() {
    // Golden: no StreamOptions ⇒ same stdout_tail the buffered runner
    // produced — every line, joined by \n, nothing truncated.
    let f = Fixture::new("#!/usr/bin/env bash\nprintf 'one\\ntwo\\nthree\\n'\n");
    let state = f.state();
    let args: Vec<String> = vec![];

    let outcome = run_streaming(spec(&f, &args), StreamOptions::default(), &state, None)
        .await
        .expect("run");

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout_tail, "one\ntwo\nthree");
    assert_eq!(outcome.stdout_truncated_lines, 0);
}

#[tokio::test]
async fn the_tail_keeps_only_the_last_tail_lines() {
    // 502 lines in, TAIL_LINES (500) kept: the ring buffer must reproduce
    // the old `tail_lines` semantics — drop count and all.
    let f = Fixture::new("#!/usr/bin/env bash\nseq 1 502\n");
    let state = f.state();
    let args: Vec<String> = vec![];

    let outcome = run_streaming(spec(&f, &args), StreamOptions::default(), &state, None)
        .await
        .expect("run");

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout_truncated_lines, 2);
    let lines: Vec<&str> = outcome.stdout_tail.split('\n').collect();
    assert_eq!(lines.len(), 500, "the ring must hold exactly TAIL_LINES");
    assert_eq!(lines[0], "3", "the first two lines were dropped");
    assert_eq!(lines[499], "502");
}

#[tokio::test]
async fn the_tap_receives_lines_while_the_process_is_still_running() {
    // The agent sleeps 2s *after* its first line. Receiving that line in
    // well under 2s proves the callback fired from the reader task while
    // the subprocess was alive — not from a post-exit buffer flush, which
    // is the only thing the old runner could ever offer.
    let f = Fixture::new("#!/usr/bin/env bash\necho first\nsleep 2\necho second\n");
    let state = f.state();
    let args: Vec<String> = vec![];

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(16);
    let stream = StreamOptions {
        on_stdout_line: Some(Arc::new(move |line: &str| {
            // Non-blocking by contract: a full channel drops, and the run
            // must never stall on a slow consumer.
            let _ = tx.try_send(line.to_string());
        })),
    };

    let run_fut = run_streaming(spec(&f, &args), stream, &state, None);
    tokio::pin!(run_fut);

    // `#[tokio::test]` is a current-thread runtime, so the pinned future is
    // only polled by whoever awaits it — the select below polls both the
    // channel and the run, which is what actually starts the agent.
    let first = tokio::time::timeout(Duration::from_millis(1500), async {
        tokio::select! {
            line = rx.recv() => line.expect("channel still open"),
            outcome = &mut run_fut => {
                panic!("run resolved before any line was tapped: {outcome:?}")
            }
        }
    })
    .await
    .expect("first line must arrive while the agent is mid-sleep");
    assert_eq!(first, "first");

    // The run future cannot have resolved yet: the agent needs another ~2s.
    // Now let it finish and take the second line.
    let second = rx.recv().await.expect("second line");
    assert_eq!(second, "second");

    let outcome = run_fut.await.expect("run");
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout_tail, "first\nsecond");
    assert_eq!(outcome.stdout_truncated_lines, 0);
}

#[tokio::test]
async fn a_slow_consumer_dropping_lines_does_not_break_the_run() {
    // Bounded channel of 1, receiver never drains until after the run: the
    // tap's try_send fails for every line but one — the run itself must be
    // unaffected and the tail complete.
    let f = Fixture::new("#!/usr/bin/env bash\nprintf 'a\\nb\\nc\\n'\n");
    let state = f.state();
    let args: Vec<String> = vec![];

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(1);
    let stream = StreamOptions {
        on_stdout_line: Some(Arc::new(move |line: &str| {
            let _ = tx.try_send(line.to_string());
        })),
    };

    let outcome = run_streaming(spec(&f, &args), stream, &state, None)
        .await
        .expect("run");

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout_tail, "a\nb\nc");
    // Whatever the channel kept (at least one line) is the caller's to
    // drain; all we assert is that draining still works.
    assert!(rx.recv().await.is_some(), "at least one line was tapped");
}

#[tokio::test]
async fn aborting_run_task_kills_agent_group_and_clears_registry() {
    let finished = Arc::new(AtomicUsize::new(0));
    let finished_for_handler = finished.clone();
    let supervisor = Supervisor::with_grace(Duration::from_millis(50)).with_event_handler(
        Arc::new(move |event| {
            if matches!(event, SupervisorEvent::Finished { .. }) {
                finished_for_handler.fetch_add(1, Ordering::SeqCst);
            }
        }),
    );
    // On the agent's argv and, via `exec -a`, on the grandchild's argv0.
    // `ps` then names our processes without consulting `-g`.
    let marker = format!(
        "dotagent-abort-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    let marker_for_task = marker.clone();
    let task_supervisor = supervisor.clone();
    let task = tokio::spawn(async move {
        let fixture = Fixture::with_script_args(
            "#!/usr/bin/env bash\nbash -c 'exec -a \"$1\" sleep 30' _ \"$1\" &\nwait\n",
            &[&marker_for_task],
        );
        let state = fixture.state();
        let args = Vec::new();
        run_streaming(
            spec(&fixture, &args),
            StreamOptions::default(),
            &state,
            Some(&task_supervisor),
        )
        .await
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let pgid = loop {
        if let Some(pgid) = supervisor.snapshot().first().and_then(|info| info.pgid) {
            break pgid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run task did not register a process"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    };

    task.abort();
    let join_error = task
        .await
        .expect_err("aborting the run task must cancel it");
    assert!(join_error.is_cancelled());

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !supervisor.snapshot().is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        supervisor.snapshot().is_empty(),
        "canceled run must remove its process from the supervisor"
    );
    assert_eq!(
        finished.load(Ordering::SeqCst),
        1,
        "cancellation must have exactly one supervisor completion event"
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !live_marked(&marker).is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let still = live_marked(&marker);
    assert!(
        still.is_empty(),
        "canceled run left a live process in group {pgid}: {still:?}"
    );
}
