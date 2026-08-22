//! End-to-end smoke for the `[assistant]` harness: a real daemon, a real
//! dispatcher agent subprocess, a real local-API client.
//!
//! The fixture agent speaks assistant-v1 and echoes what the harness
//! injected. The test proves the four behaviors the plan's acceptance names:
//! first trigger writes the registry, the second trigger receives the
//! recorded `AGENT_ASSISTANT_SESSION`, `MEMO:` lines never reach the client,
//! and the captured fact lands in the memory workspace.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

struct Daemon {
    child: Child,
    _home: TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn spawn_daemon() -> (Daemon, std::path::PathBuf) {
    let home = TempDir::new().unwrap();
    let root = home.path().join("agents");

    write(
        &root.join("smoke-dispatcher/agent.toml"),
        r#"
[agent]
name = "smoke-dispatcher"
monitor = false
timeout_seconds = 60

[run]
command = "bash"
args = ["./agent.sh"]
protocol = "assistant-v1"

[assistant]
memory = true
"#,
    );
    write(
        &root.join("smoke-dispatcher/agent.sh"),
        r#"#!/usr/bin/env bash
S="session=none"; [ -n "$AGENT_ASSISTANT_SESSION" ] && S="session=$AGENT_ASSISTANT_SESSION"
M="memory=no"; [ -n "$AGENT_ASSISTANT_MEMORY" ] && M="memory=yes"
echo "{\"type\":\"session\",\"claude_session\":\"smoke-s1\",\"transcript_bytes\":120}"
printf '{"type":"reply","text":"%s %s\\nMEMO: smoke likes rust | topics: smoke"}\n' "$S" "$M"
"#,
    );
    write(
        &home.path().join("config.toml"),
        "[telegram]\ndispatcher_agent = \"smoke-dispatcher\"\n",
    );

    let child = Command::new(env!("CARGO_BIN_EXE_dotagent"))
        .arg("daemon")
        .env("DOTAGENT_HOME", home.path())
        .env("DOTAGENT_ROOT", &root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dotagent daemon");

    let daemon = Daemon { child, _home: home };
    let socket = daemon_socket_path(&daemon);
    (daemon, socket)
}

/// `DOTAGENT_HOME` points inside a tempdir the test owns; mirror the
/// daemon's socket path resolution for the client side.
fn daemon_socket_path(daemon: &Daemon) -> std::path::PathBuf {
    daemon._home.path().join("api.sock")
}

fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("local API socket never appeared at {}", path.display());
}

/// Send one message, return the terminal `reply` event's text.
fn send_and_await_reply(socket: &Path, id: u64, session: &str, text: &str) -> String {
    let mut stream = UnixStream::connect(socket).expect("connect to local API");
    let request = format!(
        r#"{{"id":{id},"method":"message.send","params":{{"session_id":"{session}","text":{}}}}}"#,
        serde_json::json!(text)
    );
    writeln!(stream, "{request}").unwrap();
    stream.flush().unwrap();

    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line.expect("read event line");
        let value: serde_json::Value = serde_json::from_str(&line).expect("parse event JSON");
        if value.get("event").and_then(|e| e.as_str()) == Some("reply") {
            return value
                .get("text")
                .and_then(|t| t.as_str())
                .expect("reply event carries text")
                .to_string();
        }
    }
    panic!("stream closed before the terminal reply event");
}

#[test]
fn assistant_harness_end_to_end() {
    let (daemon, socket) = spawn_daemon();
    wait_for_socket(&socket);

    // First trigger: no pointer yet, fact not yet in memory.
    let first = send_and_await_reply(&socket, 1, "smoke1", "hello");
    assert!(first.contains("session=none"), "first reply: {first}");
    assert!(first.contains("memory=no"), "first reply: {first}");
    assert!(
        !first.contains("MEMO:"),
        "capture lines must not be delivered: {first}"
    );

    // The async MEMO flush and the registry write land quickly; give the
    // daemon a beat before asserting on disk state.
    std::thread::sleep(Duration::from_secs(1));

    let assistant_state = daemon._home.path().join("state/assistant");
    let registry = assistant_state.join("local-smoke1.json");
    let record = std::fs::read_to_string(&registry)
        .unwrap_or_else(|_| panic!("registry record at {} was written", registry.display()));
    assert!(record.contains("smoke-s1"), "record: {record}");
    assert!(record.contains("updated_at"), "record: {record}");

    // Second trigger: the recorded pointer and the stored fact come back.
    let second = send_and_await_reply(&socket, 2, "smoke1", "again");
    assert!(
        second.contains("session=smoke-s1"),
        "second reply: {second}"
    );
    assert!(second.contains("memory=yes"), "second reply: {second}");
    assert!(!second.contains("MEMO:"), "second reply: {second}");

    // The captured fact reached the memory workspace's projected journal.
    std::thread::sleep(Duration::from_secs(1));
    let journals = daemon._home.path().join("outl/journals");
    let mut found = false;
    if journals.exists() {
        for entry in std::fs::read_dir(&journals).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let body = std::fs::read_to_string(&path).unwrap_or_default();
                if body.contains("smoke likes rust") {
                    found = true;
                }
            }
        }
    }
    assert!(found, "captured memo reached the memory workspace journal");
}
