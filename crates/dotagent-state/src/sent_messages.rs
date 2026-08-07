//! What each outbound notification was about.
//!
//! A notification says something failed. The natural next move is to reply to
//! it — "why?", "run it again" — and for that the answer has to know which run
//! the message came from.
//!
//! The pieces already existed and were thrown away: [`NotifyContext`] carries
//! `agent` and `schedule`, the Telegram API returns the id of the message it
//! just posted, and an inbound reply carries the id it answers. Nothing kept
//! the bridge between them, so a reply arrived as prose and whoever read it had
//! to guess the agent from the wording.
//!
//! [`NotifyContext`]: https://docs.rs/dotagent-notify
//!
//! ## Shape
//!
//! ```jsonc
//! {
//!   "entries": {
//!     "4821": {                       // telegram message_id
//!       "agent": "inbox-triage",
//!       "schedule": "every-90min",
//!       "event": "preflight",
//!       "at": 1785925367
//!     }
//!   }
//! }
//! ```
//!
//! Bounded on purpose. This is a lookup table for recent notifications, not a
//! history: a chat where every failure for a year is answerable is a file that
//! grows forever to serve a question nobody asks about last March.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::StateError;

/// How many notifications stay resolvable. Oldest are dropped first.
const MAX_ENTRIES: usize = 500;

/// What one outbound notification was about.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SentMessage {
    pub agent: String,
    pub schedule: String,
    /// Lifecycle event that fired it: `attempt_failed`, `given_up`,
    /// `recovered`, `preflight`, `timed_out`.
    pub event: String,
    /// Unix seconds, used only to decide what to drop when full.
    pub at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SentMessages {
    #[serde(default)]
    pub entries: BTreeMap<String, SentMessage>,
}

impl SentMessages {
    pub fn get(&self, message_id: i64) -> Option<&SentMessage> {
        self.entries.get(&message_id.to_string())
    }

    /// Record one notification, evicting the oldest when full.
    pub fn insert(&mut self, message_id: i64, entry: SentMessage) {
        self.entries.insert(message_id.to_string(), entry);
        while self.entries.len() > MAX_ENTRIES {
            // BTreeMap orders by the key's string form, which is not the
            // insertion order, so the oldest has to be found by timestamp.
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.at)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    self.entries.remove(&k);
                }
                None => break,
            }
        }
    }
}

/// Reader/writer for the correlation table.
pub struct SentMessageStore {
    path: PathBuf,
}

impl SentMessageStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn from_home() -> Self {
        Self::new(crate::paths::telegram_sent_file())
    }

    /// An unreadable or corrupt table costs correlation, never delivery, so
    /// this never fails — it returns an empty table and the next write
    /// replaces the garbage.
    pub fn load(&self) -> SentMessages {
        if !self.path.exists() {
            return SentMessages::default();
        }
        std::fs::File::open(&self.path)
            .ok()
            .and_then(|f| serde_json::from_reader(f).ok())
            .unwrap_or_default()
    }

    /// Record what a message was about.
    pub fn record(&self, message_id: i64, entry: SentMessage) -> Result<(), StateError> {
        let mut table = self.load();
        table.insert(message_id, entry);
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&table)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Resolve a message id back to the run that produced it.
    pub fn resolve(&self, message_id: i64) -> Option<SentMessage> {
        self.load().get(message_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(agent: &str, at: i64) -> SentMessage {
        SentMessage {
            agent: agent.into(),
            schedule: "daily".into(),
            event: "given_up".into(),
            at,
        }
    }

    #[test]
    fn records_and_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let store = SentMessageStore::new(dir.path().join("sent.json"));
        store.record(4821, entry("inbox-triage", 100)).unwrap();

        let found = store.resolve(4821).expect("recorded id must resolve");
        assert_eq!(found.agent, "inbox-triage");
        assert_eq!(found.event, "given_up");
    }

    #[test]
    fn an_unknown_id_resolves_to_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = SentMessageStore::new(dir.path().join("sent.json"));
        assert!(store.resolve(1).is_none());
    }

    #[test]
    fn a_missing_file_is_an_empty_table_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = SentMessageStore::new(dir.path().join("nope.json"));
        assert!(store.load().entries.is_empty());
    }

    #[test]
    fn a_corrupt_file_costs_correlation_not_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sent.json");
        std::fs::write(&path, "{not json").unwrap();
        let store = SentMessageStore::new(path);
        assert!(store.load().entries.is_empty());
        // And it recovers: the next write replaces the garbage.
        store.record(7, entry("x", 1)).unwrap();
        assert!(store.resolve(7).is_some());
    }

    #[test]
    fn the_table_stays_bounded_and_drops_the_oldest() {
        let mut table = SentMessages::default();
        for i in 0..(MAX_ENTRIES as i64 + 10) {
            table.insert(i, entry("a", i));
        }
        assert_eq!(table.entries.len(), MAX_ENTRIES);
        // The first ten are the oldest by timestamp, so they are the ones gone.
        assert!(table.get(0).is_none());
        assert!(table.get(9).is_none());
        assert!(table.get(MAX_ENTRIES as i64 + 9).is_some());
    }

    #[test]
    fn re_recording_an_id_overwrites_rather_than_duplicates() {
        let mut table = SentMessages::default();
        table.insert(1, entry("first", 10));
        table.insert(1, entry("second", 20));
        assert_eq!(table.entries.len(), 1);
        assert_eq!(table.get(1).unwrap().agent, "second");
    }
}
