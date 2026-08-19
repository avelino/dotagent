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
        redirect_log_home();
        let dir = tempfile::tempdir().expect("tempdir");
        let script_path = dir.path().join("agent.sh");
        std::fs::write(&script_path, script).expect("write script");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let toml = r#"
[agent]
name = "fixture"
monitor = false
timeout_seconds = 30

[run]
command = "bash"
args = ["./agent.sh"]
"#;
        let manifest: AgentManifest = toml::from_str(toml).expect("fixture manifest parses");
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

fn process_group_has_live_process(pgid: i32) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-g", &pgid.to_string()])
        .output()
        .expect("ps must be available on Unix");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|stat| !stat.is_empty())
        .any(|stat| !stat.starts_with('Z'))
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
    let task_supervisor = supervisor.clone();
    let task = tokio::spawn(async move {
        let fixture = Fixture::new("#!/usr/bin/env bash\nsleep 30 & wait\n");
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
    while process_group_has_live_process(pgid) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        !process_group_has_live_process(pgid),
        "canceled run left a live process in group {pgid}"
    );
}
