//! Pure scheduling logic.
//!
//! Everything in this crate is a free function over `DateTime`s and manifest
//! types. No filesystem, no clock — callers pass `now` explicitly so the logic
//! is trivially testable.
//!
//! Replaces the heavy `_orch_expected_at` / `process_schedule` Fish functions
//! in `agents/agent-orchestrator/agent.fish` with strongly-typed helpers.

use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};
use dotagent_core::{heartbeat::Heartbeat, manifest::Schedule, state::WindowState, AgentManifest};
use serde::{Deserialize, Serialize};

/// Resolved per-schedule policy (applies overrides + agent defaults).
#[derive(Debug, Clone)]
pub struct ResolvedPolicy {
    pub max_retries: u32,
    pub retry_backoff_minutes: Vec<u32>,
    pub stale_after_minutes: u32,
}

impl ResolvedPolicy {
    pub fn resolve(manifest: &AgentManifest, schedule: &Schedule) -> Self {
        let defaults = &manifest.defaults;
        let ov = schedule.overrides();
        Self {
            max_retries: ov.max_retries.or(defaults.max_retries).unwrap_or(3),
            retry_backoff_minutes: ov
                .retry_backoff_minutes
                .clone()
                .or_else(|| defaults.retry_backoff_minutes.clone())
                .unwrap_or_else(default_backoff),
            stale_after_minutes: ov
                .stale_after_minutes
                .or(defaults.stale_after_minutes)
                .unwrap_or(120),
        }
    }
}

fn default_backoff() -> Vec<u32> {
    vec![5, 15, 30]
}

/// Compute the most recent expected window for a schedule, given `now`.
///
/// Returns `None` if there is no window today (cron-style weekday miss, or no
/// hour <= now) or if interval-style and there's no `last_success` to anchor
/// from (the OS scheduler bootstraps the first run; orchestrator never forces
/// it).
///
/// Interval schedules define an arithmetic sequence of ticks anchored on
/// `last_success` (`ls`, `ls + iv`, `ls + 2·iv`, …); this returns the greatest
/// tick `<= now`. Anchoring on success is deliberate — a run that succeeds
/// re-phases the sequence — but the sequence itself must keep advancing while
/// runs fail, exactly like the calendar keeps producing new cron windows.
/// Returning a frozen `ls + iv` forever is what deadlocked a failing interval
/// agent: its single window aged past `stale_after_minutes`, every tick was
/// skipped, `last_success` never advanced, and the window could never move.
pub fn expected_at(
    schedule: &Schedule,
    now: DateTime<Local>,
    last_success: Option<DateTime<Local>>,
) -> Option<DateTime<Local>> {
    match schedule {
        Schedule::Cron {
            weekdays,
            hours,
            minute,
            ..
        } => cron_expected_at(weekdays, hours, *minute, now),
        Schedule::Interval {
            interval_minutes, ..
        } => {
            let ls = last_success?;
            let Some(iv) = positive_interval(*interval_minutes) else {
                return Some(ls); // degenerate manifest — never comes due
            };
            // Ticks elapsed since the anchor, clamped at 0 so a clock that
            // jumped backwards cannot produce a window before the anchor.
            let ticks = (now - ls).num_minutes().div_euclid(iv).max(0);
            Some(ls + chrono::Duration::minutes(ticks * iv))
        }
        Schedule::Expression { .. } => None, // TODO: parse cron expression
    }
}

/// `interval_minutes` as a positive number of minutes, or `None` if the
/// manifest declared a zero interval (which has no meaningful cadence).
fn positive_interval(interval_minutes: u32) -> Option<i64> {
    match interval_minutes {
        0 => None,
        n => Some(n as i64),
    }
}

/// The first window an interval schedule missed after its last success.
///
/// Dispatch asks "is a run due right now?" and wants the rolling window;
/// health asks "how long has this been broken?" and wants this one. Judging
/// staleness against the rolling window would report a chronically failing
/// agent as a fresh failure forever.
fn first_missed_window(
    schedule: &Schedule,
    last_success: Option<DateTime<Local>>,
) -> Option<DateTime<Local>> {
    let Schedule::Interval {
        interval_minutes, ..
    } = schedule
    else {
        return None;
    };
    let iv = positive_interval(*interval_minutes)?;
    last_success.map(|ls| ls + chrono::Duration::minutes(iv))
}

/// Timestamp of the last successful run recorded in a heartbeat.
fn last_success_of(heartbeat: Option<&Heartbeat>) -> Option<DateTime<Local>> {
    heartbeat
        .and_then(|hb| hb.last_success_at)
        .and_then(|s| Local.timestamp_opt(s, 0).single())
}

/// Which window file describes the health being reported.
///
/// Window files are named after the window's `expected_at`
/// (`windows/<agent>-<slug>-<YYYY-MM-DD-HHMM>.json`), so the lookup key has to
/// be that timestamp — not `last_success_at`, which only lands on the same
/// filename when a run happens to finish inside the same minute its window
/// opened. That coincidence was why exactly one schedule ever reported a
/// window state and every other one silently read `None`.
///
/// Re-deriving the key from `(schedule, heartbeat, now)` fixes that for cron
/// and *only* for cron. A cron key is anchored on the calendar, so it survives
/// a success; an interval key is anchored on `last_success`, so a success
/// re-phases the whole tick sequence and the tick the daemon dispatched stops
/// belonging to it. A 90-minute agent dispatched at 10:30 that finishes at
/// 10:52 re-derives to 10:52 — a filename nobody ever wrote — which is the
/// original bug, restricted to the one schedule type this PR set out to fix.
///
/// So the key stops being reconstructed and starts being *read*:
/// `last_dispatched` is the newest window the daemon actually wrote a file
/// for, a fact about storage that no schedule can predict. It is only
/// consulted for a window that already succeeded, which is the one case where
/// the derived key drifts:
///
/// - **succeeded** (`last_success >= expected_at`) — the health being reported
///   belongs to the window the successful run was dispatched against, and only
///   `last_dispatched` knows which one that was. `None` (no window file on
///   disk yet, or a caller that cannot look) falls back to the derived key,
///   i.e. exactly the previous behaviour.
/// - **missed** — the window under judgement is the one currently due, and the
///   daemon writes against that same derived key. Deferring to an *older*
///   dispatched window here would resurrect a stale attempt count and hide the
///   `window due Nmin ago, no attempt` case, which exists to say "the daemon
///   never even tried".
///
/// Lives here rather than in a CLI module because every reader of a window
/// file has to agree with the writer on the key. Two independent
/// re-derivations of it is exactly how the same bug landed in two commands.
pub fn window_key(
    schedule: &Schedule,
    heartbeat: Option<&Heartbeat>,
    last_dispatched: Option<DateTime<Local>>,
    now: DateTime<Local>,
) -> Option<DateTime<Local>> {
    let last_success = last_success_of(heartbeat);
    let expected = expected_at(schedule, now, last_success)?;
    if !last_success.is_some_and(|ls| ls >= expected) {
        return Some(expected);
    }
    // A dispatched window newer than the one due cannot describe a success
    // that already happened — a clock jump is the only way to produce it, and
    // trusting it would report another window's attempts as this one's.
    Some(
        last_dispatched
            .filter(|w| *w <= expected)
            .unwrap_or(expected),
    )
}

fn cron_expected_at(
    weekdays: &[u8],
    hours: &[u8],
    minute: u8,
    now: DateTime<Local>,
) -> Option<DateTime<Local>> {
    let today_weekday = now.weekday().num_days_from_sunday() as u8; // 0=Sun..6=Sat
    if !weekdays.contains(&today_weekday) {
        return None;
    }

    let now_h = now.hour() as u8;
    let now_m = now.minute() as u8;

    let mut last_h: Option<u8> = None;
    for &h in hours {
        if h < now_h || (h == now_h && minute <= now_m) {
            last_h = Some(match last_h {
                Some(prev) if prev > h => prev,
                _ => h,
            });
        }
    }

    let h = last_h?;
    Local
        .with_ymd_and_hms(
            now.year(),
            now.month(),
            now.day(),
            h as u32,
            minute as u32,
            0,
        )
        .single()
}

/// Is this window so old that retrying is no longer useful?
pub fn is_stale(expected_at: DateTime<Local>, stale_after_min: u32, now: DateTime<Local>) -> bool {
    let age_min = (now - expected_at).num_minutes();
    age_min > stale_after_min as i64
}

/// Next scheduled trigger STRICTLY after `now`.
///
/// Used by the daemon to decide how long to sleep. For cron-style, walks
/// the next 7 days. For interval-style, returns `last_success + interval`
/// (or `now + interval` if never ran).
pub fn next_occurrence(
    schedule: &Schedule,
    now: DateTime<Local>,
    last_success: Option<DateTime<Local>>,
) -> Option<DateTime<Local>> {
    match schedule {
        Schedule::Cron {
            weekdays,
            hours,
            minute,
            ..
        } => cron_next_occurrence(weekdays, hours, *minute, now),
        Schedule::Interval {
            interval_minutes, ..
        } => {
            let iv = positive_interval(*interval_minutes)?;
            let anchor = last_success.unwrap_or(now);
            let next = anchor + chrono::Duration::minutes(iv);
            // If interval anchor + interval is in the past (we missed many windows),
            // catch up to the next forward-looking firing.
            if next > now {
                Some(next)
            } else {
                // (now - anchor) // interval + 1, then anchor + N*interval
                let n = (now - anchor).num_minutes().div_euclid(iv) + 1;
                Some(anchor + chrono::Duration::minutes(n * iv))
            }
        }
        Schedule::Expression { .. } => None, // TODO: cron-string parser
    }
}

fn cron_next_occurrence(
    weekdays: &[u8],
    hours: &[u8],
    minute: u8,
    now: DateTime<Local>,
) -> Option<DateTime<Local>> {
    if weekdays.is_empty() || hours.is_empty() {
        return None;
    }
    let mut sorted_hours: Vec<u8> = hours.to_vec();
    sorted_hours.sort_unstable();

    for day_offset in 0..=7 {
        let candidate_day = now + chrono::Duration::days(day_offset);
        let weekday = candidate_day.weekday().num_days_from_sunday() as u8;
        if !weekdays.contains(&weekday) {
            continue;
        }
        for &h in &sorted_hours {
            let candidate = Local
                .with_ymd_and_hms(
                    candidate_day.year(),
                    candidate_day.month(),
                    candidate_day.day(),
                    h as u32,
                    minute as u32,
                    0,
                )
                .single()?;
            if candidate > now {
                return Some(candidate);
            }
        }
    }
    None
}

/// Compute the earliest `next_occurrence` across every `(agent, schedule)`.
/// Returns `None` if there are no schedulable agents.
pub fn compute_next_event<'a, I>(agents: I, now: DateTime<Local>) -> Option<DateTime<Local>>
where
    I: IntoIterator<Item = AgentSchedulePair<'a>>,
{
    agents
        .into_iter()
        .filter_map(|p| next_occurrence(p.schedule, now, p.last_success))
        .min()
}

/// What `compute_next_event` consumes.
#[derive(Debug, Clone)]
pub struct AgentSchedulePair<'a> {
    pub agent_name: &'a str,
    pub schedule: &'a Schedule,
    pub last_success: Option<DateTime<Local>>,
}

/// Aggregate health state for a `(schedule, heartbeat, window_state)` triple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthState {
    /// Ran successfully within the current window, no retries needed.
    Ok,
    /// Recovered after at least one failed attempt.
    Degraded,
    /// Window passed without success, retrying or given up.
    Failing,
    /// Never ran, or the schedule has been broken for longer than
    /// `stale_after_minutes`.
    ///
    /// Measured from the *first* window missed since the last success, not
    /// from the one currently due. Interval windows roll forward so dispatch
    /// stays alive, so the due window is never more than one interval old —
    /// judging staleness against it would report an agent that has been dead
    /// for weeks as a fresh failure, forever. Cron windows do not roll, so for
    /// them the two are the same window.
    Stale,
}

/// Compute health for a schedule based on the latest heartbeat and (optional)
/// window state. Returns also a human-readable `reason`.
pub fn health_state(
    schedule: &Schedule,
    policy: &ResolvedPolicy,
    heartbeat: Option<&Heartbeat>,
    window_state: Option<&WindowState>,
    now: DateTime<Local>,
) -> (HealthState, String) {
    let last_success = last_success_of(heartbeat);
    let expected = expected_at(schedule, now, last_success);

    match (expected, last_success) {
        (None, None) => (HealthState::Stale, "never ran".into()),
        (None, Some(_)) => (HealthState::Ok, "no window today · last success ok".into()),
        (Some(exp), ls) if ls.is_some_and(|ls| ls >= exp) => {
            let failed = window_state.map(failed_attempts).unwrap_or(0);
            if failed > 0 {
                (
                    HealthState::Degraded,
                    format!("recovered after {}", attempts_label(failed)),
                )
            } else {
                (HealthState::Ok, "ok".into())
            }
        }
        (Some(exp), _) => {
            // Interval windows roll forward so dispatch stays alive; staleness
            // is judged from the first window we missed, so a schedule that has
            // been failing for weeks still reads as stale, not as fresh.
            let stale_ref = first_missed_window(schedule, last_success).unwrap_or(exp);
            if is_stale(stale_ref, policy.stale_after_minutes, now) {
                let age = human_age((now - stale_ref).num_minutes());
                (
                    HealthState::Stale,
                    format!("window missed {age} ago (stale)"),
                )
            } else if let Some(ws) = window_state {
                if ws.given_up {
                    (
                        HealthState::Failing,
                        format!("gave up after {}", attempts_label(ws.attempts)),
                    )
                } else {
                    (
                        HealthState::Failing,
                        format!("{}, will retry", attempts_label(ws.attempts)),
                    )
                }
            } else {
                let age = human_age((now - exp).num_minutes());
                (
                    HealthState::Failing,
                    format!("window due {age} ago, no attempt"),
                )
            }
        }
    }
}

/// Render an age the way a person reads it: `45min`, `3h 20m`, `55d 4h`.
///
/// These strings reach the user verbatim in `dotagent status` and in the daily
/// summary. The agent that motivated this work had been dead for 55 days and
/// reported `window missed 79200min ago` — arithmetically correct and useless
/// at the exact moment someone needed to grasp how bad it was.
fn human_age(minutes: i64) -> String {
    let m = minutes.max(0);
    let (d, h, rem) = (m / 1440, (m % 1440) / 60, m % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {rem}m")
    } else {
        format!("{rem}min")
    }
}

/// Render an attempt count with the right plural: "1 attempt", "2 attempts".
///
/// The counter reaches the user verbatim in `dotagent status` and in the daily
/// summary, so "1 attempts" is a visible defect, not a nitpick.
fn attempts_label(n: u32) -> String {
    if n == 1 {
        "1 attempt".into()
    } else {
        format!("{n} attempts")
    }
}

/// How many attempts in this window *failed*.
///
/// `attempts` is a dispatch counter: the daemon bumps it after every dispatch,
/// the successful one included, so a window that worked on the first try lands
/// on disk as `attempts = 1`. Reading that raw counter as a retry count is off
/// by one and calls every healthy agent `degraded`.
///
/// The successful dispatch is only discounted when the last attempt is the one
/// that exited 0. A window whose last recorded attempt failed but whose
/// heartbeat shows a later success was rescued from outside the window — a
/// manual `dotagent run` writes no window file — and really did burn every one
/// of those attempts.
fn failed_attempts(ws: &WindowState) -> u32 {
    if ws.last_attempt_exit_code == Some(0) {
        ws.attempts.saturating_sub(1)
    } else {
        ws.attempts
    }
}

/// Should we attempt a retry right now? Honors backoff progression.
///
/// `attempts` is the number of attempts already made in this window. After
/// the Nth attempt, the next retry waits `backoffs[min(N-1, len-1)]` minutes.
pub fn should_retry(
    attempts: u32,
    last_attempt: Option<DateTime<Local>>,
    backoffs: &[u32],
    now: DateTime<Local>,
) -> bool {
    let Some(last) = last_attempt else {
        return true;
    };
    if backoffs.is_empty() {
        return true;
    }
    let idx = (attempts.saturating_sub(1) as usize).min(backoffs.len() - 1);
    let wait_min = backoffs[idx] as i64;
    (now - last).num_minutes() >= wait_min
}

#[cfg(test)]
mod tests {
    use super::*;
    use dotagent_core::manifest::ScheduleOverrides;
    use dotagent_core::ScheduleDefaults;

    fn now_at(h: u32, m: u32) -> DateTime<Local> {
        // 2026-05-18 was a Monday — used as a fixed anchor.
        Local.with_ymd_and_hms(2026, 5, 18, h, m, 0).unwrap()
    }

    fn cron(id: &str, weekdays: Vec<u8>, hours: Vec<u8>, minute: u8) -> Schedule {
        Schedule::Cron {
            id: id.to_string(),
            weekdays,
            hours,
            minute,
            args: vec![],
            overrides: ScheduleOverrides::default(),
        }
    }

    #[test]
    fn cron_no_match_weekday_returns_none() {
        let s = cron("daily", vec![0, 6], vec![9], 0); // Sun/Sat only; 2026-05-18 = Mon
        assert!(expected_at(&s, now_at(10, 0), None).is_none());
    }

    #[test]
    fn cron_match_returns_latest_hour() {
        let s = cron("hourly", vec![1, 2, 3, 4, 5], vec![9, 10, 11], 0);
        let got = expected_at(&s, now_at(11, 30), None).unwrap();
        assert_eq!(got.hour(), 11);
    }

    #[test]
    fn cron_before_first_hour_returns_none() {
        let s = cron("daily", vec![1, 2, 3, 4, 5], vec![9], 30);
        assert!(expected_at(&s, now_at(8, 0), None).is_none());
    }

    #[test]
    fn interval_anchors_on_last_success() {
        let s = Schedule::Interval {
            id: "every-90".into(),
            interval_minutes: 90,
            args: vec![],
            overrides: ScheduleOverrides::default(),
        };
        let last = now_at(10, 0);
        let got = expected_at(&s, now_at(12, 0), Some(last)).unwrap();
        assert_eq!(got.hour(), 11);
        assert_eq!(got.minute(), 30);
    }

    #[test]
    fn interval_without_last_success_returns_none() {
        let s = Schedule::Interval {
            id: "every-90".into(),
            interval_minutes: 90,
            args: vec![],
            overrides: ScheduleOverrides::default(),
        };
        assert!(expected_at(&s, now_at(12, 0), None).is_none());
    }

    fn interval(id: &str, minutes: u32) -> Schedule {
        Schedule::Interval {
            id: id.to_string(),
            interval_minutes: minutes,
            args: vec![],
            overrides: ScheduleOverrides::default(),
        }
    }

    /// The bug this pins: an interval schedule that failed once stayed dead
    /// forever. `last_success` froze, so `expected_at` kept returning the very
    /// same (ancient) window. The daemon's stale gate then skipped it on every
    /// tick, so `last_success` could never advance. Found in the wild on an
    /// every-90-minute agent that had been silently dead for 55 days.
    #[test]
    fn interval_window_rolls_forward_across_missed_windows() {
        let s = interval("every-90min", 90);
        let now = now_at(12, 0);
        let last_success = now - chrono::Duration::days(55);

        let exp = expected_at(&s, now, Some(last_success)).unwrap();

        // Fresh window: at most one interval old, never in the future.
        assert!(exp <= now, "expected window must not be in the future");
        assert!(
            (now - exp).num_minutes() < 90,
            "expected window is {} min old — it did not roll forward",
            (now - exp).num_minutes()
        );
        // And it is a real tick of the sequence anchored on last_success.
        assert_eq!((exp - last_success).num_minutes() % 90, 0);
    }

    /// The window that got `given_up` is keyed by its own `expected_at`. Once
    /// the window rolls, the next dispatch reads a *different* window file, so
    /// `given_up` stops being terminal without any change to its semantics.
    #[test]
    fn interval_rolled_window_escapes_the_given_up_window() {
        let s = interval("every-90min", 90);
        let now = now_at(12, 0);
        let last_success = now - chrono::Duration::days(55);

        let dead_window = last_success + chrono::Duration::minutes(90);
        let exp = expected_at(&s, now, Some(last_success)).unwrap();

        assert_ne!(exp, dead_window, "still pinned to the given_up window");
        // And the daemon's stale gate must let the fresh window through.
        assert!(is_stale(dead_window, 120, now));
        assert!(!is_stale(exp, 120, now));
    }

    /// Successful cadence: the next tick has not arrived yet, so the most
    /// recent expected window *is* the last success — not a future timestamp.
    #[test]
    fn interval_expected_is_last_success_when_next_tick_is_future() {
        let s = interval("every-90", 90);
        let last = now_at(10, 0);
        let exp = expected_at(&s, now_at(10, 30), Some(last)).unwrap();
        assert_eq!(exp, last);
    }

    #[test]
    fn interval_expected_walks_the_tick_sequence() {
        let s = interval("every-90", 90);
        let last = now_at(9, 0);
        // 9:00, 10:30, 12:00, 13:30 …
        assert_eq!(expected_at(&s, now_at(10, 29), Some(last)).unwrap(), last);
        assert_eq!(
            expected_at(&s, now_at(10, 30), Some(last)).unwrap(),
            now_at(10, 30)
        );
        assert_eq!(
            expected_at(&s, now_at(13, 29), Some(last)).unwrap(),
            now_at(12, 0)
        );
    }

    #[test]
    fn interval_zero_minutes_does_not_panic() {
        let s = interval("degenerate", 0);
        let last = now_at(9, 0);
        assert_eq!(expected_at(&s, now_at(12, 0), Some(last)).unwrap(), last);
        assert!(next_occurrence(&s, now_at(12, 0), Some(last)).is_none());
    }

    /// Regression guard: cron never suffered from this, and must not start
    /// rolling. A missed cron window stays exactly where the calendar put it.
    #[test]
    fn cron_expected_does_not_roll_forward() {
        let s = cron("daily", vec![1, 2, 3, 4, 5], vec![9], 0);
        let last_success = now_at(9, 0) - chrono::Duration::days(55);
        let exp = expected_at(&s, now_at(23, 0), Some(last_success)).unwrap();
        assert_eq!(exp, now_at(9, 0));
        assert!(is_stale(exp, 120, now_at(23, 0)));
    }

    fn heartbeat_with_success(at: Option<DateTime<Local>>) -> Heartbeat {
        Heartbeat {
            name: "inbox-triage".into(),
            slug: "default".into(),
            args: vec![],
            started_at: 0,
            started_at_iso: String::new(),
            finished_at: Some(0),
            finished_at_iso: None,
            exit_code: Some(1),
            duration_seconds: None,
            last_success_at: at.map(|t| t.timestamp()),
            last_success_at_iso: None,
        }
    }

    fn policy(stale_after_minutes: u32) -> ResolvedPolicy {
        ResolvedPolicy {
            max_retries: 3,
            retry_backoff_minutes: default_backoff(),
            stale_after_minutes,
        }
    }

    /// Rolling the dispatch window must NOT hide chronic failure: the health
    /// report still measures staleness from the first window we missed.
    #[test]
    fn health_interval_chronic_failure_is_still_stale() {
        let s = interval("every-90min", 90);
        let now = now_at(12, 0);
        let last_success = now - chrono::Duration::days(55);
        let hb = heartbeat_with_success(Some(last_success));
        let ws = WindowState {
            agent: "inbox-triage".into(),
            schedule_id: "every-90min".into(),
            expected_at: (last_success + chrono::Duration::minutes(90)).timestamp(),
            attempts: 2,
            given_up: true,
            ..Default::default()
        };

        let (state, reason) = health_state(&s, &policy(120), Some(&hb), Some(&ws), now);
        assert_eq!(state, HealthState::Stale, "reason was: {reason}");
    }

    /// Healthy interval cadence reports ok, not a bogus "window expected
    /// -60min ago" failure.
    #[test]
    fn health_interval_ok_between_ticks() {
        let s = interval("every-90", 90);
        let hb = heartbeat_with_success(Some(now_at(10, 0)));
        let (state, reason) = health_state(&s, &policy(120), Some(&hb), None, now_at(10, 30));
        assert_eq!(state, HealthState::Ok, "reason was: {reason}");
    }

    /// `attempts` counts dispatches, not retries: the daemon bumps it after
    /// the successful one too. These four cases pin what the counter means
    /// once a window has succeeded.
    fn succeeded_window(attempts: u32, last_exit: Option<i32>) -> WindowState {
        WindowState {
            agent: "inbox-triage".into(),
            schedule_id: "morning".into(),
            expected_at: now_at(10, 0).timestamp(),
            attempts,
            last_attempt_at: Some(now_at(10, 0).timestamp()),
            last_attempt_exit_code: last_exit,
            ..Default::default()
        }
    }

    /// One dispatch, exit 0 — nothing failed. The raw counter says 1 and used
    /// to read as "recovered after 1 attempt".
    #[test]
    fn health_first_attempt_succeeded_is_ok() {
        let s = cron("morning", vec![0, 1, 2, 3, 4, 5, 6], vec![10], 0);
        let hb = heartbeat_with_success(Some(now_at(10, 0)));
        let ws = succeeded_window(1, Some(0));
        let (state, reason) = health_state(&s, &policy(120), Some(&hb), Some(&ws), now_at(10, 30));
        assert_eq!(state, HealthState::Ok, "reason was: {reason}");
    }

    /// Three dispatches, the last one exit 0 — two burned before it worked.
    #[test]
    fn health_success_after_failures_reports_only_the_failures() {
        let s = cron("morning", vec![0, 1, 2, 3, 4, 5, 6], vec![10], 0);
        let hb = heartbeat_with_success(Some(now_at(10, 20)));
        let ws = succeeded_window(3, Some(0));
        let (state, reason) = health_state(&s, &policy(120), Some(&hb), Some(&ws), now_at(10, 30));
        assert_eq!(state, HealthState::Degraded, "reason was: {reason}");
        assert!(reason.contains('2'), "expected 2 failed attempts: {reason}");
    }

    /// Every scheduled attempt failed and a manual `dotagent run` rescued it.
    /// The manual run writes no window file, so the window still shows the
    /// failed last attempt — all three attempts really were burned.
    #[test]
    fn health_manual_rescue_counts_every_failed_attempt() {
        let s = cron("morning", vec![0, 1, 2, 3, 4, 5, 6], vec![10], 0);
        let hb = heartbeat_with_success(Some(now_at(10, 45)));
        let ws = succeeded_window(3, Some(1));
        let (state, reason) = health_state(&s, &policy(120), Some(&hb), Some(&ws), now_at(11, 0));
        assert_eq!(state, HealthState::Degraded, "reason was: {reason}");
        assert!(reason.contains('3'), "expected 3 failed attempts: {reason}");
    }

    /// No window file at all — nothing to discount, nothing to report.
    #[test]
    fn health_success_without_window_state_is_ok() {
        let s = cron("morning", vec![0, 1, 2, 3, 4, 5, 6], vec![10], 0);
        let hb = heartbeat_with_success(Some(now_at(10, 0)));
        let (state, reason) = health_state(&s, &policy(120), Some(&hb), None, now_at(10, 30));
        assert_eq!(state, HealthState::Ok, "reason was: {reason}");
    }

    #[test]
    fn health_interval_failing_within_stale_horizon() {
        let s = interval("every-90", 90);
        let hb = heartbeat_with_success(Some(now_at(9, 0)));
        let ws = WindowState {
            agent: "a".into(),
            schedule_id: "every-90".into(),
            expected_at: now_at(10, 30).timestamp(),
            attempts: 1,
            ..Default::default()
        };
        // Window at 10:30 missed, now 11:00 — 30min old, well inside 120min.
        let (state, _) = health_state(&s, &policy(120), Some(&hb), Some(&ws), now_at(11, 0));
        assert_eq!(state, HealthState::Failing);
    }

    /// The shared bug this function exists to prevent: a run that finished at
    /// 10:12 belongs to the 10:00 window, and the window file is named after
    /// 10:00. Keying by `last_success_at` reads a filename that does not exist.
    #[test]
    fn window_key_is_the_due_window_not_the_last_success() {
        let s = cron("morning", vec![0, 1, 2, 3, 4, 5, 6], vec![10], 0);
        let hb = heartbeat_with_success(Some(now_at(10, 12)));
        assert_eq!(
            window_key(&s, Some(&hb), Some(now_at(10, 0)), now_at(10, 30)),
            Some(now_at(10, 0))
        );
    }

    /// Interval windows roll, so the key rolls with them — it must be the tick
    /// the daemon is dispatching against, not the frozen first missed window.
    #[test]
    fn window_key_follows_the_rolling_interval_tick() {
        let s = interval("every-90min", 90);
        let hb = heartbeat_with_success(Some(now_at(9, 0)));
        // Ticks: 09:00, 10:30, 12:00 — at 11:00 the due one is 10:30, and it
        // has not succeeded, so nothing the daemon dispatched earlier applies.
        assert_eq!(
            window_key(&s, Some(&hb), Some(now_at(9, 0)), now_at(11, 0)),
            Some(now_at(10, 30))
        );
    }

    /// The blocker this signature exists for. An every-90-minutes schedule
    /// dispatched against the 10:30 tick, burned two attempts, and succeeded
    /// on the third — which finished at 10:52. Re-deriving the key from
    /// `last_success_at` lands on 10:52, a filename the daemon never wrote, so
    /// the retries vanish and the schedule reads `ok`. Only a run that finished
    /// inside its own dispatch minute ever matched, which is precisely the
    /// coincidence this whole function was introduced to kill.
    #[test]
    fn window_key_survives_an_interval_run_that_outlived_its_dispatch_minute() {
        let s = interval("every-90min", 90);
        let hb = heartbeat_with_success(Some(now_at(10, 52)));
        assert_eq!(
            window_key(&s, Some(&hb), Some(now_at(10, 30)), now_at(11, 0)),
            Some(now_at(10, 30))
        );
    }

    /// Same shape end-to-end: the window says three dispatches with the last
    /// one exit 0, so two failed and the schedule is `degraded`, not `ok`.
    #[test]
    fn health_interval_recovered_after_retries_is_degraded() {
        let s = interval("every-90min", 90);
        let hb = heartbeat_with_success(Some(now_at(10, 52)));
        let ws = WindowState {
            agent: "inbox-triage".into(),
            schedule_id: "every-90min".into(),
            expected_at: now_at(10, 30).timestamp(),
            attempts: 3,
            last_attempt_at: Some(now_at(10, 50).timestamp()),
            last_attempt_exit_code: Some(0),
            ..Default::default()
        };
        let key = window_key(&s, Some(&hb), Some(now_at(10, 30)), now_at(11, 0));
        assert_eq!(key, Some(now_at(10, 30)), "wrong window file would be read");

        let (state, reason) = health_state(&s, &policy(120), Some(&hb), Some(&ws), now_at(11, 0));
        assert_eq!(state, HealthState::Degraded, "reason was: {reason}");
        assert!(reason.contains('2'), "expected 2 failed attempts: {reason}");
    }

    /// A manual `dotagent run` rescues an interval schedule whose scheduled
    /// attempts all failed. The rescue writes no window file, so the newest
    /// dispatched window is still the one that burned them — and that is the
    /// one the report has to read.
    #[test]
    fn window_key_after_a_manual_rescue_is_the_last_dispatched_window() {
        let s = interval("every-90min", 90);
        let hb = heartbeat_with_success(Some(now_at(10, 45)));
        assert_eq!(
            window_key(&s, Some(&hb), Some(now_at(10, 30)), now_at(11, 0)),
            Some(now_at(10, 30))
        );
    }

    /// A window the daemon never dispatched must stay unreadable, otherwise
    /// `window due Nmin ago, no attempt` — the only signal that says "the
    /// daemon is not running" — silently reports an older window's attempts.
    #[test]
    fn window_key_of_a_missed_window_ignores_older_dispatches() {
        let s = interval("every-90min", 90);
        let hb = heartbeat_with_success(Some(now_at(9, 0)));
        assert_eq!(
            window_key(&s, Some(&hb), Some(now_at(9, 0)), now_at(11, 0)),
            Some(now_at(10, 30)),
        );
    }

    /// Nothing on disk yet, or a caller that cannot look: fall back to the
    /// derived key, which is what every reader did before.
    #[test]
    fn window_key_without_a_dispatched_window_falls_back_to_expected() {
        let s = cron("morning", vec![0, 1, 2, 3, 4, 5, 6], vec![10], 0);
        let hb = heartbeat_with_success(Some(now_at(10, 12)));
        assert_eq!(
            window_key(&s, Some(&hb), None, now_at(10, 30)),
            Some(now_at(10, 0))
        );
    }

    /// A dispatched window in the future of the due one cannot describe a
    /// success that already happened; only a clock jump produces it.
    #[test]
    fn window_key_ignores_a_dispatched_window_newer_than_the_due_one() {
        let s = cron("morning", vec![0, 1, 2, 3, 4, 5, 6], vec![10], 0);
        let hb = heartbeat_with_success(Some(now_at(10, 12)));
        assert_eq!(
            window_key(&s, Some(&hb), Some(now_at(23, 0)), now_at(10, 30)),
            Some(now_at(10, 0))
        );
    }

    #[test]
    fn window_key_without_heartbeat_matches_expected_at() {
        let s = cron("morning", vec![0, 1, 2, 3, 4, 5, 6], vec![10], 0);
        assert_eq!(
            window_key(&s, None, None, now_at(10, 30)),
            Some(now_at(10, 0))
        );
        // Interval has no anchor without a success, so there is no window.
        assert_eq!(
            window_key(&interval("iv", 90), None, None, now_at(10, 30)),
            None
        );
    }

    /// The counters reach the user verbatim, and 79200 minutes is not a
    /// duration anyone reads.
    #[test]
    fn human_age_scales_past_the_minute() {
        assert_eq!(human_age(0), "0min");
        assert_eq!(human_age(45), "45min");
        assert_eq!(human_age(200), "3h 20m");
        assert_eq!(human_age(79_200), "55d 0h");
        assert_eq!(human_age(-5), "0min");
    }

    /// The 55-day agent, rendered. The old string was `79200min ago`.
    #[test]
    fn stale_reason_reads_in_days() {
        let s = interval("every-90min", 90);
        let now = now_at(12, 0);
        let hb = heartbeat_with_success(Some(now - chrono::Duration::days(55)));
        let (state, reason) = health_state(&s, &policy(120), Some(&hb), None, now);
        assert_eq!(state, HealthState::Stale);
        assert!(reason.contains("54d"), "unreadable age: {reason}");
    }

    #[test]
    fn stale_after_minutes_works() {
        let exp = now_at(10, 0);
        let now = now_at(13, 0);
        assert!(is_stale(exp, 120, now));
        assert!(!is_stale(exp, 200, now));
    }

    #[test]
    fn should_retry_first_attempt_is_immediate() {
        assert!(should_retry(0, None, &[5, 15, 30], now_at(12, 0)));
    }

    #[test]
    fn should_retry_respects_backoff() {
        let last = now_at(12, 0);
        let now_too_soon = now_at(12, 3);
        let now_ready = now_at(12, 6);
        assert!(!should_retry(1, Some(last), &[5, 15, 30], now_too_soon));
        assert!(should_retry(1, Some(last), &[5, 15, 30], now_ready));
    }

    #[test]
    fn resolve_policy_uses_overrides_first() {
        let mut m = AgentManifest {
            agent: dotagent_core::manifest::AgentMeta {
                name: "x".into(),
                description: None,
                monitor: true,
                timeout_seconds: 1800,
                version: None,
            },
            run: dotagent_core::manifest::RunConfig {
                command: "fish".into(),
                args: vec![],
                working_dir: None,
            },
            env: None,
            lifecycle: Default::default(),
            defaults: ScheduleDefaults {
                max_retries: Some(3),
                retry_backoff_minutes: Some(vec![5, 15, 30]),
                stale_after_minutes: Some(120),
            },
            schedules: vec![],
            preflight: vec![],
            notifiers: vec![],
            on_success: vec![],
            on_failure: vec![],
            security: Default::default(),
        };
        let mut sched = cron("d", vec![1], vec![9], 0);
        if let Schedule::Cron { overrides, .. } = &mut sched {
            overrides.max_retries = Some(20);
        }
        m.schedules.push(sched.clone());
        let p = ResolvedPolicy::resolve(&m, &sched);
        assert_eq!(p.max_retries, 20);
        assert_eq!(p.retry_backoff_minutes, vec![5, 15, 30]);
    }

    #[test]
    fn next_occurrence_cron_returns_today_if_hour_ahead() {
        // 2026-05-18 is Monday (weekday 1)
        let s = cron("hourly", vec![1, 2, 3, 4, 5], vec![10, 14, 18], 0);
        let now = now_at(11, 0);
        let next = next_occurrence(&s, now, None).unwrap();
        assert_eq!(next.hour(), 14);
        assert_eq!(next.day(), 18);
    }

    #[test]
    fn next_occurrence_cron_skips_to_next_matching_day() {
        // Friday → next weekday match for [1..5] is Monday
        let s = cron("daily", vec![1, 2, 3, 4, 5], vec![8], 30);
        // 2026-05-22 is a Friday
        let now = Local.with_ymd_and_hms(2026, 5, 22, 9, 0, 0).unwrap();
        let next = next_occurrence(&s, now, None).unwrap();
        assert_eq!(next.weekday().num_days_from_sunday(), 1); // Monday
    }

    #[test]
    fn next_occurrence_interval_anchors_forward() {
        let s = Schedule::Interval {
            id: "every-90".into(),
            interval_minutes: 90,
            args: vec![],
            overrides: ScheduleOverrides::default(),
        };
        // Last success 3h ago, interval 90min → next is "now + 30min"
        let last = now_at(9, 0);
        let now = now_at(12, 0);
        let next = next_occurrence(&s, now, Some(last)).unwrap();
        assert!(next > now);
        let delta = (next - now).num_minutes();
        assert!(delta <= 90 && delta > 0);
    }

    #[test]
    fn next_occurrence_interval_without_anchor_uses_now() {
        let s = Schedule::Interval {
            id: "every-90".into(),
            interval_minutes: 90,
            args: vec![],
            overrides: ScheduleOverrides::default(),
        };
        let now = now_at(12, 0);
        let next = next_occurrence(&s, now, None).unwrap();
        assert_eq!((next - now).num_minutes(), 90);
    }

    #[test]
    fn compute_next_event_picks_earliest() {
        let s1 = cron("morning", vec![1, 2, 3, 4, 5], vec![8], 30);
        let s2 = cron("afternoon", vec![1, 2, 3, 4, 5], vec![14], 0);
        let now = now_at(9, 0);

        let pairs = vec![
            AgentSchedulePair {
                agent_name: "a",
                schedule: &s1,
                last_success: None,
            },
            AgentSchedulePair {
                agent_name: "b",
                schedule: &s2,
                last_success: None,
            },
        ];
        let next = compute_next_event(pairs, now).unwrap();
        assert_eq!(next.hour(), 14);
    }

    #[test]
    fn compute_next_event_empty_returns_none() {
        let pairs: Vec<AgentSchedulePair> = vec![];
        assert!(compute_next_event(pairs, now_at(12, 0)).is_none());
    }
}
