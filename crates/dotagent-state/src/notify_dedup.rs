//! When to say it again.
//!
//! Some failures announce themselves once and then stop existing as an event.
//! A run that gives up fires `given_up` at the moment it happens; an agent that
//! silently stops being scheduled fires nothing at all, forever. Both leave the
//! operator with the same wrong impression — silence reads as success.
//!
//! Closing that gap means re-notifying from a condition rather than an event,
//! and a condition is true on every tick. This module is the part that decides
//! how often a still-true condition is allowed to speak.
//!
//! ## Shape
//!
//! ```jsonc
//! {
//!   "episodes": {
//!     "inbox-triage/every-90min/stale": {
//!       "first_notified_at": 1785925367,
//!       "last_notified_at":  1786011767,
//!       "count": 2
//!     }
//!   }
//! }
//! ```
//!
//! An *episode* starts at the first notification and ends when the pair goes
//! healthy again. Ending it is what keeps the next failure loud from its first
//! second instead of inheriting a day-long silence from the previous one.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::StateError;

/// Spacing between re-notifications of one ongoing alert, in seconds. The last
/// entry repeats for as long as the condition holds.
///
/// A ladder rather than a fixed interval because the two failure modes are
/// opposite and both end in silence. Notify once and the alert is buried by
/// whatever else arrived that hour — that is the 55-day outage this exists to
/// close. Notify every tick (the daemon wakes at least half-hourly) and the
/// reader is trained to dismiss dotagent on sight, which is the same silence
/// with extra steps. Rising spacing puts the density in the first hours, when a
/// fix is most likely to actually happen, then settles at one a day: loud
/// enough to stay on the list, quiet enough to live with while it waits.
const BACKOFF_SECONDS: [i64; 3] = [60 * 60, 6 * 60 * 60, 24 * 60 * 60];

/// One ongoing alert for a `(agent, schedule, event)` triple.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AlertEpisode {
    /// Unix seconds of the first notification in this episode. Carried so a
    /// message can say how long this has been going on.
    pub first_notified_at: i64,
    pub last_notified_at: i64,
    /// Notifications sent so far, counting the first.
    pub count: u32,
}

/// Is this alert allowed to speak at `now`?
///
/// `None` means nothing has been said yet — entering the condition always
/// notifies, which is the one send that must never be delayed.
///
/// Pure on purpose: this is the whole policy, and it should be provable without
/// a filesystem or a clock.
pub fn should_notify(episode: Option<&AlertEpisode>, now: i64) -> bool {
    let Some(e) = episode else {
        return true;
    };
    // A clock that moved backwards (NTP correction, laptop resumed in another
    // timezone offset) would otherwise wedge the ladder shut until real time
    // caught up — days of enforced silence from a condition that is still true.
    // Erring toward one extra notification is the right side to be wrong on.
    if now < e.last_notified_at {
        return true;
    }
    let step = (e.count.max(1) as usize - 1).min(BACKOFF_SECONDS.len() - 1);
    now - e.last_notified_at >= BACKOFF_SECONDS[step]
}

/// Every alert dotagent is currently repeating.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotifyDedup {
    #[serde(default)]
    pub episodes: BTreeMap<String, AlertEpisode>,
}

/// The key for one alert. Agent names and schedule ids never contain `/`, so
/// the triple round-trips as a readable path-ish string.
pub fn alert_key(agent: &str, schedule: &str, event: &str) -> String {
    format!("{agent}/{schedule}/{event}")
}

impl NotifyDedup {
    pub fn get(&self, key: &str) -> Option<&AlertEpisode> {
        self.episodes.get(key)
    }

    pub fn should_notify(&self, key: &str, now: i64) -> bool {
        should_notify(self.get(key), now)
    }

    /// Write down that the alert just fired. Returns the episode as it now
    /// stands, so callers can report how long this has been running.
    pub fn record(&mut self, key: &str, now: i64) -> AlertEpisode {
        let entry = self
            .episodes
            .entry(key.to_string())
            .or_insert(AlertEpisode {
                first_notified_at: now,
                last_notified_at: now,
                count: 0,
            });
        entry.last_notified_at = now;
        entry.count += 1;
        entry.clone()
    }

    /// End every episode for a `(agent, schedule)` pair. Called when the pair
    /// is healthy again. Returns `true` when something was actually forgotten.
    pub fn clear_pair(&mut self, agent: &str, schedule: &str) -> bool {
        let prefix = format!("{agent}/{schedule}/");
        let doomed: Vec<String> = self
            .episodes
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();
        for k in &doomed {
            self.episodes.remove(k);
        }
        !doomed.is_empty()
    }
}

/// Reader/writer for the episode table.
pub struct NotifyDedupStore {
    path: PathBuf,
}

impl NotifyDedupStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn from_home() -> Self {
        Self::new(crate::paths::state_dir().join("notify").join("alerts.json"))
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// A missing or corrupt table means "nothing has been notified yet", which
    /// costs at most one duplicate alert. Losing the alert entirely would be
    /// the expensive failure, so this never returns an error.
    pub fn load(&self) -> NotifyDedup {
        if !self.path.exists() {
            return NotifyDedup::default();
        }
        std::fs::File::open(&self.path)
            .ok()
            .and_then(|f| serde_json::from_reader(f).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, table: &NotifyDedup) -> Result<(), StateError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(table)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 60 * 60;
    const DAY: i64 = 24 * HOUR;

    fn episode(last: i64, count: u32) -> AlertEpisode {
        AlertEpisode {
            first_notified_at: 0,
            last_notified_at: last,
            count,
        }
    }

    #[test]
    fn entering_the_condition_always_notifies() {
        assert!(should_notify(None, 0));
    }

    #[test]
    fn the_same_tick_never_notifies_twice() {
        // The daemon wakes at least every 30 minutes; without this the 55-day
        // outage would have produced ~2600 identical alerts.
        assert!(!should_notify(Some(&episode(1000, 1)), 1000));
        assert!(!should_notify(Some(&episode(1000, 1)), 1000 + 29 * 60));
    }

    #[test]
    fn the_ladder_spaces_re_notifications_out() {
        // 1st send → 1h → 6h → 24h, and 24h from then on.
        assert!(should_notify(Some(&episode(0, 1)), HOUR));
        assert!(!should_notify(Some(&episode(0, 2)), HOUR));
        assert!(should_notify(Some(&episode(0, 2)), 6 * HOUR));
        assert!(!should_notify(Some(&episode(0, 3)), 6 * HOUR));
        assert!(should_notify(Some(&episode(0, 3)), DAY));
    }

    #[test]
    fn the_ladder_settles_at_daily_and_stays_there() {
        for count in [4u32, 10, 100, 5_000] {
            assert!(
                !should_notify(Some(&episode(0, count)), DAY - 1),
                "count {count} must stay quiet before a day has passed"
            );
            assert!(
                should_notify(Some(&episode(0, count)), DAY),
                "count {count} must speak again after a day"
            );
        }
    }

    #[test]
    fn a_backwards_clock_does_not_wedge_the_ladder_shut() {
        assert!(should_notify(Some(&episode(10_000, 3)), 500));
    }

    #[test]
    fn a_zero_count_episode_is_treated_as_the_first_step() {
        // Shouldn't happen — `record` always increments — but a hand-edited or
        // partially-written file must not panic on the index arithmetic.
        assert!(!should_notify(Some(&episode(0, 0)), HOUR - 1));
        assert!(should_notify(Some(&episode(0, 0)), HOUR));
    }

    #[test]
    fn recording_starts_an_episode_and_counts_up() {
        let mut t = NotifyDedup::default();
        let key = alert_key("inbox-triage", "every-90min", "stale");
        let first = t.record(&key, 100);
        assert_eq!((first.count, first.first_notified_at), (1, 100));
        let second = t.record(&key, 100 + DAY);
        assert_eq!(second.count, 2);
        // The episode start is what tells the reader how long this has run.
        assert_eq!(second.first_notified_at, 100);
        assert_eq!(second.last_notified_at, 100 + DAY);
    }

    #[test]
    fn recovery_makes_the_next_failure_loud_again() {
        let mut t = NotifyDedup::default();
        let key = alert_key("a", "daily", "stale");
        t.record(&key, 0);
        assert!(!t.should_notify(&key, HOUR - 1));
        assert!(t.clear_pair("a", "daily"));
        assert!(t.should_notify(&key, HOUR - 1));
    }

    #[test]
    fn clearing_a_pair_leaves_other_pairs_alone() {
        let mut t = NotifyDedup::default();
        t.record(&alert_key("a", "daily", "stale"), 0);
        t.record(&alert_key("a", "daily", "given_up"), 0);
        t.record(&alert_key("a", "hourly", "stale"), 0);
        t.record(&alert_key("b", "daily", "stale"), 0);

        assert!(t.clear_pair("a", "daily"));
        assert_eq!(t.episodes.len(), 2);
        assert!(t.get(&alert_key("a", "hourly", "stale")).is_some());
        assert!(t.get(&alert_key("b", "daily", "stale")).is_some());
        // Idempotent: clearing a healthy pair reports no change, so the caller
        // can skip the write.
        assert!(!t.clear_pair("a", "daily"));
    }

    #[test]
    fn a_schedule_id_that_prefixes_another_is_not_cleared_by_accident() {
        let mut t = NotifyDedup::default();
        t.record(&alert_key("a", "daily", "stale"), 0);
        t.record(&alert_key("a", "daily-late", "stale"), 0);
        t.clear_pair("a", "daily");
        assert!(t.get(&alert_key("a", "daily-late", "stale")).is_some());
    }

    #[test]
    fn the_ladder_survives_a_daemon_restart() {
        // The whole point of persisting: a daemon that restarts every hour
        // would otherwise re-alert every hour.
        let dir = tempfile::tempdir().unwrap();
        let store = NotifyDedupStore::new(dir.path().join("alerts.json"));
        let key = alert_key("inbox-triage", "every-90min", "stale");

        let mut t = store.load();
        assert!(t.should_notify(&key, 1_000));
        t.record(&key, 1_000);
        store.save(&t).unwrap();

        let reloaded = store.load();
        assert!(!reloaded.should_notify(&key, 1_000 + 60));
        assert_eq!(reloaded.get(&key).unwrap().count, 1);
    }

    #[test]
    fn a_missing_table_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = NotifyDedupStore::new(dir.path().join("nope.json"));
        assert!(store.load().episodes.is_empty());
    }

    #[test]
    fn a_corrupt_table_costs_one_duplicate_alert_not_the_alert() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alerts.json");
        std::fs::write(&path, "{not json").unwrap();
        let store = NotifyDedupStore::new(path);
        assert!(store.load().should_notify("a/b/stale", 0));
        // And it recovers: the next save replaces the garbage.
        let mut t = NotifyDedup::default();
        t.record("a/b/stale", 0);
        store.save(&t).unwrap();
        assert_eq!(store.load().episodes.len(), 1);
    }

    #[test]
    fn saving_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = NotifyDedupStore::new(dir.path().join("alerts.json"));
        store.save(&NotifyDedup::default()).unwrap();
        assert!(!dir.path().join("alerts.json.tmp").exists());
    }
}
