//! Integration tests for the persistent-agent pool.
//!
//! Every test drives a real subprocess through the real protocol. The fixture
//! agents are shell scripts written into a tempdir, because the thing under
//! test is a contract with a foreign process — a mock would only prove the
//! mock works.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dotagent_core::{audit::AuditEvent, AgentManifest};
use dotagent_runner::persistent::{PersistentPool, REQUEST_LOST_EXIT_CODE};
use dotagent_runner::{run_with_hooks, RunContext, RunSpec, RunnerError};
use dotagent_state::{AuditLog, StateStore};
use dotagent_supervisor::{Supervisor, SupervisorEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A fixture agent: `agent.toml` plus an executable `agent.sh`.
struct Fixture {
    dir: tempfile::TempDir,
    manifest: AgentManifest,
    state_root: tempfile::TempDir,
}

impl Fixture {
    /// The default startup window is 30s, and `validate` refuses a request
    /// deadline below it — so the default fixture has to outlast it.
    fn new(lifecycle: &str, script: &str) -> Self {
        Self::with_timeout(lifecycle, script, 60)
    }

    fn with_timeout(lifecycle: &str, script: &str, timeout_seconds: u64) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let script_path = dir.path().join("agent.sh");
        std::fs::write(&script_path, script).expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let toml = format!(
            r#"
[agent]
name = "fixture"
monitor = false
timeout_seconds = {timeout_seconds}

[run]
command = "bash"
args = ["./agent.sh"]

[lifecycle]
{lifecycle}
"#
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

fn spec<'a>(f: &'a Fixture, args: &'a [String], extra_env: &'a [(String, String)]) -> RunSpec<'a> {
    RunSpec {
        manifest: &f.manifest,
        manifest_dir: f.dir(),
        schedule_id: "trigger",
        args,
        dry_run: false,
        manifest_sha256: None,
        slug_override: Some("trigger-telegram"),
        extra_env,
    }
}

fn payload_env(json: &str) -> Vec<(String, String)> {
    vec![
        ("AGENT_TRIGGER_SOURCE".to_string(), "telegram".to_string()),
        ("AGENT_TRIGGER_PAYLOAD".to_string(), json.to_string()),
    ]
}

fn payload_env_with_session(json: &str, session_id: &str) -> Vec<(String, String)> {
    let mut env = payload_env(json);
    env.push(("AGENT_SESSION_ID".to_string(), session_id.to_string()));
    env
}

fn make_fifo(path: &Path) {
    let status = Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("mkfifo must be available on the Unix test platform");
    assert!(status.success(), "mkfifo failed for {}", path.display());
}

async fn read_fifo(path: PathBuf) -> Vec<u8> {
    let mut file = tokio::fs::File::open(path)
        .await
        .expect("open fifo for reading");
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await.expect("read fifo");
    bytes
}

async fn until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    condition()
}

/// Answers with its own pid and a per-process counter, so a test can tell
/// "same process answered twice" from "it was respawned".
const COUNTING_AGENT: &str = r#"#!/usr/bin/env bash
set -euo pipefail
n=0
while IFS= read -r line; do
  case "$line" in
    *'"kind":"hello"'*) echo '{"v":1,"kind":"ready","ok":true}' ;;
    *'"kind":"request"'*)
      id="$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')"
      n=$((n+1))
      echo "{\"v\":1,\"kind\":\"response\",\"id\":\"$id\",\"ok\":true,\"output\":\"$$ $n ${AGENT_PERSIST_KEY:-none}\"}"
      ;;
  esac
done
"#;

async fn run_once(
    pool: &PersistentPool,
    f: &Fixture,
    state: &StateStore,
    env: &[(String, String)],
) -> dotagent_runner::RunOutcome {
    let args: Vec<String> = vec![];
    pool.dispatch(&spec(f, &args, env), state, None)
        .await
        .expect("dispatch")
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_same_process_answers_twice() {
    let f = Fixture::new("mode = \"persistent\"", COUNTING_AGENT);
    let state = f.state();
    let pool = PersistentPool::new(Supervisor::with_grace(Duration::from_millis(100)));

    let first = run_once(&pool, &f, &state, &[]).await;
    let second = run_once(&pool, &f, &state, &[]).await;

    assert_eq!(first.exit_code, 0);
    let pid_a = first.stdout_tail.split_whitespace().next().unwrap();
    let pid_b = second.stdout_tail.split_whitespace().next().unwrap();
    assert_eq!(pid_a, pid_b, "a persistent agent must not be respawned");
    assert!(
        second.stdout_tail.contains(" 2 "),
        "the process kept its own state across requests: {}",
        second.stdout_tail
    );

    pool.shutdown(None).await;
}

#[tokio::test]
async fn each_key_gets_its_own_process() {
    let f = Fixture::new("mode = \"persistent\"\nkey = \"chat_id\"", COUNTING_AGENT);
    let state = f.state();
    let pool = PersistentPool::new(Supervisor::with_grace(Duration::from_millis(100)));

    let a = run_once(&pool, &f, &state, &payload_env(r#"{"chat_id":111}"#)).await;
    let b = run_once(&pool, &f, &state, &payload_env(r#"{"chat_id":222}"#)).await;
    let a2 = run_once(&pool, &f, &state, &payload_env(r#"{"chat_id":111}"#)).await;

    let pid = |s: &str| s.split_whitespace().next().unwrap().to_string();
    assert_ne!(
        pid(&a.stdout_tail),
        pid(&b.stdout_tail),
        "two conversations must not share a process"
    );
    assert_eq!(
        pid(&a.stdout_tail),
        pid(&a2.stdout_tail),
        "the same conversation must come back to the same process"
    );
    assert!(a.stdout_tail.ends_with("111"), "{}", a.stdout_tail);
    assert_eq!(pool.live_count().await, 2);

    pool.shutdown(None).await;
}

#[tokio::test]
async fn max_instances_evicts_the_least_recently_used() {
    let f = Fixture::new(
        "mode = \"persistent\"\nkey = \"chat_id\"\nmax_instances = 2",
        COUNTING_AGENT,
    );
    let state = f.state();
    let pool = PersistentPool::new(Supervisor::with_grace(Duration::from_millis(50)));

    let first = run_once(&pool, &f, &state, &payload_env(r#"{"chat_id":1}"#)).await;
    run_once(&pool, &f, &state, &payload_env(r#"{"chat_id":2}"#)).await;
    // Touch 2 again so 1 is unambiguously the oldest.
    run_once(&pool, &f, &state, &payload_env(r#"{"chat_id":2}"#)).await;
    run_once(&pool, &f, &state, &payload_env(r#"{"chat_id":3}"#)).await;

    assert_eq!(pool.live_count().await, 2, "the ceiling holds");

    // Chat 1 was evicted, so it comes back as a new process with a fresh count.
    let back = run_once(&pool, &f, &state, &payload_env(r#"{"chat_id":1}"#)).await;
    let pid = |s: &str| s.split_whitespace().next().unwrap().to_string();
    assert_ne!(pid(&first.stdout_tail), pid(&back.stdout_tail));
    assert!(back.stdout_tail.contains(" 1 "), "{}", back.stdout_tail);

    pool.shutdown(None).await;
}

#[tokio::test]
async fn a_request_during_lru_retirement_cannot_reuse_the_victim_key() {
    let script = r#"#!/usr/bin/env bash
set -euo pipefail
trap 'if [[ "$AGENT_PERSIST_KEY" == *":1" && ! -e "$AGENT_HOME/retire-seen" ]]; then
  : > "$AGENT_HOME/retire-seen"
  printf "retiring\n" > "$AGENT_HOME/retire.fifo"
  IFS= read -r _release < "$AGENT_HOME/release.fifo"
fi
exit 0' TERM
while IFS= read -r line; do
  case "$line" in
    *'"kind":"hello"'*)
      if [[ "$AGENT_PERSIST_KEY" == *":1" && -p "$AGENT_HOME/replacement.fifo" && -e "$AGENT_HOME/retire-seen" ]]; then
        printf "spawned\n" > "$AGENT_HOME/replacement.fifo"
      fi
      echo '{"v":1,"kind":"ready","ok":true}'
      ;;
    *'"kind":"request"'*)
      id="$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')"
      echo "{\"v\":1,\"kind\":\"response\",\"id\":\"$id\",\"ok\":true,\"output\":\"$$\"}"
      ;;
  esac
done
"#;
    let f = std::sync::Arc::new(Fixture::new(
        "mode = \"persistent\"\nkey = \"chat_id\"\nmax_instances = 1",
        script,
    ));
    let state = f.state();
    let supervisor = Supervisor::with_grace(Duration::from_millis(200));
    let pool = std::sync::Arc::new(PersistentPool::new(supervisor));

    let first = run_once(&pool, &f, &state, &payload_env(r#"{"chat_id":1}"#)).await;
    let retire_fifo = f.dir().join("retire.fifo");
    let release_fifo = f.dir().join("release.fifo");
    let replacement_fifo = f.dir().join("replacement.fifo");
    make_fifo(&retire_fifo);
    make_fifo(&release_fifo);
    make_fifo(&replacement_fifo);

    let retire_reader = tokio::spawn(read_fifo(retire_fifo.clone()));
    let mut replacement_reader = tokio::spawn(read_fifo(replacement_fifo));
    let evicted = {
        let pool = pool.clone();
        let f = f.clone();
        tokio::spawn(async move {
            let state = f.state();
            let env = payload_env(r#"{"chat_id":2}"#);
            run_once(&pool, &f, &state, &env).await
        })
    };

    let signal = retire_reader.await.expect("retire reader task");
    assert_eq!(signal, b"retiring\n");

    let next = {
        let pool = pool.clone();
        let f = f.clone();
        tokio::spawn(async move {
            let state = f.state();
            let env = payload_env(r#"{"chat_id":1}"#);
            // Give this request one extra capacity slot so the test isolates
            // reuse of the retiring key instead of evicting a second victim.
            let mut manifest = f.manifest.clone();
            manifest.lifecycle.max_instances = 2;
            let args = Vec::new();
            let spec = RunSpec {
                manifest: &manifest,
                manifest_dir: f.dir(),
                schedule_id: "trigger",
                args: &args,
                dry_run: false,
                manifest_sha256: None,
                slug_override: Some("trigger-telegram"),
                extra_env: &env,
            };
            pool.dispatch(&spec, &state, None).await
        })
    };

    let spawned_during_retirement =
        tokio::time::timeout(Duration::from_millis(100), &mut replacement_reader)
            .await
            .is_ok();

    let mut release = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&release_fifo)
        .await
        .expect("open retirement release fifo");
    release
        .write_all(b"release\n")
        .await
        .expect("release retirement");

    assert_eq!(evicted.await.expect("eviction task").exit_code, 0);
    assert_eq!(
        next.await
            .expect("replacement request task")
            .expect("replacement request")
            .exit_code,
        0
    );
    assert_ne!(
        first.stdout_tail.trim(),
        "",
        "the first request must answer"
    );
    if !spawned_during_retirement {
        let _ = replacement_reader.await;
    }
    assert!(
        !spawned_during_retirement,
        "the retiring key must not spawn a second instance during grace"
    );

    pool.shutdown(None).await;
}

#[tokio::test]
async fn max_invocations_recycles_the_instance() {
    let f = Fixture::new("mode = \"persistent\"\nmax_invocations = 2", COUNTING_AGENT);
    let state = f.state();
    let pool = PersistentPool::new(Supervisor::with_grace(Duration::from_millis(50)));

    let a = run_once(&pool, &f, &state, &[]).await;
    let b = run_once(&pool, &f, &state, &[]).await;
    let c = run_once(&pool, &f, &state, &[]).await;

    let pid = |s: &str| s.split_whitespace().next().unwrap().to_string();
    assert_eq!(pid(&a.stdout_tail), pid(&b.stdout_tail));
    assert_ne!(
        pid(&b.stdout_tail),
        pid(&c.stdout_tail),
        "the cap must retire the instance after the second answer"
    );

    pool.shutdown(None).await;
}

#[tokio::test]
async fn a_crashed_instance_is_replaced_on_the_next_request() {
    // Dies after answering once. The next request finds a dead pipe.
    let script = r#"#!/usr/bin/env bash
set -euo pipefail
while IFS= read -r line; do
  case "$line" in
    *'"kind":"hello"'*) echo '{"v":1,"kind":"ready","ok":true}' ;;
    *'"kind":"request"'*)
      id="$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')"
      echo "{\"v\":1,\"kind\":\"response\",\"id\":\"$id\",\"ok\":true,\"output\":\"$$\"}"
      exit 0
      ;;
  esac
done
"#;
    let f = Fixture::new("mode = \"persistent\"", script);
    let state = f.state();
    let pool = PersistentPool::new(Supervisor::with_grace(Duration::from_millis(50)));

    let first = run_once(&pool, &f, &state, &[]).await;
    // Give the process time to actually be gone before asking again.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let second = run_once(&pool, &f, &state, &[]).await;

    assert_eq!(
        second.exit_code, 0,
        "a crash must not lose the next message"
    );
    assert_ne!(
        first.stdout_tail.trim(),
        second.stdout_tail.trim(),
        "the replacement must be a different process"
    );

    pool.shutdown(None).await;
}

#[tokio::test]
async fn a_request_that_never_answers_times_out_and_recycles() {
    let script = r#"#!/usr/bin/env bash
set -euo pipefail
while IFS= read -r line; do
  case "$line" in
    *'"kind":"hello"'*) echo '{"v":1,"kind":"ready","ok":true}' ;;
    *'"kind":"request"'*)
      printf 'side-effect\n' >> "$AGENT_HOME/side-effects"
      while IFS= read -r _line; do :; done
      ;;
  esac
done
"#;
    let f = Fixture::with_timeout(
        "mode = \"persistent\"\nstartup_timeout_seconds = 1",
        script,
        2,
    );
    let state = f.state();
    let pool = PersistentPool::new(Supervisor::with_grace(Duration::from_millis(50)));

    let outcome = run_once(&pool, &f, &state, &[]).await;
    assert!(outcome.timed_out);
    assert_eq!(outcome.exit_code, 124);
    assert_eq!(
        pool.live_count().await,
        0,
        "a timed-out instance is retired, not reused"
    );
    assert_eq!(
        std::fs::read_to_string(f.dir().join("side-effects"))
            .expect("the timed-out request must reach the agent")
            .lines()
            .count(),
        1,
        "a timed-out request must not be dispatched a second time"
    );

    pool.shutdown(None).await;
}

#[tokio::test]
async fn a_reaper_kill_is_reported_as_timeout_not_request_loss() {
    let script = r#"#!/usr/bin/env bash
set -euo pipefail
while IFS= read -r line; do
  case "$line" in
    *'"kind":"hello"'*) echo '{"v":1,"kind":"ready","ok":true}' ;;
    *'"kind":"request"'*)
      : > "$AGENT_HOME/request-started"
      while IFS= read -r _line; do :; done
      ;;
  esac
done
"#;
    let f = Fixture::with_timeout(
        "mode = \"persistent\"\nstartup_timeout_seconds = 1",
        script,
        5,
    );
    let state = f.state();
    let timeout_events = Arc::new(AtomicUsize::new(0));
    let seen = timeout_events.clone();
    let supervisor = Supervisor::with_grace(Duration::from_millis(50)).with_event_handler(
        Arc::new(move |event: SupervisorEvent| {
            if matches!(event, SupervisorEvent::KilledTimeout { .. }) {
                seen.fetch_add(1, Ordering::SeqCst);
            }
        }),
    );
    let _reaper = supervisor.start_reaper();
    let pool = PersistentPool::new(supervisor.clone());
    let args = Vec::new();
    let env = Vec::new();
    let request_spec = spec(&f, &args, &env);
    let mut dispatch = Box::pin(pool.dispatch(&request_spec, &state, None));
    let marker = f.dir().join("request-started");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if marker.exists() {
                break;
            }
            tokio::select! {
                result = &mut dispatch => panic!("dispatch completed before the reaper deadline was shortened: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
        }
    })
    .await
    .expect("the request must reach the agent");

    let proc_id = supervisor
        .snapshot()
        .first()
        .expect("persistent process in supervisor")
        .id;
    assert!(supervisor.retime(proc_id, Duration::from_millis(50)));

    let outcome = dispatch.await.expect("reaper timeout must be an outcome");
    assert!(outcome.timed_out);
    assert_eq!(outcome.exit_code, 124);
    assert!(
        until(Duration::from_secs(1), || {
            timeout_events.load(Ordering::SeqCst) == 1
        })
        .await,
        "the reaper must emit exactly one timeout event"
    );
    assert_eq!(timeout_events.load(Ordering::SeqCst), 1);
    assert_eq!(pool.live_count().await, 0);

    pool.shutdown(None).await;
}

#[tokio::test]
async fn a_request_terminated_after_write_is_not_retried() {
    let script = r#"#!/usr/bin/env bash
set -euo pipefail
while IFS= read -r line; do
  case "$line" in
    *'"kind":"hello"'*) echo '{"v":1,"kind":"ready","ok":true}' ;;
    *'"kind":"request"'*)
      printf 'side-effect\n' >> "$AGENT_HOME/side-effects"
      exit 0
      ;;
  esac
done
"#;
    let f = Fixture::with_timeout(
        "mode = \"persistent\"\nstartup_timeout_seconds = 1",
        script,
        5,
    );
    let state = f.state();
    let pool = PersistentPool::new(Supervisor::with_grace(Duration::from_millis(50)));

    let args: Vec<String> = vec![];
    let error = pool
        .dispatch(&spec(&f, &args, &[]), &state, None)
        .await
        .expect_err("an ambiguous delivered request must fail without retrying");
    assert!(
        error.to_string().contains("not retrying"),
        "the error must make the non-retryable decision explicit: {error}"
    );
    assert_eq!(
        std::fs::read_to_string(f.dir().join("side-effects"))
            .expect("the request must have reached the agent")
            .lines()
            .count(),
        1,
        "a request with an ambiguous outcome must not be duplicated"
    );
    let heartbeat = state
        .read_heartbeat("fixture", "trigger-telegram")
        .expect("read heartbeat")
        .expect("ambiguous request must close its heartbeat");
    assert_eq!(heartbeat.exit_code, Some(-1));
    assert!(heartbeat.finished_at.is_some());
    assert_eq!(pool.live_count().await, 0);

    pool.shutdown(None).await;
}

#[tokio::test]
async fn a_request_loss_emits_one_agent_run_audit_with_delivery_metadata() {
    let f = Fixture::with_timeout(
        "mode = \"persistent\"\nstartup_timeout_seconds = 1",
        r#"#!/usr/bin/env bash
set -euo pipefail
while IFS= read -r line; do
  case "$line" in
    *'"kind":"hello"'*) echo '{"v":1,"kind":"ready","ok":true}' ;;
    *'"kind":"request"'*) exit 0 ;;
  esac
done
"#,
        5,
    );
    let state = f.state();
    let audit = AuditLog::with_path(f.dir().join("audit.log"));
    let supervisor = Supervisor::with_grace(Duration::from_millis(50));
    let plugins = dotagent_plugin::PluginClient::with_search_paths(Vec::new())
        .with_supervisor(supervisor.clone());
    let pool = PersistentPool::new(supervisor);
    let args = Vec::new();
    let spec = RunSpec {
        manifest: &f.manifest,
        manifest_dir: f.dir(),
        schedule_id: "trigger",
        args: &args,
        dry_run: false,
        manifest_sha256: Some("manifest-hash".to_string()),
        slug_override: Some("trigger-telegram"),
        extra_env: &[],
    };
    let ctx = RunContext {
        state: &state,
        plugins: Some(&plugins),
        audit: Some(&audit),
        supervisor: Some(plugins.supervisor()),
        persistent: Some(&pool),
    };

    let error = run_with_hooks(spec, &ctx)
        .await
        .expect_err("ambiguous delivery must remain terminal");
    let lost = match error {
        RunnerError::RequestLost(lost) => lost,
        other => panic!("expected RequestLost, got {other}"),
    };
    assert!(lost.duration_seconds >= 0);

    let runs: Vec<_> = audit
        .iter_entries()
        .expect("audit entries")
        .into_iter()
        .filter(|entry| matches!(&entry.event, AuditEvent::AgentRun { .. }))
        .collect();
    assert_eq!(
        runs.len(),
        1,
        "a lost request must have one run audit entry"
    );
    match &runs[0].event {
        AuditEvent::AgentRun {
            agent,
            schedule,
            slug,
            manifest_sha256,
            exit_code,
            duration_seconds,
            timed_out,
        } => {
            assert_eq!(agent, "fixture");
            assert_eq!(schedule, "trigger");
            assert_eq!(slug, "trigger-telegram");
            assert_eq!(manifest_sha256, "manifest-hash");
            assert_eq!(*exit_code, REQUEST_LOST_EXIT_CODE);
            assert_eq!(*duration_seconds, lost.duration_seconds);
            assert!(!timed_out);
        }
        other => panic!("expected AgentRun, got {other:?}"),
    }

    pool.shutdown(None).await;
}

#[tokio::test]
async fn a_normal_persistent_request_emits_one_agent_run_audit() {
    let f = Fixture::new("mode = \"persistent\"", COUNTING_AGENT);
    let state = f.state();
    let audit = AuditLog::with_path(f.dir().join("audit.log"));
    let supervisor = Supervisor::with_grace(Duration::from_millis(50));
    let plugins = dotagent_plugin::PluginClient::with_search_paths(Vec::new())
        .with_supervisor(supervisor.clone());
    let pool = PersistentPool::new(supervisor);
    let args = Vec::new();
    let spec = RunSpec {
        manifest: &f.manifest,
        manifest_dir: f.dir(),
        schedule_id: "trigger",
        args: &args,
        dry_run: false,
        manifest_sha256: Some("manifest-hash".to_string()),
        slug_override: Some("trigger-telegram"),
        extra_env: &[],
    };
    let ctx = RunContext {
        state: &state,
        plugins: Some(&plugins),
        audit: Some(&audit),
        supervisor: Some(plugins.supervisor()),
        persistent: Some(&pool),
    };

    let outcome = run_with_hooks(spec, &ctx).await.expect("persistent run");
    assert!(matches!(
        outcome,
        dotagent_runner::OrchestratedOutcome::Ran(_)
    ));
    let runs = audit
        .iter_entries()
        .expect("audit entries")
        .into_iter()
        .filter(|entry| matches!(&entry.event, AuditEvent::AgentRun { .. }))
        .count();
    assert_eq!(
        runs, 1,
        "a normal persistent request must not be duplicated"
    );

    pool.shutdown(None).await;
}

#[tokio::test]
async fn an_agent_that_never_says_ready_fails_at_startup() {
    let script = r#"#!/usr/bin/env bash
set -euo pipefail
while IFS= read -r _line; do :; done
"#;
    let f = Fixture::with_timeout(
        "mode = \"persistent\"\nstartup_timeout_seconds = 1",
        script,
        5,
    );
    let state = f.state();
    let pool = PersistentPool::new(Supervisor::with_grace(Duration::from_millis(50)));

    let args: Vec<String> = vec![];
    let err = pool
        .dispatch(&spec(&f, &args, &[]), &state, None)
        .await
        .expect_err("a handshake that never lands must fail loudly");
    assert!(
        err.to_string().contains("handshake"),
        "the error should name what went wrong: {err}"
    );
    assert_eq!(pool.live_count().await, 0);
}

#[tokio::test]
async fn noise_on_stdout_is_dropped_rather_than_fatal() {
    let script = r#"#!/usr/bin/env bash
set -euo pipefail
while IFS= read -r line; do
  case "$line" in
    *'"kind":"hello"'*)
      echo "starting up, please wait"
      echo '{"v":1,"kind":"ready","ok":true}'
      ;;
    *'"kind":"request"'*)
      id="$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')"
      echo "about to answer"
      echo '{"v":1,"kind":"response","id":"999","output":"stale answer"}'
      echo "{\"v\":1,\"kind\":\"response\",\"id\":\"$id\",\"ok\":true,\"output\":\"real answer\"}"
      ;;
  esac
done
"#;
    let f = Fixture::new("mode = \"persistent\"", script);
    let state = f.state();
    let pool = PersistentPool::new(Supervisor::with_grace(Duration::from_millis(50)));

    let outcome = run_once(&pool, &f, &state, &[]).await;
    assert_eq!(
        outcome.stdout_tail.trim(),
        "real answer",
        "a stale id and a log line must both be skipped"
    );

    pool.shutdown(None).await;
}

#[tokio::test]
async fn stderr_is_scoped_to_the_request_that_produced_it() {
    let script = r#"#!/usr/bin/env bash
set -euo pipefail
n=0
echo "noise from startup" >&2
while IFS= read -r line; do
  case "$line" in
    *'"kind":"hello"'*) echo '{"v":1,"kind":"ready","ok":true}' ;;
    *'"kind":"request"'*)
      id="$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')"
      n=$((n+1))
      echo "handling request $n" >&2
      echo "{\"v\":1,\"kind\":\"response\",\"id\":\"$id\",\"ok\":true,\"output\":\"ok\"}"
      # Let the reader drain before the next request marks the stream.
      sleep 0.2
      ;;
  esac
done
"#;
    let f = Fixture::new("mode = \"persistent\"", script);
    let state = f.state();
    let pool = PersistentPool::new(Supervisor::with_grace(Duration::from_millis(50)));

    run_once(&pool, &f, &state, &[]).await;
    // stderr is drained by a task of its own, so a line the agent wrote before
    // its answer has not necessarily reached the ring by the time the answer
    // is read. The second request's mark is taken the instant it starts; under
    // load that raced the reader and failed this test roughly one run in ten.
    // Waiting here tests what the assertion is about — scoping — instead of
    // how fast the machine is.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let second = run_once(&pool, &f, &state, &[]).await;

    assert!(
        !second.stderr_tail.contains("request 1"),
        "the second request must not inherit the first one's stderr: {:?}",
        second.stderr_tail
    );
    assert!(
        !second.stderr_tail.contains("noise from startup"),
        "nor the startup banner: {:?}",
        second.stderr_tail
    );

    pool.shutdown(None).await;
}

#[tokio::test]
async fn every_request_writes_a_heartbeat() {
    let f = Fixture::new("mode = \"persistent\"", COUNTING_AGENT);
    let state = f.state();
    let pool = PersistentPool::new(Supervisor::with_grace(Duration::from_millis(50)));

    run_once(&pool, &f, &state, &[]).await;
    let hb = state
        .read_heartbeat("fixture", "trigger-telegram")
        .expect("read")
        .expect("heartbeat written");
    assert_eq!(hb.exit_code, Some(0));
    assert!(hb.finished_at.is_some(), "the request closed the heartbeat");
    assert!(
        hb.last_success_at.is_some(),
        "a persistent run must feed health the same way a one-shot one does"
    );

    pool.shutdown(None).await;
}

#[tokio::test]
async fn the_tmpdir_survives_between_requests() {
    let script = r#"#!/usr/bin/env bash
set -euo pipefail
while IFS= read -r line; do
  case "$line" in
    *'"kind":"hello"'*) echo '{"v":1,"kind":"ready","ok":true}' ;;
    *'"kind":"request"'*)
      id="$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')"
      echo "seen" >> "$AGENT_TMPDIR/marker"
      count="$(wc -l < "$AGENT_TMPDIR/marker" | tr -d ' ')"
      echo "{\"v\":1,\"kind\":\"response\",\"id\":\"$id\",\"ok\":true,\"output\":\"$count\"}"
      ;;
  esac
done
"#;
    let f = Fixture::new("mode = \"persistent\"", script);
    let state = f.state();
    let pool = PersistentPool::new(Supervisor::with_grace(Duration::from_millis(50)));

    run_once(&pool, &f, &state, &[]).await;
    let second = run_once(&pool, &f, &state, &[]).await;
    assert_eq!(
        second.stdout_tail.trim(),
        "2",
        "AGENT_TMPDIR belongs to the instance, not to one request"
    );

    pool.shutdown(None).await;
}

#[tokio::test]
async fn the_trigger_rides_in_the_frame_not_the_environment() {
    // Proves both halves: per-request trigger context reaches the agent in the
    // frame, and the stale environment block never does.
    let script = r#"#!/usr/bin/env bash
set -euo pipefail
while IFS= read -r line; do
  case "$line" in
    *'"kind":"hello"'*) echo '{"v":1,"kind":"ready","ok":true}' ;;
    *'"kind":"request"'*)
      id="$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')"
      text="$(printf '%s' "$line" | sed -n 's/.*"text":"\([^"]*\)".*/\1/p')"
      session="$(printf '%s' "$line" | sed -n 's/.*"session_id":"\([^"]*\)".*/\1/p')"
      echo "{\"v\":1,\"kind\":\"response\",\"id\":\"$id\",\"ok\":true,\"output\":\"frame=$text session=$session env=${AGENT_TRIGGER_PAYLOAD:-unset} sid_env=${AGENT_SESSION_ID:-unset}\"}"
      ;;
  esac
done
"#;
    let f = Fixture::new("mode = \"persistent\"", script);
    let state = f.state();
    let pool = PersistentPool::new(Supervisor::with_grace(Duration::from_millis(50)));

    let first = run_once(
        &pool,
        &f,
        &state,
        &payload_env_with_session(r#"{"text":"first"}"#, "session-1"),
    )
    .await;
    assert_eq!(
        first.stdout_tail.trim(),
        "frame=first session=session-1 env=unset sid_env=unset"
    );

    // Second message, same process. If either value had been frozen in the
    // environment, the agent would still be reading the first one.
    let second = run_once(
        &pool,
        &f,
        &state,
        &payload_env_with_session(r#"{"text":"second"}"#, "session-2"),
    )
    .await;
    assert_eq!(
        second.stdout_tail.trim(),
        "frame=second session=session-2 env=unset sid_env=unset"
    );

    pool.shutdown(None).await;
}

#[tokio::test]
async fn shutdown_leaves_nothing_running() {
    let f = Fixture::new("mode = \"persistent\"\nkey = \"chat_id\"", COUNTING_AGENT);
    let state = f.state();
    let supervisor = Supervisor::with_grace(Duration::from_millis(50));
    let pool = PersistentPool::new(supervisor.clone());

    run_once(&pool, &f, &state, &payload_env(r#"{"chat_id":1}"#)).await;
    run_once(&pool, &f, &state, &payload_env(r#"{"chat_id":2}"#)).await;
    assert_eq!(supervisor.snapshot().len(), 2, "both show up in status");

    pool.shutdown(None).await;

    assert_eq!(pool.live_count().await, 0);
    assert!(
        supervisor.snapshot().is_empty(),
        "shutdown must deregister every instance"
    );
}

#[tokio::test]
async fn reload_retires_live_instances() {
    let f = Fixture::new("mode = \"persistent\"", COUNTING_AGENT);
    let state = f.state();
    let pool = PersistentPool::new(Supervisor::with_grace(Duration::from_millis(50)));

    let before = run_once(&pool, &f, &state, &[]).await;
    pool.reload(None).await;
    let after = run_once(&pool, &f, &state, &[]).await;

    let pid = |s: &str| s.split_whitespace().next().unwrap().to_string();
    assert_ne!(
        pid(&before.stdout_tail),
        pid(&after.stdout_tail),
        "a reload must not leave the old process answering"
    );

    pool.shutdown(None).await;
}

#[tokio::test]
async fn sweep_forgets_instances_the_reaper_took() {
    let f = Fixture::new(
        // One second of idle, so the reaper collects it between requests.
        "mode = \"persistent\"\nidle_timeout_seconds = 1",
        COUNTING_AGENT,
    );
    let state = f.state();
    let supervisor = Supervisor::with_grace(Duration::from_millis(50));
    let _reaper = supervisor.start_reaper();
    let pool = PersistentPool::new(supervisor.clone());

    let first = run_once(&pool, &f, &state, &[]).await;
    tokio::time::sleep(Duration::from_millis(1_800)).await;

    pool.sweep(None).await;
    assert_eq!(
        pool.live_count().await,
        0,
        "the sweep should have noticed the idle recycle"
    );

    let second = run_once(&pool, &f, &state, &[]).await;
    let pid = |s: &str| s.split_whitespace().next().unwrap().to_string();
    assert_ne!(pid(&first.stdout_tail), pid(&second.stdout_tail));

    pool.shutdown(None).await;
}
