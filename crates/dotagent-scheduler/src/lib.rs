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
        } => last_success.map(|ls| ls + chrono::Duration::minutes(*interval_minutes as i64)),
        Schedule::Expression { .. } => None, // TODO: parse cron expression
    }
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
            let anchor = last_success.unwrap_or(now);
            let next = anchor + chrono::Duration::minutes(*interval_minutes as i64);
            // If interval anchor + interval is in the past (we missed many windows),
            // catch up to the next forward-looking firing.
            if next > now {
                Some(next)
            } else {
                // (now - anchor) // interval + 1, then anchor + N*interval
                let elapsed_min = (now - anchor).num_minutes();
                let n = elapsed_min.div_euclid(*interval_minutes as i64) + 1;
                Some(anchor + chrono::Duration::minutes(n * *interval_minutes as i64))
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
    /// Recovered after attempts > 0, or interval-style is running past its
    /// next-tick but within `2 * interval` grace.
    Degraded,
    /// Window passed without success, retrying or given up.
    Failing,
    /// Never ran, or window is older than `stale_after_minutes`.
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
    // Resolve last_success time
    let last_success = heartbeat
        .and_then(|hb| hb.last_success_at)
        .and_then(|s| Local.timestamp_opt(s, 0).single());

    let expected = expected_at(schedule, now, last_success);

    match (expected, last_success) {
        (None, None) => (HealthState::Stale, "nunca rodou".into()),
        (None, Some(_)) => (
            HealthState::Ok,
            "sem janela hoje · último sucesso ok".into(),
        ),
        (Some(exp), ls) if ls.is_some_and(|ls| ls >= exp) => {
            let attempts = window_state.map(|w| w.attempts).unwrap_or(0);
            if attempts > 0 {
                (
                    HealthState::Degraded,
                    format!("recuperou após {attempts} tentativas"),
                )
            } else {
                (HealthState::Ok, "ok".into())
            }
        }
        (Some(exp), _) => {
            if is_stale(exp, policy.stale_after_minutes, now) {
                let age_min = (now - exp).num_minutes();
                (
                    HealthState::Stale,
                    format!("janela perdida há {age_min}min (stale)"),
                )
            } else if let Some(ws) = window_state {
                if ws.given_up {
                    (
                        HealthState::Failing,
                        format!("desisti após {} tentativas", ws.attempts),
                    )
                } else {
                    (
                        HealthState::Failing,
                        format!("{} tentativas, vai retentar", ws.attempts),
                    )
                }
            } else {
                let age_min = (now - exp).num_minutes();
                (
                    HealthState::Failing,
                    format!("janela esperada há {age_min}min, sem ação"),
                )
            }
        }
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
