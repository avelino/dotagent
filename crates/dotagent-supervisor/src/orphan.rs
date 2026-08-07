//! Boot-time reap of processes a *previous* daemon left behind.
//!
//! The reaper in [`crate::reaper`] enforces deadlines for as long as the
//! supervisor is alive. When the daemon itself dies without running its
//! shutdown path — `SIGKILL`, a panic, `launchctl kickstart -k` — its children
//! survive with nobody holding their clock. They keep whatever they had open
//! and run forever.
//!
//! The snapshot the daemon writes for `dotagent status`
//! (`~/.config/dotagent/state/supervisor.json`) is the only record that
//! survives it. Its presence at boot means the previous daemon did not exit
//! cleanly — the shutdown path deletes it.
//!
//! # Why a recorded pid is not enough
//!
//! The OS recycles pids. Signalling a pid read off disk, with no further
//! proof, eventually kills somebody else's process. So every record is
//! corroborated against what the OS says about that pid *right now*, on three
//! independent axes:
//!
//! 1. **Group leadership.** Every supervised spawn gets `setpgid(0, 0)`, so
//!    `pgid == pid`. A recycled pid that is not its own group leader is out.
//! 2. **Start time.** The OS-reported start must match the recorded one within
//!    [`DEFAULT_START_TOLERANCE`]. A process that inherited the number started
//!    later — after the old daemon died, which is after the record was written.
//! 3. **Command.** The observed command line must be consistent with the one
//!    that was spawned.
//!
//! All three must pass. Anything missing, ambiguous, or unparseable is a
//! **skip**, never a kill: leaving one orphan alive costs memory, and killing
//! the wrong process costs somebody's work.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDateTime};
use tracing::warn;

use crate::{signal, ProcessInfo, DEFAULT_KILL_GRACE};

/// How far the OS-reported start time may differ from the recorded one.
///
/// The record is stamped after `spawn()` returns, so the OS value is always
/// slightly *earlier*; `ps` also reports whole seconds. Five seconds absorbs
/// both without being wide enough to matter — a pid can only be reused after
/// wrapping the whole pid space, which does not happen in five seconds.
pub const DEFAULT_START_TOLERANCE: Duration = Duration::from_secs(5);

/// What the OS says about a live pid right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessFacts {
    pub pgid: i32,
    pub started_at: DateTime<Local>,
    /// Full command line, tokens separated by single spaces.
    pub command: String,
}

/// Why a recorded process was left alone. Every variant means "did not
/// signal anything".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// pid 0/1, our own pid, or no process group was ever recorded.
    Unusable,
    /// No such process — it already exited. The common case.
    Gone,
    /// The live process is not its own group leader, so it is not ours.
    NotGroupLeader,
    /// The live process started at a different time than the record says.
    StartTimeMismatch,
    /// The live process is running a different command than the record says.
    CommandMismatch,
    /// The record predates the fields identity checking needs.
    Ambiguous,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            SkipReason::Unusable => "unusable_record",
            SkipReason::Gone => "already_gone",
            SkipReason::NotGroupLeader => "not_group_leader",
            SkipReason::StartTimeMismatch => "start_time_mismatch",
            SkipReason::CommandMismatch => "command_mismatch",
            SkipReason::Ambiguous => "ambiguous_record",
        };
        f.write_str(s)
    }
}

/// Outcome of classifying one record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Identity confirmed on all axes — safe to `killpg` this group.
    Reap {
        pgid: i32,
    },
    Skip(SkipReason),
}

/// One process the reaper actually signalled.
#[derive(Debug, Clone)]
pub struct ReapedOrphan {
    pub info: ProcessInfo,
    pub pgid: i32,
}

/// Everything one boot-time sweep did, and everything it deliberately did not.
#[derive(Debug, Clone, Default)]
pub struct OrphanReport {
    pub reaped: Vec<ReapedOrphan>,
    pub skipped: Vec<(ProcessInfo, SkipReason)>,
    /// Set when the snapshot itself could not be read or parsed. Nothing is
    /// signalled in that case — a corrupt registry is exactly when guessing
    /// is most expensive.
    pub snapshot_error: Option<String>,
}

impl OrphanReport {
    pub fn is_empty(&self) -> bool {
        self.reaped.is_empty() && self.skipped.is_empty() && self.snapshot_error.is_none()
    }
}

type ProbeFn = Box<dyn Fn(u32) -> Option<ProcessFacts> + Send + Sync>;
type KillFn = Box<dyn Fn(i32, i32) + Send + Sync>;

/// Sweeps a previous daemon's snapshot and kill-trees whatever is still alive
/// *and provably ours*.
///
/// The OS probe and the signal call are both injectable so tests can drive
/// identity logic — including the pid-recycle case — without depending on a
/// real process existing, and without any chance of signalling one.
pub struct OrphanReaper {
    probe: ProbeFn,
    kill: KillFn,
    grace: Duration,
    tolerance: Duration,
    self_pid: u32,
}

impl std::fmt::Debug for OrphanReaper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrphanReaper")
            .field("grace", &self.grace)
            .field("tolerance", &self.tolerance)
            .field("self_pid", &self.self_pid)
            .finish_non_exhaustive()
    }
}

impl Default for OrphanReaper {
    fn default() -> Self {
        Self::new()
    }
}

impl OrphanReaper {
    pub fn new() -> Self {
        Self {
            probe: Box::new(process_facts),
            kill: Box::new(|pgid, sig| {
                let _ = signal::killpg(pgid, sig);
            }),
            grace: DEFAULT_KILL_GRACE,
            tolerance: DEFAULT_START_TOLERANCE,
            self_pid: std::process::id(),
        }
    }

    /// Replace the OS probe. Test affordance.
    #[must_use]
    pub fn with_probe<F>(mut self, f: F) -> Self
    where
        F: Fn(u32) -> Option<ProcessFacts> + Send + Sync + 'static,
    {
        self.probe = Box::new(f);
        self
    }

    /// Replace the signal call. Test affordance — a test that asserts "this
    /// pid must NOT be killed" needs to observe the absence of the call.
    #[must_use]
    pub fn with_killer<F>(mut self, f: F) -> Self
    where
        F: Fn(i32, i32) + Send + Sync + 'static,
    {
        self.kill = Box::new(f);
        self
    }

    #[must_use]
    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    #[must_use]
    pub fn with_tolerance(mut self, tolerance: Duration) -> Self {
        self.tolerance = tolerance;
        self
    }

    /// Read a snapshot written by a previous daemon and reap what is still
    /// alive. A missing file is not an error — it is the normal state after a
    /// clean shutdown.
    pub async fn reap_snapshot_file(&self, path: &Path) -> OrphanReport {
        let raw = match std::fs::read(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return OrphanReport::default(),
            Err(e) => {
                return OrphanReport {
                    snapshot_error: Some(e.to_string()),
                    ..Default::default()
                }
            }
        };
        let records: Vec<ProcessInfo> = match serde_json::from_slice(&raw) {
            Ok(v) => v,
            Err(e) => {
                return OrphanReport {
                    snapshot_error: Some(e.to_string()),
                    ..Default::default()
                }
            }
        };
        self.reap(&records).await
    }

    /// Classify every record, then kill-tree the confirmed ones:
    /// `SIGTERM` to each group, one shared grace window, then `SIGKILL`.
    pub async fn reap(&self, records: &[ProcessInfo]) -> OrphanReport {
        let mut report = OrphanReport::default();
        let mut victims: Vec<(ProcessInfo, i32)> = Vec::new();

        for record in records {
            let facts = if usable(record, self.self_pid) {
                (self.probe)(record.pid)
            } else {
                None
            };
            match classify(record, facts.as_ref(), self.self_pid, self.tolerance) {
                Verdict::Reap { pgid } => victims.push((record.clone(), pgid)),
                Verdict::Skip(reason) => report.skipped.push((record.clone(), reason)),
            }
        }

        if victims.is_empty() {
            return report;
        }

        for (info, pgid) in &victims {
            warn!(
                pid = info.pid,
                pgid,
                label = %info.label,
                agent = %info.owner.agent,
                kind = %info.kind,
                age_seconds = info.age_seconds,
                deadline_seconds = info.deadline_seconds,
                "orphan from a previous daemon — sending SIGTERM to its process group"
            );
            (self.kill)(*pgid, signal::SIGTERM);
        }

        tokio::time::sleep(self.grace).await;

        for (info, pgid) in victims {
            (self.kill)(pgid, signal::SIGKILL);
            report.reaped.push(ReapedOrphan { info, pgid });
        }
        report
    }
}

/// Cheap pre-filter: is this record even worth probing?
fn usable(record: &ProcessInfo, self_pid: u32) -> bool {
    record.pid > 1 && record.pid != self_pid
}

/// Decide what to do with one record, given what the OS currently reports.
///
/// Pure: no IO, no clock, no signals. This is the whole safety argument, so it
/// is the part that gets tested directly.
pub fn classify(
    record: &ProcessInfo,
    facts: Option<&ProcessFacts>,
    self_pid: u32,
    tolerance: Duration,
) -> Verdict {
    if !usable(record, self_pid) {
        return Verdict::Skip(SkipReason::Unusable);
    }
    // A snapshot written before the supervisor persisted these fields cannot
    // be identity-checked. Refusing is the point.
    let Some(pgid) = record.pgid else {
        return Verdict::Skip(SkipReason::Ambiguous);
    };
    if record.command.trim().is_empty() {
        return Verdict::Skip(SkipReason::Ambiguous);
    }
    // Every supervised spawn is its own group leader. A record claiming
    // otherwise is not something this supervisor produced.
    if pgid != record.pid as i32 {
        return Verdict::Skip(SkipReason::Ambiguous);
    }
    let Some(facts) = facts else {
        return Verdict::Skip(SkipReason::Gone);
    };
    if facts.pgid != pgid {
        return Verdict::Skip(SkipReason::NotGroupLeader);
    }
    let drift = (facts.started_at - record.started_at).abs();
    if drift.to_std().map(|d| d > tolerance).unwrap_or(true) {
        return Verdict::Skip(SkipReason::StartTimeMismatch);
    }
    if !command_matches(&record.command, &facts.command) {
        return Verdict::Skip(SkipReason::CommandMismatch);
    }
    Verdict::Reap { pgid }
}

/// Is the observed command line consistent with the one we spawned?
///
/// Two ways to agree, because one alone has a known blind spot:
///
/// - Same program basename. Handles the ordinary case (`fish …` stays `fish`).
/// - The observed line mentions the recorded first argument. Handles exec
///   shims — `/usr/bin/env python3 agent.py` shows up as `python3 agent.py`,
///   where the basenames differ but the script path is right there and is far
///   more identifying than the interpreter anyway.
///
/// Anything else is a mismatch, and a mismatch is a skip.
fn command_matches(recorded: &str, observed: &str) -> bool {
    let rec: Vec<&str> = recorded.split_whitespace().collect();
    let obs: Vec<&str> = observed.split_whitespace().collect();
    let (Some(rec_prog), Some(obs_prog)) = (rec.first(), obs.first()) else {
        return false;
    };
    if basename(rec_prog) == basename(obs_prog) {
        return true;
    }
    match rec.get(1) {
        Some(arg) if !arg.is_empty() => observed.contains(arg),
        _ => false,
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Does a process with this pid exist right now?
///
/// Used by the daemon to decide whether another instance of itself is alive
/// before it touches anything a previous one recorded.
pub fn process_exists(pid: u32) -> bool {
    process_facts(pid).is_some()
}

/// Ask the OS about a pid via `ps`.
///
/// `ps` rather than a `/proc` read because macOS has no `/proc`, and rather
/// than a `libproc`/`sysctl` binding because this runs a handful of times, at
/// boot, once. It is a plain synchronous call on purpose: it is not an
/// orchestrated subprocess, it is a probe — the same category as `osascript`
/// inside a notify driver.
#[cfg(unix)]
pub fn process_facts(pid: u32) -> Option<ProcessFacts> {
    let out = std::process::Command::new("ps")
        .args(["-o", "pgid=,lstart=,command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_ps_line(text.lines().next()?)
}

#[cfg(not(unix))]
pub fn process_facts(_pid: u32) -> Option<ProcessFacts> {
    None
}

/// Parse one `ps -o pgid=,lstart=,command=` line.
///
/// `lstart` is five whitespace-separated tokens on both macOS and Linux
/// (`Fri Aug  7 05:30:03 2026`). The weekday is dropped rather than parsed —
/// it carries no information the rest of the stamp does not, and skipping it
/// sidesteps locale.
pub(crate) fn parse_ps_line(line: &str) -> Option<ProcessFacts> {
    let mut it = line.split_whitespace();
    let pgid: i32 = it.next()?.parse().ok()?;
    let _weekday = it.next()?;
    let month = it.next()?;
    let day = it.next()?;
    let time = it.next()?;
    let year = it.next()?;
    let stamp = format!("{month} {day} {time} {year}");
    let naive = NaiveDateTime::parse_from_str(&stamp, "%b %d %H:%M:%S %Y").ok()?;
    // Ambiguous (DST fall-back) or nonexistent local times resolve to None,
    // which becomes "cannot confirm identity" upstream — the safe direction.
    let started_at = naive.and_local_timezone(Local).single()?;
    let command = it.collect::<Vec<_>>().join(" ");
    Some(ProcessFacts {
        pgid,
        started_at,
        command,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProcessKind, ProcessOwner};
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Local> {
        Local.timestamp_opt(1_700_000_000 + secs, 0).unwrap()
    }

    fn record(pid: u32, started: DateTime<Local>) -> ProcessInfo {
        ProcessInfo {
            id: 1,
            pid,
            kind: ProcessKind::PersistentAgent,
            owner: ProcessOwner {
                agent: "telegram-assistant".into(),
                ..Default::default()
            },
            label: "telegram-assistant.persistent[42]".into(),
            started_at: started,
            deadline_seconds: 600,
            age_seconds: 1600,
            deadline_pct: 100,
            pgid: Some(pid as i32),
            command: "fish /agents/telegram-assistant/agent.fish".into(),
        }
    }

    fn facts(pid: u32, started: DateTime<Local>, command: &str) -> ProcessFacts {
        ProcessFacts {
            pgid: pid as i32,
            started_at: started,
            command: command.into(),
        }
    }

    const TOL: Duration = DEFAULT_START_TOLERANCE;

    #[test]
    fn confirmed_identity_is_reaped() {
        let rec = record(2115, at(0));
        let f = facts(2115, at(0), "fish /agents/telegram-assistant/agent.fish");
        assert_eq!(
            classify(&rec, Some(&f), 999, TOL),
            Verdict::Reap { pgid: 2115 }
        );
    }

    /// The test that keeps this fix from being worse than the bug it fixes.
    #[test]
    fn recycled_pid_is_never_killed() {
        let rec = record(2115, at(0));
        // Same pid, same group leadership, same everything the record knows —
        // except it started long after the daemon that recorded it died.
        let f = facts(
            2115,
            at(9_000),
            "fish /agents/telegram-assistant/agent.fish",
        );
        assert_eq!(
            classify(&rec, Some(&f), 999, TOL),
            Verdict::Skip(SkipReason::StartTimeMismatch)
        );
    }

    #[test]
    fn recycled_pid_running_something_else_is_never_killed() {
        let rec = record(2115, at(0));
        // Start time inside tolerance AND group leader — only the command
        // gives it away. This is the case pid-recycling-under-load produces.
        let f = facts(2115, at(1), "/usr/bin/postgres -D /var/db");
        assert_eq!(
            classify(&rec, Some(&f), 999, TOL),
            Verdict::Skip(SkipReason::CommandMismatch)
        );
    }

    #[test]
    fn non_leader_is_never_killed() {
        let rec = record(2115, at(0));
        let mut f = facts(2115, at(0), "fish /agents/telegram-assistant/agent.fish");
        f.pgid = 900;
        assert_eq!(
            classify(&rec, Some(&f), 999, TOL),
            Verdict::Skip(SkipReason::NotGroupLeader)
        );
    }

    #[test]
    fn gone_process_is_a_skip_not_an_error() {
        let rec = record(2115, at(0));
        assert_eq!(
            classify(&rec, None, 999, TOL),
            Verdict::Skip(SkipReason::Gone)
        );
    }

    #[test]
    fn record_without_identity_fields_is_ambiguous() {
        let mut rec = record(2115, at(0));
        rec.pgid = None;
        let f = facts(2115, at(0), "fish /agents/telegram-assistant/agent.fish");
        assert_eq!(
            classify(&rec, Some(&f), 999, TOL),
            Verdict::Skip(SkipReason::Ambiguous)
        );

        let mut rec = record(2115, at(0));
        rec.command = String::new();
        assert_eq!(
            classify(&rec, Some(&f), 999, TOL),
            Verdict::Skip(SkipReason::Ambiguous)
        );
    }

    #[test]
    fn record_that_was_not_a_group_leader_is_ambiguous() {
        let mut rec = record(2115, at(0));
        rec.pgid = Some(900);
        let mut f = facts(2115, at(0), "fish /agents/telegram-assistant/agent.fish");
        f.pgid = 900;
        assert_eq!(
            classify(&rec, Some(&f), 999, TOL),
            Verdict::Skip(SkipReason::Ambiguous)
        );
    }

    /// A snapshot written by a build that predates `pgid` / `command` must
    /// still deserialize — and must classify as un-confirmable, not as a
    /// victim.
    #[test]
    fn a_pre_upgrade_snapshot_deserializes_and_is_refused() {
        let json = r#"[{
            "id": 1, "pid": 2115, "kind": "persistent_agent",
            "owner": {"agent": "telegram-assistant"},
            "label": "telegram-assistant.persistent[42]",
            "started_at": "2026-08-07T19:44:33-03:00",
            "deadline_seconds": 600, "age_seconds": 1600, "deadline_pct": 100
        }]"#;
        let records: Vec<ProcessInfo> = serde_json::from_str(json).unwrap();
        assert_eq!(records[0].pgid, None);
        assert_eq!(records[0].command, "");
        let f = facts(2115, records[0].started_at, "fish agent.fish");
        assert_eq!(
            classify(&records[0], Some(&f), 999, TOL),
            Verdict::Skip(SkipReason::Ambiguous)
        );
    }

    #[test]
    fn own_pid_and_init_are_never_touched() {
        let f = facts(1, at(0), "fish /agents/telegram-assistant/agent.fish");
        assert_eq!(
            classify(&record(1, at(0)), Some(&f), 999, TOL),
            Verdict::Skip(SkipReason::Unusable)
        );
        assert_eq!(
            classify(&record(999, at(0)), Some(&f), 999, TOL),
            Verdict::Skip(SkipReason::Unusable)
        );
    }

    #[test]
    fn exec_shim_still_matches_via_the_script_path() {
        let mut rec = record(2115, at(0));
        rec.command = "/usr/bin/env python3 /agents/x/agent.py".into();
        let f = facts(2115, at(0), "python3 /agents/x/agent.py");
        assert_eq!(
            classify(&rec, Some(&f), 999, TOL),
            Verdict::Reap { pgid: 2115 }
        );
    }

    #[test]
    fn ps_line_parses_on_this_platform() {
        let f = parse_ps_line("  497   Fri Aug  7 05:30:03 2026 /bin/zsh -c echo hi").unwrap();
        assert_eq!(f.pgid, 497);
        assert_eq!(f.command, "/bin/zsh -c echo hi");
        assert_eq!(
            f.started_at.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-08-07 05:30:03"
        );
    }

    #[test]
    fn ps_line_garbage_is_none() {
        assert!(parse_ps_line("").is_none());
        assert!(parse_ps_line("not-a-pgid Fri Aug 7 05:30:03 2026 x").is_none());
        assert!(parse_ps_line("497 Fri Aug 7 05:30:03").is_none());
    }

    #[tokio::test]
    async fn unreadable_snapshot_kills_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("supervisor.json");
        std::fs::write(&path, b"{ not json").unwrap();

        let killed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(i32, i32)>::new()));
        let sink = killed.clone();
        let report = OrphanReaper::new()
            .with_killer(move |pgid, sig| sink.lock().unwrap().push((pgid, sig)))
            .reap_snapshot_file(&path)
            .await;

        assert!(report.snapshot_error.is_some());
        assert!(report.reaped.is_empty());
        assert!(killed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_snapshot_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let report = OrphanReaper::new()
            .reap_snapshot_file(&dir.path().join("nope.json"))
            .await;
        assert!(report.is_empty());
    }

    /// A synthetic registry in a tempdir: one confirmed orphan, one recycled
    /// pid, one long gone. Only the confirmed one may be signalled.
    #[tokio::test]
    async fn snapshot_sweep_signals_only_the_confirmed_orphan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("supervisor.json");

        let mut orphan = record(3001, at(0));
        orphan.id = 1;
        let mut recycled = record(3002, at(0));
        recycled.id = 2;
        let mut gone = record(3003, at(0));
        gone.id = 3;

        std::fs::write(
            &path,
            serde_json::to_vec(&vec![orphan.clone(), recycled.clone(), gone.clone()]).unwrap(),
        )
        .unwrap();

        let killed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(i32, i32)>::new()));
        let sink = killed.clone();
        let report = OrphanReaper::new()
            .with_grace(Duration::from_millis(1))
            .with_probe(|pid| match pid {
                3001 => Some(facts(
                    3001,
                    at(0),
                    "fish /agents/telegram-assistant/agent.fish",
                )),
                // Same pid the record names, but it is a different process now.
                3002 => Some(facts(3002, at(50_000), "vim notes.md")),
                _ => None,
            })
            .with_killer(move |pgid, sig| sink.lock().unwrap().push((pgid, sig)))
            .reap_snapshot_file(&path)
            .await;

        assert_eq!(report.reaped.len(), 1);
        assert_eq!(report.reaped[0].info.pid, 3001);
        assert_eq!(
            report
                .skipped
                .iter()
                .map(|(i, r)| (i.pid, *r))
                .collect::<Vec<_>>(),
            vec![
                (3002, SkipReason::StartTimeMismatch),
                (3003, SkipReason::Gone)
            ]
        );
        assert_eq!(
            *killed.lock().unwrap(),
            vec![(3001, signal::SIGTERM), (3001, signal::SIGKILL)]
        );
    }

    /// End to end against the real OS, with a process this test created: spawn
    /// a group leader, snapshot it the way the daemon does, then reap it.
    #[cfg(unix)]
    #[tokio::test]
    async fn reaps_a_real_process_it_spawned() {
        use crate::{SpawnSpec, Supervisor};

        let sup = Supervisor::new();
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("120");
        let handle = sup
            .spawn_supervised(
                cmd,
                SpawnSpec {
                    kind: ProcessKind::Agent,
                    owner: ProcessOwner {
                        agent: "orphan-test".into(),
                        ..Default::default()
                    },
                    deadline: Duration::from_secs(120),
                    label: "orphan-test".into(),
                },
            )
            .await
            .unwrap();

        let snap = sup.snapshot();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("supervisor.json");
        std::fs::write(&path, serde_json::to_vec(&snap).unwrap()).unwrap();

        // Simulate the daemon dying: drop the handle without awaiting it, so
        // nothing in-process is enforcing the deadline anymore. The snapshot
        // on disk is all that is left, which is exactly the boot situation.
        drop(handle);

        let report = OrphanReaper::new()
            .with_grace(Duration::from_millis(50))
            .reap_snapshot_file(&path)
            .await;

        assert_eq!(report.reaped.len(), 1, "report: {report:?}");
        let pid = report.reaped[0].info.pid;
        // Give the OS a beat to deliver SIGKILL, then confirm it is gone.
        for _ in 0..40 {
            if !process_exists(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("pid {pid} survived the reap");
    }
}
