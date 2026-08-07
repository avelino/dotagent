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
//! 2. **Start time.** The window is deliberately *asymmetric* — see
//!    [`DEFAULT_START_TOLERANCE`] and [`START_FORWARD_TOLERANCE`].
//! 3. **Command.** The observed command line must be consistent with the one
//!    that was spawned, down to the script — not merely to the interpreter.
//!
//! All three must pass. Anything missing, ambiguous, or unparseable is a
//! **skip**, never a kill: leaving one orphan alive costs memory, and killing
//! the wrong process costs somebody's work.
//!
//! Identity is proved **twice**: once to decide who gets a `SIGTERM`, and
//! again after the grace window, before anything is escalated to `SIGKILL`.
//! An orphan has been reparented to init, which `waitpid`s it the instant it
//! dies, so a pid the `SIGTERM` freed is immediately available to somebody
//! else. The whole point of the grace window is that the process exits during
//! it, which makes the first proof stale exactly when it matters most.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDateTime, TimeDelta};
use tracing::warn;

use crate::{signal, ProcessInfo, DEFAULT_KILL_GRACE};

/// How much *earlier* than the record the OS-reported start may be.
///
/// The record is stamped after `spawn()` returns, so the OS value is always
/// slightly earlier; `ps` also truncates to whole seconds, pushing it earlier
/// still. Five seconds absorbs both, plus a small NTP step.
pub const DEFAULT_START_TOLERANCE: Duration = Duration::from_secs(5);

/// How much *later* than the record the OS-reported start may be.
///
/// Almost none, and that asymmetry is the whole point. `record.started_at` is
/// taken after `spawn()` returns, so a genuine process always started before
/// it. A recycled pid is the opposite by construction: it only exists because
/// the original died, and the original died *after* its record was written —
/// so every impostor sits on the positive side. A symmetric window would
/// donate exactly the region where no legitimate process is ever found and
/// every impostor is.
///
/// One second, not zero, because `ps` reports whole seconds and a clock step
/// can land the wrong side of a boundary.
pub const START_FORWARD_TOLERANCE: Duration = Duration::from_secs(1);

/// How long to wait for a `SIGKILL`ed group to actually disappear before
/// admitting it did not.
const KILL_CONFIRM_ATTEMPTS: u32 = 20;
const KILL_CONFIRM_INTERVAL: Duration = Duration::from_millis(25);

/// What the OS says about a live pid right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessFacts {
    pub pgid: i32,
    pub started_at: DateTime<Local>,
    /// Full command line, tokens separated by single spaces.
    pub command: String,
}

/// What one probe of a pid found.
///
/// "The OS said nothing I could read" and "there is no such process" are
/// different answers with different consequences, so they are different
/// variants. Collapsing them is how a broken parser turns into a reaper that
/// believes every pid is free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probed {
    /// `ps` answered and the line parsed. Here is what it says.
    Facts(ProcessFacts),
    /// `ps` answered — so the pid *is* alive — but the line did not parse.
    /// Alive-but-unidentifiable is never a licence to signal.
    Unreadable,
    /// `ps` found nothing. The pid is free.
    Gone,
}

/// Why a recorded process was left alone. Every variant means "did not
/// signal anything".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// pid 0/1, our own pid, or no process group was ever recorded.
    Unusable,
    /// No such process — it already exited. The common case.
    Gone,
    /// A process is there, but the OS description of it did not parse. Not
    /// the same as [`SkipReason::Gone`], and not something to signal.
    Unreadable,
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
            SkipReason::Unreadable => "unreadable_process",
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
    /// Signalled *and observed gone afterwards*. This is what feeds the audit
    /// log, which is a record of what happened — not of what was attempted.
    pub reaped: Vec<ReapedOrphan>,
    pub skipped: Vec<(ProcessInfo, SkipReason)>,
    /// Signalled, but still alive at the end of the sweep — or the signal
    /// itself failed (`EPERM`), or the pid stopped looking like ours during
    /// the grace window so the escalation was called off.
    pub survived: Vec<(ProcessInfo, String)>,
    /// Set when the snapshot itself could not be read or parsed. Nothing is
    /// signalled in that case — a corrupt registry is exactly when guessing
    /// is most expensive.
    pub snapshot_error: Option<String>,
}

impl OrphanReport {
    pub fn is_empty(&self) -> bool {
        self.reaped.is_empty()
            && self.skipped.is_empty()
            && self.survived.is_empty()
            && self.snapshot_error.is_none()
    }
}

type ProbeFn = Box<dyn Fn(u32) -> Probed + Send + Sync>;
type KillFn = Box<dyn Fn(i32, i32) -> std::io::Result<()> + Send + Sync>;

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
            probe: Box::new(probe_process),
            kill: Box::new(signal::killpg),
            grace: DEFAULT_KILL_GRACE,
            tolerance: DEFAULT_START_TOLERANCE,
            self_pid: std::process::id(),
        }
    }

    /// Replace the OS probe. Test affordance.
    #[must_use]
    pub fn with_probe<F>(mut self, f: F) -> Self
    where
        F: Fn(u32) -> Probed + Send + Sync + 'static,
    {
        self.probe = Box::new(f);
        self
    }

    /// Replace the signal call. Test affordance — a test that asserts "this
    /// pid must NOT be killed" needs to observe the absence of the call.
    ///
    /// The result is not decorative: a `killpg` that fails with `EPERM` means
    /// nothing was signalled, and the report must not claim otherwise.
    #[must_use]
    pub fn with_killer<F>(mut self, f: F) -> Self
    where
        F: Fn(i32, i32) -> std::io::Result<()> + Send + Sync + 'static,
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
    /// `SIGTERM` to each group, one shared grace window, then — for whatever
    /// is *still provably ours* — `SIGKILL`.
    pub async fn reap(&self, records: &[ProcessInfo]) -> OrphanReport {
        let mut report = OrphanReport::default();
        let mut victims: Vec<(ProcessInfo, i32)> = Vec::new();

        for record in records {
            match classify(
                record,
                &self.probe_record(record),
                self.self_pid,
                self.tolerance,
            ) {
                Verdict::Reap { pgid } => victims.push((record.clone(), pgid)),
                Verdict::Skip(reason) => {
                    if reason == SkipReason::Unreadable {
                        warn!(
                            pid = record.pid,
                            label = %record.label,
                            "a process holds this pid but the OS description did not parse — left alone"
                        );
                    }
                    report.skipped.push((record.clone(), reason));
                }
            }
        }

        if victims.is_empty() {
            return report;
        }

        let mut signalled: Vec<(ProcessInfo, i32)> = Vec::new();
        for (info, pgid) in victims {
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
            match (self.kill)(pgid, signal::SIGTERM) {
                Ok(()) => signalled.push((info, pgid)),
                Err(e) => {
                    warn!(pid = info.pid, pgid, error = %e, "SIGTERM was refused");
                    report.survived.push((info, format!("SIGTERM failed: {e}")));
                }
            }
        }

        if signalled.is_empty() {
            return report;
        }

        tokio::time::sleep(self.grace).await;

        for (info, pgid) in signalled {
            self.escalate(info, pgid, &mut report).await;
        }
        report
    }

    /// The grace window has elapsed. Prove identity *again* before escalating.
    async fn escalate(&self, info: ProcessInfo, pgid: i32, report: &mut OrphanReport) {
        match classify(
            &info,
            &self.probe_record(&info),
            self.self_pid,
            self.tolerance,
        ) {
            // The SIGTERM did the job. The pid is free — which is exactly why
            // it must not be signalled again.
            Verdict::Skip(SkipReason::Gone) => report.reaped.push(ReapedOrphan { info, pgid }),
            Verdict::Reap { .. } => match (self.kill)(pgid, signal::SIGKILL) {
                Ok(()) if self.confirm_gone(info.pid).await => {
                    report.reaped.push(ReapedOrphan { info, pgid })
                }
                Ok(()) => {
                    warn!(pid = info.pid, pgid, "still alive after SIGKILL");
                    report
                        .survived
                        .push((info, "still alive after SIGKILL".into()));
                }
                Err(e) => {
                    warn!(pid = info.pid, pgid, error = %e, "SIGKILL was refused");
                    report.survived.push((info, format!("SIGKILL failed: {e}")));
                }
            },
            // Something answers to this pid, but it is no longer the process
            // we proved before the SIGTERM. Escalating now would kill it.
            Verdict::Skip(reason) => {
                warn!(
                    pid = info.pid,
                    pgid,
                    reason = %reason,
                    "pid stopped matching during the grace window — not escalating to SIGKILL"
                );
                report
                    .survived
                    .push((info, format!("identity changed during grace: {reason}")));
            }
        }
    }

    fn probe_record(&self, record: &ProcessInfo) -> Probed {
        if usable(record, self.self_pid) {
            (self.probe)(record.pid)
        } else {
            Probed::Gone
        }
    }

    /// Did the `SIGKILL` actually land? Signal delivery is asynchronous, so a
    /// single immediate probe would report a live process that is already on
    /// its way out.
    async fn confirm_gone(&self, pid: u32) -> bool {
        for _ in 0..KILL_CONFIRM_ATTEMPTS {
            if (self.probe)(pid) == Probed::Gone {
                return true;
            }
            tokio::time::sleep(KILL_CONFIRM_INTERVAL).await;
        }
        false
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
    probed: &Probed,
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
    let facts = match probed {
        Probed::Facts(f) => f,
        Probed::Unreadable => return Verdict::Skip(SkipReason::Unreadable),
        Probed::Gone => return Verdict::Skip(SkipReason::Gone),
    };
    if facts.pgid != pgid {
        return Verdict::Skip(SkipReason::NotGroupLeader);
    }
    // Positive means the OS says it started *before* the record was stamped,
    // which is where every genuine process lives. See the two tolerance
    // constants: the window is asymmetric on purpose.
    let behind = record.started_at - facts.started_at;
    let forward = TimeDelta::from_std(START_FORWARD_TOLERANCE).unwrap_or(TimeDelta::MAX);
    let backward = TimeDelta::from_std(tolerance).unwrap_or(TimeDelta::MAX);
    if behind < -forward || behind > backward {
        return Verdict::Skip(SkipReason::StartTimeMismatch);
    }
    if !command_matches(&record.command, &facts.command) {
        return Verdict::Skip(SkipReason::CommandMismatch);
    }
    Verdict::Reap { pgid }
}

/// Is the observed command line consistent with the one we spawned?
///
/// The interpreter alone proves nothing. Every agent in this project is some
/// flavour of `fish agent.fish`, and the user's login shell is also `fish` —
/// and, being a session leader, it satisfies the process-group axis too. Since
/// the kill is a `killpg`, "same program name" as a sufficient condition means
/// the reaper can take down the user's whole working session.
///
/// So two things must hold:
///
/// 1. **The program is one this record names.** Same basename, or the observed
///    program appears as a token in the recorded line — which is what an exec
///    shim looks like: `/usr/bin/env python3 agent.py` runs as
///    `python3 agent.py`.
/// 2. **The identifying token is there, verbatim, as a whole token.** That is
///    the last recorded token that looks like a path: the script, not the
///    interpreter. `contains()` would not do — an unanchored substring match
///    lets `vim /agents/x/agent.py` pass for the agent it is editing.
///
/// When the recorded line has no path-shaped token at all (`sleep 120`), there
/// is nothing more identifying than the arguments themselves, so they must
/// match exactly.
///
/// Anything else is a mismatch, and a mismatch is a skip.
fn command_matches(recorded: &str, observed: &str) -> bool {
    let rec: Vec<&str> = recorded.split_whitespace().collect();
    let obs: Vec<&str> = observed.split_whitespace().collect();
    let (Some(rec_prog), Some(obs_prog)) = (rec.first(), obs.first()) else {
        return false;
    };

    let program_is_named = basename(rec_prog) == basename(obs_prog)
        || rec.iter().any(|t| basename(t) == basename(obs_prog));
    if !program_is_named {
        return false;
    }

    match rec.iter().rev().find(|t| looks_like_path(t)) {
        Some(anchor) => obs.iter().any(|t| t == anchor),
        None => rec[1..] == obs[1..],
    }
}

/// Does this token look like something on the filesystem — a script, a binary,
/// a data file — rather than a flag or a bare word?
fn looks_like_path(token: &str) -> bool {
    if token.starts_with('-') {
        return false;
    }
    if token.contains('/') {
        return true;
    }
    // `agent.fish`, `index.js`: a relative script still names a file.
    token.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty() && !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric())
    })
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Does a process with this pid exist right now?
///
/// Used by the daemon to decide whether another instance of itself is alive
/// before it touches anything a previous one recorded. Deliberately answered
/// by `ps`'s exit status alone: the question is "did the OS find the pid",
/// not "could I read every field it printed". Tying liveness to a successful
/// parse means one bad `ps` line silently retires the peer-daemon guard.
#[cfg(unix)]
pub fn process_exists(pid: u32) -> bool {
    ps_command(pid)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub fn process_exists(_pid: u32) -> bool {
    false
}

/// The `ps` invocation both probes share.
///
/// `LC_ALL=C` is load-bearing, not hygiene. `lstart` is rendered through the
/// locale's `%c`, so a daemon started from a terminal with, say,
/// `LC_TIME=pt_BR.UTF-8` gets `sex  7 ago 05:30:03 2026` — a different month
/// name *and* a different field order. The parse then fails for every pid,
/// which fails safe here but turns the whole feature into a silent no-op.
#[cfg(unix)]
fn ps_command(pid: u32) -> std::process::Command {
    let mut cmd = std::process::Command::new("ps");
    cmd.env("LC_ALL", "C")
        .args(["-o", "pgid=,lstart=,command=", "-p", &pid.to_string()]);
    cmd
}

/// Ask the OS about a pid via `ps`.
///
/// `ps` rather than a `/proc` read because macOS has no `/proc`, and rather
/// than a `libproc`/`sysctl` binding because this runs a handful of times, at
/// boot, once. It is a plain synchronous call on purpose: it is not an
/// orchestrated subprocess, it is a probe — the same category as `osascript`
/// inside a notify driver.
#[cfg(unix)]
pub fn probe_process(pid: u32) -> Probed {
    let Ok(out) = ps_command(pid).output() else {
        // We could not even run `ps`. That is not evidence the pid is free.
        warn!(pid, "could not run ps — treating the pid as unreadable");
        return Probed::Unreadable;
    };
    if !out.status.success() {
        return Probed::Gone;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    match text.lines().next().and_then(parse_ps_line) {
        Some(facts) => Probed::Facts(facts),
        None => {
            warn!(pid, "ps found the pid but its output did not parse");
            Probed::Unreadable
        }
    }
}

#[cfg(not(unix))]
pub fn probe_process(_pid: u32) -> Probed {
    Probed::Unreadable
}

/// Parse one `ps -o pgid=,lstart=,command=` line.
///
/// `lstart` is five whitespace-separated tokens (`Fri Aug  7 05:30:03 2026`)
/// once the locale is pinned to `C` — see [`ps_command`]. The weekday is
/// dropped rather than parsed: it carries no information the rest of the stamp
/// does not.
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
    use std::sync::atomic::{AtomicBool, Ordering};

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
            classify(&rec, &Probed::Facts(f.clone()), 999, TOL),
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
            classify(&rec, &Probed::Facts(f.clone()), 999, TOL),
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
            classify(&rec, &Probed::Facts(f.clone()), 999, TOL),
            Verdict::Skip(SkipReason::CommandMismatch)
        );
    }

    /// BLOCKER 1 (red before the fix).
    #[test]
    fn a_process_that_started_after_the_record_is_never_killed() {
        let rec = record(2115, at(0));
        let f = facts(2115, at(3), "fish /agents/telegram-assistant/agent.fish");
        assert_eq!(
            classify(&rec, &Probed::Facts(f.clone()), 999, TOL),
            Verdict::Skip(SkipReason::StartTimeMismatch)
        );
    }

    /// BLOCKER 2a (red before the fix).
    #[test]
    fn the_login_shell_is_never_killed() {
        let rec = record(2115, at(0));
        let f = facts(2115, at(0), "fish");
        assert_eq!(
            classify(&rec, &Probed::Facts(f.clone()), 999, TOL),
            Verdict::Skip(SkipReason::CommandMismatch)
        );
    }

    /// BLOCKER 2b (red before the fix).
    #[test]
    fn same_interpreter_different_script_is_never_killed() {
        for (recorded, observed) in [
            ("fish /agents/a/agent.fish", "fish /agents/b/agent.fish"),
            ("bash /agents/a/run.sh", "bash /agents/b/run.sh"),
            ("python3 /agents/a/agent.py", "python3 /agents/b/agent.py"),
            ("node /agents/a/index.js", "node /agents/b/index.js"),
        ] {
            let mut rec = record(2115, at(0));
            rec.command = recorded.into();
            let f = facts(2115, at(0), observed);
            assert_eq!(
                classify(&rec, &Probed::Facts(f.clone()), 999, TOL),
                Verdict::Skip(SkipReason::CommandMismatch),
                "{recorded:?} must not match {observed:?}"
            );
        }
    }

    #[test]
    fn non_leader_is_never_killed() {
        let rec = record(2115, at(0));
        let mut f = facts(2115, at(0), "fish /agents/telegram-assistant/agent.fish");
        f.pgid = 900;
        assert_eq!(
            classify(&rec, &Probed::Facts(f.clone()), 999, TOL),
            Verdict::Skip(SkipReason::NotGroupLeader)
        );
    }

    #[test]
    fn gone_process_is_a_skip_not_an_error() {
        let rec = record(2115, at(0));
        assert_eq!(
            classify(&rec, &Probed::Gone, 999, TOL),
            Verdict::Skip(SkipReason::Gone)
        );
    }

    #[test]
    fn record_without_identity_fields_is_ambiguous() {
        let mut rec = record(2115, at(0));
        rec.pgid = None;
        let f = facts(2115, at(0), "fish /agents/telegram-assistant/agent.fish");
        assert_eq!(
            classify(&rec, &Probed::Facts(f.clone()), 999, TOL),
            Verdict::Skip(SkipReason::Ambiguous)
        );

        let mut rec = record(2115, at(0));
        rec.command = String::new();
        assert_eq!(
            classify(&rec, &Probed::Facts(f.clone()), 999, TOL),
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
            classify(&rec, &Probed::Facts(f.clone()), 999, TOL),
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
            classify(&records[0], &Probed::Facts(f.clone()), 999, TOL),
            Verdict::Skip(SkipReason::Ambiguous)
        );
    }

    #[test]
    fn own_pid_and_init_are_never_touched() {
        let f = facts(1, at(0), "fish /agents/telegram-assistant/agent.fish");
        assert_eq!(
            classify(&record(1, at(0)), &Probed::Facts(f.clone()), 999, TOL),
            Verdict::Skip(SkipReason::Unusable)
        );
        assert_eq!(
            classify(&record(999, at(0)), &Probed::Facts(f.clone()), 999, TOL),
            Verdict::Skip(SkipReason::Unusable)
        );
    }

    #[test]
    fn exec_shim_still_matches_via_the_script_path() {
        let mut rec = record(2115, at(0));
        rec.command = "/usr/bin/env python3 /agents/x/agent.py".into();
        let f = facts(2115, at(0), "python3 /agents/x/agent.py");
        assert_eq!(
            classify(&rec, &Probed::Facts(f.clone()), 999, TOL),
            Verdict::Reap { pgid: 2115 }
        );
    }

    #[test]
    fn ps_line_parses_the_documented_shape() {
        let f = parse_ps_line("  497   Fri Aug  7 05:30:03 2026 /bin/zsh -c echo hi").unwrap();
        assert_eq!(f.pgid, 497);
        assert_eq!(f.command, "/bin/zsh -c echo hi");
        assert_eq!(
            f.started_at.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-08-07 05:30:03"
        );
    }

    /// The literal above proves the parser understands a string we wrote. It
    /// would pass just as happily on a machine whose `ps` prints something
    /// else entirely. Ask the real `ps` about the one pid we are certain
    /// exists — our own.
    #[cfg(unix)]
    #[test]
    fn ps_describes_this_very_process_on_this_platform() {
        let me = std::process::id();
        assert!(process_exists(me), "our own pid must be visible to ps");
        match probe_process(me) {
            Probed::Facts(f) => {
                assert!(f.pgid > 0, "pgid: {}", f.pgid);
                assert!(!f.command.is_empty());
                assert!(
                    f.started_at <= Local::now(),
                    "a process cannot start in the future: {}",
                    f.started_at
                );
            }
            other => panic!("ps could not describe our own pid: {other:?}"),
        }
    }

    /// The locale fix is only observable in the command we build — pinning it
    /// is what keeps `lstart` in the one format [`parse_ps_line`] knows.
    #[cfg(unix)]
    #[test]
    fn the_ps_probe_pins_the_locale() {
        let has_lc_all = ps_command(1)
            .get_envs()
            .any(|(k, v)| k == "LC_ALL" && v == Some("C".as_ref()));
        assert!(has_lc_all, "ps must run under LC_ALL=C");
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
            .with_killer(move |pgid, sig| {
                sink.lock().unwrap().push((pgid, sig));
                Ok(())
            })
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
        let alive = std::sync::Arc::new(AtomicBool::new(true));
        let probe_alive = alive.clone();
        let kill_alive = alive.clone();

        let report = OrphanReaper::new()
            .with_grace(Duration::from_millis(1))
            .with_probe(move |pid| match pid {
                3001 if probe_alive.load(Ordering::SeqCst) => Probed::Facts(facts(
                    3001,
                    at(0),
                    "fish /agents/telegram-assistant/agent.fish",
                )),
                // Same pid the record names, but it is a different process now.
                3002 => Probed::Facts(facts(3002, at(50_000), "vim notes.md")),
                _ => Probed::Gone,
            })
            .with_killer(move |pgid, sig| {
                kill_alive.store(false, Ordering::SeqCst);
                sink.lock().unwrap().push((pgid, sig));
                Ok(())
            })
            .reap_snapshot_file(&path)
            .await;

        assert_eq!(report.reaped.len(), 1);
        assert_eq!(report.reaped[0].info.pid, 3001);
        assert!(report.survived.is_empty());
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
        assert_eq!(*killed.lock().unwrap(), vec![(3001, signal::SIGTERM)]);
    }

    /// BLOCKER 3 (red before the fix): the SIGTERM worked, so by the time the
    /// grace window ends nothing holds that pid. Sending SIGKILL anyway aims
    /// at a number the OS is free to have handed to somebody else.
    #[tokio::test]
    async fn sigkill_is_not_sent_to_a_pid_the_sigterm_already_freed() {
        let alive = std::sync::Arc::new(AtomicBool::new(true));
        let probe_alive = alive.clone();
        let kill_alive = alive.clone();

        let killed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(i32, i32)>::new()));
        let sink = killed.clone();

        let report = OrphanReaper::new()
            .with_grace(Duration::from_millis(1))
            .with_probe(move |pid| {
                if probe_alive.load(Ordering::SeqCst) {
                    Probed::Facts(facts(
                        pid,
                        at(0),
                        "fish /agents/telegram-assistant/agent.fish",
                    ))
                } else {
                    Probed::Gone
                }
            })
            .with_killer(move |pgid, sig| {
                kill_alive.store(false, Ordering::SeqCst);
                sink.lock().unwrap().push((pgid, sig));
                Ok(())
            })
            .reap(&[record(3001, at(0))])
            .await;

        assert_eq!(
            report.reaped.len(),
            1,
            "the orphan is gone — that is a reap"
        );
        assert_eq!(
            *killed.lock().unwrap(),
            vec![(3001, signal::SIGTERM)],
            "no SIGKILL may be sent to a pid that is already free"
        );
    }

    /// BLOCKER 3, the expensive half: the SIGTERM freed the pid and something
    /// else took it during the grace window. The pre-grace proof still says
    /// "ours". Only a fresh proof catches this.
    #[tokio::test]
    async fn a_pid_reused_during_the_grace_window_is_not_sigkilled() {
        let terminated = std::sync::Arc::new(AtomicBool::new(false));
        let probe_flag = terminated.clone();
        let kill_flag = terminated.clone();

        let killed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(i32, i32)>::new()));
        let sink = killed.clone();

        let report = OrphanReaper::new()
            .with_grace(Duration::from_millis(1))
            .with_probe(move |pid| {
                if probe_flag.load(Ordering::SeqCst) {
                    // The orphan died; the OS handed the number to a build.
                    Probed::Facts(facts(pid, at(9_000), "cargo build --release"))
                } else {
                    Probed::Facts(facts(
                        pid,
                        at(0),
                        "fish /agents/telegram-assistant/agent.fish",
                    ))
                }
            })
            .with_killer(move |pgid, sig| {
                kill_flag.store(true, Ordering::SeqCst);
                sink.lock().unwrap().push((pgid, sig));
                Ok(())
            })
            .reap(&[record(3001, at(0))])
            .await;

        assert_eq!(
            *killed.lock().unwrap(),
            vec![(3001, signal::SIGTERM)],
            "the pid belongs to somebody else now — no SIGKILL"
        );
        assert!(
            report.reaped.is_empty(),
            "nothing was confirmed dead: {report:?}"
        );
        assert_eq!(report.survived.len(), 1);
    }

    /// A `killpg` that fails is not a reap. The audit log is fed from
    /// `reaped`, and it is supposed to record what happened.
    #[tokio::test]
    async fn a_refused_signal_is_never_reported_as_reaped() {
        let report = OrphanReaper::new()
            .with_grace(Duration::from_millis(1))
            .with_probe(move |pid| {
                Probed::Facts(facts(
                    pid,
                    at(0),
                    "fish /agents/telegram-assistant/agent.fish",
                ))
            })
            .with_killer(|_, _| Err(std::io::ErrorKind::PermissionDenied.into()))
            .reap(&[record(3001, at(0))])
            .await;

        assert!(report.reaped.is_empty(), "EPERM killed nothing");
        assert_eq!(report.survived.len(), 1);
        assert!(report.survived[0].1.contains("SIGTERM failed"));
    }

    /// A live process whose `ps` line did not parse is alive, not gone — and
    /// must be told apart from a free pid.
    #[test]
    fn an_unreadable_process_is_not_the_same_as_a_gone_one() {
        let rec = record(2115, at(0));
        assert_eq!(
            classify(&rec, &Probed::Unreadable, 999, TOL),
            Verdict::Skip(SkipReason::Unreadable)
        );
        assert_ne!(SkipReason::Unreadable, SkipReason::Gone);
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
