//! Shared test doubles for the gateway module. No network, no filesystem.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dotagent_core::{TriggerRequest, TriggerSource};
use dotagent_runner::{OrchestratedOutcome, RunOutcome, StreamOptions};

use super::sink::{ReplySink, SinkFuture};
use super::{GatewayRunner, RunFuture};

/// A successful one-shot outcome carrying `stdout` as its tail.
pub(crate) fn ran_ok(stdout: &str) -> OrchestratedOutcome {
    OrchestratedOutcome::Ran(RunOutcome {
        exit_code: 0,
        timed_out: false,
        duration_seconds: 0,
        stdout_tail: stdout.to_string(),
        stderr_tail: String::new(),
        stdout_truncated_lines: 0,
        stderr_truncated_lines: 0,
    })
}

/// A minimal local-source trigger. Args and reply_to are set per test.
pub(crate) fn local_req(agent: &str) -> TriggerRequest {
    TriggerRequest {
        source: TriggerSource::Local,
        agent: agent.into(),
        schedule: None,
        args: Vec::new(),
        payload: None,
        actor: Some("4242".into()),
        reply_to: None,
        session_id: None,
    }
}

/// Sink that records every delivery into a shared vector.
#[derive(Default)]
pub(crate) struct RecordingSink {
    pub events: Mutex<Vec<String>>,
}

impl RecordingSink {
    pub(crate) fn events(&self) -> Vec<String> {
        self.events.lock().expect("recorder poisoned").clone()
    }

    pub(crate) fn replies(&self) -> usize {
        self.events()
            .iter()
            .filter(|e| e.starts_with("reply"))
            .count()
    }
}

impl ReplySink for RecordingSink {
    fn started(&self, session: Option<&str>, agent: &str) {
        let entry = format_entry(session, "started", agent);
        self.events.lock().expect("recorder poisoned").push(entry);
    }

    fn reply<'a>(&'a self, session: Option<&'a str>, text: &'a str) -> SinkFuture<'a> {
        record(self, session, "reply", text)
    }

    fn typing<'a>(&'a self, session: Option<&'a str>) -> SinkFuture<'a> {
        record(self, session, "typing", "")
    }

    fn delta<'a>(&'a self, session: Option<&'a str>, line: &'a str) -> SinkFuture<'a> {
        record(self, session, "delta", line)
    }
}

/// Sink whose deliveries always "fail": it records the attempt and swallows
/// the error — exactly how a real transport logs-and-continues per the trait
/// contract.
#[derive(Default)]
pub(crate) struct FailingSink {
    pub attempts: Mutex<Vec<String>>,
}

impl FailingSink {
    pub(crate) fn attempts(&self) -> usize {
        self.attempts.lock().expect("failing sink poisoned").len()
    }
}

impl ReplySink for FailingSink {
    fn started(&self, session: Option<&str>, _agent: &str) {
        let entry = format!("attempted started[{}]", session.unwrap_or("-"));
        self.attempts
            .lock()
            .expect("failing sink poisoned")
            .push(entry);
    }

    fn reply<'a>(&'a self, session: Option<&'a str>, _text: &'a str) -> SinkFuture<'a> {
        attempt(self, session, "reply")
    }

    fn typing<'a>(&'a self, session: Option<&'a str>) -> SinkFuture<'a> {
        attempt(self, session, "typing")
    }

    fn delta<'a>(&'a self, session: Option<&'a str>, _line: &'a str) -> SinkFuture<'a> {
        attempt(self, session, "delta")
    }
}

fn format_entry(session: Option<&str>, kind: &str, body: &str) -> String {
    let session = session.unwrap_or("-");
    if body.is_empty() {
        format!("{kind}[{session}]")
    } else {
        format!("{kind}[{session}] {body}")
    }
}

fn record<'a>(
    sink: &'a RecordingSink,
    session: Option<&'a str>,
    kind: &str,
    body: &'a str,
) -> SinkFuture<'a> {
    let entry = format_entry(session, kind, body);
    Box::pin(async move {
        sink.events.lock().expect("recorder poisoned").push(entry);
    })
}

fn attempt<'a>(sink: &'a FailingSink, session: Option<&'a str>, kind: &str) -> SinkFuture<'a> {
    let entry = format!("attempted {kind}[{}]", session.unwrap_or("-"));
    Box::pin(async move {
        sink.attempts
            .lock()
            .expect("failing sink poisoned")
            .push(entry);
    })
}

/// Concurrency trace shared with a [`FakeRunner`]: start/end events per run
/// (labeled by the request's first arg) plus a live/max in-flight gauge.
#[derive(Default)]
pub(crate) struct Trace {
    pub events: Mutex<Vec<String>>,
    active: AtomicUsize,
    pub max_active: AtomicUsize,
}

impl Trace {
    pub(crate) fn events(&self) -> Vec<String> {
        self.events.lock().expect("trace poisoned").clone()
    }

    pub(crate) fn max_active(&self) -> usize {
        self.max_active.load(Ordering::SeqCst)
    }
}

/// Runner double: feeds fixed lines through the stdout tap, optionally gates
/// on a watch / barrier / delay, and records run ordering into a trace.
pub(crate) struct FakeRunner {
    pub lines: Vec<String>,
    pub outcome: Result<OrchestratedOutcome, String>,
    pub assistant_protocol: bool,
    pub hold: Option<tokio::sync::watch::Receiver<bool>>,
    pub barrier: Option<Arc<tokio::sync::Barrier>>,
    pub delay: Duration,
    pub trace: Option<Arc<Trace>>,
}

impl Default for FakeRunner {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            outcome: Ok(ran_ok("ok")),
            assistant_protocol: false,
            hold: None,
            barrier: None,
            delay: Duration::ZERO,
            trace: None,
        }
    }
}

impl GatewayRunner for FakeRunner {
    fn uses_assistant_protocol(&self, _req: &TriggerRequest) -> bool {
        self.assistant_protocol
    }

    fn run_trigger(&self, req: TriggerRequest, stream: StreamOptions) -> RunFuture {
        let lines = self.lines.clone();
        let outcome = self.outcome.clone();
        let hold = self.hold.clone();
        let barrier = self.barrier.clone();
        let delay = self.delay;
        let trace = self.trace.clone();
        Box::pin(async move {
            let label = req.args.first().cloned().unwrap_or_else(|| "-".into());
            if let Some(t) = &trace {
                t.events
                    .lock()
                    .expect("trace poisoned")
                    .push(format!("start {label}"));
                let now = t.active.fetch_add(1, Ordering::SeqCst) + 1;
                t.max_active.fetch_max(now, Ordering::SeqCst);
            }
            if let Some(b) = &barrier {
                let _ = b.wait().await;
            }
            if let Some(hold) = hold {
                let mut h = hold.clone();
                loop {
                    if *h.borrow() {
                        break;
                    }
                    if h.changed().await.is_err() {
                        break;
                    }
                }
            }
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if let Some(tap) = stream.on_stdout_line.as_ref() {
                for line in &lines {
                    tap(line);
                }
            }
            if let Some(t) = &trace {
                t.events
                    .lock()
                    .expect("trace poisoned")
                    .push(format!("end {label}"));
                t.active.fetch_sub(1, Ordering::SeqCst);
            }
            outcome.map_err(|e| anyhow::anyhow!("{e}"))
        })
    }
}
