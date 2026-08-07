//! Textual health dashboard.
//!
//! Read-only: never dispatches anything, never writes to audit. Substitutes
//! `agent-orchestrator --status` from the legacy Fish framework.

use anyhow::Result;
use chrono::Local;
use dotagent_core::{AgentManifest, Heartbeat, Schedule};
use dotagent_scheduler::{health_state, window_key, HealthState, ResolvedPolicy};
use dotagent_state::{slug_from_args, StateStore};
use dotagent_supervisor::ProcessInfo;

use crate::discovery::{self, DiscoveredAgent};

struct Row {
    agent: String,
    schedule: String,
    state: HealthState,
    last_run: String,
    reason: String,
}

pub async fn run() -> Result<()> {
    let agents = discovery::discover_all()?;
    if agents.is_empty() {
        println!("no agents discovered");
        return Ok(());
    }
    let state = StateStore::from_home()?;
    let now = Local::now();

    let mut rows: Vec<Row> = Vec::new();
    for agent in &agents {
        if !agent.manifest.agent.monitor {
            continue;
        }
        for sched in &agent.manifest.schedules {
            rows.push(compute_row(agent, sched, &state, now));
        }
    }

    print_dashboard(&rows, now);
    if let Some(snap) = read_supervisor_snapshot() {
        print_live_subprocesses(&snap);
    }
    Ok(())
}

/// Read the daemon's last-written supervisor snapshot. Returns `None` if the
/// daemon isn't running (file missing) or the file is unreadable.
pub fn read_supervisor_snapshot() -> Option<Vec<ProcessInfo>> {
    let path = dotagent_state::paths::supervisor_snapshot_file();
    let raw = std::fs::read(&path).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn print_live_subprocesses(snap: &[ProcessInfo]) {
    if snap.is_empty() {
        return;
    }
    println!();
    println!("─── Live subprocesses ({} running) ───", snap.len());
    println!(
        "{:<10} {:<8} {:<14} {:<30} AGE / DEADLINE",
        "KIND", "PID", "OWNER", "LABEL"
    );
    let sep = "─".repeat(100);
    println!("{sep}");
    // Sort: highest deadline_pct first — surfaces near-deadline processes.
    let mut sorted: Vec<&ProcessInfo> = snap.iter().collect();
    sorted.sort_by_key(|p| std::cmp::Reverse(p.deadline_pct));
    let mut has_persistent = false;
    for p in sorted {
        // For a persistent agent the clock is the idle window, not a run that
        // is running late. Reaching it means "recycled for sitting idle",
        // which is the pool working, so it never gets an alarm colour.
        let persistent = p.kind == dotagent_supervisor::ProcessKind::PersistentAgent;
        has_persistent |= persistent;
        let icon = if persistent {
            "  "
        } else if p.deadline_pct >= 100 {
            "🔴"
        } else if p.deadline_pct >= 80 {
            "⚠️ "
        } else {
            "  "
        };
        let kind = p.kind.to_string();
        let owner = p.owner.agent.as_str();
        let clock = if persistent { "idle" } else { "" };
        println!(
            "{icon} {kind:<8} {:<8} {:<14} {:<30} {}{}s / {}s ({}%)",
            p.pid, owner, p.label, clock, p.age_seconds, p.deadline_seconds, p.deadline_pct
        );
    }
    if has_persistent {
        println!();
        println!(
            "  persistent: AGE is time since the last answer; DEADLINE is the \
             idle window before recycling."
        );
    }
}

fn compute_row(
    agent: &DiscoveredAgent,
    sched: &Schedule,
    state: &StateStore,
    now: chrono::DateTime<Local>,
) -> Row {
    let policy = ResolvedPolicy::resolve(&agent.manifest, sched);
    let hb = read_hb(&agent.manifest, sched, state);
    let slug = slug_from_args(sched.args());

    let last_run = hb
        .as_ref()
        .and_then(|h| h.finished_at_iso.clone())
        .unwrap_or_else(|| "never".into());

    let window = window_key(sched, hb.as_ref(), now).and_then(|exp| {
        state
            .read_window(&agent.manifest.agent.name, &slug, exp)
            .ok()
            .flatten()
    });
    let (state_val, reason) = health_state(sched, &policy, hb.as_ref(), window.as_ref(), now);

    Row {
        agent: agent.manifest.agent.name.clone(),
        schedule: sched.id().to_string(),
        state: state_val,
        last_run,
        reason,
    }
}

fn read_hb(manifest: &AgentManifest, sched: &Schedule, state: &StateStore) -> Option<Heartbeat> {
    let slug = slug_from_args(sched.args());
    state
        .read_heartbeat(&manifest.agent.name, &slug)
        .ok()
        .flatten()
}

fn print_dashboard(rows: &[Row], now: chrono::DateTime<Local>) {
    let mut ok = 0;
    let mut deg = 0;
    let mut fail = 0;
    let mut stale = 0;
    for r in rows {
        match r.state {
            HealthState::Ok => ok += 1,
            HealthState::Degraded => deg += 1,
            HealthState::Failing => fail += 1,
            HealthState::Stale => stale += 1,
        }
    }
    let total = ok + deg + fail + stale;
    println!();
    println!("═══ Agent Health · {} ═══", now.format("%Y-%m-%d %H:%M"));
    println!();
    println!("  ✅ ok       {ok}/{total}");
    println!("  ⚠️  degraded {deg}");
    println!("  ❌ failing  {fail}");
    println!("  🕑 stale    {stale}");
    println!();
    println!(
        "{:<36} {:<11} {:<26} REASON",
        "AGENT/SCHEDULE", "STATE", "LAST RUN"
    );
    let sep = "─".repeat(100);
    println!("{sep}");

    // Order: failing → degraded → stale → ok (most-urgent-first).
    let order = [
        HealthState::Failing,
        HealthState::Degraded,
        HealthState::Stale,
        HealthState::Ok,
    ];
    for state in order {
        for row in rows.iter().filter(|r| r.state == state) {
            let icon = match row.state {
                HealthState::Ok => "✅ ok      ",
                HealthState::Degraded => "⚠️  degraded",
                HealthState::Failing => "❌ failing ",
                HealthState::Stale => "🕑 stale   ",
            };
            println!(
                "{:<36} {}  {:<26} {}",
                format!("{}/{}", row.agent, row.schedule),
                icon,
                row.last_run,
                row.reason
            );
        }
    }
    println!();
    let home = dotagent_state::paths::home();
    println!("Logs:    {}/logs/", home.display());
    println!("State:   {}/state/agents/", home.display());
    println!("Audit:   {}/audit.log", home.display());
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use dotagent_core::WindowState;
    use std::path::Path;

    const CRON_MANIFEST: &str = r#"
[agent]
name = "hn-digest"
timeout_seconds = 600

[run]
command = "/bin/true"

[[schedules]]
id = "weekday-morning"
type = "cron"
weekdays = [1, 2, 3, 4, 5]
hours = [10]
minute = 0
"#;

    const INTERVAL_MANIFEST: &str = r#"
[agent]
name = "inbox-triage"
timeout_seconds = 600

[run]
command = "/bin/true"

[[schedules]]
id = "every-90min"
type = "interval"
interval_minutes = 90
"#;

    fn at(h: u32, m: u32, s: u32) -> chrono::DateTime<Local> {
        Local.with_ymd_and_hms(2026, 8, 6, h, m, s).unwrap()
    }

    fn agent_from(dir: &Path, manifest: &str) -> DiscoveredAgent {
        let path = dir.join("agent.toml");
        std::fs::write(&path, manifest).unwrap();
        DiscoveredAgent {
            manifest: AgentManifest::load(&path).unwrap(),
            dir: dir.to_path_buf(),
        }
    }

    fn heartbeat(agent: &str, last_success: chrono::DateTime<Local>) -> Heartbeat {
        Heartbeat {
            name: agent.into(),
            slug: "default".into(),
            args: vec![],
            started_at: last_success.timestamp(),
            started_at_iso: String::new(),
            finished_at: Some(last_success.timestamp()),
            finished_at_iso: Some(last_success.to_rfc3339()),
            exit_code: Some(0),
            duration_seconds: Some(1),
            last_success_at: Some(last_success.timestamp()),
            last_success_at_iso: None,
        }
    }

    fn window(
        agent: &str,
        schedule: &str,
        expected: chrono::DateTime<Local>,
        attempts: u32,
        exit_code: i32,
    ) -> WindowState {
        WindowState {
            agent: agent.into(),
            schedule_id: schedule.into(),
            expected_at: expected.timestamp(),
            attempts,
            last_attempt_at: Some(expected.timestamp()),
            last_attempt_exit_code: Some(exit_code),
            last_attempt_stderr: None,
            given_up: false,
            given_up_at: None,
        }
    }

    /// The bug: the window was looked up by `last_success_at` instead of the
    /// window's own `expected_at`, so the filename almost never matched and
    /// `degraded` could not be reached. Here the 10:00 window needed a retry
    /// and succeeded at 10:12 — two different filenames.
    #[test]
    fn degraded_when_the_due_window_needed_a_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateStore::with_root(tmp.path().join("state"));
        let agent = agent_from(tmp.path(), CRON_MANIFEST);
        let sched = &agent.manifest.schedules[0];

        state
            .write_heartbeat(&heartbeat("hn-digest", at(10, 12, 0)))
            .unwrap();
        state
            .write_window(
                &window("hn-digest", "weekday-morning", at(10, 0, 0), 2, 0),
                "default",
                at(10, 0, 0),
            )
            .unwrap();

        let row = compute_row(&agent, sched, &state, at(10, 30, 0));
        assert_eq!(row.state, HealthState::Degraded, "reason: {}", row.reason);
        assert!(
            row.reason.contains('1'),
            "one failed attempt before the success, got: {}",
            row.reason
        );
    }

    /// The other half of the bug, and the one the dashboard actually showed:
    /// `hn-digest/weekday-morning` finished inside its own window minute, so
    /// the wrong key matched by coincidence — and reported `degraded` for a
    /// run that worked on the first try, because `attempts` counts the
    /// successful dispatch too.
    #[test]
    fn ok_when_the_first_attempt_succeeded() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateStore::with_root(tmp.path().join("state"));
        let agent = agent_from(tmp.path(), CRON_MANIFEST);
        let sched = &agent.manifest.schedules[0];

        state
            .write_heartbeat(&heartbeat("hn-digest", at(10, 0, 51)))
            .unwrap();
        state
            .write_window(
                &window("hn-digest", "weekday-morning", at(10, 0, 0), 1, 0),
                "default",
                at(10, 0, 0),
            )
            .unwrap();

        let row = compute_row(&agent, sched, &state, at(10, 30, 0));
        assert_eq!(row.state, HealthState::Ok, "reason: {}", row.reason);
    }

    /// A failing window must report the attempts it burned, not "no attempt".
    #[test]
    fn failing_window_reports_its_attempts() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateStore::with_root(tmp.path().join("state"));
        let agent = agent_from(tmp.path(), CRON_MANIFEST);
        let sched = &agent.manifest.schedules[0];

        // Last success was yesterday's window; today's 10:00 window failed.
        let yesterday = at(10, 0, 0) - chrono::Duration::days(1);
        state
            .write_heartbeat(&heartbeat("hn-digest", yesterday))
            .unwrap();
        state
            .write_window(
                &window("hn-digest", "weekday-morning", at(10, 0, 0), 1, 1),
                "default",
                at(10, 0, 0),
            )
            .unwrap();

        let row = compute_row(&agent, sched, &state, at(10, 30, 0));
        assert_eq!(row.state, HealthState::Failing, "reason: {}", row.reason);
        assert!(
            row.reason.contains("1 attempt"),
            "window state was not read: {}",
            row.reason
        );
    }

    /// Interval windows roll forward, so the key is the tick currently due —
    /// the same one the daemon dispatches against. Ticks here are 09:00,
    /// 10:30, 12:00; at 11:00 the due window is 10:30.
    #[test]
    fn interval_reads_the_rolled_window() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateStore::with_root(tmp.path().join("state"));
        let agent = agent_from(tmp.path(), INTERVAL_MANIFEST);
        let sched = &agent.manifest.schedules[0];

        state
            .write_heartbeat(&heartbeat("inbox-triage", at(9, 0, 0)))
            .unwrap();
        state
            .write_window(
                &window("inbox-triage", "every-90min", at(10, 30, 0), 1, 1),
                "default",
                at(10, 30, 0),
            )
            .unwrap();

        let row = compute_row(&agent, sched, &state, at(11, 0, 0));
        assert_eq!(row.state, HealthState::Failing, "reason: {}", row.reason);
        assert!(
            row.reason.contains("1 attempt"),
            "rolled window was not read: {}",
            row.reason
        );
    }

    #[test]
    fn window_key_is_the_due_window_not_the_last_success() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = agent_from(tmp.path(), CRON_MANIFEST);
        let sched = &agent.manifest.schedules[0];
        let hb = heartbeat("hn-digest", at(10, 12, 0));

        assert_eq!(
            window_key(sched, Some(&hb), at(10, 30, 0)),
            Some(at(10, 0, 0))
        );
    }
}
